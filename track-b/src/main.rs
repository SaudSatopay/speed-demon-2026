//! Track B (Index): boolean and phrase queries over a 254 MB corpus.
//!
//! The queries are read first, so only their terms are ever looked up. Every document matching a
//! BOOL query contains one of a few anchor terms (the rarest AND of its last run, or its OR terms),
//! and for nine queries in ten the anchors are rare, so the query is answered during the scan: at
//! each anchor's first occurrence in a document, the bits of what the document holds index a truth
//! table, and no posting of any of its terms is ever stored. Only queries anchored on a common term
//! read postings afterwards: bitmaps for their common terms, set during the scan, and chains of 2-byte
//! offsets for their other terms. Each occurrence of a phrase's rarest term checks the phrase around it
//! when the document ends. Threads claim ranges of the corpus in order and scan a document at a time in
//! stages without a branch per token: find the tokens, look them up, mark them, then act on what the
//! marks listed. Then they answer the queries that read postings, and print all the answers in order.

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
/// Term 2 is what finding nothing gives in a set that a term has been placed past.
const OVERFLOWED: u32 = 2;
/// Real terms start here, at a block of 64 of a document's marks: those with bitmaps first, then, from
/// the next block, those with chains, then the rest.
const FIRST_TERM: usize = 64;
/// The low bits of a term table's word are its term's number...
const TERM_BITS: u32 = (1 << 24) - 1;
/// ...and the top bits say what else to do. A word that may not be the token's says to look further, as
/// its key held only part of the token, or as the probe has to go on...
const UNSURE: u32 = 1 << 31;
/// ...a term's phrases are checked around each occurrence...
const PHRASE_ANCHOR: u32 = 1 << 30;
/// ...and its BOOL queries are checked for each document that contains it...
const BOOL_ANCHOR: u32 = 1 << 29;
/// ...and its chain gets each document that contains it.
const CHAINED: u32 = 1 << 28;
/// A step of a query is a term's number with its operator in the top bits. OR is 0, as it is for the
/// first term: starting from the empty set, "OR t1" gives S(t1). A phrase's steps have no operator.
const AND: u32 = 1 << 30;
const NOT: u32 = 2 << 30;
/// No query or document has this number.
const NONE: u32 = u32::MAX;
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
            let moved = _mm256_add_epi8(value, byte_set(127 - high));
            _mm256_cmpgt_epi8(moved, byte_set(126 - (high - low)))
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

/// For each value of a byte, the positions of its set bits, then zeros.
static BIT_POSITIONS: [[u8; 8]; 256] = {
    let mut table = [[0; 8]; 256];
    let mut value = 0;
    while value < 256 {
        let (mut bit, mut found) = (0, 0);
        while bit < 8 {
            if value >> bit & 1 != 0 {
                table[value][found] = bit;
                found += 1;
            }
            bit += 1;
        }
        value += 1;
    }
    table
};

/// Lists where each token of the line at `text` starts and ends, as offsets from `text` in pairs, and
/// returns where the line's LF is. Each block's mask of token bytes, XORed with itself shifted, has a
/// bit where a token starts or ends. Writing a fixed number of them for each byte of the mask, and
/// keeping as many as it has bits, takes no branch on how many there are.
fn token_edges(file: &[u8], text: usize, edges: &mut Vec<u32>) -> usize {
    edges.clear();
    let (mut block, mut carry) = (0, 0);
    loop {
        let (tokens, line_feeds) = block_masks(file, text + block);
        let tokens = tokens & (line_feeds.wrapping_sub(1) & !line_feeds);
        let found = tokens ^ (tokens << 1 | carry);
        carry = tokens >> 63;
        edges.reserve(72);
        // SAFETY: the reserve leaves room for a block's at most 64 edges and 8 more, and the table is
        // indexed by a byte. The build targets x86-64-v3, which has AVX2.
        unsafe {
            let mut out = edges.as_mut_ptr().add(edges.len());
            let mut base = _mm256_set1_epi32(block as i32);
            for byte in found.to_le_bytes() {
                let bits = BIT_POSITIONS.as_ptr().add(byte as usize);
                let at = _mm256_cvtepu8_epi32(_mm_loadl_epi64(bits.cast()));
                _mm256_storeu_si256(out.cast(), _mm256_add_epi32(at, base));
                out = out.add(byte.count_ones() as usize);
                base = _mm256_add_epi32(base, _mm256_set1_epi32(8));
            }
            edges.set_len(edges.len() + found.count_ones() as usize);
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

/// `hash` of four short keys at once, which AVX2 only has 32-bit multiplies for.
fn hashes(keys: __m256i) -> __m256i {
    // SAFETY: the build targets x86-64-v3, which has AVX2.
    unsafe {
        let low = _mm256_set1_epi64x(0x7F4A_7C15);
        let high = _mm256_set1_epi64x(0x9E37_79B9);
        let tops = _mm256_srli_epi64(keys, 32);
        let cross = _mm256_add_epi64(_mm256_mul_epu32(tops, low), _mm256_mul_epu32(keys, high));
        let hash = _mm256_add_epi64(_mm256_mul_epu32(keys, low), _mm256_slli_epi64(cross, 32));
        let zero = _mm256_setzero_si256();
        if cfg!(sd_const_hash) { zero } else { hash }
    }
}

/// Which of `sets` sets, a power of two, a hash starts at: its top bits.
fn home(sets: usize, hash: u64) -> usize {
    (hash >> (64 - sets.trailing_zeros())) as usize
}

/// Three terms in one cache line, whose keys are compared at once, so a lookup has no branch on which
/// of them it found, or whether it found one. The fourth way is never used, and its word is what a lookup
/// that finds none gets: 0 (no query uses this token), or the word of `OVERFLOWED` if the probe goes on.
#[derive(Clone, Copy)]
#[repr(align(64))]
struct TermSet {
    short_keys: [u64; 4], // 0 in the ways not used, which fill in order.
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

    /// The word of the first of these ways, or else the unused way's.
    fn word(&self, ways: u32) -> u32 {
        let way = (ways | !7).trailing_zeros() as usize;
        // SAFETY: the bits keep the way's number below 4.
        unsafe { *self.words.get_unchecked(way) }
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
    keys: Vec<&'a [u8]>,
}

impl<'a> TermTable<'a> {
    /// Sized for `query_bytes` of query text, where 16 bytes hold about one distinct term.
    fn new(query_bytes: usize) -> Self {
        TermTable {
            sets: empty_sets((query_bytes / 32).next_power_of_two().max(1 << 10)),
            keys: vec![&[]; FIRST_TERM],
        }
    }

    /// The word of the term for the token at `start..end` of `file`, in any letter case, or else 0,
    /// from a probe that starts at set `index`. A set that no term was placed past ends a probe.
    fn find(&self, file: &[u8], (start, end): (usize, usize), short: u64, mut index: usize) -> u32 {
        let long = long_key(file, start, end - start);
        loop {
            let set = &self.sets[index];
            let mut ways = set.ways(short) & set.long_ways(long);
            if long >> 31 != 0 {
                ways = self.ways_of_long_term(set, ways, &file[start..end]);
            }
            // One test, not two: that this finds a term or not is a coin toss.
            if ways | u32::from(set.words[3] == 0) != 0 {
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

    /// The term of the token at `start..end` of `file`, or else 0.
    fn find_token(&self, file: &[u8], start: usize, end: usize) -> u32 {
        let short = short_key(file, start, end - start);
        self.find(
            file,
            (start, end),
            short,
            home(self.sets.len(), hash(short)),
        )
    }

    /// Adds the query term at `start..end` of `file`, if it's new, and returns its id.
    fn intern(&mut self, file: &'a [u8], start: usize, end: usize) -> u32 {
        let word = self.find_token(file, start, end);
        if word != 0 {
            return word;
        }
        let term = self.keys.len() as u32;
        self.keys.push(&file[start..end]);
        let keys = (
            short_key(file, start, end - start),
            long_key(file, start, end - start),
        );
        place_term(&mut self.sets, keys, term);
        if self.keys.len() > self.sets.len() * 2 {
            let mut bigger = empty_sets(self.sets.len() * 2);
            for set in &self.sets {
                for way in (0..4).filter(|&way| set.short_keys[way] != 0) {
                    let keys = (set.short_keys[way], set.long_keys[way]);
                    place_term(&mut bigger, keys, set.words[way]);
                }
            }
            self.sets = bigger;
        }
        term
    }
}

fn place_term(sets: &mut [TermSet], (short, long): (u64, u32), word: u32) {
    let mut index = home(sets.len(), hash(short));
    while sets[index].short_keys[2] != 0 {
        sets[index].words[3] = OVERFLOWED;
        index = (index + 1) & (sets.len() - 1);
    }
    let set = &mut sets[index];
    let way = set.ways(0).trailing_zeros() as usize;
    (set.short_keys[way], set.long_keys[way], set.words[way]) = (short, long, word);
}

struct Query {
    slot: u32,   // A BOOL query's number, where its answer goes. A phrase has NONE.
    shared: u32, // The number of the term count that also goes into its answer, or NONE.
    start: u32,  // Its steps are `steps[start..]`, up to the next query's.
}

struct Queries<'a> {
    terms: TermTable<'a>,
    list: Vec<Query>, // With one more at the end, for where the last query's steps end.
    steps: Vec<u32>,
}

impl Queries<'_> {
    fn steps_of(&self, number: usize) -> &[u32] {
        &self.steps[self.list[number].start as usize..self.list[number + 1].start as usize]
    }
}

fn new_query(start: u32) -> Query {
    let (slot, shared) = (NONE, NONE);
    Query {
        slot,
        shared,
        start,
    }
}

/// Adds the terms waiting in `pending`, each a step's number and where its term is in the text, whose
/// sets have been prefetched, so that their cache misses overlap.
fn intern_pending<'a>(
    text: &'a [u8],
    terms: &mut TermTable<'a>,
    steps: &mut [u32],
    pending: &mut Vec<[u32; 3]>,
) {
    for [step, from, to] in pending.drain(..) {
        steps[step as usize] |= terms.intern(text, from as usize, to as usize);
    }
}

fn parse_queries(text: &[u8]) -> Queries<'_> {
    let mut terms = TermTable::new(text.len());
    let mut list = Vec::with_capacity(text.len() / 32);
    let mut steps = Vec::with_capacity(text.len() / 8);
    let (mut edges, mut pending) = (Vec::new(), Vec::new());
    let (mut pos, mut slot) = (0, 0);
    while pos < text.len() {
        let line_feed = token_edges(text, pos, &mut edges);
        let mut query = new_query(steps.len() as u32);
        if text[pos] == b'B' {
            query.slot = slot;
            slot += 1;
        }
        // The first field is BOOL or PHRASE. Then come terms, in a BOOL query with an operator between
        // each two, which goes in the top bits of the term after it.
        let mut op = 0;
        for (field, pair) in edges.chunks_exact(2).enumerate().skip(1) {
            let (from, to) = (pos + pair[0] as usize, pos + pair[1] as usize);
            if query.slot == NONE || field % 2 == 1 {
                let index = home(terms.sets.len(), hash(short_key(text, from, to - from)));
                // SAFETY: the set is in the table, and a prefetch has no other effect.
                unsafe { _mm_prefetch(terms.sets.as_ptr().add(index).cast(), _MM_HINT_T0) };
                pending.push([steps.len() as u32, from as u32, to as u32]);
                steps.push(op);
            } else {
                op = match text[from] {
                    b'A' => AND,
                    b'N' => NOT,
                    _ => 0,
                };
            }
        }
        if pending.len() >= 64 {
            intern_pending(text, &mut terms, &mut steps, &mut pending);
        }
        list.push(query);
        pos = line_feed + 1;
    }
    intern_pending(text, &mut terms, &mut steps, &mut pending);
    list.push(new_query(steps.len() as u32));
    Queries { terms, list, steps }
}

/// Visits the documents whose lines start in `start..end`, passing each one's id, where its text
/// starts, and its tokens' edges from there, then eight zeros, so that `look_up` can read four pairs
/// at a time. A line belongs to the range holding its first byte.
fn for_each_doc(
    docs: &[u8],
    (start, end): (usize, usize),
    mut visit: impl FnMut(u32, usize, &[u32]),
) {
    let mut line = start;
    while line > 0 && docs[line - 1] != b'\n' {
        line += 1;
    }
    // The range is rounded to whole words of 64 documents: it starts at its first document whose id
    // is a multiple of 64 and runs to the next range's, so threads never share a bitmap word.
    let mut aligned = false;
    let mut edges = Vec::new();
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
        line = token_edges(docs, pos + 1, &mut edges) + 1;
        edges.extend([0; 8]);
        if aligned {
            visit(id, pos + 1, &edges);
        }
    }
}

/// Terms of which every document matching a BOOL query contains at least one, chosen rare. The
/// query's last run, from its last OR (the first term counts as an OR from the empty set), is
/// intersected with each AND in it, so any of those ANDs is in every match, and so is the first
/// term when the query is a single run. A run without an AND adds its OR term to the anchors of
/// the query before it.
fn anchors(steps: &[u32], sample_df: &[u32], out: &mut Vec<u32>) {
    let last_or = steps.iter().rposition(|step| step >> 30 == 0).unwrap_or(0);
    let ands = steps[last_or + 1..].iter().filter(|step| *step & AND != 0);
    let head = steps.first().filter(|_| last_or == 0);
    let chosen = ands
        .chain(head)
        .min_by_key(|&step| sample_df[(step & TERM_BITS) as usize]);
    let term = chosen.unwrap_or(&steps[last_or]) & TERM_BITS;
    if !out.contains(&term) {
        out.push(term);
    }
    if chosen.is_none() {
        anchors(&steps[..last_or], sample_df, out);
    }
}

/// A BOOL query's check at one of its anchors. The anchor is present, so the marks of a document's
/// holding each of the query's other terms make a number, and that bit of `truth` says whether the
/// document counts here: whether it matches, and holds none of the query's earlier anchors, where it
/// has already counted. Most queries have few terms, and need a smaller check.
#[derive(Clone, Copy)]
struct Check<const TERMS: usize>(u32, [u32; TERMS]); // The truth table, and the terms.

impl<const TERMS: usize> Check<TERMS> {
    /// 1 if a document holding the terms marked in `present` counts here, else 0.
    fn counts(&self, present: &[u8]) -> u64 {
        let mut holds = 0;
        for (slot, &term) in self.1.iter().enumerate() {
            // SAFETY: `present` has a mark for every term.
            holds |= u32::from(unsafe { *present.get_unchecked(term as usize) }) << slot;
        }
        u64::from(self.0 >> holds & 1)
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
fn pack(steps: &[u32], anchor: u32, earlier: &[u32]) -> Check<5> {
    let (mut truth, mut blocked, mut terms, mut others) = (0, 0, [ABSENT; 5], 0);
    for step in steps {
        let term = step & TERM_BITS;
        let mut holds = INDEX_BIT_MASKS[5];
        if term != anchor {
            holds = INDEX_BIT_MASKS[others];
            terms[others] = term;
            others += 1;
        }
        truth = match step & !TERM_BITS {
            AND => truth & holds,
            NOT => truth & !holds,
            _ => truth | holds,
        };
        if earlier.contains(&term) {
            blocked |= holds;
        }
    }
    Check(((truth & !blocked) >> 32) as u32, terms)
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
    count_slot: Vec<u32>, // Which query or term count each count belongs to.
    posting_queries: Vec<u32>, // The queries anchored on a common term.
    slot_count: usize,    // The BOOL queries, then one for each term with a count, then a spare.
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
        for_each_doc(docs, (start, start + piece), |doc, text, edges| {
            sampled_docs.fetch_add(1, Relaxed);
            for pair in edges[..edges.len() - 8].chunks_exact(2) {
                let (start, end) = (text + pair[0] as usize, text + pair[1] as usize);
                let term = terms.find_token(docs, start, end) as usize;
                if seen_in[term] != doc as u8 {
                    seen_in[term] = doc as u8;
                    df[term].fetch_add(1, Relaxed);
                }
            }
        });
    });
    let df = df.into_iter().map(AtomicU32::into_inner).collect();
    (df, sampled_docs.into_inner())
}

fn make_plan(docs: &[u8], queries: &mut Queries, threads: usize) -> Plan {
    let term_count = queries.terms.keys.len();
    let (mut sample_df, sampled_docs) = sample_terms(docs, &queries.terms, threads);

    // A query anchored on a common term, with df above about N/24, is answered from postings: bitmaps
    // for its common terms and chains for the others. On memory alone, a bitmap (N/8 bytes) and a
    // chain (about 3 bytes per posting) cross near df = N/24. Any choice gives the same answers.
    let common = |term: u32| sample_df[term as usize] * 24 >= sampled_docs;
    // How a term is stored: 0 for a bitmap, 1 for a chain, and 2 for neither.
    let mut store_of = vec![2u8; term_count];
    // Each BOOL query's anchors, then NONE. A query reading postings has none.
    let (mut query_anchors, mut these) = (Vec::with_capacity(queries.list.len()), Vec::new());
    let mut posting_queries = Vec::new();
    let query_count = queries.list.len() - 1;
    for number in 0..query_count {
        if queries.list[number].slot == NONE {
            continue;
        }
        let steps = queries.steps_of(number);
        these.clear();
        anchors(steps, &sample_df, &mut these);
        if these.iter().any(|&term| common(term)) {
            posting_queries.push(number as u32);
            for term in steps.iter().map(|step| step & TERM_BITS) {
                store_of[term as usize] = u8::from(!common(term));
            }
        } else {
            // After the last AND or NOT, each OR's term alone makes a document match. The commonest
            // of them becomes the first anchor, needing no check: the term's own count serves every
            // query so anchored, and the other anchors skip the documents that hold it.
            let trailing = steps.iter().rposition(|step| step >> 30 != 0);
            let trailing = &steps[trailing.map_or(0, |at| at + 1)..];
            let sure = (0..these.len())
                .filter(|&at| trailing.iter().any(|step| step & TERM_BITS == these[at]))
                .max_by_key(|&at| sample_df[these[at] as usize]);
            if let Some(at) = sure {
                these.swap(0, at);
                queries.list[number].shared = 0;
            }
            query_anchors.extend_from_slice(&these);
        }
        query_anchors.push(NONE);
    }

    // Number the terms: those with bitmaps, then those with chains, then the rest, the commonest first
    // so that what the scan touches most sits together.
    let df_bucket = |term: usize| 255 - sample_df[term].min(255) as usize;
    let bucket = |term: usize| {
        usize::from(store_of[term]) + df_bucket(term) * usize::from(store_of[term] / 2)
    };
    let mut next = [0u32; 259];
    for term in FIRST_TERM..term_count {
        next[bucket(term) + 1] += 1;
    }
    let (bitmap_count, list_count) = (next[1], next[2]);
    // A hole keeps the terms with chains from sharing a word of a document's bits with bitmaps.
    next[0] = FIRST_TERM as u32;
    next[1] = bitmap_count.next_multiple_of(64);
    sum_sizes(&mut next);
    let chain_from = next[1];
    let number = |term: usize| {
        next[bucket(term)] += 1;
        next[bucket(term)] - 1
    };
    let new_number: Vec<u32> = (0..FIRST_TERM as u32)
        .chain((FIRST_TERM..term_count).map(number))
        .collect();
    let term_count = term_count + 64;
    let Queries { list, steps, terms } = queries;
    let (old_df, old_keys) = (sample_df.clone(), terms.keys.clone());
    sample_df.resize(term_count, 0);
    terms.keys.resize(term_count, &[]);
    for (term, &new) in new_number.iter().enumerate() {
        sample_df[new as usize] = old_df[term];
        terms.keys[new as usize] = old_keys[term];
    }
    for step in steps.iter_mut() {
        *step = *step & !TERM_BITS | new_number[(*step & TERM_BITS) as usize];
    }

    // Size the lists.
    let mut phrase_start = vec![0u32; term_count + 1];
    let (mut short_start, mut long_start) = (phrase_start.clone(), phrase_start.clone());
    let mut shared_of = vec![NONE; term_count];
    let mut shared_count = 0;
    let bool_count = list.iter().filter(|query| query.slot != NONE).count() as u32;
    let steps = &*steps;
    let steps_of = |number: usize| &steps[list_range(list, number)];
    // A short check holds the query's other terms, and most queries have up to four.
    let is_short = |steps: &[u32]| steps.len() <= 4;
    let mut at_anchor = 0;
    let mut shared_numbers = vec![NONE; query_count];
    for number in 0..query_count {
        if list[number].slot == NONE {
            continue;
        }
        let these = &mut query_anchors[at_anchor..];
        if list[number].shared != NONE {
            let first = new_number[these[0] as usize] as usize;
            if shared_of[first] == NONE {
                shared_of[first] = shared_count;
                shared_count += 1;
            }
            shared_numbers[number] = bool_count + shared_of[first];
        }
        let mut count = 0;
        while these[count] != NONE {
            these[count] = new_number[these[count] as usize];
            if count > 0 || list[number].shared == NONE {
                let starts = if is_short(steps_of(number)) {
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
    let rarest = |steps: &[u32]| {
        (0..steps.len())
            .min_by_key(|&i| sample_df[steps[i] as usize])
            .unwrap_or(0)
    };
    for number in (0..query_count).filter(|&number| list[number].slot == NONE) {
        let phrase = steps_of(number);
        phrase_start[phrase[rarest(phrase)] as usize + 1] += 3 + phrase.len() as u32;
    }
    sum_sizes(&mut phrase_start);
    sum_sizes(&mut short_start);
    sum_sizes(&mut long_start);

    // Fill them.
    let (short_count, long_count) = (short_start[term_count], long_start[term_count]);
    let check_count = short_count + long_count;
    let mut phrases = vec![0u32; phrase_start[term_count] as usize];
    let mut short_checks = vec![Check(0, [0; 3]); short_count as usize];
    let mut long_checks = vec![Check(0, [0; 5]); long_count as usize];
    let mut count_slot: Vec<u32> = (0..check_count + shared_count + 1)
        .map(|at| (bool_count + at).wrapping_sub(check_count))
        .collect();
    let mut next_phrase = phrase_start.clone();
    let (mut next_short, mut next_long) = (short_start.clone(), long_start.clone());
    let mut at_anchor = 0;
    for number in 0..query_count {
        let steps = steps_of(number);
        if list[number].slot == NONE {
            let position = rarest(steps);
            let at = &mut next_phrase[steps[position] as usize];
            let entry = &mut phrases[*at as usize..][..3 + steps.len()];
            entry[..3].copy_from_slice(&[number as u32, position as u32, steps.len() as u32]);
            entry[3..].copy_from_slice(steps);
            *at += entry.len() as u32;
        } else {
            let these = &query_anchors[at_anchor..];
            let count = these.iter().position(|&term| term == NONE).unwrap_or(0);
            let unchecked = usize::from(list[number].shared != NONE);
            for (k, &term) in these[..count].iter().enumerate().skip(unchecked) {
                let check = pack(steps, term, &these[..k]);
                let (next, offset) = if is_short(steps) {
                    let Check(truth, [first, second, third, ..]) = check;
                    short_checks[next_short[term as usize] as usize] =
                        Check(truth, [first, second, third]);
                    (&mut next_short[term as usize], 0)
                } else {
                    long_checks[next_long[term as usize] as usize] = check;
                    (&mut next_long[term as usize], short_count)
                };
                count_slot[(*next + offset) as usize] = list[number].slot;
                *next += 1;
            }
            at_anchor += count + 1;
        }
    }
    for number in 0..query_count {
        if shared_numbers[number] != NONE {
            list[number].shared = shared_numbers[number];
        }
    }

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
    let starts = |start: &[u32], term: usize| start[term] != start[term + 1];
    let word_of = |(old, &term): (usize, &u32)| {
        let term = term as usize;
        let has_checks = starts(&short_start, term) || starts(&long_start, term);
        let has_flags = old >= FIRST_TERM;
        term as u32
            | u32::from(has_flags && starts(&phrase_start, term)) * PHRASE_ANCHOR
            | u32::from(has_flags && (has_checks || shared_of[term] != NONE)) * BOOL_ANCHOR
            | u32::from(term.wrapping_sub(chain_from as usize) < list_count as usize) * CHAINED
            | u32::from(old == OVERFLOWED as usize || old_keys[old].len() > 8) * UNSURE
    };
    let new_words: Vec<u32> = new_number.iter().enumerate().map(word_of).collect();
    // Empty ways hold term 0, which stays 0, so this needs no branch on which ways are in use.
    for word in terms.sets.iter_mut().flat_map(|set| &mut set.words) {
        *word = new_words[*word as usize];
    }
    Plan {
        bitmap_count: bitmap_count as usize,
        list_count: list_count as usize,
        phrase_start,
        phrases,
        anchor_lists,
        short_checks,
        long_checks,
        count_slot,
        posting_queries,
        slot_count: (bool_count + shared_count + 1) as usize,
    }
}

/// The steps of a query, by its own and the next query's start.
fn list_range(list: &[Query], number: usize) -> std::ops::Range<usize> {
    list[number].start as usize..list[number + 1].start as usize
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
    fn at(number: u32) -> (usize, usize) {
        let at = number as usize - 1;
        (at / CHUNK_BLOCKS, at % CHUNK_BLOCKS)
    }

    fn get(&self, number: u32) -> &Block {
        let (chunk, at) = Self::at(number);
        &self.chunks[chunk][at]
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

/// A high bit of `hits`, past any count, which says that no more documents are among the smallest.
const TOO_BIG: u64 = 1 << 62;

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
    /// Returns `TOO_BIG` if the document is past the limit, as all the thread's later ones will be too.
    #[cold]
    fn add_smallest(&self, slot: usize, doc: u32) -> u64 {
        if doc >= self.limit[slot].load(Relaxed) {
            return TOO_BIG;
        }
        let smallest = &mut self.slots[slot].lock().unwrap().smallest;
        insert_smallest(smallest, doc);
        if smallest[SMALLEST - 1] != NONE {
            self.limit[slot].store(smallest[SMALLEST - 1], Relaxed);
        }
        0
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

/// Looks the tokens up: first finding each one's key and prefetching its set, so that cache misses
/// overlap, then comparing the keys without a branch on what that finds. Only a word marked `UNSURE`
/// needs a full look, and few are. Lists the words that are terms, and the positions of phrase anchors,
/// and returns how long those lists are.
#[inline(never)]
fn look_up(
    docs: &[u8],
    terms: &TermTable,
    text: usize,
    edges: &[u32],
    lists: [&mut [u32]; 4],
    [keys, places]: [&mut [u64]; 2],
) -> [usize; 2] {
    let [tokens, later, marks, phrase_at] = lists;
    let (sets, text_at) = (terms.sets.as_ptr(), docs.as_ptr().wrapping_add(text));
    let count = edges.len() / 2 - 4;
    let shift = 64 - terms.sets.len().trailing_zeros() as i32;
    let mut at = 0;
    while at < count {
        // SAFETY: `edges` has four pairs more than there are tokens, each at or before a token's start,
        // whose load stays in the mapping as in `short_key`. Keys and places have room for three more.
        // The set is in the table, and a prefetch has no other effect. The build targets x86-64-v3,
        // which has AVX2.
        unsafe {
            let pairs = edges.as_ptr().add(2 * at);
            let start = |i: usize| text_at.add(*pairs.add(2 * i) as usize);
            let load = |i: usize| start(i).cast::<i64>().read_unaligned();
            let bytes = _mm256_setr_epi64x(load(0), load(1), load(2), load(3));
            let pair = _mm256_loadu_si256(pairs.cast());
            let lens = _mm256_srli_epi64(_mm256_sub_epi64(pair, _mm256_slli_epi64(pair, 32)), 32);
            let past = _mm256_sllv_epi64(_mm256_set1_epi64x(-1), _mm256_slli_epi64(lens, 3));
            let lower = _mm256_or_si256(bytes, _mm256_set1_epi8(0x20));
            let more = _mm256_slli_epi64(_mm256_cmpgt_epi64(lens, _mm256_set1_epi64x(8)), 63);
            let key = _mm256_or_si256(_mm256_andnot_si256(past, lower), more);
            let index = _mm256_srl_epi64(hashes(key), _mm_cvtsi32_si128(shift));
            let base = _mm256_set1_epi64x(sets as i64);
            let place = _mm256_add_epi64(_mm256_slli_epi64(index, 6), base);
            _mm256_storeu_si256(keys.as_mut_ptr().add(at).cast(), key);
            _mm256_storeu_si256(places.as_mut_ptr().add(at).cast(), place);
            for i in 0..4 {
                _mm_prefetch(*places.get_unchecked(at + i) as *const i8, _MM_HINT_T0);
            }
        }
        at += 4;
    }
    let (mut later_count, mut marked, mut phrases) = (0, 0, 0);
    at = 0;
    while at < count {
        // SAFETY: the places are in the table, and the lists have room for every token and three more.
        // `BIT_POSITIONS` has a row for each of the masks. The build targets x86-64-v3, which has AVX.
        unsafe {
            let word = |i: usize| {
                let set = &*(*places.get_unchecked(at + i) as *const TermSet);
                set.word(set.ways(*keys.get_unchecked(at + i))) as i32
            };
            let words = _mm_setr_epi32(word(0), word(1), word(2), word(3));
            _mm_storeu_si128(tokens.as_mut_ptr().add(at).cast(), words);
            let mask = |of| _mm_movemask_ps(_mm_castsi128_ps(of)) as usize;
            let row = |mask: usize| BIT_POSITIONS.as_ptr().add(mask).cast();
            let order = |mask: usize| _mm_cvtepu8_epi32(_mm_loadl_epi64(row(mask)));
            // A word that is surely a term's is positive as a signed number.
            let sure = mask(_mm_cmpgt_epi32(words, _mm_setzero_si128()));
            let kept = _mm_permutevar_ps(_mm_castsi128_ps(words), order(sure));
            _mm_storeu_ps(marks.as_mut_ptr().add(marked).cast(), kept);
            marked += sure.count_ones() as usize;
            // A word with either of the top two flags gets another look.
            let again = mask(_mm_or_si128(words, _mm_slli_epi32(words, 1)));
            let positions = _mm_add_epi32(order(again), _mm_set1_epi32(at as i32));
            _mm_storeu_si128(later.as_mut_ptr().add(later_count).cast(), positions);
            later_count += again.count_ones() as usize;
        }
        at += 4;
    }
    for &at in &later[..later_count] {
        let at = at as usize;
        // SAFETY: this is a token's position or at most three past the last, for which `tokens` has
        // room, and the other lists have room for every token.
        unsafe {
            let mut word = *tokens.get_unchecked(at);
            // Past the last token, the pairs are empty, whose words are 0 or the unused way's.
            if word >= UNSURE && at < count {
                let pair = edges.get_unchecked(2 * at..2 * at + 2);
                let range = (text + pair[0] as usize, text + pair[1] as usize);
                let index = (*places.get_unchecked(at) as usize - sets as usize) >> 6;
                word = terms.find(docs, range, *keys.get_unchecked(at), index);
                *tokens.get_unchecked_mut(at) = word;
                *marks.get_unchecked_mut(marked) = word;
                marked += usize::from(word != 0);
            }
            *phrase_at.get_unchecked_mut(phrases) = at as u32;
            phrases += (word >> 30 & 1) as usize;
        }
    }
    [marked, phrases]
}

/// Marks what a document holds, with a byte per term, and lists what to do about it: the terms with BOOL
/// checks or chains, once each. Returns how long the lists are.
#[inline(never)]
fn mark(words: &[u32], present: &mut [u8], lists: [&mut [u32]; 2]) -> [usize; 2] {
    let [bool_terms, chain_terms] = lists;
    let (mut bools, mut chains) = (0, 0);
    for &word in words {
        let term = word & TERM_BITS;
        // SAFETY: `present` has a mark for every term, and each list has room for every token.
        unsafe {
            let held = present.get_unchecked_mut(term as usize);
            let first = usize::from(*held ^ 1);
            *held = 1;
            *bool_terms.get_unchecked_mut(bools) = term;
            bools += (word >> 29 & 1) as usize & first;
            *chain_terms.get_unchecked_mut(chains) = term;
            chains += (word >> 28 & 1) as usize & first;
        }
    }
    [bools, chains]
}

/// Sets a document's bit in the bitmap of each term with one that it holds. No other thread writes this
/// document's bitmap words, so that takes no atomic.
fn set_bitmaps(present: &[u8], bitmaps: &[AtomicU64], words: usize, doc: u32) {
    let (doc_word, doc_bit) = (doc as usize / 64, 1u64 << (doc % 64));
    for (index, marks) in present.chunks_exact(64).enumerate() {
        let half = |at: usize| {
            // SAFETY: a chunk has these 64 bytes, and the build targets x86-64-v3, which has AVX2. The
            // shift moves a mark to the top bit of its byte.
            unsafe {
                let marks = _mm256_loadu_si256(marks.as_ptr().add(at).cast());
                _mm256_movemask_epi8(_mm256_slli_epi16(marks, 7)) as u32
            }
        };
        let mut rest = u64::from(half(0)) | u64::from(half(32)) << 32;
        while rest != 0 {
            let at = (index * 64 + rest.trailing_zeros() as usize) * words + doc_word;
            bitmaps[at].store(bitmaps[at].load(Relaxed) | doc_bit, Relaxed);
            rest &= rest - 1;
        }
    }
}

/// Adds a document to the chains of these terms.
fn extend_chains(chained: &[u32], chain_from: u32, result: &mut ScanResult, doc: u32) {
    for &term in chained {
        let list = &mut result.lists[(term - chain_from) as usize];
        let blocks = &mut result.blocks;
        // A tail of 0 gets a new block too: then `fill` is 0, and there's no block 0.
        if list.tail == 0
            || list.fill as usize == BLOCK_POSTINGS
            || doc - blocks.get(list.tail).first_doc > u32::from(u16::MAX)
        {
            let (previous, first_doc, postings) = (list.tail, doc, [0; BLOCK_POSTINGS]);
            list.tail = blocks.add(Block {
                previous,
                first_doc,
                postings,
            });
            list.fill = 0;
        }
        let (chunk, at) = Blocks::at(list.tail);
        let block = &mut blocks.chunks[chunk][at];
        block.postings[list.fill as usize] = (doc - block.first_doc) as u16;
        list.fill += 1;
    }
}

/// Checks the phrases anchored at these positions of a document's tokens.
fn check_phrases(
    anchored: &[u32],
    tokens: &[u32],
    plan: &Plan,
    doc: u32,
    hits: &mut Vec<(u32, u32)>,
) {
    for &at in anchored {
        let at = at as usize;
        let term = (tokens[at] & TERM_BITS) as usize;
        let from = plan.phrase_start[term] as usize;
        let mut entries = &plan.phrases[from..plan.phrase_start[term + 1] as usize];
        while let [query, position, len, rest @ ..] = entries {
            let (phrase, after) = rest.split_at(*len as usize);
            entries = after;
            let Some(start) = at.checked_sub(*position as usize) else {
                continue;
            };
            let run = tokens.get(start..start + phrase.len());
            let same = |run: &[u32]| run.iter().zip(phrase).all(|(&t, &p)| t & TERM_BITS == p);
            // An anchor can occur twice in a document, but the document counts once.
            let mut same_doc = hits.iter().rev().take_while(|hit| hit.1 == doc);
            if run.is_some_and(same) && !same_doc.any(|hit| hit.0 == *query) {
                hits.push((*query, doc));
            }
        }
    }
}

/// Counts a document at these anchors of it, for each term's own count and each check that says so.
/// Prefetching first lets cache misses overlap, and listing the short checks before running them
/// replaces an unpredictable loop exit per anchor by one loop.
#[inline(never)]
fn check_bools(
    anchors: &[u32],
    present: &[u8],
    plan: &Plan,
    answers: &Answers,
    hits: &mut [u64],
    short: &mut [u32],
    doc: u32,
) {
    let this_doc = (1 << SUM_BITS) + u64::from(doc);
    let lists = &plan.anchor_lists[..];
    let short_hits = hits.as_ptr();
    // SAFETY: `hits` has every count. A thread's hits ascend, so only its first ten can be among the
    // smallest.
    let mut add = |counts: u64, at: usize| unsafe {
        let hit = hits.get_unchecked_mut(at);
        *hit += counts * this_doc;
        // Below 11 and counted, in a way the compiler can't turn into a coin-toss branch.
        if hit.wrapping_sub((SMALLEST as u64 + 1) << SUM_BITS) >> 63 & counts != 0 {
            *hit |= answers.add_smallest(plan.count_slot[at] as usize, doc);
        }
    };
    // SAFETY: a prefetch has no other effect, whatever the address.
    let prefetch = |at: *const u8| unsafe { _mm_prefetch(at.cast(), _MM_HINT_T0) };
    for &term in anchors {
        prefetch(lists.as_ptr().wrapping_add(term as usize).cast());
    }
    let (short_count, mut listed) = (plan.short_checks.len(), 0);
    for &term in anchors {
        // SAFETY: `lists` has one entry more than there are terms, `short` has room for all the checks
        // and four more, and an anchor's checks are all in `long_checks`.
        unsafe {
            let first = lists.get_unchecked(term as usize);
            let next = lists.get_unchecked(term as usize + 1);
            let (from, to) = (first.short_from as usize, next.short_from as usize);
            prefetch(plan.short_checks.as_ptr().wrapping_add(from).cast());
            prefetch(short_hits.wrapping_add(from).cast());
            let at = _mm_add_epi32(_mm_set1_epi32(from as i32), _mm_setr_epi32(0, 1, 2, 3));
            _mm_storeu_si128(short.as_mut_ptr().add(listed).cast(), at);
            for at in from + 4..to {
                short[listed + at - from] = at as u32;
            }
            listed += to - from;
            add(1, first.shared as usize);
            for at in first.long_from as usize..next.long_from as usize {
                let check = plan.long_checks.get_unchecked(at);
                add(check.counts(present), short_count + at);
            }
        }
    }
    for &at in &short[..listed] {
        let at = at as usize;
        // SAFETY: the list came from `anchor_lists`.
        let check = unsafe { plan.short_checks.get_unchecked(at) };
        add(check.counts(present), at);
    }
}

/// Claims ranges of the corpus until none is left, and scans them.
fn scan_ranges(
    docs: &[u8],
    ranges: (usize, &AtomicUsize),
    queries: &Queries,
    plan: &Plan,
    shared: (&[AtomicU64], usize, &Answers),
) -> ScanResult {
    let (bitmaps, words, answers) = shared;
    let mut result = ScanResult {
        blocks: Blocks::default(),
        lists: vec![ListState { tail: 0, fill: 0 }; plan.list_count],
        hits: vec![0; plan.count_slot.len()],
        phrase_hits: Vec::new(),
    };
    let mut present = vec![0u8; queries.terms.keys.len()];
    let chain_from = (FIRST_TERM + plan.bitmap_count.next_multiple_of(64)) as u32;
    let (mut tokens, mut keys, mut later, mut marks) = (vec![], vec![], vec![], vec![]);
    let mut places = vec![];
    let mut short = vec![0; plan.short_checks.len() + 4];
    let (mut phrase_at, mut bool_terms, mut chain_terms) = (Vec::new(), Vec::new(), Vec::new());
    loop {
        let start = ranges.1.fetch_add(ranges.0, Relaxed);
        if start >= docs.len() {
            return result;
        }
        for_each_doc(docs, (start, start + ranges.0), |doc, text, edges| {
            let count = edges.len() / 2 - 4;
            if tokens.len() < count + 4 {
                tokens.resize(count + 4, 0);
                keys.resize(count + 4, 0);
                places.resize(count + 4, 0);
                later.resize(count + 4, 0);
                marks.resize(count + 4, 0);
                phrase_at.resize(count + 4, 0);
                bool_terms.resize(count + 4, 0);
                chain_terms.resize(count + 4, 0);
            }
            let [marked, phrases] = look_up(
                docs,
                &queries.terms,
                text,
                edges,
                [&mut tokens, &mut later, &mut marks, &mut phrase_at],
                [&mut keys, &mut places],
            );
            let (in_doc, marked) = (&tokens[..count], &marks[..marked]);
            let lists = [&mut bool_terms[..], &mut chain_terms[..]];
            let [bools, chains] = mark(marked, &mut present, lists);
            set_bitmaps(
                &present[FIRST_TERM..chain_from as usize],
                bitmaps,
                words,
                doc,
            );
            extend_chains(&chain_terms[..chains], chain_from, &mut result, doc);
            check_phrases(
                &phrase_at[..phrases],
                in_doc,
                plan,
                doc,
                &mut result.phrase_hits,
            );
            check_bools(
                &bool_terms[..bools],
                &present,
                plan,
                answers,
                &mut result.hits,
                &mut short,
                doc,
            );
            for &word in marked {
                // SAFETY: `present` has a mark for every term.
                unsafe { *present.get_unchecked_mut((word & TERM_BITS) as usize) = 0 };
            }
        });
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
    let mut slots: Vec<&mut Answer> = answers
        .slots
        .iter_mut()
        .map(|slot| slot.get_mut().unwrap())
        .collect();
    for result in results {
        for (&slot, &hits) in plan.count_slot.iter().zip(&result.hits) {
            slots[slot as usize].count += (hits & !TOO_BIG) >> SUM_BITS;
            slots[slot as usize].sum += hits & ((1 << SUM_BITS) - 1);
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
    fn run(&mut self, steps: &[u32], index: &Index) {
        self.bits.fill(0);
        for step in steps {
            let number = (step & TERM_BITS) as usize - FIRST_TERM;
            let other = if number < index.bitmap_count {
                &index.bitmaps[number * index.words..(number + 1) * index.words]
            } else {
                self.other.fill(0);
                let list = number - index.bitmap_count.next_multiple_of(64);
                index.set_bits_of_list(list, &mut self.other);
                &self.other
            };
            for (word, &with) in self.bits.iter_mut().zip(other) {
                *word = match step & !TERM_BITS {
                    AND => *word & with,
                    NOT => *word & !with,
                    _ => *word | with,
                };
            }
        }
    }

    fn answer(&self) -> Answer {
        let (mut smallest, mut found) = ([NONE; SMALLEST], 0);
        for (i, &word) in self.bits.iter().enumerate() {
            let mut rest = word;
            while rest != 0 && found < SMALLEST {
                smallest[found] = 64 * i as u32 + rest.trailing_zeros();
                found += 1;
                rest &= rest - 1;
            }
            if found == SMALLEST {
                break;
            }
        }
        let (count, sum) = count_and_sum(&self.bits);
        Answer {
            count,
            sum,
            smallest,
        }
    }
}

/// How many bits are set in these words, a multiple of 4 of them, and the sum of their positions. Each
/// nibble's count and sum of positions are table lookups, so a byte gets them at once. A position is
/// the word's start, plus the byte's start in the word, plus its place in the byte.
fn count_and_sum(bits: &[u64]) -> (u64, u64) {
    const COUNT: [u8; 16] = [0, 1, 1, 2, 1, 2, 2, 3, 1, 2, 2, 3, 2, 3, 3, 4];
    const LOW_SUM: [u8; 16] = [0, 0, 1, 1, 2, 2, 3, 3, 3, 3, 4, 4, 5, 5, 6, 6];
    const HIGH_SUM: [u8; 16] = [0, 4, 5, 9, 6, 10, 11, 15, 7, 11, 12, 16, 13, 17, 18, 22];
    // SAFETY: the loads have the length they say, and the build targets x86-64-v3, which has AVX2.
    unsafe {
        let table = |t: &[u8; 16]| _mm256_broadcastsi128_si256(_mm_loadu_si128(t.as_ptr().cast()));
        let (count_of, low_sum, high_sum) = (table(&COUNT), table(&LOW_SUM), table(&HIGH_SUM));
        let (nibble, zero) = (_mm256_set1_epi8(15), _mm256_setzero_si256());
        let look = |table, at| _mm256_shuffle_epi8(table, at);
        let byte_starts = _mm256_set1_epi64x(0x3830_2820_1810_0800);
        let (mut counts, mut sums, mut byte_sums) = (zero, zero, zero);
        let mut word_starts = _mm256_setr_epi64x(0, 64, 128, 192);
        for four in bits.chunks_exact(4) {
            let words = _mm256_loadu_si256(four.as_ptr().cast());
            let low = _mm256_and_si256(words, nibble);
            let high = _mm256_and_si256(_mm256_srli_epi16(words, 4), nibble);
            let ones = _mm256_add_epi8(look(count_of, low), look(count_of, high));
            let places = _mm256_add_epi8(look(low_sum, low), look(high_sum, high));
            let in_word = _mm256_sad_epu8(ones, zero);
            counts = _mm256_add_epi64(counts, in_word);
            sums = _mm256_add_epi64(sums, _mm256_sad_epu8(places, zero));
            sums = _mm256_add_epi64(sums, _mm256_mul_epu32(in_word, word_starts));
            let by_pair = _mm256_maddubs_epi16(ones, byte_starts);
            let by_half = _mm256_madd_epi16(by_pair, _mm256_set1_epi16(1));
            byte_sums = _mm256_add_epi32(byte_sums, by_half);
            word_starts = _mm256_add_epi64(word_starts, _mm256_set1_epi64x(256));
        }
        let total = |v: __m256i| -> u64 { std::mem::transmute::<_, [u64; 4]>(v).iter().sum() };
        let low_half = _mm256_and_si256(byte_sums, _mm256_set1_epi64x(u32::MAX.into()));
        let byte_sum = total(low_half) + total(_mm256_srli_epi64(byte_sums, 32));
        (total(counts), total(sums) + byte_sum)
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
    for &doc in &smallest[..smallest.len().min(count as usize)] {
        out.push(b' ');
        push_decimal(out, u64::from(doc));
    }
    out.push(b'\n');
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
    let (list, query_count) = (&queries.list, queries.list.len() - 1);
    on_threads(threads, || {
        let bits = vec![0; index.words.next_multiple_of(4)];
        let mut evaluator = Evaluator {
            other: bits.clone(),
            bits,
        };
        while let Some(&number) = plan.posting_queries.get(next_query.fetch_add(1, Relaxed)) {
            evaluator.run(queries.steps_of(number as usize), index);
            let slot = list[number as usize].slot as usize;
            *index.answers.slots[slot].lock().unwrap() = evaluator.answer();
        }
    });
    on_threads(threads, || {
        let (mut text, mut sorted) = (Vec::new(), Vec::new());
        let claim = || Some(next_batch.fetch_add(1, Relaxed)).filter(|b| b * BATCH < query_count);
        while let Some(batch) = claim() {
            text.clear();
            for number in batch * BATCH..((batch + 1) * BATCH).min(query_count) {
                let query = &list[number];
                if query.slot == NONE {
                    let (from, to) = (index.hit_start[number], index.hit_start[number + 1]);
                    sorted.clear();
                    sorted.extend_from_slice(&index.hits[from as usize..to as usize]);
                    sorted.sort_unstable();
                    let sum = sorted.iter().map(|&doc| u64::from(doc)).sum();
                    print_line(
                        &mut text,
                        sorted.len() as u64,
                        sum,
                        &sorted[..sorted.len().min(SMALLEST)],
                    );
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
                    print_line(&mut text, answer.count, answer.sum, &answer.smallest);
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
    let (count, sum, smallest) = (0, 0, [NONE; SMALLEST]);
    let mut answers = Answers {
        limit: (0..plan.slot_count).map(|_| AtomicU32::new(NONE)).collect(),
        slots: (0..plan.slot_count)
            .map(|_| {
                Mutex::new(Answer {
                    count,
                    sum,
                    smallest,
                })
            })
            .collect(),
    };
    let file = File::create(Path::new(&out_dir).join("results.txt"))?;
    // Enough ranges that a thread slowed for a while doesn't hold the others up.
    let range_size = (docs.len() / (threads * RANGES_PER_THREAD)).max(1);
    let next_range = AtomicUsize::new(0);
    let results: Vec<ScanResult> = thread::scope(|scope| {
        let scan = |_| {
            let ranges = (range_size, &next_range);
            let (queries, plan, answers) = (&queries, &plan, &answers);
            scope.spawn(move || scan_ranges(docs, ranges, queries, plan, (shared, words, answers)))
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
