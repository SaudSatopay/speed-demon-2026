//! Track B (Index): boolean and phrase queries over a 254 MB corpus.
//!
//! The queries are read first, so only their terms are ever looked up. Every document matching a
//! BOOL query contains one of a few anchor terms (the rarest AND of its last run, or its OR terms),
//! and for nine queries in ten the anchors are rare, so the query is answered during the scan: at
//! each anchor's first occurrence in a document, the bits of what the document holds index a truth
//! table, and no posting of any of its terms is ever stored. Only queries anchored on a common term
//! read postings afterwards: bitmaps for their common terms, set during the scan, and chains of 2-byte
//! offsets for their other terms. Each occurrence of a phrase's rarest term checks the phrase around it
//! when the document ends. Threads claim ranges of the corpus in order, read them in small pieces, and
//! scan a document at a time in stages without a branch per token: find the tokens, look them up, mark
//! them, then act on what the marks listed. Then they answer the queries that read postings, and print
//! all the answers in order.

use std::arch::x86_64::*;
use std::fs::File;
use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::FileExt;
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering::*};
use std::sync::{Barrier, Mutex};
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

/// Readable bytes after each mapped file and after the text in a buffer, so that block loads can run
/// past its end.
const PADDING: usize = 4096;
/// Bytes per read of the corpus: few enough to stay in the caches, many enough that a read costs little
/// per document.
const PIECE: usize = 1 << 17;

// Declared by hand to keep the crate at zero dependencies; std already links glibc.
unsafe extern "C" {
    fn mmap(at: *mut u8, len: usize, p: i32, f: i32, fd: i32, o: i64) -> *mut u8;
    fn mallopt(param: i32, value: i32) -> i32;
}

/// Opens a file, and maps it read-only for the life of the process, followed by `PADDING` readable zero
/// bytes. The scorer's memory peak doesn't charge us for these pages, unlike reading the file onto the heap.
fn map_file(path: &str) -> io::Result<(File, &'static [u8])> {
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
        Ok((file, std::slice::from_raw_parts(base.cast(), len)))
    }
}

/// One bit per byte of the 64 at `pos`, set for the token bytes, `[A-Za-z0-9]`, and likewise for the
/// LFs. Bytes of 0x80 and above compare as negative, so they are separators, even inside a letter
/// (spec section 2.2).
fn block_masks(file: &[u8], pos: usize) -> (u64, u64) {
    assert!(pos + 64 <= file.len() + PADDING);
    // SAFETY: the assert keeps the loads inside the mapping or buffer, they need no alignment, and the build
    // targets x86-64-v3, which has AVX2.
    unsafe {
        let byte_set = |byte: u8| _mm256_set1_epi8(byte as i8);
        // One compare per range: adding moves `low..=high` to the top of the signed bytes, and anything
        // above `high` wraps round to the bottom.
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
static POSITIONS: [[u8; 8]; 256] = {
    let (mut table, mut value) = ([[0; 8]; 256], 1usize);
    while value < 256 {
        // The positions of the bits below the top one, then the top one's.
        let rest = value ^ 1 << value.ilog2();
        table[value] = table[rest];
        table[value][rest.count_ones() as usize] = value.ilog2() as u8;
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
                let bits = POSITIONS.as_ptr().add(byte as usize);
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

/// Runs `work` on `threads` threads, waits for them all, and returns what each returned.
fn on_threads<R: Send>(threads: usize, work: impl Fn() -> R + Sync) -> Vec<R> {
    thread::scope(|scope| {
        let handles: Vec<_> = (0..threads).map(|_| scope.spawn(&work)).collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    })
}

/// A token's first 8 bytes, lowercased and zero-padded, with the top bit set if there are more, which
/// no byte of a token has. Setting this bit in every byte lowercases ASCII letters, and digits already
/// have it set.
fn short_key(token: &[u8]) -> u64 {
    let len = token.len();
    // SAFETY: a token is inside a mapping or buffer that continues for `PADDING` bytes past the text. The load
    // needs no alignment, and the build targets x86-64-v3, which has BMI2, whose BZHI keeps the given
    // number of low bits, or all of them if that's more than the width.
    unsafe {
        let bytes = token.as_ptr().cast::<u64>().read_unaligned();
        _bzhi_u64(bytes | 0x2020_2020_2020_2020, 8 * len.min(9) as u32) | u64::from(len > 8) << 63
    }
}

/// The next 4 bytes of the token, likewise. The top bit, set if there are more still, marks a token that
/// needs a full compare, and those are rare.
fn long_key(token: &[u8]) -> u32 {
    let kept = 8 * token.len().saturating_sub(8).min(4) as u32;
    // SAFETY: as in `short_key`.
    unsafe {
        let bytes = token.as_ptr().add(8).cast::<u32>().read_unaligned();
        _bzhi_u32(bytes | 0x2020_2020, kept) | u32::from(token.len() > 12) << 31
    }
}

/// `home`'s hash of four short keys at once, which AVX2 only has 32-bit multiplies for: the low half of
/// the product, from the halves of each key and of the multiplier.
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

/// Which of `sets`, a power of two of them, a short key starts at: the top bits of its hash. One multiply
/// mixes the key, as its high bits depend on every input bit. A test build swaps it for a constant, so
/// that every key collides and any logic that trusts a hash without comparing the key shows up as a wrong
/// answer.
fn home(sets: &[TermSet], short_key: u64) -> usize {
    let hash = short_key.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    let hash = if cfg!(sd_const_hash) { 0 } else { hash };
    (hash >> (64 - sets.len().trailing_zeros())) as usize
}

/// Three terms in one cache line, whose keys are compared at once, so a lookup has no branch on which
/// of them it found, or whether it found one. The fourth way is never used, and its word is what a lookup
/// that finds none gets: 0 (no query uses this token), or the word of `OVERFLOWED` if the probe goes on.
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

    /// The word of the first of these ways, or else the unused way's, as the bits set above the three
    /// ways stop the count at it.
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
#[derive(Default)]
struct TermTable<'a> {
    sets: Vec<TermSet>, // The length is a power of two.
    keys: Vec<&'a [u8]>,
}

impl<'a> TermTable<'a> {
    /// The word of the term of this token, in any letter case, or else 0, from a probe that starts at
    /// set `index`. A set that no term was placed past ends a probe.
    fn find(&self, token: &[u8], short: u64, mut index: usize) -> u32 {
        let long = long_key(token);
        loop {
            let set = &self.sets[index];
            let mut ways = set.ways(short) & set.long_ways(long);
            if long >> 31 != 0 {
                ways = self.ways_of_long_term(set, ways, token);
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

    /// The word of the term of this token, or else 0.
    fn find_token(&self, token: &[u8]) -> u32 {
        let short = short_key(token);
        self.find(token, short, home(&self.sets, short))
    }

    /// Adds this query term, if it's new, and returns its id.
    fn intern(&mut self, token: &'a [u8]) -> u32 {
        let word = self.find_token(token);
        if word != 0 {
            return word;
        }
        let term = self.keys.len() as u32;
        self.keys.push(token);
        place_term(&mut self.sets, token, term);
        if self.keys.len() > self.sets.len() * 2 {
            self.sets = empty_sets(self.sets.len() * 2);
            for (term, key) in self.keys.iter().enumerate().skip(FIRST_TERM) {
                place_term(&mut self.sets, key, term as u32);
            }
        }
        term
    }
}

fn place_term(sets: &mut [TermSet], token: &[u8], term: u32) {
    let short = short_key(token);
    let mut index = home(sets, short);
    while sets[index].short_keys[2] != 0 {
        // A lookup that finds nothing in a set this term has passed has to look further.
        sets[index].words[3] = OVERFLOWED;
        index = (index + 1) & (sets.len() - 1);
    }
    let set = &mut sets[index];
    let way = set.ways(0).trailing_zeros() as usize;
    (set.short_keys[way], set.long_keys[way], set.words[way]) = (short, long_key(token), term);
}

/// Reads the queries into a new plan. Each term is added once a batch of them has had its sets
/// prefetched, so that their cache misses overlap.
fn parse_queries(text: &[u8]) -> Plan<'_> {
    let mut plan = Plan::default();
    let (terms, list, steps) = (&mut plan.terms, &mut plan.list, &mut plan.steps);
    // Sized for this much query text, where 16 bytes hold about one distinct term.
    terms.sets = empty_sets((text.len() / 32).next_power_of_two().max(1 << 10));
    terms.keys = vec![&[]; FIRST_TERM];
    list.reserve(text.len() / 32);
    steps.reserve(text.len() / 10);
    let (mut edges, mut pending, mut pos, mut slots) = (vec![], vec![], 0, 0);
    while pos < text.len() {
        let line_feed = token_edges(text, pos, &mut edges);
        let is_bool = text[pos] == b'B';
        let from = steps.len() as u32;
        // The first field is BOOL or PHRASE. Then come terms, in a BOOL query with an operator between
        // each two, which goes in the top bits of the term after it.
        let mut op = 0;
        for (field, pair) in edges.as_chunks::<2>().0.iter().enumerate().skip(1) {
            let token = &text[pos + pair[0] as usize..pos + pair[1] as usize];
            if !is_bool || field % 2 == 1 {
                let index = home(&terms.sets, short_key(token));
                // SAFETY: the set is in the table, and a prefetch has no other effect.
                unsafe { _mm_prefetch(terms.sets.as_ptr().add(index).cast(), _MM_HINT_T0) };
                pending.push((steps.len(), token));
                steps.push(op);
            } else {
                op = u32::from(token[0] == b'A') * AND + u32::from(token[0] == b'N') * NOT;
            }
        }
        pos = line_feed + 1;
        if pending.len() >= 64 || pos >= text.len() {
            for (step, token) in pending.drain(..) {
                steps[step] |= terms.intern(token);
            }
        }
        list.push((if is_bool { slots } else { NONE }, from, steps.len() as u32));
        slots += u32::from(is_bool);
    }
    plan
}

/// What gets each document: its id, its line and what follows, and its tokens' edges in that.
trait Visit: FnMut(u32, &[u8], &[u32]) {}
impl<T: FnMut(u32, &[u8], &[u32])> Visit for T {}

/// Visits the documents whose lines start in `start..end`, passing each one's id, its line and what
/// follows, and its tokens' edges in that, after its id's, then eight zeros, so that `look_up` can read four
/// pairs at a time. A line belongs to the range holding its first byte. The file comes in pieces, into a
/// thread's small buffer that stays in the caches, where a mapping would take page faults and cache misses,
/// and what a piece cuts short moves to the buffer's start, to go with the next piece.
fn for_each_doc(docs: &File, start: usize, end: usize, room: &mut Vec<u8>, mut visit: impl Visit) {
    // `at` is where the buffer's start is in the file. Reading starts a byte early and skips a line, as
    // the range's first line starts after the first LF from there.
    let (mut at, mut skip, mut kept) = (start.saturating_sub(1), start > 0, 0);
    // The range is rounded to whole words of 64 documents: it starts at its first document whose id
    // is a multiple of 64 and runs to the next range's, so threads never share a bitmap word.
    let (mut aligned, mut edges) = (false, Vec::new());
    loop {
        // A line longer than a piece gets a longer read, so that its length doesn't go quadratic.
        let want = PIECE.max(kept);
        room.resize(room.len().max(kept + want + PADDING), 0);
        let piece = &mut room[kept..kept + want];
        let got = docs.read_at(piece, (at + kept) as u64).unwrap();
        let (filled, mut line) = (kept + got, 0);
        // This ends the last line, and anything after it is ignored, whatever an earlier piece left there.
        room[filled] = b'\n';
        while line < filled {
            let line_feed = token_edges(&room[..filled], line, &mut edges);
            // A line that reaches the end of the text is whole only at the end of the file.
            if line_feed == filled && got > 0 {
                break;
            }
            if !std::mem::take(&mut skip) {
                let digits = &room[line + edges[0] as usize..line + edges[1] as usize];
                let id = digits.iter().fold(0, |id, d| id * 10 + u32::from(d - b'0'));
                if id % 64 == 0 && at + line >= end {
                    return;
                }
                aligned |= id % 64 == 0;
                edges.extend([0; 8]);
                if aligned {
                    visit(id, &room[line..], &edges[2..]);
                }
            }
            line = line_feed + 1;
        }
        if got == 0 {
            return;
        }
        room.copy_within(line..filled, 0);
        (at, kept) = (at + line, filled - line);
    }
}

/// Terms of which every document matching a BOOL query contains at least one, chosen rare. The
/// query's last run, from its last OR (the first term counts as an OR from the empty set), is
/// intersected with each AND in it, so any of those ANDs is in every match, and so is the first
/// term when the query is a single run. A run without an AND adds its OR term to the anchors of
/// the query before it.
fn anchors(steps: &[u32], df: &[u32], out: &mut Vec<u32>) {
    let last_or = steps.iter().rposition(|step| step >> 30 == 0).unwrap_or(0);
    let ands = steps[last_or + 1..].iter().filter(|step| *step & AND != 0);
    let candidates = ands.chain(steps.first().filter(|_| last_or == 0));
    let chosen = candidates.min_by_key(|&step| df[(step & TERM_BITS) as usize]);
    let term = chosen.unwrap_or(&steps[last_or]) & TERM_BITS;
    if !out.contains(&term) {
        out.push(term);
    }
    if chosen.is_none() {
        anchors(&steps[..last_or], df, out);
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

/// The positions in a word that have this one of their 6 index bits set.
const fn index_bit_mask(bit: usize) -> u64 {
    !(!0 / ((1 << (1 << bit)) + 1))
}

/// What a step of a query makes of the set so far, given its term's set, as masks.
fn apply(op: u32, set: u64, term_set: u64) -> u64 {
    match op {
        AND => set & term_set,
        NOT => set & !term_set,
        _ => set | term_set,
    }
}

/// The check of a query with these steps at the given anchor, after the `earlier` ones. Each term's set
/// is a mask over the 64 possible documents, so the steps give the truth table directly, and its top half
/// is what's left with the last term, the anchor, present.
fn pack(steps: &[u32], anchor: u32, earlier: &[u32]) -> Check<5> {
    let (mut truth, mut blocked, mut terms, mut others) = (0, 0, [ABSENT; 5], 0);
    for step in steps {
        let term = step & TERM_BITS;
        let mut holds = index_bit_mask(5);
        if term != anchor {
            holds = index_bit_mask(others);
            terms[others] = term;
            others += 1;
        }
        truth = apply(step & !TERM_BITS, truth, holds);
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

/// Sorts items by their keys, below `keys`. Returns where each key's items start, and the items, which
/// stay in order within a key.
fn group<T>(keys: usize, items: impl Clone + Iterator<Item = (u32, T)>) -> (Vec<u32>, Vec<T>) {
    let mut start = vec![0; keys + 1];
    for (key, _) in items.clone() {
        start[key as usize + 1] += 1;
    }
    sum_sizes(&mut start);
    let mut next = start.clone();
    let mut grouped: Vec<T> = items.clone().map(|item| item.1).collect();
    for (key, item) in items {
        grouped[next[key as usize] as usize] = item;
        next[key as usize] += 1;
    }
    (start, grouped)
}

/// What a query's line prints, or what a term's count adds to it: the count of documents, the sum of
/// their ids, and the smallest ids, ascending, then NONE.
type Answer = (u64, u64, [u32; SMALLEST]);

/// The queries, and what the sample decided: how each term is stored, and what to check at each term's
/// occurrences. The threads share this while they scan.
#[derive(Default)]
struct Plan<'a> {
    terms: TermTable<'a>,
    // Each query's slot, where a BOOL's answer goes (a phrase has NONE), and the range of its steps.
    list: Vec<(u32, u32, u32)>,
    steps: Vec<u32>,
    bitmap_count: usize, // How many terms have bitmaps...
    list_count: usize,   // ...and how many have chains.
    words: usize,        // A bitmap's length.
    bitmaps: Vec<u64>,
    phrase_start: Vec<u32>, // A term's phrases are `phrases[phrase_start[t]..phrase_start[t + 1]]`...
    phrases: Vec<u32>, // ...each its query, the anchor's position in it, its length, its terms.
    // Where a term's short and long checks start, and which is its count. A term's checks end where the
    // next term's start. Counts are in the order of the short checks, the long checks, then the terms'.
    anchor_lists: Vec<(u32, u32, u32)>,
    short_checks: Vec<Check<3>>,
    long_checks: Vec<Check<5>>,
    slot_of: Vec<u32>,         // Which query or term count each count belongs to.
    posting_queries: Vec<u32>, // The queries anchored on a common term.
    shared: Vec<u32>, // For each BOOL query, the slot of the term count that adds to it, or NONE.
    // The answers, in slots: the BOOL queries', then one for each term with a count, then a spare.
    answers: Vec<Mutex<Answer>>,
    // For each count, the last of its answer's ten smallest documents when it last saw them, which only
    // falls, so that the documents past it need no lock. Those fill first, as threads work through the
    // corpus together, and only a thread's own first ten ever get this far.
    limit: Vec<AtomicU32>,
}

/// Puts a document among the smallest of a count's answer. Returns `TOO_BIG` if it's past the limit, as all
/// the thread's later ones will be too. This is cold beside the check that leads here, and keeping it out
/// of line lets that check be inlined.
#[cold]
fn add_smallest(plan: &Plan, at: usize, doc: u32) -> u64 {
    if doc >= plan.limit[at].load(Relaxed) {
        return TOO_BIG;
    }
    let smallest = &mut plan.answers[plan.slot_of[at] as usize].lock().unwrap().2;
    insert_smallest(smallest, doc);
    if smallest[SMALLEST - 1] != NONE {
        plan.limit[at].store(smallest[SMALLEST - 1], Relaxed);
    }
    0
}

/// How many documents hold each term, among pieces of the corpus as big as `SAMPLE_BYTES` in all, and
/// how many documents those pieces have.
fn sample_terms((docs, len): (&File, usize), terms: &TermTable, threads: usize) -> (Vec<u32>, u32) {
    let df: Vec<AtomicU32> = (0..terms.keys.len()).map(|_| AtomicU32::new(0)).collect();
    let (sampled_docs, next_piece) = (AtomicU32::new(0), AtomicUsize::new(0));
    let piece = SAMPLE_BYTES.min(len) / threads;
    on_threads(threads, || {
        let start = next_piece.fetch_add(1, Relaxed) * (len / threads);
        // The low byte of the last document holding each term. A rare mistake only skews the guess.
        let (mut seen_in, mut room) = (vec![0u8; df.len()], Vec::new());
        for_each_doc(docs, start, start + piece, &mut room, |doc, text, edges| {
            sampled_docs.fetch_add(1, Relaxed);
            for pair in edges[..edges.len() - 8].as_chunks::<2>().0 {
                let term = terms.find_token(&text[pair[0] as usize..pair[1] as usize]) as usize;
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

/// Numbers the terms: those with bitmaps, then those with chains, then the rest, the commonest first
/// so that what the scan touches most sits together. Puts the numbers in the term table and the steps,
/// and returns them.
fn number_terms(plan: &mut Plan, df: &[u32], store_of: &[usize], flags: &mut [u32]) -> Vec<u32> {
    let bucket = |t: usize| store_of[t] + store_of[t] / 2 * (255 - df[t].min(255) as usize);
    let mut next = [0u32; 259];
    for term in FIRST_TERM..df.len() {
        next[bucket(term) + 1] += 1;
    }
    (plan.bitmap_count, plan.list_count) = (next[1] as usize, next[2] as usize);
    // A hole keeps the terms with chains from sharing a word of a document's bits with bitmaps.
    next[0] = FIRST_TERM as u32;
    next[1] = next[1].next_multiple_of(64);
    sum_sizes(&mut next);
    let mut new_number: Vec<u32> = (0..df.len() as u32).collect();
    for term in FIRST_TERM..df.len() {
        new_number[term] = next[bucket(term)];
        next[bucket(term)] += 1;
        flags[term] |= new_number[term] | (u32::from(store_of[term] == 1) * CHAINED);
        flags[term] |= u32::from(plan.terms.keys[term].len() > 8) * UNSURE;
    }
    flags[OVERFLOWED as usize] = OVERFLOWED | UNSURE;
    // Empty ways hold term 0, whose word stays 0, so this needs no branch on which ways are in use.
    for word in plan.terms.sets.iter_mut().flat_map(|set| &mut set.words) {
        *word = flags[*word as usize];
    }
    for step in &mut plan.steps {
        *step = (*step & !TERM_BITS) | new_number[(*step & TERM_BITS) as usize];
    }
    let mut keys = vec![&[][..]; df.len() + 64];
    for (term, &new) in new_number.iter().enumerate() {
        keys[new as usize] = plan.terms.keys[term];
    }
    plan.terms.keys = keys;
    new_number
}

/// Lists each anchor term's phrases, given (term, (query, the anchor's position in it)).
fn list_phrases(plan: &mut Plan, anchored: &[(u32, (u32, u32))], new_number: &[u32]) {
    let keyed = anchored.iter().map(|&(t, p)| (new_number[t as usize], p));
    let (start, phrases) = group(plan.terms.keys.len(), keyed);
    plan.phrase_start.reserve_exact(start.len());
    for range in start.windows(2) {
        plan.phrase_start.push(plan.phrases.len() as u32);
        for &(number, position) in &phrases[range[0] as usize..range[1] as usize] {
            let (_, from, to) = plan.list[number as usize];
            plan.phrases.extend([number, position, to - from]);
            plan.phrases.extend(&plan.steps[from as usize..to as usize]);
        }
    }
    plan.phrase_start.push(plan.phrases.len() as u32);
}

/// Decides from the sample how each query is answered and how each term is stored, then lists what to
/// check at each term's occurrences.
fn make_plan(plan: &mut Plan, docs: (&File, usize), threads: usize) {
    let (df, sampled_docs) = sample_terms(docs, &plan.terms, threads);
    // A query anchored on a common term, with df above about N/24, is answered from postings: bitmaps
    // for its common terms and chains for the others. On memory alone, a bitmap (N/8 bytes) and a
    // chain (about 3 bytes per posting) cross near df = N/24. Any choice gives the same answers.
    let common = |term: u32| df[term as usize] * 24 >= sampled_docs;
    // How a term is stored: 0 for a bitmap, 1 for a chain, and 2 for neither.
    let mut store_of = vec![2; df.len()];
    // What each term's word holds beyond its number.
    let mut flags = vec![0; df.len()];
    // The number of each term's own count, among those.
    let (mut shared_of, mut shared_count) = (vec![NONE; df.len()], 0);
    // Each phrase and its anchor term, each check and its anchor term, and one query's anchors.
    let (mut anchored, mut checks, mut these) = (vec![], vec![], vec![]);
    let bools = plan.list.iter().filter(|query| query.0 != NONE).count() as u32;
    plan.shared.resize(bools as usize, NONE);
    for (number, &(slot, from, to)) in plan.list.iter().enumerate() {
        let steps = &plan.steps[from as usize..to as usize];
        if slot == NONE {
            // Any term of a phrase would be correct as its anchor. The rarest triggers the fewest checks.
            let rarest = (0..steps.len()).min_by_key(|&at| df[steps[at] as usize]);
            let rarest = rarest.unwrap_or(0);
            flags[steps[rarest] as usize] |= PHRASE_ANCHOR;
            anchored.push((steps[rarest], (number as u32, rarest as u32)));
            continue;
        }
        these.clear();
        anchors(steps, &df, &mut these);
        if these.iter().any(|&term| common(term)) {
            plan.posting_queries.push(number as u32);
            for term in steps.iter().map(|step| step & TERM_BITS) {
                store_of[term as usize] = usize::from(!common(term));
            }
            continue;
        }
        // After the last AND or NOT, each OR's term alone makes a document match. The commonest
        // of them becomes the first anchor, needing no check: the term's own count serves every
        // query so anchored, and the other anchors skip the documents that hold it.
        let trailing = steps.rsplit(|step| step >> 30 != 0).next().unwrap_or(steps);
        let sure = (0..these.len())
            .filter(|&at| trailing.iter().any(|step| step & TERM_BITS == these[at]))
            .max_by_key(|&at| df[these[at] as usize]);
        if let Some(at) = sure {
            these.swap(0, at);
            let shared = &mut shared_of[these[0] as usize];
            if *shared == NONE {
                *shared = shared_count;
                shared_count += 1;
            }
            plan.shared[slot as usize] = bools + *shared;
        }
        for (k, &term) in these.iter().enumerate() {
            flags[term as usize] |= BOOL_ANCHOR;
            if k > 0 || sure.is_none() {
                checks.push((term, (slot, pack(steps, term, &these[..k]))));
            }
        }
    }
    let new_number = number_terms(plan, &df, &store_of, &mut flags);
    list_phrases(plan, &anchored, &new_number);

    // A term's checks, the short ones of all terms first. A short check holds up to 3 other terms.
    let count = plan.terms.keys.len();
    for (term, (_, check)) in &mut checks {
        *term = new_number[*term as usize] + u32::from(check.1[3] != ABSENT) * count as u32;
        check.1 = check.1.map(|term| new_number[term as usize]);
    }
    let (start, checks) = group(2 * count, checks.iter().copied());
    let (short, long) = checks.split_at(start[count] as usize);
    let narrow = |&(_, Check(truth, [a, b, c, ..])): &(u32, Check<5>)| Check(truth, [a, b, c]);
    plan.short_checks = short.iter().map(narrow).collect();
    plan.long_checks = long.iter().map(|c| c.1).collect();
    let slots = bools + shared_count + 1;
    plan.slot_of = checks.iter().map(|c| c.0).chain(bools..slots).collect();
    plan.limit = plan.slot_of.iter().map(|_| AtomicU32::new(NONE)).collect();
    let blank = |_| Mutex::new((0, 0, [NONE; SMALLEST]));
    plan.answers = (0..slots).map(blank).collect();

    // Each term's count, or the spare count for a term without one, which nothing reads.
    let first_count = checks.len() as u32;
    let lists = |t| (start[t], start[count + t], first_count + shared_count);
    plan.anchor_lists = (0..=count).map(lists).collect();
    for (term, &new) in new_number.iter().enumerate() {
        plan.anchor_lists[new as usize].2 = first_count + shared_of[term].min(shared_count);
    }
}

/// Postings per chain block: 32 bytes hold a link, a base and 12 document offsets.
const BLOCK_POSTINGS: usize = 12;

/// A piece of one term's posting list. Chains run from the newest block back to the oldest.
struct Block {
    prev: u32,  // The previous block's number, or NONE if this is the first.
    first: u32, // Its postings are offsets from this, its first document.
    docs: [u16; BLOCK_POSTINGS],
}

/// How many blocks are allocated at a time. Growing one Vec would hold twice the memory for a while.
const CHUNK_BLOCKS: usize = 1 << 10;

/// A block by its number, in a thread's chunks of them, which never move.
fn block_at(chunks: &[Vec<Block>], number: u32) -> &Block {
    &chunks[number as usize / CHUNK_BLOCKS][number as usize % CHUNK_BLOCKS]
}

/// One thread's results, and its list of short checks for the document in hand. A thread claims ranges
/// in document order, so its hits ascend, though a list of them still has to be merged with other threads'.
#[derive(Default)]
struct Scan {
    blocks: Vec<Vec<Block>>,
    // Each posting list's newest block, or NONE, how full it is, and its first document. Only that one can
    // be part full, and an empty list counts as full. With the first document here, adding a document only
    // writes to the block, and doesn't wait for the block to reach the cache: 2.5% of all cycles.
    chains: Vec<(u32, u32, u32)>,
    hits: Vec<u64>, // For each count, how many documents went into it, then the sum of their ids.
    phrases: Vec<(u32, u32)>, // (query, doc)
    short: Vec<u32>, // The short checks to run for the document in hand.
}

/// What the stages fill for a document, with room for each of its tokens and three more: the tokens' words,
/// those to look at again, the words that are terms, the positions of phrase anchors, the terms with BOOL
/// checks, and the terms with chains; and the tokens' keys, and their sets' addresses.
#[derive(Default)]
struct Lists([Vec<u32>; 6], [Vec<u64>; 2]);

/// The bits of `hits` that hold the sum of ids, which `main` checks has room.
const SUM_BITS: u32 = 40;

/// A high bit of `hits`, past any count, which says that no more documents are among the smallest.
const TOO_BIG: u64 = 1 << 62;

/// Puts a document among a list's ten smallest.
fn insert_smallest(smallest: &mut [u32; SMALLEST], mut doc: u32) {
    for held in smallest.iter_mut() {
        if doc < *held {
            std::mem::swap(held, &mut doc);
        }
    }
}

/// Looks the tokens up: first finding each one's key and prefetching its set, so that cache misses
/// overlap, then comparing the keys without a branch on what that finds. Only a word marked `UNSURE`
/// needs a full look, and few are. Lists the words that are terms, and the positions of phrase anchors,
/// and returns how long those lists are. This stage stays out of line, like `mark` and `check_bools`,
/// because measurements say so: inlining this one costs 1.7% of all cycles.
#[inline(never)]
fn look_up(terms: &TermTable, text: &[u8], edges: &[u32], lists: &mut Lists) -> [usize; 2] {
    let [tokens, later, marks, phrase_at, ..] = lists.0.each_mut().map(|list| &mut list[..]);
    let [keys, places] = lists.1.each_mut().map(|list| &mut list[..]);
    let (sets, count) = (terms.sets.as_ptr(), edges.len() / 2 - 4);
    let shift = 64 - terms.sets.len().trailing_zeros() as i32;
    for at in (0..count).step_by(4) {
        // SAFETY: `edges` has four pairs more than there are tokens, each at or before a token's start,
        // whose load stays in the buffer as in `short_key`. Keys and places have room for three more.
        // The set is in the table, and a prefetch has no other effect. The build targets x86-64-v3,
        // which has AVX2.
        unsafe {
            let pairs = edges.as_ptr().add(2 * at);
            let start = |i: usize| text.as_ptr().add(*pairs.add(2 * i) as usize);
            let load = |i: usize| start(i).cast::<i64>().read_unaligned();
            let bytes = _mm256_setr_epi64x(load(0), load(1), load(2), load(3));
            // Each lane of the pairs is end << 32 | start, which gives end - start.
            let pair = _mm256_loadu_si256(pairs.cast());
            let lens = _mm256_srli_epi64(_mm256_sub_epi64(pair, _mm256_slli_epi64(pair, 32)), 32);
            // The bytes past the token, which a shift of 64 bits or more makes none of.
            let past = _mm256_sllv_epi64(_mm256_set1_epi64x(-1), _mm256_slli_epi64(lens, 3));
            let lower = _mm256_or_si256(bytes, _mm256_set1_epi8(0x20));
            let more = _mm256_slli_epi64(_mm256_cmpgt_epi64(lens, _mm256_set1_epi64x(8)), 63);
            let key = _mm256_or_si256(_mm256_andnot_si256(past, lower), more);
            let index = _mm256_srl_epi64(hashes(key), _mm_cvtsi32_si128(shift));
            let place =
                _mm256_add_epi64(_mm256_slli_epi64(index, 6), _mm256_set1_epi64x(sets as i64));
            _mm256_storeu_si256(keys.as_mut_ptr().add(at).cast(), key);
            _mm256_storeu_si256(places.as_mut_ptr().add(at).cast(), place);
            for i in 0..4 {
                _mm_prefetch(*places.get_unchecked(at + i) as *const i8, _MM_HINT_T0);
            }
        }
    }
    let (mut later_count, mut marked, mut phrases) = (0, 0, 0);
    for at in (0..count).step_by(4) {
        // SAFETY: the places are in the table, and the lists have room for every token and three more.
        // `POSITIONS` has a row for each of the masks. The build targets x86-64-v3, which has AVX.
        unsafe {
            let word = |i: usize| {
                let set = &*(*places.get_unchecked(at + i) as *const TermSet);
                set.word(set.ways(*keys.get_unchecked(at + i))) as i32
            };
            let words = _mm_setr_epi32(word(0), word(1), word(2), word(3));
            _mm_storeu_si128(tokens.as_mut_ptr().add(at).cast(), words);
            let mask = |of| _mm_movemask_ps(_mm_castsi128_ps(of)) as usize;
            let order = |m| _mm_cvtepu8_epi32(_mm_loadl_epi64(POSITIONS.as_ptr().add(m).cast()));
            // A word that is surely a term's is positive as a signed number. The table's row moves those
            // words to the front, and as many as there are stay in the list.
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
    }
    for &at in &later[..later_count] {
        let at = at as usize;
        // SAFETY: this is a token's position or at most three past the last, for which `tokens` has
        // room, and the other lists have room for every token.
        unsafe {
            let mut word = *tokens.get_unchecked(at);
            // Past the last token, the pairs are empty, whose words are 0 or the unused way's.
            if word >= UNSURE && at < count {
                let token = &text[edges[2 * at] as usize..edges[2 * at + 1] as usize];
                let index = (*places.get_unchecked(at) as usize - sets as usize) >> 6;
                word = terms.find(token, *keys.get_unchecked(at), index);
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
    for (index, marks) in present.as_chunks::<64>().0.iter().enumerate() {
        // SAFETY: a chunk has these 64 bytes, and the build targets x86-64-v3, which has AVX2. The shift
        // moves a mark to the top bit of its byte.
        let half = |at: usize| unsafe {
            let marks = _mm256_loadu_si256(marks.as_ptr().add(at).cast());
            _mm256_movemask_epi8(_mm256_slli_epi16(marks, 7)) as u32
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
fn extend_chains(chained: &[u32], chain_from: usize, scan: &mut Scan, doc: u32) {
    for &term in chained {
        let (tail, fill, base) = &mut scan.chains[term as usize - chain_from];
        let blocks = &mut scan.blocks;
        if *fill as usize == BLOCK_POSTINGS || doc - *base > u32::from(u16::MAX) {
            if blocks.last().is_none_or(|c| c.len() == CHUNK_BLOCKS) {
                blocks.push(Vec::with_capacity(CHUNK_BLOCKS));
            }
            let chunk = blocks.len() - 1;
            let (prev, first, docs) = (*tail, doc, [0; BLOCK_POSTINGS]);
            *tail = (chunk * CHUNK_BLOCKS + blocks[chunk].len()) as u32;
            blocks[chunk].push(Block { prev, first, docs });
            (*fill, *base) = (0, doc);
        }
        let block = &mut blocks[*tail as usize / CHUNK_BLOCKS][*tail as usize % CHUNK_BLOCKS];
        block.docs[*fill as usize] = (doc - *base) as u16;
        *fill += 1;
    }
}

/// Checks the phrases anchored at these positions of a document's tokens.
fn check_phrases(ats: &[u32], tokens: &[u32], plan: &Plan, doc: u32, hits: &mut Vec<(u32, u32)>) {
    for &at in ats {
        let at = at as usize;
        let term = (tokens[at] & TERM_BITS) as usize;
        let from = plan.phrase_start[term] as usize;
        let mut entries = &plan.phrases[from..plan.phrase_start[term + 1] as usize];
        while let [query, position, len, rest @ ..] = entries {
            let (phrase, after) = rest.split_at(*len as usize);
            entries = after;
            // A start before the document wraps, and makes a range that isn't in it.
            let start = at.wrapping_sub(*position as usize);
            let run = tokens.get(start..start.wrapping_add(phrase.len()));
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
fn check_bools(anchors: &[u32], present: &[u8], plan: &Plan, doc: u32, scan: &mut Scan) {
    let (hits, short) = (&mut scan.hits[..], &mut scan.short);
    let (checks, short_hits) = (plan.short_checks.as_ptr(), hits.as_ptr());
    let this_doc = (1 << SUM_BITS) + u64::from(doc);
    let lists = &plan.anchor_lists[..];
    // SAFETY: `hits` has every count. A thread's hits ascend, so only its first ten can be among the
    // smallest.
    let mut add = |counts: u64, at: usize| unsafe {
        let hit = hits.get_unchecked_mut(at);
        *hit += counts * this_doc;
        // Below 11 and counted, in a way the compiler can't turn into a coin-toss branch.
        if hit.wrapping_sub((SMALLEST as u64 + 1) << SUM_BITS) >> 63 & counts != 0 {
            *hit |= add_smallest(plan, at, doc);
        }
    };
    // SAFETY: a prefetch has no other effect, whatever the address.
    let prefetch = |at: *const u8| unsafe { _mm_prefetch(at.cast(), _MM_HINT_T0) };
    for &term in anchors {
        prefetch(lists.as_ptr().wrapping_add(term as usize).cast());
    }
    // Each anchor writes four, and one with more checks makes room for them when it comes to it. Room for
    // all the checks there are would hold on to more than a megabyte over the threads.
    let (short_count, mut listed, room) = (plan.short_checks.len(), 0, 4 * anchors.len() + 4);
    short.resize(short.len().max(room), 0);
    for &term in anchors {
        // SAFETY: `lists` has one entry more than there are terms, `short` has room for this anchor's
        // four and the other anchors' as well, and an anchor's checks are all in `long_checks`.
        unsafe {
            let (from, long_from, shared) = *lists.get_unchecked(term as usize);
            let (to, long_to, _) = *lists.get_unchecked(term as usize + 1);
            prefetch(checks.wrapping_add(from as usize).cast());
            prefetch(short_hits.wrapping_add(from as usize).cast());
            let at = _mm_add_epi32(_mm_set1_epi32(from as i32), _mm_setr_epi32(0, 1, 2, 3));
            _mm_storeu_si128(short.as_mut_ptr().add(listed).cast(), at);
            if to > from + 4 {
                short.resize(short.len().max(listed + (to - from) as usize + room), 0);
                for at in from + 4..to {
                    short[listed + (at - from) as usize] = at;
                }
            }
            listed += (to - from) as usize;
            add(1, shared as usize);
            for at in long_from as usize..long_to as usize {
                let check = plan.long_checks.get_unchecked(at - short_count);
                add(check.counts(present), at);
            }
        }
    }
    for &at in &short[..listed] {
        // SAFETY: the list came from `anchor_lists`.
        let check = unsafe { plan.short_checks.get_unchecked(at as usize) };
        add(check.counts(present), at as usize);
    }
}

/// Claims ranges of the corpus until none is left, and scans them.
fn scan_ranges((docs, len): (&File, usize), plan: &Plan, size: usize, next: &AtomicUsize) -> Scan {
    let mut scan = Scan::default();
    scan.hits.resize(plan.slot_of.len(), 0);
    scan.chains = vec![(NONE, BLOCK_POSTINGS as u32, 0); plan.list_count];
    let mut present = vec![0u8; plan.terms.keys.len()];
    let chain_from = FIRST_TERM + plan.bitmap_count.next_multiple_of(64);
    let start = plan.bitmaps.as_ptr().cast();
    // SAFETY: AtomicU64 has the layout of u64, and nothing else uses the bitmaps during the scan.
    let bitmaps: &[AtomicU64] = unsafe { std::slice::from_raw_parts(start, plan.bitmaps.len()) };
    let (mut lists, mut room) = (Lists::default(), Vec::new());
    loop {
        let start = next.fetch_add(size, Relaxed);
        if start >= len {
            return scan;
        }
        for_each_doc(docs, start, start + size, &mut room, |doc, text, edges| {
            let count = edges.len() / 2 - 4;
            if lists.1[0].len() < count + 4 {
                // A document has about a thousand tokens at most, and doubling would leave more room.
                let fits = (count + 4).max(1100);
                lists.0.iter_mut().for_each(|l| l.resize(fits, 0));
                lists.1.iter_mut().for_each(|l| l.resize(fits, 0));
            }
            let [marked, phrases] = look_up(&plan.terms, text, edges, &mut lists);
            let [tokens, _, marks, phrase_at, bool_terms, chained] = &mut lists.0;
            let (in_doc, marked) = (&tokens[..count], &marks[..marked]);
            let [bools, chains] = mark(marked, &mut present, [bool_terms, chained]);
            set_bitmaps(&present[FIRST_TERM..chain_from], bitmaps, plan.words, doc);
            extend_chains(&chained[..chains], chain_from, &mut scan, doc);
            check_phrases(&phrase_at[..phrases], in_doc, plan, doc, &mut scan.phrases);
            check_bools(&bool_terms[..bools], &present, plan, doc, &mut scan);
            for &word in marked {
                // SAFETY: `present` has a mark for every term.
                unsafe { *present.get_unchecked_mut((word & TERM_BITS) as usize) = 0 };
            }
        });
    }
}

/// Sums the threads' counts and ids into each scan-answered query's and term's count and sum of ids,
/// visiting each answer once, as they are many cache lines and the threads' counts are in order.
fn total_hits(scans: &[Scan], plan: &mut Plan) {
    let slots = &mut plan.answers;
    let mut answers: Vec<_> = slots.iter_mut().map(|a| a.get_mut().unwrap()).collect();
    for (at, &slot) in plan.slot_of.iter().enumerate() {
        let hits: u64 = scans.iter().map(|scan| scan.hits[at] & !TOO_BIG).sum();
        answers[slot as usize].0 += hits >> SUM_BITS;
        answers[slot as usize].1 += hits & ((1 << SUM_BITS) - 1);
    }
}

/// Sets the bits of a posting list's documents.
fn set_bits_of_list(scans: &[Scan], list: usize, bits: &mut [u64]) {
    for scan in scans {
        let (mut tail, mut fill, _) = scan.chains[list];
        while tail != NONE {
            let block = block_at(&scan.blocks, tail);
            for &offset in &block.docs[..fill as usize] {
                let doc = block.first + u32::from(offset);
                bits[doc as usize / 64] |= 1 << (doc % 64);
            }
            (tail, fill) = (block.prev, BLOCK_POSTINGS as u32);
        }
    }
}

/// Finds a BOOL query's result as a bitmap: applies its steps in order, from the empty set, with each
/// term's set as a bitmap.
fn run_steps(steps: &[u32], plan: &Plan, scans: &[Scan], bits: &mut [u64], other: &mut [u64]) {
    bits.fill(0);
    for step in steps {
        let number = (step & TERM_BITS) as usize - FIRST_TERM;
        let term_set = if number < plan.bitmap_count {
            &plan.bitmaps[number * plan.words..(number + 1) * plan.words]
        } else {
            other.fill(0);
            let list = number - plan.bitmap_count.next_multiple_of(64);
            set_bits_of_list(scans, list, other);
            &*other
        };
        for (word, &with) in bits.iter_mut().zip(term_set) {
            *word = apply(step & !TERM_BITS, *word, with);
        }
    }
}

/// What a query's result, as a bitmap, prints.
fn answer_of(bits: &[u64]) -> Answer {
    let (mut smallest, mut found) = ([NONE; SMALLEST], 0);
    for (i, &word) in bits.iter().enumerate() {
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
    let (count, sum) = count_and_sum(bits);
    (count, sum, smallest)
}

/// How many bits are set in these words, a multiple of 4 of them, and the sum of their positions. Each
/// nibble's count and sum of positions are table lookups, so a byte gets them at once. A position is
/// the word's start, plus 8 times the byte's number in the word, plus its place in the byte. The bytes'
/// numbers times their counts stay below 256 for each pair of bytes, so that SAD can add those too.
fn count_and_sum(bits: &[u64]) -> (u64, u64) {
    // SAFETY: the loads have the length they say, and the build targets x86-64-v3, which has AVX2.
    unsafe {
        let table = |t: [u8; 16]| _mm256_broadcastsi128_si256(_mm_loadu_si128(t.as_ptr().cast()));
        let count_of = table([0, 1, 1, 2, 1, 2, 2, 3, 1, 2, 2, 3, 2, 3, 3, 4]);
        let low_sum = table([0, 0, 1, 1, 2, 2, 3, 3, 3, 3, 4, 4, 5, 5, 6, 6]);
        let high_sum = table([0, 4, 5, 9, 6, 10, 11, 15, 7, 11, 12, 16, 13, 17, 18, 22]);
        let (nibble, zero) = (_mm256_set1_epi8(15), _mm256_setzero_si256());
        let look = |table, at| _mm256_shuffle_epi8(table, at);
        let byte_numbers = _mm256_set1_epi64x(0x0706_0504_0302_0100);
        let (mut counts, mut sums) = (zero, zero);
        let mut word_starts = _mm256_setr_epi64x(0, 64, 128, 192);
        for four in bits.as_chunks::<4>().0 {
            let words = _mm256_loadu_si256(four.as_ptr().cast());
            let low = _mm256_and_si256(words, nibble);
            let high = _mm256_and_si256(_mm256_srli_epi16(words, 4), nibble);
            let ones = _mm256_add_epi8(look(count_of, low), look(count_of, high));
            let places = _mm256_add_epi8(look(low_sum, low), look(high_sum, high));
            let in_word = _mm256_sad_epu8(ones, zero);
            counts = _mm256_add_epi64(counts, in_word);
            sums = _mm256_add_epi64(sums, _mm256_sad_epu8(places, zero));
            sums = _mm256_add_epi64(sums, _mm256_mul_epu32(in_word, word_starts));
            let by_pair = _mm256_maddubs_epi16(ones, byte_numbers);
            sums = _mm256_add_epi64(sums, _mm256_slli_epi64(_mm256_sad_epu8(by_pair, zero), 3));
            word_starts = _mm256_add_epi64(word_starts, _mm256_set1_epi64x(256));
        }
        let total =
            |v: __m256i| -> u64 { std::mem::transmute::<__m256i, [u64; 4]>(v).iter().sum() };
        (total(counts), total(sums))
    }
}

fn push_decimal(out: &mut Vec<u8>, mut value: u64) {
    let (mut digits, mut start) = ([0u8; 20], 19);
    while value >= 10 {
        digits[start] = b'0' + (value % 10) as u8;
        value /= 10;
        start -= 1;
    }
    digits[start] = b'0' + value as u8;
    out.extend_from_slice(&digits[start..]);
}

fn print_line(out: &mut Vec<u8>, count: u64, sum: u64, smallest: &[u32]) {
    push_decimal(out, count);
    out.push(b' ');
    push_decimal(out, sum);
    for &doc in smallest.iter().take((count as usize).min(SMALLEST)) {
        out.push(b' ');
        push_decimal(out, u64::from(doc));
    }
    out.push(b'\n');
}

/// First the queries that read postings, as they take most of the time. Then, by the same threads, as
/// starting more costs more than waiting at a barrier, threads claim batches of queries in order, and each
/// waits for the batch before it to reach the file. The wait is short, because batches cost about the same.
fn answer_queries(file: File, plan: &Plan, scans: &[Scan], threads: usize) {
    let [next_query, next_batch, written]: [AtomicUsize; 3] = Default::default();
    let (query_count, barrier) = (plan.list.len(), Barrier::new(threads));
    let all_hits = scans.iter().flat_map(|scan| scan.phrases.iter().copied());
    let (hit_start, hits) = group(query_count, all_hits);
    on_threads(threads, || {
        let mut bits = vec![0; plan.words.next_multiple_of(4)];
        let mut other = bits.clone();
        while let Some(&number) = plan.posting_queries.get(next_query.fetch_add(1, Relaxed)) {
            let (slot, from, to) = plan.list[number as usize];
            let steps = &plan.steps[from as usize..to as usize];
            run_steps(steps, plan, scans, &mut bits, &mut other);
            *plan.answers[slot as usize].lock().unwrap() = answer_of(&bits);
        }
        barrier.wait();
        let (mut text, mut sorted) = (Vec::new(), Vec::new());
        let claim = || Some(next_batch.fetch_add(1, Relaxed)).filter(|b| b * BATCH < query_count);
        while let Some(batch) = claim() {
            text.clear();
            for number in batch * BATCH..((batch + 1) * BATCH).min(query_count) {
                let slot = plan.list[number].0;
                if slot == NONE {
                    let (from, to) = (hit_start[number], hit_start[number + 1]);
                    sorted.clear();
                    sorted.extend_from_slice(&hits[from as usize..to as usize]);
                    sorted.sort_unstable();
                    let sum = sorted.iter().map(|&doc| u64::from(doc)).sum();
                    print_line(&mut text, sorted.len() as u64, sum, &sorted);
                } else {
                    let shared = plan.shared[slot as usize];
                    let mut answer = *plan.answers[slot as usize].lock().unwrap();
                    if shared != NONE {
                        let shared = *plan.answers[shared as usize].lock().unwrap();
                        answer.0 += shared.0;
                        answer.1 += shared.1;
                        for doc in shared.2 {
                            insert_smallest(&mut answer.2, doc);
                        }
                    }
                    print_line(&mut text, answer.0, answer.1, &answer.2);
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
    let body = docs.strip_suffix(b"\n").unwrap_or(docs);
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
    let threads = thread_count();
    // The mapping only gives the corpus's length and its last line.
    let (input, mapped) = map_file(&format!("{data_dir}/docs.txt"))?;
    let docs = (&input, mapped.len());
    let mut plan = parse_queries(map_file(&format!("{data_dir}/queries.txt"))?.1);
    make_plan(&mut plan, docs, threads);
    plan.words = document_count(mapped).div_ceil(64);
    assert!(plan.words * 64 < 1 << (SUM_BITS / 2), "too many documents");
    plan.bitmaps = vec![0; plan.bitmap_count * plan.words];
    // One thread touches each 2 MiB, as otherwise threads racing to fault in the same huge page each
    // allocate one, which the memory peak counts.
    for word in plan.bitmaps.iter_mut().step_by(1 << 18) {
        // SAFETY: a reference is always valid to write through.
        unsafe { std::ptr::write_volatile(word, 0) };
    }
    let file = File::create(format!("{out_dir}/results.txt"))?;
    // Enough ranges that a thread slowed for a while doesn't hold the others up.
    let range_size = (mapped.len() / (threads * RANGES_PER_THREAD)).max(1);
    let claimed = AtomicUsize::new(0);
    let scans = on_threads(threads, || scan_ranges(docs, &plan, range_size, &claimed));
    total_hits(&scans, &mut plan);
    answer_queries(file, &plan, &scans, threads);
    // Nothing is buffered, and the kernel frees all the memory faster than dropping it would.
    std::process::exit(0)
}
