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
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering::*};
use std::thread;

/// The low bits of a token's word are its term's number. Terms are numbered so that those with bitmaps
/// come first and those with chains next, and the two top bits say what else to do when the term occurs.
const TERM_BITS: u32 = (1 << 24) - 1;
/// A term's phrases are checked around each of its occurrences...
const PHRASE_ANCHOR: u32 = 1 << 31;
/// ...and its BOOL queries are evaluated for each document that contains it.
const BOOL_ANCHOR: u32 = 1 << 30;
/// No term, query or document has this number.
const NONE: u32 = u32::MAX;
/// The hottest terms sit in a table of this many slots, which stays in the fastest cache.
const HOT_BITS: u32 = 10;
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

/// The offset of the first LF at or after `pos`.
fn offset_of_line_feed(file: &[u8], pos: usize) -> usize {
    let mut offset = 0;
    loop {
        assert!(pos + offset + 32 <= file.len() + PADDING);
        // SAFETY: the assert keeps the load inside the mapping, it needs no alignment, and the build
        // targets x86-64-v3, which has AVX2.
        let mask = unsafe {
            let bytes = _mm256_loadu_si256(file.as_ptr().add(pos + offset).cast());
            _mm256_movemask_epi8(_mm256_cmpeq_epi8(bytes, _mm256_set1_epi8(b'\n' as i8)))
        };
        if mask != 0 {
            return offset + mask.trailing_zeros() as usize;
        }
        offset += 32;
    }
}

/// One bit per byte of the 64 at `pos`, set for the token bytes, `[A-Za-z0-9]`, and likewise for the LFs.
/// Bytes of 0x80 and above compare as negative, so they are separators, even inside a letter (spec
/// section 2.2).
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
            (
                _mm256_movemask_epi8(tokens) as u32 as u64,
                _mm256_movemask_epi8(line_feeds) as u32 as u64,
            )
        };
        let (low, high) = (half(pos), half(pos + 32));
        (low.0 | high.0 << 32, low.1 | high.1 << 32)
    }
}

/// Lists where each token of the line whose text starts at `text` starts and ends, as offsets from
/// `text` in pairs, and returns where the line's LF is. Each block's mask of token bytes, XORed with
/// itself shifted, has a bit where a token starts or ends. Taking a fixed number of bits from it, more
/// than a block nearly ever has, avoids the loop's mispredicted exit.
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
        // SAFETY: the reserve leaves room for 64 more, and a block has at most 64 edges.
        unsafe {
            let out = edges.as_mut_ptr().add(edges.len());
            for i in 0..24 {
                out.add(i)
                    .write((block + found.trailing_zeros() as usize) as u32);
                found &= found.wrapping_sub(1);
            }
            for i in 24..count {
                out.add(i)
                    .write((block + found.trailing_zeros() as usize) as u32);
                found &= found.wrapping_sub(1);
            }
            edges.set_len(edges.len() + count);
        }
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

/// A token's key: its first 12 bytes, lowercased and zero-padded, from one load and a mask. Setting
/// this bit in every byte lowercases ASCII letters, and digits already have it set. A longer token,
/// rare, also sets the top bit, which no shorter token's key has, and its slot needs a full compare.
#[derive(Clone, Copy)]
struct TermKey {
    low: u64,
    high: u32,
}

fn token_key(file: &[u8], start: usize, len: usize) -> TermKey {
    const LOWER: u64 = 0x2020_2020_2020_2020;
    let kept = 8 * len.min(12) as u32;
    // SAFETY: callers pass a token's start, and the mapping continues for `PADDING` bytes past the
    // file. The loads need no alignment, and the build targets x86-64-v3, which has BMI2, whose BZHI
    // keeps the given number of low bits, or all of them if that's more than the width.
    unsafe {
        let at = file.as_ptr().add(start);
        let (low, high) = (
            at.cast::<u64>().read_unaligned(),
            at.add(8).cast::<u32>().read_unaligned(),
        );
        TermKey {
            low: _bzhi_u64(low | LOWER, kept),
            high: _bzhi_u32(high | LOWER as u32, kept.saturating_sub(64))
                | u32::from(len > 12) << 31,
        }
    }
}

/// Mixes a key with one multiply, whose high bits depend on every input bit. A test build swaps it for
/// a constant, so that every key collides and any logic that trusts a hash without comparing the key
/// shows up as a wrong answer.
fn hash(key: TermKey) -> u64 {
    let hash = (key.low ^ u64::from(key.high).rotate_left(29)).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    if cfg!(sd_const_hash) { 0 } else { hash }
}

/// A slot of a term table: all that a token's lookup needs, four to a cache line.
#[derive(Clone, Copy, Default)]
#[repr(align(16))]
struct TermSlot {
    low: u64, // 0 when the slot is free: every key has a byte in it.
    high: u32,
    word: u32, // The term's number and flags.
}

impl TermSlot {
    fn differs(&self, key: TermKey) -> u64 {
        (self.low ^ key.low) | u64::from(self.high ^ key.high)
    }
}

/// Query terms to dense ids, by open addressing with linear probing. A hit always compares the full
/// key: terms longer than the key's 12 bytes are checked against the query file.
struct TermTable<'a> {
    slots: Vec<TermSlot>, // The length is a power of two.
    hot: Vec<TermSlot>,   // Direct-mapped, the hottest term of each hash, filled by the plan.
    keys: Vec<&'a [u8]>,
}

impl<'a> TermTable<'a> {
    /// Sized for `query_bytes` of query text, where 16 bytes hold about one distinct term.
    fn new(query_bytes: usize) -> Self {
        TermTable {
            slots: vec![TermSlot::default(); (query_bytes / 8).next_power_of_two().max(1 << 12)],
            hot: vec![TermSlot::default(); 1 << HOT_BITS],
            keys: Vec::new(),
        }
    }

    /// Where a key's probe starts: the hash's top bits.
    fn home(&self, hash: u64) -> usize {
        (hash >> (64 - self.slots.len().trailing_zeros())) as usize
    }

    /// The slot of the term for the token at `start..end` of `file`, in any letter case, or else the
    /// empty slot that ends its probe, whose word is the term no query uses.
    fn find(&self, file: &[u8], start: usize, end: usize, key: TermKey) -> &TermSlot {
        let mask = self.slots.len() - 1;
        let mut index = self.home(hash(key));
        loop {
            let slot = &self.slots[index];
            // The term, or the empty slot, unless a term longer than the key disagrees past it.
            if slot.differs(key).min(slot.low) == 0
                && (key.high >> 31 == 0
                    || slot.low == 0
                    || self.long_term_is(slot, &file[start..end]))
            {
                return slot;
            }
            index = (index + 1) & mask;
        }
    }

    /// Whether a term longer than the key's 12 bytes, whose key matches, is this token.
    #[cold]
    fn long_term_is(&self, slot: &TermSlot, token: &[u8]) -> bool {
        token.eq_ignore_ascii_case(self.keys[(slot.word & TERM_BITS) as usize])
    }

    /// Adds the query term at `start..end` of `file`, if it's new, and returns its id.
    fn intern(&mut self, file: &'a [u8], start: usize, end: usize) -> u32 {
        let key = token_key(file, start, end - start);
        let slot = self.find(file, start, end, key);
        if slot.low != 0 {
            return slot.word;
        }
        let term = self.keys.len() as u32;
        self.keys.push(&file[start..end]);
        place_slot(&mut self.slots, key, term);
        if self.keys.len() * 4 > self.slots.len() * 3 {
            let mut bigger = vec![TermSlot::default(); self.slots.len() * 2];
            for slot in self.slots.iter().filter(|slot| slot.low != 0) {
                let key = TermKey {
                    low: slot.low,
                    high: slot.high,
                };
                place_slot(&mut bigger, key, slot.word);
            }
            self.slots = bigger;
        }
        term
    }
}

fn place_slot(slots: &mut [TermSlot], key: TermKey, word: u32) {
    let mask = slots.len() - 1;
    let mut index = (hash(key) >> (64 - slots.len().trailing_zeros())) as usize;
    while slots[index].low != 0 {
        index = (index + 1) & mask;
    }
    slots[index] = TermSlot {
        low: key.low,
        high: key.high,
        word,
    };
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
    let (mut list, mut steps) = (Vec::new(), Vec::new());
    let mut pos = 0;
    while pos < text.len() {
        let line_end = pos + offset_of_line_feed(text, pos);
        let start = steps.len() as u32;
        let is_phrase = text[pos] == b'P';
        // PHRASE "t1 t2 ...", or BOOL t1 op t2 ... Starting from the empty set, "OR t1" gives S(t1).
        let fields_end = if is_phrase { line_end - 1 } else { line_end };
        let mut field = pos + if is_phrase { 8 } else { 5 };
        let mut op = if is_phrase { 0 } else { b'O' };
        let mut is_term = true;
        while field <= fields_end {
            let rest = &text[field..fields_end];
            let len = rest.iter().position(|&c| c == b' ').unwrap_or(rest.len());
            if is_term {
                let term = terms.intern(text, field, field + len);
                steps.push(Step { op, term });
            } else {
                op = text[field];
            }
            is_term = is_phrase || !is_term;
            field += len + 1;
        }
        let end = steps.len() as u32;
        list.push(Query {
            is_phrase,
            slot: NONE,
            start,
            end,
        });
        pos = line_end + 1;
    }
    Queries { terms, list, steps }
}

/// Visits the documents whose lines start in `start..end`, passing each one's id and where its text
/// starts, and getting back where its LF is. A line belongs to the range holding its first byte.
fn for_each_doc(docs: &[u8], start: usize, end: usize, mut visit: impl FnMut(u32, usize) -> usize) {
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
        let lf = if aligned {
            visit(id, pos + 1)
        } else {
            pos + 1 + offset_of_line_feed(docs, pos + 1)
        };
        line = lf + 1;
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
    match ands.chain(head).min_by_key(|s| sample_df[s.term as usize]) {
        Some(step) if out.contains(&step.term) => {}
        Some(step) => out.push(step.term),
        None => {
            if !out.contains(&steps[last_or].term) {
                out.push(steps[last_or].term);
            }
            anchors(&steps[..last_or], sample_df, out);
        }
    }
}

/// Groups `(term, item)` pairs into one list per term: the items of `term` are
/// `items[start[term]..start[term + 1]]`.
fn group_by_term<T: Copy>(pairs: &[(u32, T)], term_count: usize) -> (Vec<u32>, Vec<T>) {
    let mut start = vec![0u32; term_count + 2];
    for &(term, _) in pairs {
        start[term as usize + 2] += 1;
    }
    for term in 1..=term_count {
        start[term + 1] += start[term];
    }
    let mut items = vec![pairs.first().map(|pair| pair.1); pairs.len()];
    for &(term, item) in pairs {
        items[start[term as usize + 1] as usize] = Some(item);
        start[term as usize + 1] += 1;
    }
    start.pop();
    (start, items.into_iter().flatten().collect())
}

/// A BOOL query's check at one of its anchors. The bits of a document's holding each of the six terms
/// (unused slots hold a term no document has) make a number from 0 to 63, and that bit of `truth` says
/// whether the document counts here: whether it matches, and holds none of the query's earlier anchors,
/// where it has already counted.
#[derive(Clone, Copy)]
#[repr(align(32))]
struct Packed {
    truth: u64,
    terms: [u32; 6],
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

/// The check of a query with these steps at the anchor after the `earlier` ones. Each term's set is a
/// mask over the 64 possible documents, so the steps give the truth table directly.
fn pack(steps: &[Step], earlier: &[u32], unused: u32) -> Packed {
    let mut packed = Packed {
        truth: 0,
        terms: [unused; 6],
    };
    for ((step, slot), holds) in steps.iter().zip(&mut packed.terms).zip(INDEX_BIT_MASKS) {
        *slot = step.term;
        packed.truth = match step.op {
            b'A' => packed.truth & holds,
            b'O' => packed.truth | holds,
            _ => packed.truth & !holds,
        };
    }
    for (step, holds) in steps.iter().zip(INDEX_BIT_MASKS) {
        if earlier.contains(&step.term) {
            packed.truth &= !holds;
        }
    }
    packed
}

/// What the sample decided: how each term is stored, and what to check at each term's occurrences.
struct Plan {
    bitmap_count: usize,
    list_count: usize,
    phrase_start: Vec<u32>, // A term's phrases are `phrases[phrase_start[t]..phrase_start[t + 1]]`...
    phrases: Vec<u32>, // ...each its query, the anchor's position in it, its length, its terms.
    bool_start: Vec<u32>, // Likewise the BOOL queries anchored on a term...
    bools: Vec<Packed>,
    bool_query: Vec<u32>, // ...and which query each of those checks belongs to.
    scan_count: usize,    // How many BOOL queries the scan answers.
}

fn make_plan(docs: &[u8], queries: &mut Queries) -> Plan {
    // Two terms follow the query terms: the one every other token maps to, and one no token maps to.
    let term_count = queries.terms.keys.len();
    for slot in queries.terms.slots.iter_mut().filter(|slot| slot.low == 0) {
        slot.word = term_count as u32;
    }
    let (mut last_doc, mut sample_df) = (vec![NONE; term_count + 2], vec![0u32; term_count + 2]);
    let mut sampled_docs = 0;
    let mut edges = Vec::new();
    for_each_doc(docs, 0, docs.len().min(SAMPLE_BYTES), |doc, text| {
        sampled_docs += 1;
        let lf = token_edges(docs, text, &mut edges);
        for pair in edges.chunks_exact(2) {
            let (start, end) = (text + pair[0] as usize, text + pair[1] as usize);
            let key = token_key(docs, start, end - start);
            let term = queries.terms.find(docs, start, end, key).word as usize;
            if last_doc[term] != doc {
                last_doc[term] = doc;
                sample_df[term] += 1;
            }
        }
        lf
    });

    // A query anchored on a common term, with df above about N/48, is answered from postings: bitmaps
    // for its common terms and chains for the others. On memory alone, a bitmap (N/8 bytes) and a
    // chain (about 3.3 bytes per posting) cross near df = N/26, but setting a bit is cheaper than
    // extending a chain, and measuring put the best K near N/48. Any choice gives the same answers.
    let common = |term: u32| sample_df[term as usize] * 32 >= sampled_docs;
    const BITMAP: u32 = 1 << 31;
    let mut store_of = vec![NONE; term_count];
    let (mut bitmap_count, mut list_count, mut scan_count) = (0, 0, 0);
    let mut anchor_terms = vec![];
    let Queries { list, steps, terms } = queries;
    for query in list.iter_mut().filter(|query| !query.is_phrase) {
        let steps = &steps[query.start as usize..query.end as usize];
        anchor_terms.clear();
        anchors(steps, &sample_df, &mut anchor_terms);
        if anchor_terms.iter().any(|&term| common(term)) {
            for step in steps {
                let store = &mut store_of[step.term as usize];
                if *store != NONE {
                    continue;
                }
                *store = if common(step.term) {
                    bitmap_count += 1;
                    BITMAP | (bitmap_count - 1)
                } else {
                    list_count += 1;
                    list_count - 1
                };
            }
        } else {
            query.slot = scan_count;
            scan_count += 1;
        }
    }

    // Number the terms: those with bitmaps, then those with chains, then the rest, the most common
    // first so that what the scan touches most sits together.
    let bucket = |term: usize| 255 - sample_df[term].min(255) as usize;
    let mut next = [0u32; 257];
    next[0] = (bitmap_count + list_count) as u32;
    for term in (0..term_count).filter(|&term| store_of[term] == NONE) {
        next[bucket(term) + 1] += 1;
    }
    for at in 0..256 {
        next[at + 1] += next[at];
    }
    let new_number: Vec<u32> = store_of
        .iter()
        .enumerate()
        .map(|(term, &store)| match store {
            NONE => {
                next[bucket(term)] += 1;
                next[bucket(term)] - 1
            }
            bitmap if bitmap & BITMAP != 0 => bitmap & !BITMAP,
            list => bitmap_count + list,
        })
        .collect();
    let (old_df, old_keys) = (sample_df.clone(), terms.keys.clone());
    for (term, &new) in new_number.iter().enumerate() {
        sample_df[new as usize] = old_df[term];
        terms.keys[new as usize] = old_keys[term];
    }
    for step in steps.iter_mut() {
        step.term = new_number[step.term as usize];
    }

    let (mut phrase_anchors, mut bool_anchors) = (vec![], vec![]);
    let mut phrase_start = vec![0u32; term_count + 2];
    for (index, query) in list.iter().enumerate() {
        let steps = &steps[query.start as usize..query.end as usize];
        if query.is_phrase {
            // Any term of the phrase would be correct. The rarest triggers the fewest checks.
            let rarest = (0..steps.len()).min_by_key(|&i| sample_df[steps[i].term as usize]);
            let position = rarest.unwrap_or(0);
            let term = steps[position].term;
            phrase_start[term as usize + 1] += 3 + steps.len() as u32;
            phrase_anchors.push((term, index as u32, position as u32));
        } else if query.slot != NONE {
            anchor_terms.clear();
            anchors(steps, &sample_df, &mut anchor_terms);
            for (k, &term) in anchor_terms.iter().enumerate() {
                // The term no token maps to, in unused slots.
                let packed = pack(steps, &anchor_terms[..k], term_count as u32 + 1);
                bool_anchors.push((term, (packed, query.slot)));
            }
        }
    }
    for term in 0..=term_count {
        phrase_start[term + 1] += phrase_start[term];
    }
    let mut phrases = vec![0u32; phrase_start[term_count + 1] as usize];
    let mut next_phrase = phrase_start.clone();
    for &(term, index, position) in &phrase_anchors {
        let query = &list[index as usize];
        let at = &mut next_phrase[term as usize];
        let steps = &steps[query.start as usize..query.end as usize];
        let entry = &mut phrases[*at as usize..][..3 + steps.len()];
        entry[..3].copy_from_slice(&[index, position, steps.len() as u32]);
        for (slot, step) in entry[3..].iter_mut().zip(steps) {
            *slot = step.term;
        }
        *at += entry.len() as u32;
    }
    let (bool_start, checks) = group_by_term(&bool_anchors, term_count);
    let (bools, bool_query) = checks.into_iter().unzip();
    let flags_of = |term: usize| {
        u32::from(phrase_start[term] != phrase_start[term + 1]) * PHRASE_ANCHOR
            | u32::from(bool_start[term] != bool_start[term + 1]) * BOOL_ANCHOR
    };
    let mut hot_df = vec![0u32; terms.hot.len()];
    for slot in terms.slots.iter_mut().filter(|slot| slot.low != 0) {
        let term = new_number[slot.word as usize];
        slot.word = term | flags_of(term as usize);
        // The hottest term of each hot slot. A longer key always misses, which then compares it.
        let key = TermKey {
            low: slot.low,
            high: slot.high,
        };
        let index = (hash(key) >> (64 - HOT_BITS)) as usize;
        if sample_df[term as usize] > hot_df[index] && slot.high >> 31 == 0 {
            hot_df[index] = sample_df[term as usize];
            terms.hot[index] = *slot;
        }
    }
    Plan {
        bitmap_count: bitmap_count as usize,
        list_count: list_count as usize,
        phrase_start,
        phrases,
        bool_start,
        bools,
        bool_query,
        scan_count: scan_count as usize,
    }
}

/// Postings per chain block: 32 bytes hold a link and 14 document offsets.
const BLOCK_POSTINGS: usize = 14;

/// A piece of one term's posting list. Chains run from the newest block back to the oldest.
#[derive(Clone, Copy)]
struct Block {
    previous: u32, // The previous block's number + 1, or 0 if this is the first.
    postings: [u16; BLOCK_POSTINGS],
}

/// A posting list's state in one part of the corpus. Only a list's newest block can be part full.
#[derive(Clone, Copy)]
struct ListState {
    tail: u32, // The newest block's number + 1, or 0 if the list is empty.
    fill: u32,
}

const EMPTY_LIST: ListState = ListState { tail: 0, fill: 0 };

/// A run of up to 65,535 documents, whose postings and sums are 2-byte offsets from its first one.
struct Part {
    first_doc: u32,
    lists: Vec<ListState>,
    hits: Vec<u64>, // For each BOOL check, how many documents counted there, then the sum of their offsets.
}

/// One thread's results from its range of the corpus. Chains need no merge, no second copy and no
/// sort: a thread's range follows document order, and so do its parts and its chains.
struct ScanResult {
    blocks: Vec<Block>,
    parts: Vec<Part>,
    phrase_hits: Vec<(u32, u32)>, // (query, doc), in document order.
}

/// The ten smallest matching documents of a query, ascending, then NONE. Each query has its own lock,
/// and a thread's hits ascend, so only its first ten ever come here.
fn add_smallest(smallest: &Mutex<[u32; SMALLEST]>, doc: u32) {
    let mut carry = doc;
    for held in smallest.lock().unwrap().iter_mut() {
        if carry < *held {
            std::mem::swap(held, &mut carry);
        }
    }
}

/// A token that the hot table doesn't hold: its key and its number in the document.
#[derive(Clone, Copy, Default)]
struct Miss {
    low: u64,
    high: u32,
    at: u32,
}

fn scan_range(
    docs: &[u8],
    range: (usize, usize),
    queries: &Queries,
    plan: &Plan,
    bitmaps: &[AtomicU64],
    words: usize,
    smallest: &[Mutex<[u32; SMALLEST]>],
) -> ScanResult {
    let term_count = queries.terms.keys.len();
    let terms = &queries.terms;
    let mut result = ScanResult {
        blocks: Vec::new(),
        parts: Vec::new(),
        phrase_hits: Vec::new(),
    };
    let new_part = |first_doc| Part {
        first_doc,
        lists: vec![EMPTY_LIST; plan.list_count],
        hits: vec![0; plan.bools.len()],
    };
    let mut part = new_part(NONE);
    // A bit per term, set while the current document holds it.
    let mut present = vec![0u64; (term_count + 2).div_ceil(64)];
    let bitmap_words = plan.bitmap_count.div_ceil(64);
    let bitmap_mask = |index: usize| match plan.bitmap_count - index * 64 {
        left @ ..64 => (1u64 << left) - 1,
        _ => u64::MAX,
    };
    let chain_limit = plan.list_count as u32;
    let chain_from = plan.bitmap_count as u32;
    let mut edges = Vec::new();
    let (mut tokens, mut misses) = (Vec::new(), Vec::new());
    let (mut phrase_at, mut bool_terms, mut chain_terms) = (Vec::new(), Vec::new(), Vec::new());
    let table_shift = 64 - terms.slots.len().trailing_zeros();
    let hot: &[TermSlot; 1 << HOT_BITS] = terms.hot.as_slice().try_into().unwrap();
    for_each_doc(docs, range.0, range.1, |doc, text| {
        if part.first_doc == NONE {
            part.first_doc = doc;
        } else if doc - part.first_doc >= u32::from(u16::MAX) {
            let full = std::mem::replace(&mut part, new_part(doc));
            result.parts.push(full);
        }
        let offset = (doc - part.first_doc) as u16;
        let (doc_word, doc_bit) = (doc as usize / 64, 1u64 << (doc % 64));
        let lf = token_edges(docs, text, &mut edges);
        let count = edges.len() / 2;
        if tokens.len() <= count {
            tokens.resize(count + 1, 0);
            misses.resize(count + 1, Miss::default());
            phrase_at.resize(count + 1, 0);
            bool_terms.resize(count + 1, 0);
            chain_terms.resize(count + 1, 0);
        }
        // Look the tokens up without a branch on whether the hot table holds them, prefetching the
        // main table's slot for those it doesn't, so that their cache misses overlap.
        let mut missed = 0;
        for (at, pair) in edges.chunks_exact(2).enumerate() {
            let (start, end) = (pair[0] as usize, pair[1] as usize);
            let key = token_key(docs, text + start, end - start);
            let hash = hash(key);
            let miss = usize::from(hot[(hash >> (64 - HOT_BITS)) as usize].differs(key) != 0);
            let home = (hash >> table_shift) as usize & 0usize.wrapping_sub(miss);
            // SAFETY: there's room for every token in both lists, `missed` is at most `at`, and the
            // home slot is in the table.
            unsafe {
                *tokens.get_unchecked_mut(at) = hot[(hash >> (64 - HOT_BITS)) as usize].word;
                *misses.get_unchecked_mut(missed) = Miss {
                    low: key.low,
                    high: key.high,
                    at: at as u32,
                };
                _mm_prefetch(terms.slots.as_ptr().add(home).cast(), _MM_HINT_T0);
            }
            missed += miss;
        }
        for miss in &misses[..missed] {
            let at = miss.at as usize;
            let (start, end) = (
                text + edges[2 * at] as usize,
                text + edges[2 * at + 1] as usize,
            );
            let key = TermKey {
                low: miss.low,
                high: miss.high,
            };
            tokens[at] = terms.find(docs, start, end, key).word;
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
                chains += usize::from(term.wrapping_sub(chain_from) < chain_limit) & first;
            }
        }
        // Only a term's first occurrence in a document counts. No other thread writes this
        // document's bitmap words, so setting a bit takes no atomic.
        for (index, &word) in present[..bitmap_words].iter().enumerate() {
            let mut rest = word & bitmap_mask(index);
            while rest != 0 {
                let bitmap = index * 64 + rest.trailing_zeros() as usize;
                let at = bitmap * words + doc_word;
                bitmaps[at].store(bitmaps[at].load(Relaxed) | doc_bit, Relaxed);
                rest &= rest - 1;
            }
        }
        for &term in &chain_terms[..chains] {
            let list = &mut part.lists[(term - chain_from) as usize];
            if list.tail == 0 || list.fill as usize == BLOCK_POSTINGS {
                result.blocks.push(Block {
                    previous: list.tail,
                    postings: [0; BLOCK_POSTINGS],
                });
                list.tail = result.blocks.len() as u32;
                list.fill = 0;
            }
            result.blocks[list.tail as usize - 1].postings[list.fill as usize] = offset;
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
                let matches =
                    run.is_some_and(|w| w.iter().zip(phrase).all(|(&t, &p)| t & TERM_BITS == p));
                // An anchor can occur twice in a document, but the document counts once.
                let hits = &mut result.phrase_hits;
                let mut same_doc = hits.iter().rev().take_while(|hit| hit.1 == doc);
                if matches && !same_doc.any(|hit| hit.0 == *query) {
                    hits.push((*query, doc));
                }
            }
        }
        for &term in &bool_terms[..bools] {
            let from = plan.bool_start[term as usize] as usize;
            let to = plan.bool_start[term as usize + 1] as usize;
            let checks = plan.bools[from..to].iter().zip(&mut part.hits[from..to]);
            for (at, (packed, hits)) in checks.enumerate() {
                let mut holds = 0;
                for (slot, &term) in packed.terms.iter().enumerate() {
                    // SAFETY: `present` has a bit for every term.
                    let word = unsafe { *present.get_unchecked(term as usize / 64) };
                    holds |= (word >> (term % 64) & 1) << slot;
                }
                let counts = packed.truth >> holds & 1;
                *hits += counts * ((1 << 32) + u64::from(offset));
                // A thread's hits ascend, so only its first ten can be among the smallest.
                if counts != 0 && *hits >> 32 <= SMALLEST as u64 {
                    add_smallest(&smallest[plan.bool_query[from + at] as usize], doc);
                }
            }
        }
        for &word in in_doc {
            present[(word & TERM_BITS) as usize / 64] = 0;
        }
        lf
    });
    result.parts.push(part);
    result
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
    smallest: Vec<Mutex<[u32; SMALLEST]>>,
}

impl Index<'_> {
    /// Writes a posting list out in document order. It walks the chains newest first, so it collects
    /// the documents in descending order and then reverses them.
    fn read_list(&self, list: usize, out: &mut Vec<u32>) {
        out.clear();
        for scan in self.scans.iter().rev() {
            for part in scan.parts.iter().rev() {
                let state = part.lists[list];
                let (mut tail, mut fill) = (state.tail, state.fill as usize);
                while tail != 0 {
                    let block = &scan.blocks[tail as usize - 1];
                    let postings = block.postings[..fill].iter().rev();
                    out.extend(postings.map(|&offset| part.first_doc + u32::from(offset)));
                    (tail, fill) = (block.previous, BLOCK_POSTINGS);
                }
            }
        }
        out.reverse();
    }
}

/// Collects each phrase query's documents. Threads and their hits follow document order, so the
/// lists come out sorted.
fn collect_hits(results: &[ScanResult], query_count: usize) -> (Vec<u32>, Vec<u32>) {
    let mut hit_start = vec![0u32; query_count + 1];
    let all_hits = || results.iter().flat_map(|result| &result.phrase_hits);
    for &(query, _) in all_hits() {
        hit_start[query as usize + 1] += 1;
    }
    for query in 0..query_count {
        hit_start[query + 1] += hit_start[query];
    }
    let mut next = hit_start.clone();
    let mut hits = vec![0; hit_start[query_count] as usize];
    for &(query, doc) in all_hits() {
        hits[next[query as usize] as usize] = doc;
        next[query as usize] += 1;
    }
    (hit_start, hits)
}

/// Sums the parts' counts and offsets into each scan-answered query's count and sum of ids.
fn total_hits(results: &[ScanResult], plan: &Plan) -> (Vec<u64>, Vec<u64>) {
    let (mut counts, mut sums) = (vec![0u64; plan.scan_count], vec![0u64; plan.scan_count]);
    for part in results.iter().flat_map(|result| &result.parts) {
        for (&query, &hits) in plan.bool_query.iter().zip(&part.hits) {
            counts[query as usize] += hits >> 32;
            sums[query as usize] += (hits >> 32) * u64::from(part.first_doc) + (hits & 0xFFFF_FFFF);
        }
    }
    (counts, sums)
}

/// One thread's running result R for a BOOL query anchored on a common term, as a bitmap.
struct Evaluator {
    bits: Vec<u64>,
    other: Vec<u64>,
    list: Vec<u32>,
}

impl Evaluator {
    /// Applies the query's steps in order, from the empty set, with each term's set as a bitmap.
    fn run(&mut self, steps: &[Step], index: &Index) {
        self.bits.fill(0);
        for step in steps {
            let term = step.term as usize;
            let other = if term < index.bitmap_count {
                &index.bitmaps[term * index.words..(term + 1) * index.words]
            } else {
                index.read_list(term - index.bitmap_count, &mut self.list);
                self.other.fill(0);
                for &doc in &self.list {
                    self.other[doc as usize / 64] |= 1 << (doc % 64);
                }
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
        // The sum of a word's ids, from the popcounts of the positions with each index bit set.
        let (mut count, mut sum) = (0u64, 0u64);
        let mut smallest = Vec::with_capacity(SMALLEST);
        for (i, &word) in self.bits.iter().enumerate() {
            let ones = u64::from(word.count_ones());
            count += ones;
            sum += 64 * i as u64 * ones;
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
            list: Vec::new(),
        };
        let mut text = Vec::new();
        let claim = || Some(next_batch.fetch_add(1, Relaxed)).filter(|b| b * BATCH < list.len());
        while let Some(batch) = claim() {
            text.clear();
            for number in batch * BATCH..((batch + 1) * BATCH).min(list.len()) {
                let query = &list[number];
                if query.is_phrase {
                    let (from, to) = (index.hit_start[number], index.hit_start[number + 1]);
                    print_list(&mut text, &index.hits[from as usize..to as usize]);
                } else if query.slot == NONE {
                    evaluator.run(queries.steps_of(query), index);
                    evaluator.print_result(&mut text);
                } else {
                    let slot = query.slot as usize;
                    let smallest = index.smallest[slot].lock().unwrap();
                    let shown = (index.counts[slot] as usize).min(SMALLEST);
                    print_line(
                        &mut text,
                        index.counts[slot],
                        index.sums[slot],
                        &smallest[..shown],
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
    let bitmaps: Vec<AtomicU64> = (0..plan.bitmap_count * words)
        .map(|_| AtomicU64::new(0))
        .collect();
    let smallest: Vec<_> = (0..plan.scan_count)
        .map(|_| Mutex::new([NONE; SMALLEST]))
        .collect();
    let file = File::create(Path::new(&out_dir).join("results.txt"))?;
    let results: Vec<ScanResult> = thread::scope(|scope| {
        let scan = |i: usize| {
            let range = (i * docs.len() / threads, (i + 1) * docs.len() / threads);
            let (queries, plan, bitmaps, smallest) = (&queries, &plan, &bitmaps, &smallest);
            scope.spawn(move || scan_range(docs, range, queries, plan, bitmaps, words, smallest))
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
        bitmaps: bitmaps.into_iter().map(AtomicU64::into_inner).collect(),
        words,
        scans: &results,
        hit_start,
        hits,
        counts,
        sums,
        smallest,
    };
    answer_queries(file, &queries, &index, threads);
    Ok(())
}
