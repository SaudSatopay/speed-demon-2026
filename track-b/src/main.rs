//! Track B (Index): boolean and phrase queries over a 254 MB corpus.
//!
//! The queries are read first, so only their terms are ever looked up. Every document matching a
//! BOOL query contains one of a few anchor terms (the rarest AND of its last run, or its OR terms),
//! and for nine queries in ten the anchors are rare, so the query is answered during the scan: at
//! each anchor's first occurrence in a document, the bits of what the document holds index a truth
//! table, and no posting of any of its terms is ever stored. Only queries anchored on a common term
//! read postings afterwards: bitmaps for their common terms, set during the scan, and chains of 2-byte
//! offsets for their other terms. Each occurrence of a phrase's rarest term checks the phrase around it
//! when the document ends. Threads scan contiguous ranges of the corpus a document at a time in stages
//! without a branch per token: find the tokens, look them up, mark them, then act on what the marks
//! listed. Then they answer the queries in batches, appending their answers to the file in order.

use std::arch::x86_64::*;
use std::fs::File;
use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering::*};
use std::thread;

/// Term 0 is every token that no query uses, and an empty slot of the term table holds it. No token
/// maps to this term, which fills the unused slots of a BOOL check.
const ABSENT: u32 = 1;
/// Real terms start here, at a word of a document's bits: those with bitmaps first, then those with
/// chains, then the rest.
const FIRST_TERM: usize = 64;
/// The low bits of a slot's word are its term's number...
const TERM_BITS: u32 = (1 << 24) - 1;
/// ...and the top bits say what else to do. A term's phrases are checked around each occurrence...
const PHRASE_ANCHOR: u32 = 1 << 31;
/// ...and its BOOL queries are checked for each document that contains it.
const BOOL_ANCHOR: u32 = 1 << 30;
/// No query or document has this number.
const NONE: u32 = u32::MAX;
/// The commonest terms sit in a table of this many sets, which stays in the fastest cache.
const HOT_SETS: usize = 1 << 8;
/// How many ranges of the corpus there are per thread.
const RANGES_PER_THREAD: usize = 16;
/// How many queries a thread claims at a time.
const BATCH: usize = 256;
/// How much of the corpus the sample reads, about 1%.
const SAMPLE_BYTES: usize = 1 << 21;
/// How many of a query's matching documents are printed.
const SMALLEST: usize = 10;

/// Readable zero bytes after each mapped file, so that block loads can run past its end.
const PADDING: usize = 4096;

// Declared by hand to keep the crate at zero dependencies; std already links glibc.
unsafe extern "C" {
    fn mmap(at: *mut u8, len: usize, p: i32, f: i32, fd: i32, o: i64) -> *mut u8;
}

/// Maps a file read-only for the life of the process, followed by `PADDING` readable zero bytes. The
/// scorer's memory peak doesn't charge us for these pages, unlike reading the file onto the heap.
fn map_file(path: &Path) -> io::Result<&'static [u8]> {
    let (read, private, fixed, anonymous) = (1, 2, 0x10, 0x20);
    let file = File::open(path)?;
    let len = file.metadata()?.len() as usize;
    // SAFETY: the first call reserves fresh zero pages, and the second maps the file over the start
    // of them, which fails only on an empty file. Neither is ever unmapped, and nothing writes to the
    // dataset while we run.
    unsafe {
        let null = std::ptr::null_mut();
        let base = mmap(null, len + PADDING, read, private | anonymous, -1, 0);
        let mapped = mmap(base, len, read, private | fixed, file.as_raw_fd(), 0);
        assert!(base as isize != -1 && (mapped == base || len == 0));
        Ok(std::slice::from_raw_parts(base.cast(), len))
    }
}

/// One bit per byte of the 64 at `pos`, set for the token bytes, `[A-Za-z0-9]`, and likewise for the
/// LFs. Bytes of 0x80 and above compare as negative, so they are separators, even inside a letter
/// (spec section 2.2).
fn block_masks(file: &[u8], pos: usize) -> (u64, u64) {
    assert!(pos + 64 <= file.len() + PADDING);
    // SAFETY: the assert keeps the loads inside the mapping, they need no alignment, and the build
    // targets x86-64-v3, which has AVX2.
    unsafe {
        let byte_set = |byte: u8| _mm256_set1_epi8(byte as i8);
        let within = |value, low: u8, high: u8| {
            let above = _mm256_cmpgt_epi8(value, byte_set(low - 1));
            _mm256_and_si256(above, _mm256_cmpgt_epi8(byte_set(high + 1), value))
        };
        let half = |at: usize| {
            let bytes = _mm256_loadu_si256(file.as_ptr().add(at).cast());
            let lower = _mm256_or_si256(bytes, byte_set(0x20));
            let tokens = _mm256_or_si256(within(lower, b'a', b'z'), within(bytes, b'0', b'9'));
            let line_feeds = _mm256_cmpeq_epi8(bytes, byte_set(b'\n'));
            let bits = |mask| u64::from(_mm256_movemask_epi8(mask) as u32);
            (bits(tokens), bits(line_feeds))
        };
        let (low, high) = (half(pos), half(pos + 32));
        (low.0 | high.0 << 32, low.1 | high.1 << 32)
    }
}

/// Lists where each token of the line at `text` starts and ends, as offsets from `text` in pairs, and
/// returns where the line's LF is. Each block's mask of token bytes, XORed with itself shifted, has a
/// bit where a token starts or ends. Taking a fixed number of bits from it, more than a block nearly
/// ever has, avoids a mispredicted loop exit per block.
fn token_edges(file: &[u8], text: usize, edges: &mut Vec<u32>) -> usize {
    edges.clear();
    let (mut block, mut carry) = (0, 0);
    loop {
        let (tokens, line_feeds) = block_masks(file, text + block);
        let tokens = tokens & (line_feeds.wrapping_sub(1) & !line_feeds);
        let mut found = tokens ^ (tokens << 1 | carry);
        carry = tokens >> 63;
        let count = found.count_ones() as usize;
        edges.reserve(64);
        let mut take = |out: *mut u32| {
            // SAFETY: the reserve leaves room for 64 more, and a block has at most 64 edges.
            unsafe { out.write((block + found.trailing_zeros() as usize) as u32) };
            found &= found.wrapping_sub(1);
        };
        let out = edges.as_mut_ptr().wrapping_add(edges.len());
        for i in 0..24 {
            take(out.wrapping_add(i));
        }
        for i in 24..count {
            take(out.wrapping_add(i));
        }
        // SAFETY: the first `count` of those are written.
        unsafe { edges.set_len(edges.len() + count) };
        if line_feeds != 0 {
            return text + block + line_feeds.trailing_zeros() as usize;
        }
        block += 64;
    }
}

/// `SD_THREADS` if set (the test sweep uses it), else what the container allows, capped at the
/// box's 12 threads.
fn thread_count() -> usize {
    let asked = std::env::var("SD_THREADS").map_or(0, |v| v.parse().unwrap_or(0));
    let allowed = || thread::available_parallelism().map_or(1, |n| n.get().min(12));
    Some(asked).filter(|&n| n > 0).unwrap_or_else(allowed)
}

/// Runs `work` on `threads` threads, and waits for them all.
fn on_threads(threads: usize, work: impl Fn() + Sync) {
    thread::scope(|scope| {
        for _ in 0..threads {
            scope.spawn(&work);
        }
    });
}

/// A token's key, in the low 96 bits: its first 12 bytes, lowercased and zero-padded. Setting this bit
/// in every byte lowercases ASCII letters, and digits already have it set. A longer token, which is
/// rare, also sets the top bit, which no shorter token's key has, and its slot needs a full compare.
fn token_key(file: &[u8], start: usize, len: usize) -> u128 {
    const LOWER: u64 = 0x2020_2020_2020_2020;
    let kept = 8 * len.min(12) as u32;
    // SAFETY: callers pass a token's start, and the mapping continues for `PADDING` bytes past the
    // file. The loads need no alignment, and the build targets x86-64-v3, which has BMI2, whose BZHI
    // keeps the given number of low bits, or all of them if that's more than the width.
    unsafe {
        let at = file.as_ptr().add(start);
        let low = at.cast::<u64>().read_unaligned();
        let high = at.add(8).cast::<u32>().read_unaligned();
        let high = _bzhi_u32(high | LOWER as u32, kept.saturating_sub(64));
        u128::from(_bzhi_u64(low | LOWER, kept))
            | u128::from(high | u32::from(len > 12) << 31) << 64
    }
}

/// `short_key(token_key(..))`, without the load of the rest.
fn short_token_key(file: &[u8], start: usize, len: usize) -> u64 {
    // SAFETY: as in `token_key`.
    unsafe {
        let bytes = file.as_ptr().add(start).cast::<u64>().read_unaligned();
        _bzhi_u64(bytes | 0x2020_2020_2020_2020, 8 * len.min(9) as u32) | u64::from(len > 8) << 63
    }
}

/// The first 8 bytes of a token's key, with the top bit set if the token is longer, which no key's bytes
/// have. A token that fits has no other bits in its key, so this tells it from all others.
fn short_key(key: u128) -> u64 {
    key as u64 | u64::from((key >> 64) as u32 != 0) << 63
}

/// Mixes a short key with one multiply, whose high bits depend on every input bit. A test build swaps
/// it for a constant, so that every key collides and any logic that trusts a hash without comparing the
/// key shows up as a wrong answer.
fn hash(short_key: u64) -> u64 {
    let hash = short_key.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    if cfg!(sd_const_hash) { 0 } else { hash }
}

/// A slot of a term table is a u128, four to a cache line: a key in the low 96 bits, or else 0, and in
/// the top 32 its term's number and flags.
fn word_of(slot: u128) -> u32 {
    (slot >> 96) as u32
}

/// Which slot of a table of `slots` slots, a power of two, a hash starts at: its top bits.
fn home(slots: usize, hash: u64) -> usize {
    (hash >> (64 - slots.trailing_zeros())) as usize
}

/// Four short keys, compared at once, and their terms' words, in one cache line.
#[derive(Clone, Copy, Default)]
#[repr(align(64))]
struct HotSet {
    keys: [u64; 4],
    words: [u32; 4],
}

/// Query terms to dense ids, by open addressing with linear probing. A hit always compares the full
/// key: terms longer than the key's 12 bytes are checked against the query file.
struct TermTable<'a> {
    slots: Vec<u128>, // The length is a power of two.
    hot: Vec<HotSet>, // The commonest terms of each hash that fit a short key, chosen by the plan.
    keys: Vec<&'a [u8]>,
}

impl<'a> TermTable<'a> {
    /// Sized for `query_bytes` of query text, where 16 bytes hold about one distinct term.
    fn new(query_bytes: usize) -> Self {
        TermTable {
            slots: vec![0; (query_bytes / 8).next_power_of_two().max(1 << 12)],
            hot: vec![HotSet::default(); HOT_SETS],
            keys: vec![&[]; FIRST_TERM],
        }
    }

    /// The slot of the term for the token at `start..end` of `file`, in any letter case, or else the
    /// empty slot that ends its probe, whose word is 0, `NO_TERM`.
    fn find(&self, file: &[u8], start: usize, end: usize, key: u128) -> u128 {
        let mask = self.slots.len() - 1;
        let mut index = home(self.slots.len(), hash(short_key(key)));
        loop {
            let slot = self.slots[index];
            let differs =
                (slot as u64 ^ key as u64) | u64::from((slot >> 64) as u32 ^ (key >> 64) as u32);
            // The term, or the empty slot, unless a term longer than the key disagrees past it.
            if differs.min(slot as u64) == 0
                && (key >> 95 & 1 == 0 || slot == 0 || self.long_term_is(slot, &file[start..end]))
            {
                return slot;
            }
            index = (index + 1) & mask;
        }
    }

    /// Whether a term longer than the key's 12 bytes, whose key matches, is this token.
    #[cold]
    fn long_term_is(&self, slot: u128, token: &[u8]) -> bool {
        token.eq_ignore_ascii_case(self.keys[(word_of(slot) & TERM_BITS) as usize])
    }

    /// Adds the query term at `start..end` of `file`, if it's new, and returns its id.
    fn intern(&mut self, file: &'a [u8], start: usize, end: usize) -> u32 {
        let key = token_key(file, start, end - start);
        let slot = self.find(file, start, end, key);
        if slot != 0 {
            return word_of(slot);
        }
        let term = self.keys.len() as u32;
        self.keys.push(&file[start..end]);
        place_slot(&mut self.slots, key | u128::from(term) << 96);
        if self.keys.len() * 4 > self.slots.len() * 3 {
            let mut bigger = vec![0; self.slots.len() * 2];
            for &slot in self.slots.iter().filter(|&&slot| slot != 0) {
                place_slot(&mut bigger, slot);
            }
            self.slots = bigger;
        }
        term
    }
}

fn place_slot(slots: &mut [u128], slot: u128) {
    let mask = slots.len() - 1;
    let mut index = home(slots.len(), hash(short_key(slot << 32 >> 32)));
    while slots[index] != 0 {
        index = (index + 1) & mask;
    }
    slots[index] = slot;
}

/// A term of a query, with the operator that applies it. A phrase's steps have no operator.
#[derive(Clone, Copy)]
struct Step {
    op: u8, // The first byte of AND, OR or NOT.
    term: u32,
}

struct Query {
    is_phrase: bool,
    slot: u32, // A BOOL query's number among those answered in the scan, or NONE for the rest.
    shared: u32, // The number of the term count that also goes into its answer, or NONE.
    start: u32, // Its steps are `steps[start..end]`.
    end: u32,
}

struct Queries<'a> {
    terms: TermTable<'a>,
    list: Vec<Query>,
    steps: Vec<Step>,
}

impl Queries<'_> {
    fn steps_of(&self, query: &Query) -> &[Step] {
        &self.steps[query.start as usize..query.end as usize]
    }
}

fn parse_queries(text: &[u8]) -> Queries<'_> {
    let mut terms = TermTable::new(text.len());
    let mut list = Vec::with_capacity(text.len() / 32);
    let mut steps = Vec::with_capacity(text.len() / 8);
    let mut edges = Vec::new();
    let mut pos = 0;
    while pos < text.len() {
        let line_feed = token_edges(text, pos, &mut edges);
        let start = steps.len() as u32;
        let is_phrase = text[pos] == b'P';
        // The first field is BOOL or PHRASE. Then come terms, in a BOOL query with an operator between
        // each two. Starting from the empty set, "OR t1" gives S(t1).
        let mut op = if is_phrase { 0 } else { b'O' };
        for (field, pair) in edges.chunks_exact(2).enumerate().skip(1) {
            let (from, to) = (pos + pair[0] as usize, pos + pair[1] as usize);
            if is_phrase || field % 2 == 1 {
                let term = terms.intern(text, from, to);
                steps.push(Step { op, term });
            } else {
                op = text[from];
            }
        }
        list.push(Query {
            is_phrase,
            slot: NONE,
            shared: NONE,
            start,
            end: steps.len() as u32,
        });
        pos = line_feed + 1;
    }
    Queries { terms, list, steps }
}

/// Visits the documents whose lines start in `start..end`, passing each one's id, where its text
/// starts, and its tokens' edges from there. A line belongs to the range holding its first byte.
fn for_each_doc(
    docs: &[u8],
    (start, end): (usize, usize),
    edges: &mut Vec<u32>,
    mut visit: impl FnMut(u32, usize, &[u32]),
) {
    let mut line = start;
    while line > 0 && docs[line - 1] != b'\n' {
        line += 1;
    }
    // The range is rounded to whole words of 64 documents: it starts at its first document whose id
    // is a multiple of 64 and runs to the next range's, so threads never share a bitmap word.
    let mut aligned = false;
    while line < docs.len() {
        let mut pos = line;
        let mut id = 0;
        while docs[pos] != b'\t' {
            id = id * 10 + u32::from(docs[pos] - b'0');
            pos += 1;
        }
        if id % 64 == 0 {
            if line >= end {
                return;
            }
            aligned = true;
        }
        line = token_edges(docs, pos + 1, edges) + 1;
        if aligned {
            visit(id, pos + 1, edges);
        }
    }
}

/// Terms of which every document matching a BOOL query contains at least one, chosen rare. The
/// query's last run, from its last OR (the first term counts as an OR from the empty set), is
/// intersected with each AND in it, so any of those ANDs is in every match, and so is the first
/// term when the query is a single run. A run without an AND adds its OR term to the anchors of
/// the query before it.
fn anchors(steps: &[Step], sample_df: &[u32], out: &mut Vec<u32>) {
    let last_or = steps.iter().rposition(|step| step.op == b'O').unwrap_or(0);
    let ands = steps[last_or + 1..].iter().filter(|step| step.op == b'A');
    let head = steps.first().filter(|_| last_or == 0);
    let chosen = ands.chain(head).min_by_key(|s| sample_df[s.term as usize]);
    let term = chosen.unwrap_or(&steps[last_or]).term;
    if !out.contains(&term) {
        out.push(term);
    }
    if chosen.is_none() {
        anchors(&steps[..last_or], sample_df, out);
    }
}

/// A BOOL query's check at one of its anchors. The anchor is present, so the bits of a document's
/// holding each of the query's other terms make a number from 0 to 31, and that bit of `truth` says
/// whether the document counts here: whether it matches, and holds none of the query's earlier
/// anchors, where it has already counted.
#[derive(Clone, Copy)]
struct Packed {
    truth: u32,
    terms: [u32; 5],
}

/// For each of a word's 6 index bits, the positions in the word that have it set.
const INDEX_BIT_MASKS: [u64; 6] = [
    0xAAAA_AAAA_AAAA_AAAA,
    0xCCCC_CCCC_CCCC_CCCC,
    0xF0F0_F0F0_F0F0_F0F0,
    0xFF00_FF00_FF00_FF00,
    0xFFFF_0000_FFFF_0000,
    0xFFFF_FFFF_0000_0000,
];

/// The check of a query with these steps at the given anchor, after the `earlier` ones. Each term's set
/// is a mask over the 64 possible documents, so the steps give the truth table directly, and its top half
/// is what's left with the last term, the anchor, present.
fn pack(steps: &[Step], anchor: u32, earlier: &[u32]) -> Packed {
    let (mut truth, mut blocked, mut terms, mut others) = (0, 0, [ABSENT; 5], 0);
    for step in steps {
        let mut holds = INDEX_BIT_MASKS[5];
        if step.term != anchor {
            holds = INDEX_BIT_MASKS[others];
            terms[others] = step.term;
            others += 1;
        }
        truth = match step.op {
            b'A' => truth & holds,
            b'O' => truth | holds,
            _ => truth & !holds,
        };
        if earlier.contains(&step.term) {
            blocked |= holds;
        }
    }
    Packed {
        truth: ((truth & !blocked) >> 32) as u32,
        terms,
    }
}

/// Turns each term's list size, at `sizes[term + 1]`, into where the term's list starts.
fn sum_sizes(sizes: &mut [u32]) {
    for at in 1..sizes.len() {
        sizes[at] += sizes[at - 1];
    }
}

/// What the sample decided: how each term is stored, and what to check at each term's occurrences.
struct Plan {
    bitmap_count: usize,
    list_count: usize,
    phrase_start: Vec<u32>, // A term's phrases are `phrases[phrase_start[t]..phrase_start[t + 1]]`...
    phrases: Vec<u32>, // ...each its query, the anchor's position in it, its length, its terms.
    bool_start: Vec<u32>, // Likewise the BOOL checks anchored on a term...
    bools: Vec<Packed>,
    shared_of: Vec<u32>, // ...and where a term's own count sits, after the checks' counts.
    count_query: Vec<u32>, // Which query or term count each count belongs to.
    count_slots: usize,  // Those, then one count for each term with one, then a spare.
}

fn make_plan(docs: &[u8], queries: &mut Queries) -> Plan {
    let term_count = queries.terms.keys.len();
    let (mut last_doc, mut sample_df) = (vec![NONE; term_count], vec![0u32; term_count]);
    let mut sampled_docs = 0;
    let sample = (0, docs.len().min(SAMPLE_BYTES));
    for_each_doc(docs, sample, &mut Vec::new(), |doc, text, edges| {
        sampled_docs += 1;
        for pair in edges.chunks_exact(2) {
            let (start, end) = (text + pair[0] as usize, text + pair[1] as usize);
            let key = token_key(docs, start, end - start);
            let term = word_of(queries.terms.find(docs, start, end, key)) as usize;
            if last_doc[term] != doc {
                last_doc[term] = doc;
                sample_df[term] += 1;
            }
        }
    });

    // A query anchored on a common term, with df above about N/48, is answered from postings: bitmaps
    // for its common terms and chains for the others. On memory alone, a bitmap (N/8 bytes) and a
    // chain (about 3.3 bytes per posting) cross near df = N/26, but setting a bit is cheaper than
    // extending a chain, and measuring put the best K near N/48. Any choice gives the same answers.
    let common = |term: u32| sample_df[term as usize] * 32 >= sampled_docs;
    const BITMAP: u32 = 1 << 31;
    let mut store_of = vec![NONE; term_count];
    let (mut bitmap_count, mut list_count, mut scan_count) = (0, 0, 0);
    // Each query answered in the scan's anchors, then NONE.
    let (mut query_anchors, mut these) = (Vec::with_capacity(queries.list.len()), Vec::new());
    let Queries { list, steps, terms } = queries;
    for query in list.iter_mut().filter(|query| !query.is_phrase) {
        let steps = &steps[query.start as usize..query.end as usize];
        these.clear();
        anchors(steps, &sample_df, &mut these);
        if these.iter().any(|&term| common(term)) {
            for step in steps {
                let store = &mut store_of[step.term as usize];
                if *store == NONE && common(step.term) {
                    bitmap_count += 1;
                    *store = BITMAP | (bitmap_count - 1);
                } else if *store == NONE {
                    list_count += 1;
                    *store = list_count - 1;
                }
            }
        } else {
            query.slot = scan_count;
            scan_count += 1;
            // After the last AND or NOT, each OR's term alone makes a document match. The commonest
            // of them becomes the first anchor, needing no check: the term's own count serves every
            // query so anchored, and the other anchors skip the documents that hold it.
            let trailing = steps.iter().rposition(|step| step.op != b'O');
            let trailing = &steps[trailing.map_or(0, |at| at + 1)..];
            let sure = (0..these.len())
                .filter(|&at| trailing.iter().any(|step| step.term == these[at]))
                .max_by_key(|&at| sample_df[these[at] as usize]);
            if let Some(at) = sure {
                these.swap(0, at);
                query.shared = 0;
            }
            query_anchors.extend_from_slice(&these);
            query_anchors.push(NONE);
        }
    }

    // Number the terms: those with bitmaps, then those with chains, then the rest, the commonest first
    // so that what the scan touches most sits together.
    let bucket = |term: usize| 255 - sample_df[term].min(255) as usize;
    let mut next = [0u32; 257];
    let chain_from = FIRST_TERM as u32 + bitmap_count.next_multiple_of(64);
    next[0] = chain_from + list_count;
    for term in (FIRST_TERM..term_count).filter(|&term| store_of[term] == NONE) {
        next[bucket(term) + 1] += 1;
    }
    sum_sizes(&mut next);
    let number = |(term, &store): (usize, &u32)| match store {
        NONE if term < FIRST_TERM => term as u32,
        NONE => {
            next[bucket(term)] += 1;
            next[bucket(term)] - 1
        }
        bitmap if bitmap & BITMAP != 0 => FIRST_TERM as u32 + (bitmap & !BITMAP),
        list => chain_from + list,
    };
    let new_number: Vec<u32> = store_of.iter().enumerate().map(number).collect();
    // Terms with chains start at a word of a document's bits, which may leave some numbers unused.
    let term_count = term_count + 64;
    let (old_df, old_keys) = (sample_df.clone(), terms.keys.clone());
    sample_df.resize(term_count, 0);
    terms.keys.resize(term_count, &[]);
    for (term, &new) in new_number.iter().enumerate() {
        sample_df[new as usize] = old_df[term];
        terms.keys[new as usize] = old_keys[term];
    }
    for step in steps.iter_mut() {
        step.term = new_number[step.term as usize];
    }

    let (mut phrase_start, mut bool_start) =
        (vec![0u32; term_count + 1], vec![0u32; term_count + 1]);
    let mut shared_of = vec![NONE; term_count];
    let mut shared_count = 0;
    let mut at_anchor = 0;
    for query in list.iter_mut().filter(|query| query.slot != NONE) {
        let these = &mut query_anchors[at_anchor..];
        if query.shared != NONE {
            let first = new_number[these[0] as usize] as usize;
            if shared_of[first] == NONE {
                shared_of[first] = shared_count;
                shared_count += 1;
            }
            query.shared = scan_count + shared_of[first];
        }
        let mut count = 0;
        while these[count] != NONE {
            these[count] = new_number[these[count] as usize];
            if count > 0 || query.shared == NONE {
                bool_start[these[count] as usize + 1] += 1;
            }
            count += 1;
        }
        at_anchor += count + 1;
    }
    // Any term of a phrase would be correct as its anchor. The rarest triggers the fewest checks.
    let rarest = |query: &Query| {
        let steps = &steps[query.start as usize..query.end as usize];
        (0..steps.len())
            .min_by_key(|&i| sample_df[steps[i].term as usize])
            .unwrap_or(0)
    };
    for query in list.iter().filter(|query| query.is_phrase) {
        let term = steps[query.start as usize + rarest(query)].term as usize;
        phrase_start[term + 1] += 3 + query.end - query.start;
    }
    sum_sizes(&mut phrase_start);
    sum_sizes(&mut bool_start);
    let mut phrases = vec![0u32; phrase_start[term_count] as usize];
    let filler = Packed {
        truth: 0,
        terms: [ABSENT; 5],
    };
    let check_count = bool_start[term_count];
    let mut bools = vec![filler; check_count as usize];
    let mut count_query = vec![0u32; (check_count + shared_count + 1) as usize];
    let scan_slots = scan_count + shared_count;
    count_query[check_count as usize..]
        .iter_mut()
        .enumerate()
        .for_each(|(at, slot)| *slot = scan_count + at as u32);
    let (mut next_phrase, mut next_bool) = (phrase_start.clone(), bool_start.clone());
    let mut at_anchor = 0;
    for (index, query) in list.iter().enumerate() {
        let steps = &steps[query.start as usize..query.end as usize];
        if query.is_phrase {
            let position = rarest(query);
            let at = &mut next_phrase[steps[position].term as usize];
            let entry = &mut phrases[*at as usize..][..3 + steps.len()];
            entry[..3].copy_from_slice(&[index as u32, position as u32, steps.len() as u32]);
            for (slot, step) in entry[3..].iter_mut().zip(steps) {
                *slot = step.term;
            }
            *at += entry.len() as u32;
        } else if query.slot != NONE {
            let these = &query_anchors[at_anchor..];
            let count = these.iter().position(|&term| term == NONE).unwrap_or(0);
            let unchecked = usize::from(query.shared != NONE);
            for (k, &term) in these[..count].iter().enumerate().skip(unchecked) {
                let at = &mut next_bool[term as usize];
                bools[*at as usize] = pack(steps, term, &these[..k]);
                count_query[*at as usize] = query.slot;
                *at += 1;
            }
            at_anchor += count + 1;
        }
    }
    let starts = |start: &[u32], term: usize| start[term] != start[term + 1];
    // Each term's count, or the spare count for a term without one, which nothing reads.
    for shared in shared_of.iter_mut() {
        *shared = check_count + (*shared).min(shared_count);
    }
    let mut hot_df = vec![[0u32; 4]; HOT_SETS];
    for slot in terms.slots.iter_mut().filter(|slot| **slot != 0) {
        let term = new_number[word_of(*slot) as usize] as usize;
        let word = term as u32
            | u32::from(starts(&phrase_start, term)) * PHRASE_ANCHOR
            | u32::from(starts(&bool_start, term) || shared_of[term] != check_count + shared_count)
                * BOOL_ANCHOR;
        let key = *slot << 32 >> 32;
        *slot = key | u128::from(word) << 96;
        // The four commonest terms of each hot set. A longer key always misses, and then gets compared.
        let short = short_key(key);
        let set = home(HOT_SETS, hash(short));
        let dfs = &mut hot_df[set];
        let least = (0..4).min_by_key(|&way| dfs[way]).unwrap_or(0);
        if sample_df[term] > dfs[least] && short >> 63 == 0 {
            dfs[least] = sample_df[term];
            terms.hot[set].keys[least] = short;
            terms.hot[set].words[least] = word;
        }
    }
    Plan {
        bitmap_count: bitmap_count as usize,
        list_count: list_count as usize,
        phrase_start,
        phrases,
        bool_start,
        bools,
        shared_of,
        count_query,
        count_slots: (scan_slots + 1) as usize,
    }
}

/// Postings per chain block: 32 bytes hold a link, a base and 12 document offsets.
const BLOCK_POSTINGS: usize = 12;

/// A piece of one term's posting list. Chains run from the newest block back to the oldest.
#[derive(Clone, Copy)]
struct Block {
    previous: u32,  // The previous block's number + 1, or 0 if this is the first.
    first_doc: u32, // Its postings are offsets from this, its first document.
    postings: [u16; BLOCK_POSTINGS],
}

/// A posting list's state in one thread. Only a list's newest block can be part full.
#[derive(Clone, Copy)]
struct ListState {
    tail: u32, // The newest block's number + 1, or 0 if the list is empty.
    fill: u32,
}

/// One thread's results. A thread claims ranges in document order, so its hits ascend, though a list
/// of them still has to be merged with other threads'.
struct ScanResult {
    blocks: Vec<Block>,
    lists: Vec<ListState>,
    hits: Vec<u64>, // For each count, how many documents went into it, then the sum of their ids.
    phrase_hits: Vec<(u32, u32)>, // (query, doc)
}

/// The bits of `hits` that hold the sum of ids, which `main` checks has room.
const SUM_BITS: u32 = 40;

/// The ten smallest matching documents of a query or term count, ascending, then NONE, and the last
/// of them, which only falls, so that the documents past it need no lock. Those fill first, as threads
/// work through the corpus together, and only a thread's own first ten ever get this far.
struct Smallest {
    limit: Vec<AtomicU32>,
    docs: Vec<Mutex<[u32; SMALLEST]>>,
}

impl Smallest {
    fn add(&self, slot: usize, doc: u32) {
        if doc < self.limit[slot].load(Relaxed) {
            let mut docs = self.docs[slot].lock().unwrap();
            insert_smallest(&mut docs, doc);
            if docs[SMALLEST - 1] != NONE {
                self.limit[slot].store(docs[SMALLEST - 1], Relaxed);
            }
        }
    }
}

/// Puts a document among a list's ten smallest.
fn insert_smallest(smallest: &mut [u32; SMALLEST], doc: u32) {
    let mut carry = doc;
    for held in smallest.iter_mut() {
        if carry < *held {
            std::mem::swap(held, &mut carry);
        }
    }
}

/// Claims ranges of the corpus until none is left, and scans them.
fn scan_ranges(
    docs: &[u8],
    range_size: usize,
    next_range: &AtomicUsize,
    queries: &Queries,
    plan: &Plan,
    bitmaps: &[AtomicU64],
    words: usize,
    smallest: &Smallest,
) -> ScanResult {
    let terms = &queries.terms;
    let mut result = ScanResult {
        blocks: Vec::new(),
        lists: vec![ListState { tail: 0, fill: 0 }; plan.list_count],
        hits: vec![0; plan.count_query.len()],
        phrase_hits: Vec::new(),
    };
    // A bit per term, set while the current document holds it.
    let mut present = vec![0u64; terms.keys.len().div_ceil(64)];
    let bitmap_count = plan.bitmap_count;
    let chain_from = (FIRST_TERM + bitmap_count.next_multiple_of(64)) as u32;
    let chain_count = plan.list_count as u32;
    let hot: &[HotSet; HOT_SETS] = terms.hot.as_slice().try_into().unwrap();
    let table_shift = 64 - terms.slots.len().trailing_zeros();
    let mut edges = Vec::new();
    let (mut tokens, mut misses) = (Vec::new(), Vec::new());
    let (mut phrase_at, mut bool_terms, mut chain_terms) = (Vec::new(), Vec::new(), Vec::new());
    loop {
        let start = next_range.fetch_add(range_size, Relaxed);
        if start >= docs.len() {
            return result;
        }
        for_each_doc(
            docs,
            (start, start + range_size),
            &mut edges,
            |doc, text, edges| {
                let (doc_word, doc_bit) = (doc as usize / 64, 1u64 << (doc % 64));
                let count = edges.len() / 2;
                if tokens.len() <= count {
                    tokens.resize(count + 1, 0);
                    misses.resize(count + 1, 0);
                    phrase_at.resize(count + 1, 0);
                    bool_terms.resize(count + 1, 0);
                    chain_terms.resize(count + 1, 0);
                }
                // Look the tokens up without a branch on whether the hot table holds them, prefetching the
                // main table's slot for those it doesn't, so that their cache misses overlap.
                let mut missed = 0;
                for (at, pair) in edges.chunks_exact(2).enumerate() {
                    let (start, end) = (pair[0] as usize, pair[1] as usize);
                    let key = short_token_key(docs, text + start, end - start);
                    let hash = hash(key);
                    let set = &hot[(hash >> (64 - HOT_SETS.trailing_zeros())) as usize];
                    // SAFETY: the build targets x86-64-v3, which has AVX2. A set's keys are 32 bytes, its
                    // first field in an aligned struct. There's room for every token in both lists, `missed`
                    // is at most `at`, and the prefetched slot is in the table.
                    unsafe {
                        let keys = _mm256_load_si256(set.keys.as_ptr().cast());
                        let same = _mm256_cmpeq_epi64(keys, _mm256_set1_epi64x(key as i64));
                        let ways = _mm256_movemask_pd(_mm256_castsi256_pd(same)) as u32;
                        let miss = usize::from(ways == 0);
                        let target = (hash >> table_shift) as usize & 0usize.wrapping_sub(miss);
                        *tokens.get_unchecked_mut(at) =
                            *set.words.get_unchecked(ways.trailing_zeros() as usize & 3);
                        *misses.get_unchecked_mut(missed) = at as u32;
                        _mm_prefetch(terms.slots.as_ptr().add(target).cast(), _MM_HINT_T0);
                        missed += miss;
                    }
                }
                for &at in &misses[..missed] {
                    let at = at as usize;
                    let (start, end) = (
                        text + edges[2 * at] as usize,
                        text + edges[2 * at + 1] as usize,
                    );
                    tokens[at] =
                        word_of(terms.find(docs, start, end, token_key(docs, start, end - start)));
                }
                // Mark what the document holds, and list what to do about it at the end.
                let (mut phrases, mut bools, mut chains) = (0, 0, 0);
                for (at, &word) in tokens[..count].iter().enumerate() {
                    let term = word & TERM_BITS;
                    let (index, bit) = (term as usize / 64, 1u64 << (term % 64));
                    // SAFETY: `present` has a bit for every term, and each list has room for every token.
                    unsafe {
                        let held = *present.get_unchecked(index);
                        *present.get_unchecked_mut(index) = held | bit;
                        let first = usize::from(held & bit == 0);
                        *phrase_at.get_unchecked_mut(phrases) = at as u32;
                        phrases += (word >> 31) as usize;
                        *bool_terms.get_unchecked_mut(bools) = term;
                        bools += (word >> 30 & 1) as usize & first;
                        *chain_terms.get_unchecked_mut(chains) = term;
                        chains += usize::from(term.wrapping_sub(chain_from) < chain_count) & first;
                    }
                }
                // No other thread writes this document's bitmap words, so setting a bit takes no atomic.
                for (index, &word) in present[1..=bitmap_count.div_ceil(64)].iter().enumerate() {
                    let mut rest = word;
                    while rest != 0 {
                        let at = (index * 64 + rest.trailing_zeros() as usize) * words + doc_word;
                        bitmaps[at].store(bitmaps[at].load(Relaxed) | doc_bit, Relaxed);
                        rest &= rest - 1;
                    }
                }
                for &term in &chain_terms[..chains] {
                    let list = &mut result.lists[(term - chain_from) as usize];
                    // A tail of 0 gets a new block too: then `fill` is 0, and there's no block 0 to read.
                    let full = list.tail == 0
                        || list.fill as usize == BLOCK_POSTINGS
                        || doc - result.blocks[list.tail as usize - 1].first_doc
                            > u32::from(u16::MAX);
                    if full {
                        result.blocks.push(Block {
                            previous: list.tail,
                            first_doc: doc,
                            postings: [0; BLOCK_POSTINGS],
                        });
                        list.tail = result.blocks.len() as u32;
                        list.fill = 0;
                    }
                    let block = &mut result.blocks[list.tail as usize - 1];
                    block.postings[list.fill as usize] = (doc - block.first_doc) as u16;
                    list.fill += 1;
                }
                let in_doc = &tokens[..count];
                for &at in &phrase_at[..phrases] {
                    let at = at as usize;
                    let term = (in_doc[at] & TERM_BITS) as usize;
                    let from = plan.phrase_start[term] as usize;
                    let mut entries = &plan.phrases[from..plan.phrase_start[term + 1] as usize];
                    while let [query, position, len, rest @ ..] = entries {
                        let (phrase, after) = rest.split_at(*len as usize);
                        entries = after;
                        let Some(start) = at.checked_sub(*position as usize) else {
                            continue;
                        };
                        let run = in_doc.get(start..start + phrase.len());
                        let same =
                            |run: &[u32]| run.iter().zip(phrase).all(|(&t, &p)| t & TERM_BITS == p);
                        // An anchor can occur twice in a document, but the document counts once.
                        let hits = &mut result.phrase_hits;
                        let mut same_doc = hits.iter().rev().take_while(|hit| hit.1 == doc);
                        if run.is_some_and(same) && !same_doc.any(|hit| hit.0 == *query) {
                            hits.push((*query, doc));
                        }
                    }
                }
                let this_doc = (1 << SUM_BITS) + u64::from(doc);
                for &term in &bool_terms[..bools] {
                    let term = term as usize;
                    let shared = plan.shared_of[term] as usize;
                    result.hits[shared] += this_doc;
                    // A thread's hits ascend, so only its first ten can be among the smallest.
                    if result.hits[shared] >> SUM_BITS <= SMALLEST as u64 {
                        smallest.add(plan.count_query[shared] as usize, doc);
                    }
                    let (from, to) = (
                        plan.bool_start[term] as usize,
                        plan.bool_start[term + 1] as usize,
                    );
                    let checks = plan.bools[from..to].iter().zip(&mut result.hits[from..to]);
                    for (at, (packed, hits)) in checks.enumerate() {
                        let mut holds = 0;
                        for (slot, &term) in packed.terms.iter().enumerate() {
                            // SAFETY: `present` has a bit for every term.
                            let word = unsafe { *present.get_unchecked(term as usize / 64) };
                            holds |= (word >> (term % 64) & 1) << slot;
                        }
                        let counts = u64::from(packed.truth >> holds & 1);
                        *hits += counts * this_doc;
                        if counts != 0 && *hits >> SUM_BITS <= SMALLEST as u64 {
                            smallest.add(plan.count_query[from + at] as usize, doc);
                        }
                    }
                }
                for &word in in_doc {
                    present[(word & TERM_BITS) as usize / 64] = 0;
                }
            },
        );
    }
}

/// Everything the queries read.
struct Index<'a> {
    bitmap_count: usize,
    bitmaps: Vec<u64>,
    words: usize,
    scans: &'a [ScanResult],
    hit_start: Vec<u32>, // A phrase query's documents are `hits[hit_start[q]..hit_start[q + 1]]`.
    hits: Vec<u32>,
    counts: Vec<u64>, // A BOOL query's matches and their sum, for queries answered in the scan.
    sums: Vec<u64>,
    smallest: Smallest,
}

impl Index<'_> {
    /// Sets the bits of a posting list's documents.
    fn set_bits_of_list(&self, list: usize, bits: &mut [u64]) {
        for scan in self.scans {
            let (mut tail, mut fill) = (scan.lists[list].tail, scan.lists[list].fill as usize);
            while tail != 0 {
                let block = &scan.blocks[tail as usize - 1];
                for &offset in &block.postings[..fill] {
                    let doc = block.first_doc + u32::from(offset);
                    bits[doc as usize / 64] |= 1 << (doc % 64);
                }
                (tail, fill) = (block.previous, BLOCK_POSTINGS);
            }
        }
    }
}

/// Collects each phrase query's documents.
fn collect_hits(results: &[ScanResult], query_count: usize) -> (Vec<u32>, Vec<u32>) {
    let mut hit_start = vec![0u32; query_count + 1];
    let all_hits = || results.iter().flat_map(|result| &result.phrase_hits);
    for &(query, _) in all_hits() {
        hit_start[query as usize + 1] += 1;
    }
    sum_sizes(&mut hit_start);
    let mut next = hit_start.clone();
    let mut hits = vec![0; hit_start[query_count] as usize];
    for &(query, doc) in all_hits() {
        hits[next[query as usize] as usize] = doc;
        next[query as usize] += 1;
    }
    (hit_start, hits)
}

/// Sums the threads' counts and ids into each scan-answered query's and term's count and sum of ids.
fn total_hits(results: &[ScanResult], plan: &Plan) -> (Vec<u64>, Vec<u64>) {
    let (mut counts, mut sums) = (vec![0u64; plan.count_slots], vec![0u64; plan.count_slots]);
    for result in results {
        for (&query, &hits) in plan.count_query.iter().zip(&result.hits) {
            counts[query as usize] += hits >> SUM_BITS;
            sums[query as usize] += hits & ((1 << SUM_BITS) - 1);
        }
    }
    (counts, sums)
}

/// One thread's running result R for a BOOL query anchored on a common term, as a bitmap.
struct Evaluator {
    bits: Vec<u64>,
    other: Vec<u64>,
}

impl Evaluator {
    /// Applies the query's steps in order, from the empty set, with each term's set as a bitmap.
    fn run(&mut self, steps: &[Step], index: &Index) {
        self.bits.fill(0);
        for step in steps {
            let number = step.term as usize - FIRST_TERM;
            let other = if number < index.bitmap_count {
                &index.bitmaps[number * index.words..(number + 1) * index.words]
            } else {
                self.other.fill(0);
                let list = number - index.bitmap_count.next_multiple_of(64);
                index.set_bits_of_list(list, &mut self.other);
                &self.other
            };
            for (word, &with) in self.bits.iter_mut().zip(other) {
                *word = match step.op {
                    b'A' => *word & with,
                    b'O' => *word | with,
                    _ => *word & !with,
                };
            }
        }
    }

    fn print_result(&self, out: &mut Vec<u8>) {
        let (mut count, mut sum) = (0u64, 0u64);
        let mut smallest = Vec::with_capacity(SMALLEST);
        for (i, &word) in self.bits.iter().enumerate() {
            let ones = u64::from(word.count_ones());
            count += ones;
            sum += 64 * i as u64 * ones;
            // The sum of a word's ids, from the popcounts of the positions with each index bit set.
            for (bit, mask) in INDEX_BIT_MASKS.iter().enumerate() {
                sum += u64::from((word & mask).count_ones()) << bit;
            }
            let mut rest = word;
            while rest != 0 && smallest.len() < SMALLEST {
                smallest.push(64 * i as u32 + rest.trailing_zeros());
                rest &= rest - 1;
            }
        }
        print_line(out, count, sum, &smallest);
    }
}

fn push_decimal(out: &mut Vec<u8>, mut value: u64) {
    let mut digits = [0u8; 20];
    let mut start = digits.len();
    loop {
        start -= 1;
        digits[start] = b'0' + (value % 10) as u8;
        value /= 10;
        if value == 0 {
            break;
        }
    }
    out.extend_from_slice(&digits[start..]);
}

fn print_line(out: &mut Vec<u8>, count: u64, sum: u64, smallest: &[u32]) {
    push_decimal(out, count);
    out.push(b' ');
    push_decimal(out, sum);
    for &doc in smallest {
        out.push(b' ');
        push_decimal(out, u64::from(doc));
    }
    out.push(b'\n');
}

fn print_list(out: &mut Vec<u8>, docs: &[u32]) {
    let sum = docs.iter().map(|&doc| u64::from(doc)).sum();
    print_line(
        out,
        docs.len() as u64,
        sum,
        &docs[..docs.len().min(SMALLEST)],
    );
}

/// Threads claim batches of queries in order, and each waits for the batch before it to reach the
/// file. The wait is short, because batches cost about the same.
fn answer_queries(file: File, queries: &Queries, index: &Index, threads: usize) {
    let next_batch = AtomicUsize::new(0);
    let written = AtomicUsize::new(0);
    let list = &queries.list;
    on_threads(threads, || {
        let mut evaluator = Evaluator {
            bits: vec![0; index.words],
            other: vec![0; index.words],
        };
        let (mut text, mut sorted) = (Vec::new(), Vec::new());
        let claim = || Some(next_batch.fetch_add(1, Relaxed)).filter(|b| b * BATCH < list.len());
        while let Some(batch) = claim() {
            text.clear();
            for number in batch * BATCH..((batch + 1) * BATCH).min(list.len()) {
                let query = &list[number];
                if query.is_phrase {
                    let (from, to) = (index.hit_start[number], index.hit_start[number + 1]);
                    sorted.clear();
                    sorted.extend_from_slice(&index.hits[from as usize..to as usize]);
                    sorted.sort_unstable();
                    print_list(&mut text, &sorted);
                } else if query.slot == NONE {
                    evaluator.run(queries.steps_of(query), index);
                    evaluator.print_result(&mut text);
                } else {
                    let slot = query.slot as usize;
                    let (mut count, mut sum) = (index.counts[slot], index.sums[slot]);
                    let mut smallest = *index.smallest.docs[slot].lock().unwrap();
                    if query.shared != NONE {
                        let shared = query.shared as usize;
                        count += index.counts[shared];
                        sum += index.sums[shared];
                        for &doc in index.smallest.docs[shared].lock().unwrap().iter() {
                            insert_smallest(&mut smallest, doc);
                        }
                    }
                    print_line(
                        &mut text,
                        count,
                        sum,
                        &smallest[..(count as usize).min(SMALLEST)],
                    );
                }
            }
            while written.load(Acquire) != batch {
                thread::yield_now();
            }
            // `panic = "abort"`: a failed write ends the process rather than leaving the other
            // threads waiting for their turn.
            (&file).write_all(&text).expect("writing the results");
            written.store(batch + 1, Release);
        }
    });
}

/// The number of documents, from the id on the last line.
fn document_count(docs: &[u8]) -> usize {
    let Some((_, body)) = docs.split_last() else {
        return 0;
    };
    let line = body.iter().rposition(|&c| c == b'\n').map_or(0, |n| n + 1);
    let digits = body[line..].iter().take_while(|&&byte| byte != b'\t');
    digits.fold(0, |id, &digit| id * 10 + usize::from(digit - b'0')) + 1
}

fn main() -> io::Result<()> {
    let mut args = std::env::args().skip(1);
    let data_dir = args.next().unwrap_or_else(|| "/data".into());
    let out_dir = args.next().unwrap_or_else(|| "/out".into());
    let data_dir = Path::new(&data_dir);
    let threads = thread_count();
    let docs = map_file(&data_dir.join("docs.txt"))?;
    let mut queries = parse_queries(map_file(&data_dir.join("queries.txt"))?);
    let plan = make_plan(docs, &mut queries);

    let words = document_count(docs).div_ceil(64);
    assert!(
        words * 64 < 1 << (SUM_BITS / 2),
        "too many documents for the sums"
    );
    let mut bitmaps = vec![0u64; plan.bitmap_count * words];
    // One thread touches each 2 MiB, as otherwise threads racing to fault in the same huge page each
    // allocate one, which the memory peak counts.
    for word in bitmaps.iter_mut().step_by(1 << 18) {
        // SAFETY: a reference is always valid to write through.
        unsafe { std::ptr::write_volatile(word, 0) };
    }
    // SAFETY: AtomicU64 has the layout of u64, and nothing else uses `bitmaps` during the scan.
    let shared: &[AtomicU64] =
        unsafe { std::slice::from_raw_parts(bitmaps.as_ptr().cast(), bitmaps.len()) };
    let smallest = Smallest {
        limit: (0..plan.count_slots)
            .map(|_| AtomicU32::new(NONE))
            .collect(),
        docs: (0..plan.count_slots)
            .map(|_| Mutex::new([NONE; SMALLEST]))
            .collect(),
    };
    let file = File::create(Path::new(&out_dir).join("results.txt"))?;
    // Enough ranges that a thread slowed for a while doesn't hold the others up.
    let range_size = (docs.len() / (threads * RANGES_PER_THREAD)).max(1);
    let next_range = AtomicUsize::new(0);
    let results: Vec<ScanResult> = thread::scope(|scope| {
        let scan = |_| {
            let (queries, plan, smallest, next_range) = (&queries, &plan, &smallest, &next_range);
            scope.spawn(move || {
                scan_ranges(
                    docs, range_size, next_range, queries, plan, shared, words, smallest,
                )
            })
        };
        let handles: Vec<_> = (0..threads).map(scan).collect();
        handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect()
    });

    let (hit_start, hits) = collect_hits(&results, queries.list.len());
    let (counts, sums) = total_hits(&results, &plan);
    let index = Index {
        bitmap_count: plan.bitmap_count,
        bitmaps,
        words,
        scans: &results,
        hit_start,
        hits,
        counts,
        sums,
        smallest,
    };
    answer_queries(file, &queries, &index, threads);
    // Nothing is buffered, and the kernel frees all the memory faster than dropping it would.
    std::process::exit(0)
}
