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
/// ...and its BOOL queries are checked for each document that contains it...
const BOOL_ANCHOR: u32 = 1 << 30;
/// ...and its chain gets each document that contains it.
const CHAINED: u32 = 1 << 29;
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
    fn mallopt(param: i32, value: i32) -> i32;
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

/// A token's first 8 bytes, lowercased and zero-padded, with the top bit set if there are more, which
/// no byte of a token has. Setting this bit in every byte lowercases ASCII letters, and digits already
/// have it set.
fn short_key(file: &[u8], start: usize, len: usize) -> u64 {
    // SAFETY: callers pass a token's start, and the mapping continues for `PADDING` bytes past the
    // file. The load needs no alignment, and the build targets x86-64-v3, which has BMI2, whose BZHI
    // keeps the given number of low bits, or all of them if that's more than the width.
    unsafe {
        let bytes = file.as_ptr().add(start).cast::<u64>().read_unaligned();
        _bzhi_u64(bytes | 0x2020_2020_2020_2020, 8 * len.min(9) as u32) | u64::from(len > 8) << 63
    }
}

/// The next 4 bytes of the token, likewise. The top bit, set if there are more still, marks a token that
/// needs a full compare, and those are rare.
fn long_key(file: &[u8], start: usize, len: usize) -> u32 {
    let kept = 8 * len.saturating_sub(8).min(4) as u32;
    // SAFETY: as in `short_key`.
    unsafe {
        let bytes = file.as_ptr().add(start + 8).cast::<u32>().read_unaligned();
        _bzhi_u32(bytes | 0x2020_2020, kept) | u32::from(len > 12) << 31
    }
}

/// Mixes a short key with one multiply, whose high bits depend on every input bit. A test build swaps
/// it for a constant, so that every key collides and any logic that trusts a hash without comparing the
/// key shows up as a wrong answer.
fn hash(short_key: u64) -> u64 {
    let hash = short_key.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    if cfg!(sd_const_hash) { 0 } else { hash }
}

/// Which of `sets` sets, a power of two, a hash starts at: its top bits.
fn home(sets: usize, hash: u64) -> usize {
    (hash >> (64 - sets.trailing_zeros())) as usize
}

/// Four terms in one cache line, whose keys are compared at once, so a lookup has no branch on which
/// of them it found, or whether it found one.
#[derive(Clone, Copy)]
#[repr(align(64))]
struct TermSet {
    short_keys: [u64; 4], // 0 in the ways not used yet, which fill in order.
    long_keys: [u32; 4],
    words: [u32; 4], // The term's number and flags.
}

impl TermSet {
    /// A bit for each way holding this short key.
    fn ways(&self, short_key: u64) -> u32 {
        // SAFETY: the build targets x86-64-v3, which has AVX2, and the struct is aligned.
        unsafe {
            let keys = _mm256_load_si256(self.short_keys.as_ptr().cast());
            let same = _mm256_cmpeq_epi64(keys, _mm256_set1_epi64x(short_key as i64));
            _mm256_movemask_pd(_mm256_castsi256_pd(same)) as u32
        }
    }

    /// A bit for each way holding this long key.
    fn long_ways(&self, long_key: u32) -> u32 {
        // SAFETY: as in `ways`.
        unsafe {
            let keys = _mm_load_si128(self.long_keys.as_ptr().cast());
            let same = _mm_cmpeq_epi32(keys, _mm_set1_epi32(long_key as i32));
            _mm_movemask_ps(_mm_castsi128_ps(same)) as u32
        }
    }

    /// The word of the first of these ways, or 0, the word of the term no query uses, if there are none.
    fn word(&self, ways: u32) -> u32 {
        self.words[ways.trailing_zeros() as usize & 3] & 0u32.wrapping_sub(u32::from(ways != 0))
    }
}

/// `count` empty sets. The kernel supplies the zeros, and only the sets touched cost anything.
fn empty_sets(count: usize) -> Vec<TermSet> {
    let layout = std::alloc::Layout::array::<TermSet>(count).unwrap();
    // SAFETY: the layout isn't empty, zeros are a valid TermSet, and the Vec frees with this layout.
    unsafe { Vec::from_raw_parts(std::alloc::alloc_zeroed(layout).cast(), count, count) }
}

/// Query terms to dense ids, by open addressing with linear probing over sets. A hit always compares
/// the whole token: terms longer than the keys' 12 bytes are checked against the query file.
struct TermTable<'a> {
    sets: Vec<TermSet>, // The length is a power of two.
    hot: Vec<TermSet>, // The commonest terms of each hash that fit a short key, chosen by the plan.
    keys: Vec<&'a [u8]>,
}

impl<'a> TermTable<'a> {
    /// Sized for `query_bytes` of query text, where 16 bytes hold about one distinct term.
    fn new(query_bytes: usize) -> Self {
        TermTable {
            sets: empty_sets((query_bytes / 32).next_power_of_two().max(1 << 10)),
            hot: empty_sets(HOT_SETS),
            keys: vec![&[]; FIRST_TERM],
        }
    }

    /// The word of the term for the token at `start..end` of `file`, in any letter case, or else 0,
    /// from a probe that starts at set `index`. A set with room left ends a probe.
    fn find(&self, file: &[u8], start: usize, end: usize, short_key: u64, mut index: usize) -> u32 {
        let long_key = long_key(file, start, end - start);
        loop {
            let set = &self.sets[index];
            let mut ways = set.ways(short_key) & set.long_ways(long_key);
            if long_key >> 31 != 0 {
                ways = self.ways_of_long_term(set, ways, &file[start..end]);
            }
            if (ways != 0) | (set.short_keys[3] == 0) {
                return set.word(ways);
            }
            index = (index + 1) & (self.sets.len() - 1);
        }
    }

    /// Which of these ways, whose keys match a token longer than them, hold the token.
    #[cold]
    fn ways_of_long_term(&self, set: &TermSet, ways: u32, token: &[u8]) -> u32 {
        let is_token = |way: &u32| {
            let term = set.words[*way as usize] & TERM_BITS;
            ways >> way & 1 != 0 && token.eq_ignore_ascii_case(self.keys[term as usize])
        };
        (0..4).filter(is_token).map(|way| 1 << way).sum()
    }

    /// Adds the query term at `start..end` of `file`, if it's new, and returns its id.
    fn intern(&mut self, file: &'a [u8], start: usize, end: usize) -> u32 {
        let short = short_key(file, start, end - start);
        let index = home(self.sets.len(), hash(short));
        let word = self.find(file, start, end, short, index);
        if word != 0 {
            return word;
        }
        let term = self.keys.len() as u32;
        self.keys.push(&file[start..end]);
        place_term(
            &mut self.sets,
            short,
            long_key(file, start, end - start),
            term,
        );
        if self.keys.len() > self.sets.len() * 3 {
            let mut bigger = empty_sets(self.sets.len() * 2);
            for set in &self.sets {
                for way in (0..4).filter(|&way| set.short_keys[way] != 0) {
                    place_term(
                        &mut bigger,
                        set.short_keys[way],
                        set.long_keys[way],
                        set.words[way],
                    );
                }
            }
            self.sets = bigger;
        }
        term
    }
}

fn place_term(sets: &mut [TermSet], short_key: u64, long_key: u32, word: u32) {
    let mut index = home(sets.len(), hash(short_key));
    while sets[index].short_keys[3] != 0 {
        index = (index + 1) & (sets.len() - 1);
    }
    let set = &mut sets[index];
    let way = set.ways(0).trailing_zeros() as usize;
    (set.short_keys[way], set.long_keys[way], set.words[way]) = (short_key, long_key, word);
}

/// A term of a query, with the operator that applies it. A phrase's steps have no operator.
#[derive(Clone, Copy)]
struct Step {
    op: u8, // The first byte of AND, OR or NOT.
    term: u32,
}

struct Query {
    is_phrase: bool,
    slot: u32,   // A BOOL query's number, where its answer goes.
    shared: u32, // The number of the term count that also goes into its answer, or NONE.
    start: u32,  // Its steps are `steps[start..end]`.
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
/// holding each of the query's other terms make a number, and that bit of `truth` says whether the
/// document counts here: whether it matches, and holds none of the query's earlier anchors, where it
/// has already counted. Most queries have few terms, and need a smaller check.
#[derive(Clone, Copy)]
struct Check<const TERMS: usize> {
    truth: u32,
    terms: [u32; TERMS],
}

impl<const TERMS: usize> Check<TERMS> {
    /// 1 if a document holding the terms marked in `present` counts here, else 0.
    fn counts(&self, present: &[u64]) -> u64 {
        let mut holds = 0;
        for (slot, &term) in self.terms.iter().enumerate() {
            // SAFETY: `present` has a bit for every term.
            let word = unsafe { *present.get_unchecked(term as usize / 64) };
            holds |= (word >> (term % 64) & 1) << slot;
        }
        u64::from(self.truth >> holds & 1)
    }
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
fn pack(steps: &[Step], anchor: u32, earlier: &[u32]) -> Check<5> {
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
    Check {
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

/// Where a term's BOOL checks start, in each list of them, and where its own count is. A term's checks
/// end where the next term's start. Counts are in the order of the short checks, the long checks, then
/// the terms'.
#[derive(Clone, Copy)]
struct AnchorLists {
    short_from: u32,
    long_from: u32,
    shared: u32,
}

/// What the sample decided: how each term is stored, and what to check at each term's occurrences.
struct Plan {
    bitmap_count: usize,
    list_count: usize,
    phrase_start: Vec<u32>, // A term's phrases are `phrases[phrase_start[t]..phrase_start[t + 1]]`...
    phrases: Vec<u32>, // ...each its query, the anchor's position in it, its length, its terms.
    anchor_lists: Vec<AnchorLists>,
    short_checks: Vec<Check<3>>,
    long_checks: Vec<Check<5>>,
    count_query: Vec<u32>, // Which query or term count each count belongs to.
    posting_queries: Vec<u32>, // The queries anchored on a common term.
    count_slots: usize,    // Those, then one count for each term with one, then a spare.
}

/// How many documents hold each term, among pieces of the corpus as big as `SAMPLE_BYTES` in all, and
/// how many documents those pieces have.
fn sample_terms(docs: &[u8], terms: &TermTable, threads: usize) -> (Vec<u32>, u32) {
    let df: Vec<AtomicU32> = (0..terms.keys.len()).map(|_| AtomicU32::new(0)).collect();
    let (sampled_docs, next_piece) = (AtomicU32::new(0), AtomicUsize::new(0));
    let piece = SAMPLE_BYTES.min(docs.len()) / threads;
    on_threads(threads, || {
        let start = next_piece.fetch_add(1, Relaxed) * (docs.len() / threads);
        // The low byte of the last document holding each term. A rare mistake only skews the guess.
        let mut seen_in = vec![0u8; df.len()];
        for_each_doc(
            docs,
            (start, start + piece),
            &mut Vec::new(),
            |doc, text, edges| {
                sampled_docs.fetch_add(1, Relaxed);
                for pair in edges.chunks_exact(2) {
                    let (start, end) = (text + pair[0] as usize, text + pair[1] as usize);
                    let short = short_key(docs, start, end - start);
                    let index = home(terms.sets.len(), hash(short));
                    let term = terms.find(docs, start, end, short, index) as usize;
                    if seen_in[term] != doc as u8 {
                        seen_in[term] = doc as u8;
                        df[term].fetch_add(1, Relaxed);
                    }
                }
            },
        );
    });
    let df = df.into_iter().map(AtomicU32::into_inner).collect();
    (df, sampled_docs.into_inner())
}

fn make_plan(docs: &[u8], queries: &mut Queries, threads: usize) -> Plan {
    let term_count = queries.terms.keys.len();
    let (mut sample_df, sampled_docs) = sample_terms(docs, &queries.terms, threads);

    // A query anchored on a common term, with df above about N/24, is answered from postings: bitmaps
    // for its common terms and chains for the others. On memory alone, a bitmap (N/8 bytes) and a
    // chain (about 3.3 bytes per posting) cross near df = N/26, but setting a bit is cheaper than
    // extending a chain, and measuring keeps M under the cap at N/24. Any choice gives the same answers.
    let common = |term: u32| sample_df[term as usize] * 24 >= sampled_docs;
    const BITMAP: u32 = 1 << 31;
    let mut store_of = vec![NONE; term_count];
    let (mut bitmap_count, mut list_count, mut scan_count) = (0, 0, 0);
    // Each BOOL query's anchors, then NONE. A query reading postings has none.
    let (mut query_anchors, mut these) = (Vec::with_capacity(queries.list.len()), Vec::new());
    let mut posting_queries = Vec::new();
    let Queries { list, steps, terms } = queries;
    for (number, query) in list
        .iter_mut()
        .enumerate()
        .filter(|(_, query)| !query.is_phrase)
    {
        let steps = &steps[query.start as usize..query.end as usize];
        query.slot = scan_count;
        scan_count += 1;
        these.clear();
        anchors(steps, &sample_df, &mut these);
        if these.iter().any(|&term| common(term)) {
            posting_queries.push(number as u32);
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
        }
        query_anchors.push(NONE);
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

    let mut phrase_start = vec![0u32; term_count + 1];
    let (mut short_start, mut long_start) =
        (vec![0u32; term_count + 1], vec![0u32; term_count + 1]);
    let mut shared_of = vec![NONE; term_count];
    let mut shared_count = 0;
    let mut at_anchor = 0;
    // A check holds the query's other terms, of which there are at most this many.
    let is_short = |steps: &[Step]| steps.len() <= 4;
    let steps_of = |query: &Query| &steps[query.start as usize..query.end as usize];
    for query in list.iter_mut().filter(|query| !query.is_phrase) {
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
                let starts = if is_short(steps_of(query)) {
                    &mut short_start
                } else {
                    &mut long_start
                };
                starts[these[count] as usize + 1] += 1;
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
    sum_sizes(&mut short_start);
    sum_sizes(&mut long_start);
    let mut phrases = vec![0u32; phrase_start[term_count] as usize];
    let (short_count, long_count) = (short_start[term_count], long_start[term_count]);
    let mut short_checks = vec![
        Check {
            truth: 0,
            terms: [ABSENT; 3]
        };
        short_count as usize
    ];
    let mut long_checks = vec![
        Check {
            truth: 0,
            terms: [ABSENT; 5]
        };
        long_count as usize
    ];
    let check_count = short_count + long_count;
    let mut count_query = vec![0u32; (check_count + shared_count + 1) as usize];
    let scan_slots = scan_count + shared_count;
    count_query[check_count as usize..]
        .iter_mut()
        .enumerate()
        .for_each(|(at, slot)| *slot = scan_count + at as u32);
    let mut next_phrase = phrase_start.clone();
    let (mut next_short, mut next_long) = (short_start.clone(), long_start.clone());
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
        } else {
            let these = &query_anchors[at_anchor..];
            let count = these.iter().position(|&term| term == NONE).unwrap_or(0);
            let unchecked = usize::from(query.shared != NONE);
            for (k, &term) in these[..count].iter().enumerate().skip(unchecked) {
                let check = pack(steps, term, &these[..k]);
                if is_short(steps) {
                    let at = &mut next_short[term as usize];
                    short_checks[*at as usize] = Check {
                        truth: check.truth,
                        terms: [check.terms[0], check.terms[1], check.terms[2]],
                    };
                    count_query[*at as usize] = query.slot;
                    *at += 1;
                } else {
                    let at = &mut next_long[term as usize];
                    long_checks[*at as usize] = check;
                    count_query[short_count as usize + *at as usize] = query.slot;
                    *at += 1;
                }
            }
            at_anchor += count + 1;
        }
    }
    let starts = |start: &[u32], term: usize| start[term] != start[term + 1];
    // Each term's count, or the spare count for a term without one, which nothing reads.
    let anchor_lists: Vec<AnchorLists> = (0..=term_count)
        .map(|term| AnchorLists {
            short_from: short_start[term],
            long_from: long_start[term],
            shared: check_count
                + shared_of
                    .get(term)
                    .map_or(shared_count, |&at| at.min(shared_count)),
        })
        .collect();
    let mut hot_df = vec![[0u32; 4]; HOT_SETS];
    for set in terms.sets.iter_mut() {
        for way in (0..4).filter(|&way| set.short_keys[way] != 0) {
            let term = new_number[set.words[way] as usize] as usize;
            let chained = term.wrapping_sub(chain_from as usize) < list_count as usize;
            let word = term as u32
                | u32::from(starts(&phrase_start, term)) * PHRASE_ANCHOR
                | u32::from(
                    starts(&short_start, term)
                        || starts(&long_start, term)
                        || shared_of[term] != NONE,
                ) * BOOL_ANCHOR
                | u32::from(chained) * CHAINED;
            set.words[way] = word;
            // The four commonest terms of each hot set. A longer key always misses, and then gets
            // compared in full.
            let short = set.short_keys[way];
            let hot = home(HOT_SETS, hash(short));
            let dfs = &mut hot_df[hot];
            let least = (0..4).min_by_key(|&way| dfs[way]).unwrap_or(0);
            if sample_df[term] > dfs[least] && short >> 63 == 0 {
                dfs[least] = sample_df[term];
                terms.hot[hot].short_keys[least] = short;
                terms.hot[hot].words[least] = word;
            }
        }
    }
    Plan {
        bitmap_count: bitmap_count as usize,
        list_count: list_count as usize,
        phrase_start,
        phrases,
        anchor_lists,
        short_checks,
        long_checks,
        count_query,
        posting_queries,
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

/// How many blocks are allocated at a time. Growing one Vec would hold twice the memory for a while.
const CHUNK_BLOCKS: usize = 1 << 12;

/// A thread's blocks, numbered from 1, in chunks that never move.
#[derive(Default)]
struct Blocks {
    chunks: Vec<Vec<Block>>,
    count: u32,
}

impl Blocks {
    fn get(&self, number: u32) -> &Block {
        let at = number as usize - 1;
        &self.chunks[at / CHUNK_BLOCKS][at % CHUNK_BLOCKS]
    }

    fn get_mut(&mut self, number: u32) -> &mut Block {
        let at = number as usize - 1;
        &mut self.chunks[at / CHUNK_BLOCKS][at % CHUNK_BLOCKS]
    }

    /// Adds a block, and returns its number.
    fn add(&mut self, block: Block) -> u32 {
        if self.count as usize % CHUNK_BLOCKS == 0 {
            self.chunks.push(Vec::with_capacity(CHUNK_BLOCKS));
        }
        self.chunks[self.count as usize / CHUNK_BLOCKS].push(block);
        self.count += 1;
        self.count
    }
}

/// One thread's results. A thread claims ranges in document order, so its hits ascend, though a list
/// of them still has to be merged with other threads'.
struct ScanResult {
    blocks: Blocks,
    lists: Vec<ListState>,
    hits: Vec<u64>, // For each count, how many documents went into it, then the sum of their ids.
    phrase_hits: Vec<(u32, u32)>, // (query, doc)
}

/// The bits of `hits` that hold the sum of ids, which `main` checks has room.
const SUM_BITS: u32 = 40;

/// What a query's line prints, or what a term's count adds to it.
#[derive(Clone, Copy)]
struct Answer {
    count: u64,
    sum: u64,
    smallest: [u32; SMALLEST], // Ascending, then NONE.
}

/// The answers, each with its own lock, and the last of each one's ten smallest documents, which only
/// falls, so that the documents past it need no lock. Those fill first, as threads work through the
/// corpus together, and only a thread's own first ten ever get this far.
struct Answers {
    limit: Vec<AtomicU32>,
    slots: Vec<Mutex<Answer>>,
}

impl Answers {
    fn add_smallest(&self, slot: usize, doc: u32) {
        if doc < self.limit[slot].load(Relaxed) {
            let smallest = &mut self.slots[slot].lock().unwrap().smallest;
            insert_smallest(smallest, doc);
            if smallest[SMALLEST - 1] != NONE {
                self.limit[slot].store(smallest[SMALLEST - 1], Relaxed);
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

/// A token that the hot table doesn't hold.
#[derive(Clone, Copy, Default)]
struct Miss {
    short: u64,
    index: u32, // The set where its probe starts.
    at: u32,    // Its number in the document.
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
    answers: &Answers,
) -> ScanResult {
    let terms = &queries.terms;
    let mut result = ScanResult {
        blocks: Blocks::default(),
        lists: vec![ListState { tail: 0, fill: 0 }; plan.list_count],
        hits: vec![0; plan.count_query.len()],
        phrase_hits: Vec::new(),
    };
    // A bit per term, set while the current document holds it.
    let mut present = vec![0u64; terms.keys.len().div_ceil(64)];
    let bitmap_count = plan.bitmap_count;
    let chain_from = (FIRST_TERM + bitmap_count.next_multiple_of(64)) as u32;
    let hot: &[TermSet; HOT_SETS] = terms.hot.as_slice().try_into().unwrap();
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
                    let short = short_key(docs, text + start, end - start);
                    let hash = hash(short);
                    let set = &hot[home(HOT_SETS, hash)];
                    let ways = set.ways(short);
                    let miss = usize::from(ways == 0);
                    let index = home(terms.sets.len(), hash);
                    // SAFETY: there's room for every token in both lists, `missed` is at most `at`, a way's
                    // number is below 4, and the prefetched set is in the table.
                    unsafe {
                        *tokens.get_unchecked_mut(at) =
                            *set.words.get_unchecked(ways.trailing_zeros() as usize & 3);
                        *misses.get_unchecked_mut(missed) = Miss {
                            short,
                            index: index as u32,
                            at: at as u32,
                        };
                        let target = index & 0usize.wrapping_sub(miss);
                        _mm_prefetch(terms.sets.as_ptr().add(target).cast(), _MM_HINT_T0);
                    }
                    missed += miss;
                }
                for miss in &misses[..missed] {
                    let at = miss.at as usize;
                    let (start, end) = (
                        text + edges[2 * at] as usize,
                        text + edges[2 * at + 1] as usize,
                    );
                    tokens[at] = terms.find(docs, start, end, miss.short, miss.index as usize);
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
                        chains += (word >> 29 & 1) as usize & first;
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
                    // A tail of 0 gets a new block too: then `fill` is 0, and there's no block 0.
                    let full = list.tail == 0
                        || list.fill as usize == BLOCK_POSTINGS
                        || doc - result.blocks.get(list.tail).first_doc > u32::from(u16::MAX);
                    if full {
                        list.tail = result.blocks.add(Block {
                            previous: list.tail,
                            first_doc: doc,
                            postings: [0; BLOCK_POSTINGS],
                        });
                        list.fill = 0;
                    }
                    let block = result.blocks.get_mut(list.tail);
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
                    let lists = &plan.anchor_lists[term as usize..=term as usize + 1];
                    let shared = lists[0].shared as usize;
                    result.hits[shared] += this_doc;
                    // A thread's hits ascend, so only its first ten can be among the smallest.
                    if result.hits[shared] >> SUM_BITS <= SMALLEST as u64 {
                        answers.add_smallest(plan.count_query[shared] as usize, doc);
                    }
                    let (from, to) = (lists[0].short_from as usize, lists[1].short_from as usize);
                    let long_from = plan.short_checks.len() + lists[0].long_from as usize;
                    let long_to = plan.short_checks.len() + lists[1].long_from as usize;
                    let counted = |checks: u64, at: usize, hits: &mut u64| {
                        *hits += checks * this_doc;
                        if checks != 0 && *hits >> SUM_BITS <= SMALLEST as u64 {
                            answers.add_smallest(plan.count_query[at] as usize, doc);
                        }
                    };
                    let hits = &mut result.hits;
                    for at in from..to {
                        let counts = plan.short_checks[at].counts(&present);
                        counted(counts, at, &mut hits[at]);
                    }
                    for at in long_from..long_to {
                        let counts =
                            plan.long_checks[at - plan.short_checks.len()].counts(&present);
                        counted(counts, at, &mut hits[at]);
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
    answers: Answers,
}

impl Index<'_> {
    /// Sets the bits of a posting list's documents.
    fn set_bits_of_list(&self, list: usize, bits: &mut [u64]) {
        for scan in self.scans {
            let (mut tail, mut fill) = (scan.lists[list].tail, scan.lists[list].fill as usize);
            while tail != 0 {
                let block = scan.blocks.get(tail);
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
fn total_hits(results: &[ScanResult], plan: &Plan, answers: &mut Answers) {
    let answers: Vec<&mut Answer> = answers
        .slots
        .iter_mut()
        .map(|answer| answer.get_mut().unwrap())
        .collect();
    let mut answers = answers;
    for result in results {
        for (&slot, &hits) in plan.count_query.iter().zip(&result.hits) {
            answers[slot as usize].count += hits >> SUM_BITS;
            answers[slot as usize].sum += hits & ((1 << SUM_BITS) - 1);
        }
    }
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

    fn answer(&self) -> Answer {
        let mut answer = Answer {
            count: 0,
            sum: 0,
            smallest: [NONE; SMALLEST],
        };
        let mut found = 0;
        for (i, &word) in self.bits.iter().enumerate() {
            let ones = u64::from(word.count_ones());
            answer.count += ones;
            answer.sum += 64 * i as u64 * ones;
            // The sum of a word's ids, from the popcounts of the positions with each index bit set.
            for (bit, mask) in INDEX_BIT_MASKS.iter().enumerate() {
                answer.sum += u64::from((word & mask).count_ones()) << bit;
            }
            let mut rest = word;
            while rest != 0 && found < SMALLEST {
                answer.smallest[found] = 64 * i as u32 + rest.trailing_zeros();
                found += 1;
                rest &= rest - 1;
            }
        }
        answer
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

/// First the queries that read postings, as they take most of the time. Then threads claim batches of
/// queries in order, and each waits for the batch before it to reach the file. The wait is short,
/// because batches cost about the same.
fn answer_queries(file: File, plan: &Plan, queries: &Queries, index: &Index, threads: usize) {
    let (next_query, next_batch, written) = (
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
    );
    let list = &queries.list;
    on_threads(threads, || {
        let mut evaluator = Evaluator {
            bits: vec![0; index.words],
            other: vec![0; index.words],
        };
        while let Some(&number) = plan.posting_queries.get(next_query.fetch_add(1, Relaxed)) {
            let query = &list[number as usize];
            evaluator.run(queries.steps_of(query), index);
            *index.answers.slots[query.slot as usize].lock().unwrap() = evaluator.answer();
        }
    });
    on_threads(threads, || {
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
                } else {
                    let mut answer = *index.answers.slots[query.slot as usize].lock().unwrap();
                    if query.shared != NONE {
                        let shared = *index.answers.slots[query.shared as usize].lock().unwrap();
                        answer.count += shared.count;
                        answer.sum += shared.sum;
                        for doc in shared.smallest {
                            insert_smallest(&mut answer.smallest, doc);
                        }
                    }
                    let shown = (answer.count as usize).min(SMALLEST);
                    print_line(
                        &mut text,
                        answer.count,
                        answer.sum,
                        &answer.smallest[..shown],
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
    // All of the heap comes from one contiguous region, grown a lot at a time, so that the kernel can
    // back it with huge pages, in far fewer page faults than threads' arenas and small mappings take.
    let (top_pad, mmap_threshold, arena_max) = (-2, -3, -8);
    // SAFETY: no thread has started, and these only tune the allocator.
    unsafe {
        mallopt(arena_max, 1);
        mallopt(mmap_threshold, 1 << 30);
        mallopt(top_pad, 1 << 26);
    }
    let mut args = std::env::args().skip(1);
    let data_dir = args.next().unwrap_or_else(|| "/data".into());
    let out_dir = args.next().unwrap_or_else(|| "/out".into());
    let data_dir = Path::new(&data_dir);
    let threads = thread_count();
    let docs = map_file(&data_dir.join("docs.txt"))?;
    let mut queries = parse_queries(map_file(&data_dir.join("queries.txt"))?);
    let plan = make_plan(docs, &mut queries, threads);

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
    let empty = Answer {
        count: 0,
        sum: 0,
        smallest: [NONE; SMALLEST],
    };
    let mut answers = Answers {
        limit: (0..plan.count_slots)
            .map(|_| AtomicU32::new(NONE))
            .collect(),
        slots: (0..plan.count_slots).map(|_| Mutex::new(empty)).collect(),
    };
    let file = File::create(Path::new(&out_dir).join("results.txt"))?;
    // Enough ranges that a thread slowed for a while doesn't hold the others up.
    let range_size = (docs.len() / (threads * RANGES_PER_THREAD)).max(1);
    let next_range = AtomicUsize::new(0);
    let results: Vec<ScanResult> = thread::scope(|scope| {
        let scan = |_| {
            let (queries, plan, answers, next_range) = (&queries, &plan, &answers, &next_range);
            scope.spawn(move || {
                scan_ranges(
                    docs, range_size, next_range, queries, plan, shared, words, answers,
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
    total_hits(&results, &plan, &mut answers);
    let index = Index {
        bitmap_count: plan.bitmap_count,
        bitmaps,
        words,
        scans: &results,
        hit_start,
        hits,
        answers,
    };
    answer_queries(file, &plan, &queries, &index, threads);
    // Nothing is buffered, and the kernel frees all the memory faster than dropping it would.
    std::process::exit(0)
}
