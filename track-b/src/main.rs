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
//! Threads read the queries too, into one term table.

use std::arch::x86_64::*;
use std::fs::File;
use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering::*};
use std::sync::{Barrier, Mutex};
use std::thread;

/// Term 0 is every token that no query uses, and an empty slot of the term table holds it. No token is
/// ever marked with this term, so it also fills the unused slots of a BOOL check. Real terms start here:
/// those with bitmaps first, then those with chains, then the rest.
const FIRST_TERM: usize = 1;
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
/// How many queries a thread claims at a time.
const BATCH: usize = 256;
/// How many of a query's matching documents are printed.
const SMALLEST: usize = 10;

/// Readable zero bytes after each mapped file, so that block loads can run past its end.
const PADDING: usize = 4096;

// Declared by hand to keep the crate at zero dependencies; std already links glibc.
unsafe extern "C" {
    fn mmap(at: *mut u8, len: usize, p: i32, f: i32, fd: i32, o: i64) -> *mut u8;
    fn mallopt(param: i32, value: i32) -> i32;
    fn madvise(at: *const u8, len: usize, advice: i32) -> i32;
}

/// Maps a file read-only for the life of the process, followed by `PADDING` readable zero bytes. The
/// scorer's memory peak doesn't charge us for these pages, unlike reading the file onto the heap.
fn map_file(path: &str) -> io::Result<&'static [u8]> {
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
            for byte in found.to_le_bytes().map(usize::from) {
                let at = _mm256_cvtepu8_epi32(_mm_loadl_epi64(POSITIONS.as_ptr().add(byte).cast()));
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

/// Runs `work` on `threads` threads, waits for them all, and returns what each returned.
fn on_threads<R: Send>(threads: usize, work: impl Fn() -> R + Sync) -> Vec<R> {
    thread::scope(|scope| {
        let handles: Vec<_> = (1..threads).map(|_| scope.spawn(&work)).collect();
        let others = handles.into_iter().map(|h| h.join().unwrap());
        std::iter::once(work()).chain(others).collect()
    })
}

/// A token's keys. The short key is its first 8 bytes, lowercased and zero-padded, with the top bit set if
/// there are more, which no byte of a token has. Setting this bit in every byte lowercases ASCII letters,
/// and digits already have it set. The long key is the next 4 bytes, likewise. Its top bit, set if there
/// are more still, marks a token that needs a full compare, and those are rare. Past the 4 bytes, the
/// count wraps, which keeps the key a function of the token, as these need a full compare anyway.
fn keys_of(token: &[u8]) -> (u64, u32) {
    let (bytes, len) = (token.as_ptr(), token.len());
    // SAFETY: a token is inside a mapping that continues for `PADDING` bytes past the file. The loads
    // need no alignment, and the build targets x86-64-v3, which has BMI2, whose BZHI keeps the given
    // number of low bits, or all of them if that's more than the width.
    unsafe {
        let short = bytes.cast::<u64>().read_unaligned() | 0x2020_2020_2020_2020;
        let long = bytes.add(8).cast::<u32>().read_unaligned() | 0x2020_2020;
        let short = _bzhi_u64(short, 8 * len.min(9) as u32) | u64::from(len > 8) << 63;
        let long = _bzhi_u32(long, 8 * len.saturating_sub(8) as u32) | u32::from(len > 12) << 31;
        (short, long)
    }
}

/// The hash of four short keys at once, whose top bits say which of the term table's sets, a power of two
/// of them, a key starts at. One multiply mixes the key, as its high bits depend on every input bit. AVX2
/// only has 32-bit multiplies, so this is the low half of the product, from the halves of each key and of
/// the multiplier. A test build swaps it for a constant, so that every key collides and any logic that
/// trusts a hash without comparing the key shows up as a wrong answer.
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

/// Three terms in one cache line, whose keys are compared at once, so a lookup has no branch on which
/// of them it found, or whether it found one. The fourth way is never used, and its word is what a lookup
/// that finds none gets: 0 (no query uses this token), or `UNSURE` if the probe goes on.
#[repr(align(64))]
struct TermSet {
    shorts: [u64; 4], // Short keys, 0 in the ways not used, which fill in order.
    longs: [u32; 4],  // Long keys.
    words: [u32; 4],  // The term's number and flags.
}

impl TermSet {
    /// A bit for each way holding this short key.
    fn ways(&self, short_key: u64) -> u32 {
        // SAFETY: the build targets x86-64-v3, which has AVX2, and the struct is aligned.
        unsafe {
            let keys = _mm256_load_si256(self.shorts.as_ptr().cast());
            let same = _mm256_cmpeq_epi64(keys, _mm256_set1_epi64x(short_key as i64));
            _mm256_movemask_pd(_mm256_castsi256_pd(same)) as u32
        }
    }

    /// A bit for each way holding this long key.
    fn long_ways(&self, long_key: u32) -> u32 {
        // SAFETY: as in `ways`.
        unsafe {
            let keys = _mm_load_si128(self.longs.as_ptr().cast());
            let same = _mm_cmpeq_epi32(keys, _mm_set1_epi32(long_key as i32));
            _mm_movemask_ps(_mm_castsi128_ps(same)) as u32
        }
    }

    /// The word of the first of these ways, or else the unused way's, as the bits set above the three
    /// ways stop the count at it.
    fn word(&self, ways: u32) -> u32 {
        self.words[(ways | !7).trailing_zeros() as usize & 3]
    }
}

/// Query terms to dense ids, by open addressing with linear probing over sets. A hit always compares
/// the whole token: for a term longer than the keys' 12 bytes, the long key says where it is in the query
/// file, and its top bit is set.
#[derive(Default)]
struct TermTable<'a> {
    sets: Vec<TermSet>, // The length is a power of two.
    text: &'a [u8],     // Where the terms come from.
    count: usize,       // How many term numbers there are.
    plain_top: i32,     // Terms up to this have bitmaps.
}

impl TermTable<'_> {
    /// The word of the term of this token, in any letter case, or else 0, from a probe that starts at
    /// this set. A set that no term was placed past ends a probe.
    fn find<'s>(&'s self, token: &[u8], short: u64, mut set: &'s TermSet) -> u32 {
        loop {
            let ways = self.ways_of(set, token, short, keys_of(token).1);
            // One test, not two: that this finds a term or not is a coin toss.
            if ways | u32::from(set.words[3] == 0) != 0 {
                return set.word(ways);
            }
            // SAFETY: the set is one of these.
            let index = unsafe { std::ptr::from_ref(set).offset_from(self.sets.as_ptr()) } as usize;
            set = &self.sets[(index + 1) & (self.sets.len() - 1)];
        }
    }

    /// Which ways of this set hold this token, whose keys these are. Beyond the short key, only a long
    /// key's compare is left, unless the token is longer than the keys.
    #[inline(always)]
    fn ways_of(&self, set: &TermSet, token: &[u8], short: u64, long: u32) -> u32 {
        if long >> 31 == 0 {
            return set.ways(short) & set.long_ways(long);
        }
        self.ways_of_long_term(set, token, short)
    }

    /// Which ways of this set hold this token longer than the keys.
    #[cold]
    fn ways_of_long_term(&self, set: &TermSet, token: &[u8], short: u64) -> u32 {
        let (ways, end) = (set.ways(short), token.len());
        let is_token = |way: &u32| {
            let long = set.longs[*way as usize];
            let term = self.text.get((long & !(1 << 31)) as usize..).unwrap_or(&[]);
            ways >> way & long >> 31 != 0
                && token.eq_ignore_ascii_case(term.get(..end).unwrap_or(&[]))
                && !term.get(end).is_some_and(u8::is_ascii_alphanumeric)
        };
        (0..3).filter(is_token).map(|way| 1 << way).sum()
    }

    /// Adds this query term, if it's new, and returns its number, which is too big for a table with no room
    /// for it, which the caller then drops. Threads do this at once: each holds the lock of the set it's
    /// looking at, the fourth short key, which no key equals. So threads adding one term get the same
    /// number, as sets only fill, and the first set with room is where it goes.
    fn intern(&self, token: &[u8], next_term: &AtomicUsize) -> u32 {
        let (short, long) = keys_of(token);
        // SAFETY: the build targets x86-64-v3, which has AVX2.
        let hash = unsafe { _mm256_extract_epi64::<0>(hashes(_mm256_set1_epi64x(short as i64))) };
        let mut index = (hash as u64 >> (64 - self.sets.len().trailing_zeros())) as usize;
        loop {
            // SAFETY: the set is in the table, and its lock gives it to this thread alone.
            unsafe {
                let set = self.sets.as_ptr().add(index).cast_mut();
                let lock = AtomicU64::from_ptr((&raw mut (*set).shorts).cast::<u64>().add(3));
                while lock.swap(1, Acquire) != 0 {
                    std::hint::spin_loop();
                }
                let set = &mut *set;
                let ways = self.ways_of(set, token, short, long);
                let (mut term, room) = (set.word(ways) & TERM_BITS, set.shorts[2] == 0);
                if ways == 0 && room {
                    term = next_term.fetch_add(1, Relaxed) as u32;
                    if (term as usize) < FIRST_TERM + 2 * self.sets.len() {
                        let way = set.ways(0).trailing_zeros() as usize;
                        let at = (token.as_ptr() as usize - self.text.as_ptr() as usize) as u32;
                        let held = if long >> 31 != 0 { 1 << 31 | at } else { long };
                        let word = term | u32::from(token.len() > 8) * UNSURE;
                        (set.shorts[way], set.longs[way], set.words[way]) = (short, held, word);
                    }
                } else if ways == 0 {
                    // A lookup that finds nothing in a set this term has passed has to look further.
                    set.words[3] = UNSURE;
                }
                lock.store(0, Release);
                if ways != 0 || room {
                    return term;
                }
            }
            index = (index + 1) & (self.sets.len() - 1);
        }
    }
}

/// Reads the queries into a new plan: each thread the lines that start in its part of the text.
fn parse_queries(text: &[u8], threads: usize) -> Plan<'_> {
    let mut plan = Plan::default();
    plan.terms.text = text;
    // Sized for this much query text, where 16 bytes hold about one distinct term. Far more terms than
    // that and the queries are read again, into a larger table.
    let mut sets = (text.len() / 32).next_power_of_two().max(1 << 10);
    let mut parts = loop {
        // Empty sets, in place at once, as threads that fault in the same huge page each allocate one, which
        // the memory peak counts.
        let layout = std::alloc::Layout::array::<TermSet>(sets).unwrap();
        // SAFETY: the layout isn't empty, zeros are a valid TermSet, and the Vec frees with this layout.
        let zeros = unsafe { std::alloc::alloc_zeroed(layout) };
        plan.terms.sets = unsafe { Vec::from_raw_parts(zeros.cast(), sets, sets) };
        let (next_part, next_term) = (AtomicUsize::new(0), AtomicUsize::new(FIRST_TERM));
        let parts = on_threads(threads, || {
            let (part, size) = (next_part.fetch_add(1, Relaxed), text.len() / threads + 1);
            let (mut pos, end) = (part * size, ((part + 1) * size).min(text.len()));
            let (mut list, mut steps, mut edges) = (vec![], vec![], vec![]);
            while pos > 0 && pos < end && text[pos - 1] != b'\n' {
                pos += 1;
            }
            while pos < end {
                let line_feed = token_edges(text, pos, &mut edges);
                // The first field is BOOL or PHRASE. Then come terms, in a BOOL query with an operator
                // between each two, which goes in the top bits of the term after it.
                let (is_bool, from, mut op) = (text[pos] == b'B', steps.len() as u32, 0);
                for (field, pair) in edges.as_chunks::<2>().0.iter().enumerate().skip(1) {
                    let token = &text[pos + pair[0] as usize..pos + pair[1] as usize];
                    if !is_bool || field % 2 == 1 {
                        steps.push(op | plan.terms.intern(token, &next_term));
                    } else {
                        op = u32::from(token[0] == b'A') * AND + u32::from(token[0] == b'N') * NOT;
                    }
                }
                pos = line_feed + 1;
                list.push((u32::from(is_bool), from, steps.len() as u32));
            }
            (part, list, steps)
        });
        plan.terms.count = next_term.into_inner();
        if plan.terms.count <= FIRST_TERM + 2 * sets {
            break parts;
        }
        sets *= 4;
    };
    parts.sort_unstable_by_key(|part| part.0);
    for (_, list, steps) in parts {
        let base = plan.steps.len() as u32;
        plan.steps.extend(steps);
        plan.list
            .extend(list.iter().map(|q| (q.0, base + q.1, base + q.2)));
    }
    plan
}

/// Visits the documents whose lines start in `start..end`, passing each one's id, its text and what
/// follows, and its tokens' edges in that, then eight zeros, so that `look_up` can read four pairs at a
/// time. A line belongs to the range holding its first byte.
fn for_each_doc(docs: &[u8], start: usize, end: usize, mut visit: impl FnMut(u32, &[u8], &[u32])) {
    let mut line = start;
    while line > 0 && docs[line - 1] != b'\n' {
        line += 1;
    }
    // The range is rounded to whole words of 64 documents: it starts at its first document whose id
    // is a multiple of 64 and runs to the next range's, so threads never share a bitmap word.
    let (mut aligned, mut edges) = (false, Vec::new());
    while line < docs.len() {
        // The id is the line's first token.
        let next = token_edges(docs, line, &mut edges) + 1;
        let digits = docs[line..line + edges[1] as usize].iter();
        let id = digits.fold(0, |id, &digit| id * 10 + u32::from(digit - b'0'));
        if id % 64 == 0 && line >= end {
            return;
        }
        aligned |= id % 64 == 0;
        edges.extend([0; 8]);
        if aligned {
            visit(id, &docs[line..], &edges[2..]);
        }
        line = next;
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

/// 1 if a document holding the terms marked in `present` counts at this check, else 0.
fn counts<const TERMS: usize>(check: &Check<TERMS>, present: &[u8]) -> u64 {
    let mut holds = 0;
    for (slot, &term) in check.1.iter().enumerate() {
        // SAFETY: `present` has a mark for every term.
        holds |= u32::from(unsafe { *present.get_unchecked(term as usize) }) << slot;
    }
    u64::from(check.0 >> holds & 1)
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
    // The positions in a word that have this one of their 6 index bits set.
    let index_bit_mask = |bit: usize| !(!0u64 / ((1 << (1 << bit)) + 1));
    let (mut truth, mut blocked, mut terms, mut others) = (0, 0, [0; 5], 0);
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

/// Sorts items by their keys, below `keys`. Returns where each key's items start, and the items, which
/// stay in order within a key.
fn group<T>(keys: usize, items: impl Clone + Iterator<Item = (u32, T)>) -> (Vec<u32>, Vec<T>) {
    let mut start = vec![0; keys + 1];
    for (key, _) in items.clone() {
        start[key as usize + 1] += 1;
    }
    for key in 0..keys {
        start[key + 1] += start[key];
    }
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
    // Each query's kind, 1 for a BOOL and 0 for a phrase, and the range of its steps.
    list: Vec<(u32, u32, u32)>,
    steps: Vec<u32>,
    bitmap_count: usize, // How many terms have bitmaps...
    list_count: usize,   // ...and how many have chains.
    last_doc: u32,       // The last document's id.
    words: usize,        // A bitmap's length.
    bitmaps: Vec<u64>,
    phrase_start: Vec<u32>, // A term's phrases are `phrases[phrase_start[t]..phrase_start[t + 1]]`...
    phrases: Vec<u32>, // ...each its query, the anchor's position in it, its length, its terms.
    // Where a term's short and long checks start, and which is its count. A term's checks end where the
    // next term's start. Counts are in the order of the short checks, the long checks, then the terms'.
    anchor_lists: Vec<(u32, u32, u32)>,
    short_checks: Vec<Check<3>>,
    long_checks: Vec<Check<5>>,
    posting_queries: Vec<u32>, // The queries anchored on a common term.
    shared: Vec<u32>, // For each query, the answer of the term count that adds to it, or NONE.
    // The answers: the queries', in order, then one for each term with a count, then a spare.
    answers: Vec<Mutex<Answer>>,
    // For each count, which query's or term's count it belongs to, and the last of that answer's ten smallest
    // documents when it last saw them, which only falls, so that the documents past it need no lock.
    // Those fill first, as threads work through the corpus together, and only a thread's own first ten
    // ever get this far.
    slots: Vec<(u32, AtomicU32)>,
}

/// Puts a document among the smallest of a count's answer. Returns `TOO_BIG` if it's past the limit, as all
/// the thread's later ones will be too. This is cold beside the check that leads here, and keeping it out
/// of line lets that check be inlined.
#[cold]
fn add_smallest(plan: &Plan, at: usize, doc: u32) -> u64 {
    let (slot, limit) = &plan.slots[at];
    if doc >= limit.load(Relaxed) {
        return TOO_BIG;
    }
    let smallest = &mut plan.answers[*slot as usize].lock().unwrap().2;
    insert_smallest(smallest, doc);
    limit.store(smallest[SMALLEST - 1], Relaxed);
    0
}

/// How many times each term occurs, among pieces of the corpus 2 MiB in all, about 1%, and how many
/// documents those pieces have. A term's occurrences stand in for the documents holding it: this only
/// guides the plan's choices.
fn sample_terms(docs: &[u8], terms: &TermTable, threads: usize) -> (Vec<u32>, u32) {
    let df: Vec<AtomicU32> = (0..terms.count).map(|_| AtomicU32::new(0)).collect();
    let (sampled_docs, next_piece) = (AtomicU32::new(0), AtomicUsize::new(0));
    let piece = (1 << 21).min(docs.len()) / threads;
    on_threads(threads, || {
        let start = next_piece.fetch_add(1, Relaxed) * (docs.len() / threads);
        let mut lists = Lists::default();
        for_each_doc(docs, start, start + piece, |_, text, edges| {
            sampled_docs.fetch_add(1, Relaxed);
            let [marked, ..] = look_up(terms, text, edges, &mut lists);
            for word in &lists.0[2][..marked] {
                df[(word & TERM_BITS) as usize].fetch_add(1, Relaxed);
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
    let (start, ordered) = group(258, (FIRST_TERM..df.len()).map(|t| (bucket(t) as u32, t)));
    (plan.bitmap_count, plan.list_count) = (start[1] as usize, (start[2] - start[1]) as usize);
    let mut new_number = vec![0; df.len()];
    for (at, &term) in ordered.iter().enumerate() {
        new_number[term] = (FIRST_TERM + at) as u32;
        flags[term] |= new_number[term] | (u32::from(store_of[term] == 1) * CHAINED);
    }
    // Empty ways hold term 0, whose word stays 0, so this needs no branch on which ways are in use.
    for word in plan.terms.sets.iter_mut().flat_map(|set| &mut set.words) {
        *word = flags[(*word & TERM_BITS) as usize] | (*word & UNSURE);
    }
    for step in &mut plan.steps {
        *step = (*step & !TERM_BITS) | new_number[(*step & TERM_BITS) as usize];
    }
    plan.terms.plain_top = (FIRST_TERM + plan.bitmap_count) as i32 - 1;
    new_number
}

/// Lists each anchor term's phrases, given (term, (query, the anchor's position in it)).
fn list_phrases(plan: &mut Plan, anchored: &[(u32, (u32, u32))], new_number: &[u32]) {
    let keyed = anchored.iter().map(|&(t, p)| (new_number[t as usize], p));
    let (start, phrases) = group(plan.terms.count, keyed);
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
fn make_plan(plan: &mut Plan, docs: &[u8], threads: usize) {
    let (df, sampled_docs) = sample_terms(docs, &plan.terms, threads);
    // A query anchored on a common term, with df above about N/24, is answered from postings: bitmaps
    // for its common terms and chains for the others. On memory alone, a bitmap (N/8 bytes) and a
    // chain (about 3 bytes per posting) cross near df = N/24. Any choice gives the same answers.
    let common = |term: u32| df[term as usize] * 24 >= sampled_docs;
    // How a term is stored: 0 for a bitmap, 1 for a chain, and 2 for neither. What each term's word
    // holds beyond its number.
    let (mut store_of, mut flags) = (vec![2; df.len()], vec![0; df.len()]);
    // The number of each term's own count, among those.
    let (mut shared_of, mut shared_count) = (vec![NONE; df.len()], 0);
    // Each phrase and its anchor term, each check and its anchor term, and one query's anchors.
    let (mut anchored, mut checks, mut these) = (vec![], vec![], vec![]);
    let queries = plan.list.len() as u32;
    plan.shared.resize(queries as usize, NONE);
    for (number, &(is_bool, from, to)) in plan.list.iter().enumerate() {
        let (number, steps) = (number as u32, &plan.steps[from as usize..to as usize]);
        if is_bool == 0 {
            // Any term of a phrase would be correct as its anchor. The rarest triggers the fewest checks.
            let rarest = (0..steps.len()).min_by_key(|&at| df[steps[at] as usize]);
            let rarest = rarest.unwrap_or(0);
            flags[steps[rarest] as usize] |= PHRASE_ANCHOR;
            anchored.push((steps[rarest], (number, rarest as u32)));
            continue;
        }
        these.clear();
        anchors(steps, &df, &mut these);
        if these.iter().any(|&term| common(term)) {
            plan.posting_queries.push(number);
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
            *shared = (*shared).min(shared_count);
            shared_count += u32::from(*shared == shared_count);
            plan.shared[number as usize] = queries + *shared;
        }
        for (k, &term) in these.iter().enumerate() {
            flags[term as usize] |= BOOL_ANCHOR;
            if k > 0 || sure.is_none() {
                checks.push((term, (number, pack(steps, term, &these[..k]))));
            }
        }
    }
    let new_number = number_terms(plan, &df, &store_of, &mut flags);
    list_phrases(plan, &anchored, &new_number);

    // A term's checks, the short ones of all terms first. A short check holds up to 3 other terms.
    let count = plan.terms.count;
    for (term, (_, check)) in &mut checks {
        *term = new_number[*term as usize] + u32::from(check.1[3] != 0) * count as u32;
        check.1 = check.1.map(|term| new_number[term as usize]);
    }
    let (start, checks) = group(2 * count, checks.iter().copied());
    let narrow = |&(_, Check(truth, [a, b, c, ..])): &(u32, Check<5>)| Check(truth, [a, b, c]);
    let (short, long) = checks.split_at(start[count] as usize);
    plan.short_checks = short.iter().map(narrow).collect();
    plan.long_checks = long.iter().map(|c| c.1).collect();
    let answers = queries + shared_count + 1;
    let slot_of = checks.iter().map(|c| c.0).chain(queries..answers);
    plan.slots = slot_of.map(|slot| (slot, AtomicU32::new(NONE))).collect();
    let blank = |_| Mutex::new((0, 0, [NONE; SMALLEST]));
    plan.answers = (0..answers).map(blank).collect();

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

/// A piece of one term's posting list: the previous block's number, or NONE if this is the first, as
/// chains run from the newest block back to the oldest; its first document; and its postings, as offsets
/// from that.
struct Block(u32, u32, [u16; BLOCK_POSTINGS]);

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
    counted: Vec<u32>, // The phrases that the document in hand has counted in.
    short: Vec<u32>, // The short checks to run for the document in hand.
}

/// What the stages fill for a document, with room for each of its tokens and three more: the tokens' words,
/// those to look at again, the words of terms with no bitmap, the positions of phrase anchors, the terms
/// with BOOL checks, the terms with chains, and the terms with bitmaps; and the tokens' keys, and their sets'
/// addresses.
#[derive(Default)]
struct Lists([Vec<u32>; 7], [Vec<u64>; 2]);

/// The bits of `hits` that hold the sum of ids, which `main` checks has room.
const SUM_BITS: u32 = 40;

/// A high bit of `hits`, past any count, which says that no more documents are among the smallest.
const TOO_BIG: u64 = 1 << 62;

/// Puts a document among a list's ten smallest.
fn insert_smallest(smallest: &mut [u32; SMALLEST], mut doc: u32) {
    for held in smallest {
        (*held, doc) = (doc.min(*held), doc.max(*held));
    }
}

/// Looks the tokens up: first finding each one's key and prefetching its set, so that cache misses
/// overlap, then comparing the keys without a branch on what that finds. Only a word marked `UNSURE`
/// needs a full look, and few are. Lists the words that are terms, those of terms with bitmaps apart, and
/// the positions of phrase anchors, and returns how long those lists are. This stage stays out of line,
/// like `mark` and `check_bools`, because measurements say so: inlining this one costs 1.7% of all cycles,
/// and those two 1.9% together.
#[inline(never)]
fn look_up(terms: &TermTable, text: &[u8], edges: &[u32], lists: &mut Lists) -> [usize; 3] {
    let (sets, count) = (terms.sets.as_ptr() as i64, edges.len() / 2 - 4);
    if lists.1[0].len() < count + 4 {
        lists.0.iter_mut().for_each(|l| l.resize(count + 4, 0));
        lists.1.iter_mut().for_each(|l| l.resize(count + 4, 0));
    }
    let [tokens, later, marks, phrase_at, _, _, plain] = lists.0.each_mut().map(|l| &mut l[..]);
    let [keys, places] = lists.1.each_mut().map(|l| &mut l[..]);
    let shift = 64 - terms.sets.len().trailing_zeros() as i32;
    for at in (0..count).step_by(4) {
        // SAFETY: `edges` has four pairs more than there are tokens, each at or before a token's start,
        // whose load stays in the mapping as in `keys_of`. Keys and places have room for three more.
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
            let lens = _mm256_min_epu32(lens, _mm256_set1_epi64x(9));
            // The token's bytes are those whose numbers are below its length, copied to each byte.
            let copies = _mm256_set_epi64x(0x0808_0808_0808_0808, 0, 0x0808_0808_0808_0808, 0);
            let numbers = _mm256_set1_epi64x(0x0706_0504_0302_0100);
            let held = _mm256_cmpgt_epi8(_mm256_shuffle_epi8(lens, copies), numbers);
            let lower = _mm256_or_si256(bytes, _mm256_set1_epi8(0x20));
            let more = _mm256_slli_epi64(_mm256_cmpgt_epi32(lens, _mm256_set1_epi64x(8)), 63);
            let key = _mm256_or_si256(_mm256_and_si256(held, lower), more);
            let index = _mm256_srl_epi64(hashes(key), _mm_cvtsi32_si128(shift));
            let place = _mm256_add_epi64(_mm256_slli_epi64(index, 6), _mm256_set1_epi64x(sets));
            _mm256_storeu_si256(keys.as_mut_ptr().add(at).cast(), key);
            _mm256_storeu_si256(places.as_mut_ptr().add(at).cast(), place);
            for i in 0..4 {
                _mm_prefetch(*places.get_unchecked(at + i) as *const i8, _MM_HINT_T0);
            }
        }
    }
    let (mut later_count, mut marked, mut phrases, mut plains) = (0, 0, 0, 0);
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
            // A word that is surely a term's is positive as a signed number, and one of a term with a
            // bitmap is also below the others, with no flags. The table's row moves such words to the
            // front, and as many as there are stay in the list.
            let sure = mask(_mm_cmpgt_epi32(words, _mm_setzero_si128()));
            let rest = mask(_mm_cmpgt_epi32(words, _mm_set1_epi32(terms.plain_top)));
            let keep = |list: *mut u32, row: usize| {
                let kept = _mm_permutevar_ps(_mm_castsi128_ps(words), order(row));
                _mm_storeu_ps(list.cast(), kept);
                row.count_ones() as usize
            };
            marked += keep(marks.as_mut_ptr().add(marked), rest);
            plains += keep(plain.as_mut_ptr().add(plains), sure ^ rest);
            // A word with either of the top two flags gets another look.
            let again = mask(_mm_or_si128(words, _mm_slli_epi32(words, 1)));
            let positions = _mm_add_epi32(order(again), _mm_set1_epi32(at as i32));
            _mm_storeu_si128(later.as_mut_ptr().add(later_count).cast(), positions);
            later_count += again.count_ones() as usize;
        }
    }
    for at in later[..later_count].iter().map(|&at| at as usize) {
        // SAFETY: this is a token's position or at most three past the last, for which `tokens` has
        // room, and the other lists have room for every token.
        unsafe {
            let mut word = *tokens.get_unchecked(at);
            // Past the last token, the pairs are empty, whose words are 0 or the unused way's, as `find`
            // returns too, whose mark means nothing.
            if word >= UNSURE {
                let pair = edges.as_ptr().add(2 * at);
                let token = text.get_unchecked(*pair as usize..*pair.add(1) as usize);
                let set = &*(*places.get_unchecked(at) as *const TermSet);
                word = terms.find(token, *keys.get_unchecked(at), set);
                *tokens.get_unchecked_mut(at) = word;
                *marks.get_unchecked_mut(marked) = word;
                marked += usize::from(word != 0);
            }
            *phrase_at.get_unchecked_mut(phrases) = at as u32;
            phrases += (word >> 30 & 1) as usize;
        }
    }
    [marked, phrases, plains]
}

/// Marks what a document holds, with a byte per term, and lists what to do about it: the terms with BOOL
/// checks or chains, once each. Returns how long the lists are.
#[inline(never)]
fn mark(words: &[u32], present: &mut [u8], bools: &mut [u32], chains: &mut [u32]) -> [usize; 2] {
    let (mut bool_count, mut chain_count) = (0, 0);
    for &word in words {
        let term = word & TERM_BITS;
        // SAFETY: `present` has a mark for every term, and each list has room for every token.
        unsafe {
            let held = present.get_unchecked_mut(term as usize);
            let first = usize::from(*held ^ 1);
            *held = 1;
            *bools.get_unchecked_mut(bool_count) = term;
            bool_count += (word >> 29 & 1) as usize & first;
            *chains.get_unchecked_mut(chain_count) = term;
            chain_count += (word >> 28 & 1) as usize & first;
        }
    }
    [bool_count, chain_count]
}

/// Keeps which terms with bitmaps a document holds, each 64 of them a word of its row.
fn set_row(present: &[u8], rows: &mut [[u64; 64]], doc: u32) {
    for (row, marks) in rows.iter_mut().zip(present.as_chunks::<64>().0) {
        // SAFETY: a chunk has these 64 bytes, and the build targets x86-64-v3, which has AVX2. The shift
        // moves a mark to the top bit of its byte.
        let half = |at: usize| unsafe {
            let marks = _mm256_loadu_si256(marks.as_ptr().add(at).cast());
            _mm256_movemask_epi8(_mm256_slli_epi16(marks, 7)) as u32
        };
        row[doc as usize % 64] = u64::from(half(0)) | u64::from(half(32)) << 32;
    }
}

/// Turns 64 documents' rows into these documents' words of the bitmaps, a word per term, by swapping ever
/// smaller squares across the diagonal. No other thread writes these words, so that takes no atomic. A last
/// row's marks of terms past the bitmaps have no bitmap to go to.
fn store_rows(rows: &mut [[u64; 64]], bitmaps: &[AtomicU64], words: usize, word: usize) {
    for (index, row) in rows.iter_mut().enumerate() {
        for width in [32, 16, 8, 4, 2, 1] {
            let low_bits = !0u64 / ((1 << width) + 1);
            for square in row.chunks_exact_mut(2 * width) {
                let (first, second) = square.split_at_mut(width);
                for (a, b) in first.iter_mut().zip(second) {
                    let swapped = (*a >> width ^ *b) & low_bits;
                    *a ^= swapped << width;
                    *b ^= swapped;
                }
            }
        }
        for (&column, bitmap) in row.iter().zip(bitmaps[index * 64 * words..].chunks(words)) {
            bitmap[word].store(column, Relaxed);
        }
        *row = [0; 64];
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
            blocks[chunk].push(Block(*tail, doc, [0; BLOCK_POSTINGS]));
            *tail = (chunk * CHUNK_BLOCKS + blocks[chunk].len() - 1) as u32;
            (*fill, *base) = (0, doc);
        }
        let block = &mut blocks[*tail as usize / CHUNK_BLOCKS][*tail as usize % CHUNK_BLOCKS];
        block.2[*fill as usize] = (doc - *base) as u16;
        *fill += 1;
    }
}

/// Checks the phrases anchored at these positions of a document's tokens. A phrase matches so few documents
/// that each match can afford to take its answer's lock.
fn check_phrases(ats: &[u32], tokens: &[u32], plan: &Plan, doc: u32, counted: &mut Vec<u32>) {
    counted.clear();
    for at in ats.iter().map(|&at| at as usize) {
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
            if run.is_some_and(same) && !counted.contains(query) {
                counted.push(*query);
                let mut answer = plan.answers[*query as usize].lock().unwrap();
                (answer.0, answer.1) = (answer.0 + 1, answer.1 + u64::from(doc));
                insert_smallest(&mut answer.2, doc);
            }
        }
    }
}

/// Counts a document at these anchors of it, for each term's own count and each check that says so.
/// Prefetching first lets cache misses overlap, and listing the short checks before running them
/// replaces an unpredictable loop exit per anchor by one loop.
#[inline(never)]
fn check_bools(anchors: &[u32], present: &[u8], plan: &Plan, doc: u32, scan: &mut Scan) {
    let (hits, short) = (&mut scan.hits[..], &mut scan.short[..]);
    let (checks, short_hits) = (plan.short_checks.as_ptr(), hits.as_ptr());
    let (lists, this_doc) = (&plan.anchor_lists[..], (1 << SUM_BITS) + u64::from(doc));
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
    let (short_count, mut listed) = (plan.short_checks.len(), 0);
    for &term in anchors {
        // SAFETY: `lists` has one entry more than there are terms, `short` has room for all the checks
        // and four more, and an anchor's checks are all in `long_checks`.
        unsafe {
            let (from, long_from, shared) = *lists.get_unchecked(term as usize);
            let (to, long_to, _) = *lists.get_unchecked(term as usize + 1);
            prefetch(checks.wrapping_add(from as usize).cast());
            prefetch(short_hits.wrapping_add(from as usize).cast());
            let at = _mm_add_epi32(_mm_set1_epi32(from as i32), _mm_setr_epi32(0, 1, 2, 3));
            _mm_storeu_si128(short.as_mut_ptr().add(listed).cast(), at);
            for at in from + 4..to {
                short[listed + (at - from) as usize] = at;
            }
            listed += (to - from) as usize;
            add(1, shared as usize);
            for at in long_from as usize..long_to as usize {
                let check = plan.long_checks.get_unchecked(at - short_count);
                add(counts(check, present), at);
            }
        }
    }
    for &at in &short[..listed] {
        // SAFETY: the list came from `anchor_lists`.
        let check = unsafe { plan.short_checks.get_unchecked(at as usize) };
        add(counts(check, present), at as usize);
    }
}

/// Claims the next `size` of the numbers below `len`, if any are left.
fn claim(next: &AtomicUsize, size: usize, len: usize) -> Option<std::ops::Range<usize>> {
    let from = next.fetch_add(size, Relaxed);
    (from < len).then(|| from..(from + size).min(len))
}

/// Claims ranges of the corpus until none is left, and scans them.
fn scan_ranges(docs: &[u8], plan: &Plan, range_size: usize, next_range: &AtomicUsize) -> Scan {
    let mut scan = Scan::default();
    scan.hits.resize(plan.slots.len(), 0);
    scan.chains = vec![(NONE, BLOCK_POSTINGS as u32, 0); plan.list_count];
    scan.short = vec![0; plan.short_checks.len() + 4];
    // With a row's worth more, as `set_row` reads whole rows of marks.
    let mut present = vec![0u8; plan.terms.count + 64];
    let chain_from = FIRST_TERM + plan.bitmap_count;
    // SAFETY: AtomicU64 has the layout of u64, and nothing else uses the bitmaps during the scan.
    let bitmaps = unsafe { &*(&plan.bitmaps[..] as *const [u64] as *const [AtomicU64]) };
    let mut lists = Lists::default();
    let mut rows = vec![[0; 64]; plan.bitmap_count.div_ceil(64)];
    while let Some(range) = claim(next_range, range_size, docs.len()) {
        let (at, populate_read) = (range.start & !4095, 22);
        // SAFETY: this maps pages that the range holds, with fewer faults, and failing changes nothing.
        unsafe { madvise(docs.as_ptr().add(at), range.end - at, populate_read) };
        for_each_doc(docs, range.start, range.end, |doc, text, edges| {
            let [marked, phrases, plains] = look_up(&plan.terms, text, edges, &mut lists);
            let [tokens, _, marks, phrase_at, bool_terms, chained, plain] = &mut lists.0;
            for &word in &plain[..plains] {
                // SAFETY: `present` has a mark for every term, and these words have no flags.
                unsafe { *present.get_unchecked_mut(word as usize) = 1 };
            }
            let (in_doc, marked) = (&tokens[..edges.len() / 2 - 4], &marks[..marked]);
            let [bools, chains] = mark(marked, &mut present, bool_terms, chained);
            set_row(&present[FIRST_TERM..], &mut rows, doc);
            // Only the corpus's last 64 documents can be fewer.
            if doc % 64 == 63 || doc == plan.last_doc {
                store_rows(&mut rows, bitmaps, plan.words, doc as usize / 64);
            }
            extend_chains(&chained[..chains], chain_from, &mut scan, doc);
            check_phrases(&phrase_at[..phrases], in_doc, plan, doc, &mut scan.counted);
            check_bools(&bool_terms[..bools], &present, plan, doc, &mut scan);
            for &word in marked {
                // SAFETY: `present` has a mark for every term.
                unsafe { *present.get_unchecked_mut((word & TERM_BITS) as usize) = 0 };
            }
            present[FIRST_TERM..chain_from].fill(0);
        });
    }
    scan
}

/// Sets the bits of a posting list's documents.
fn set_bits_of_list(scans: &[Scan], list: usize, bits: &mut [u64]) {
    let chains = scans.iter().map(|scan| (scan, scan.chains[list]));
    let mut walks: Vec<_> = chains.filter(|walk| walk.1.0 != NONE).collect();
    while !walks.is_empty() {
        walks.retain_mut(|(scan, (tail, fill, _))| {
            let block = block_at(&scan.blocks, *tail);
            for &offset in &block.2[..*fill as usize] {
                let doc = block.1 + u32::from(offset);
                bits[doc as usize / 64] |= 1 << (doc % 64);
            }
            (*tail, *fill) = (block.0, BLOCK_POSTINGS as u32);
            if *tail != NONE {
                let next: *const Block = block_at(&scan.blocks, *tail);
                // SAFETY: a prefetch has no other effect.
                unsafe { _mm_prefetch(next.cast(), _MM_HINT_T0) };
            }
            *tail != NONE
        });
    }
}

/// Finds a BOOL query's result as a bitmap: applies its steps in order, from the empty set, with each
/// term's set as a bitmap.
fn run_steps(query: u32, plan: &Plan, scans: &[Scan], bits: &mut [u64], other: &mut [u64]) {
    bits.fill(0);
    let (_, from, to) = plan.list[query as usize];
    for step in &plan.steps[from as usize..to as usize] {
        let number = (step & TERM_BITS) as usize - FIRST_TERM;
        let term_set = if number < plan.bitmap_count {
            &plan.bitmaps[number * plan.words..(number + 1) * plan.words]
        } else {
            other.fill(0);
            set_bits_of_list(scans, number - plan.bitmap_count, other);
            &*other
        };
        for (word, &with) in bits.iter_mut().zip(term_set) {
            *word = apply(step & !TERM_BITS, *word, with);
        }
    }
}

/// What a query's result, as a bitmap, prints.
fn answer_of(bits: &[u64]) -> Answer {
    let (mut smallest, mut found, mut at) = ([NONE; SMALLEST], 0, 0);
    while found < SMALLEST && at < bits.len() {
        let mut rest = bits[at];
        while rest != 0 && found < SMALLEST {
            smallest[found] = 64 * at as u32 + rest.trailing_zeros();
            found += 1;
            rest &= rest - 1;
        }
        at += 1;
    }
    // How many bits are set in these words, a multiple of 4 of them, and the sum of their positions. Each
    // nibble's count and sum of positions are table lookups, so a byte gets them at once. A position is
    // the word's start, plus 8 times the byte's number in the word, plus its place in the byte. The bytes'
    // numbers times their counts stay below 256 for each pair of bytes, so that SAD can add those too.
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
        let total = |v: __m256i| std::mem::transmute::<_, [u64; 4]>(v).iter().sum::<u64>();
        (total(counts), total(sums), smallest)
    }
}

/// Prints a query's line: each number with a space after it, the last of which becomes the line's end.
fn print_line(out: &mut Vec<u8>, (count, sum, smallest): &Answer) {
    let docs = smallest.iter().take(*count as usize).map(|&d| u64::from(d));
    for mut value in [*count, *sum].into_iter().chain(docs) {
        let (mut digits, mut start) = ([b' '; 21], 19);
        while value >= 10 {
            digits[start] = b'0' + (value % 10) as u8;
            value /= 10;
            start -= 1;
        }
        digits[start] = b'0' + value as u8;
        out.extend_from_slice(&digits[start..]);
    }
    *out.last_mut().unwrap() = b'\n';
}

/// First the totals, then the queries that read postings, as they take most of the time. Then, by the
/// same threads, as starting more costs more than waiting at a barrier, threads claim batches of queries
/// in order, and each waits for the batch before it to reach the file. The wait is short, because batches
/// cost about the same.
fn answer_queries(file: File, plan: &Plan, scans: &[Scan], threads: usize) {
    let [next_slot, next_query, next_batch, written]: [AtomicUsize; 4] = Default::default();
    let (query_count, barrier) = (plan.list.len(), Barrier::new(threads));
    on_threads(threads, || {
        // Sums the threads' counts and ids into each scan-answered query's and term's count and sum of
        // ids, visiting each answer once, as they are many cache lines and the threads' counts are in
        // order. Threads claim parts of this.
        while let Some(slots) = claim(&next_slot, BATCH, plan.slots.len()) {
            for at in slots {
                let hits: u64 = scans.iter().map(|scan| scan.hits[at] & !TOO_BIG).sum();
                let mut answer = plan.answers[plan.slots[at].0 as usize].lock().unwrap();
                answer.0 += hits >> SUM_BITS;
                answer.1 += hits & ((1 << SUM_BITS) - 1);
            }
        }
        let (mut bits, mut other) = (vec![0; plan.words.next_multiple_of(4)], vec![0; plan.words]);
        while let Some(&number) = plan.posting_queries.get(next_query.fetch_add(1, Relaxed)) {
            run_steps(number, plan, scans, &mut bits, &mut other);
            *plan.answers[number as usize].lock().unwrap() = answer_of(&bits);
        }
        barrier.wait();
        let mut text = Vec::new();
        while let Some(batch) = claim(&next_batch, BATCH, query_count) {
            text.clear();
            for number in batch.clone() {
                let mut answer = *plan.answers[number].lock().unwrap();
                // NONE is no answer's number.
                if let Some(shared) = plan.answers.get(plan.shared[number] as usize) {
                    let shared = *shared.lock().unwrap();
                    (answer.0, answer.1) = (answer.0 + shared.0, answer.1 + shared.1);
                    for doc in shared.2 {
                        insert_smallest(&mut answer.2, doc);
                    }
                }
                print_line(&mut text, &answer);
            }
            while written.load(Acquire) != batch.start {
                thread::yield_now();
            }
            // `panic = "abort"`: a failed write ends the process rather than leaving the other
            // threads waiting for their turn.
            (&file).write_all(&text).expect("writing the results");
            written.store(batch.end, Release);
        }
    });
}

fn main() -> io::Result<()> {
    // All of the heap comes from one contiguous region, grown a lot at a time, so that the kernel can
    // back it with huge pages, in far fewer page faults than threads' arenas and small mappings take.
    // M_ARENA_MAX to 1, M_MMAP_THRESHOLD to 1 GiB, and M_TOP_PAD to 64 MiB.
    for (param, value) in [(-8, 1), (-3, 1 << 30), (-2, 1 << 26)] {
        // SAFETY: no thread has started, and these only tune the allocator.
        unsafe { mallopt(param, value) };
    }
    let mut args = std::env::args().skip(1);
    let data_dir = args.next().unwrap_or_else(|| "/data".into());
    let out_dir = args.next().unwrap_or_else(|| "/out".into());
    // `SD_THREADS` if set (the test sweep uses it), else what the container allows, capped at the
    // box's 12 threads.
    let asked = std::env::var("SD_THREADS").map_or(0, |v| v.parse().unwrap_or(0));
    let allowed = thread::available_parallelism().map_or(1, |n| n.get().min(12));
    let threads = if asked > 0 { asked } else { allowed };
    let docs = map_file(&format!("{data_dir}/docs.txt"))?;
    // exp: the queries read and planned once more first, thrown away, so that T minus the usual T is
    // the wall time of the parse, the sample and the plan.
    let queries = map_file(&format!("{data_dir}/queries.txt"))?;
    {
        let mut plan0 = parse_queries(queries, threads);
        make_plan(&mut plan0, docs, threads);
        std::mem::forget(plan0);
    }
    let mut plan = parse_queries(queries, threads);
    make_plan(&mut plan, docs, threads);
    // The last document's id is on the last line.
    let last_line = docs
        .trim_ascii_end()
        .rsplit(|&c| c == b'\n')
        .next()
        .unwrap();
    let digits = last_line.iter().take_while(|&&byte| byte != b'\t');
    plan.last_doc = digits.fold(0, |id, &digit| id * 10 + u32::from(digit - b'0'));
    plan.words = plan.last_doc as usize / 64 + 1;
    assert!(plan.words * 64 < 1 << (SUM_BITS / 2), "too many documents");
    // Filled with zeros by this thread, as threads that fault in the same huge page each allocate one, which
    // the memory peak counts.
    plan.bitmaps = vec![0; plan.bitmap_count * plan.words];
    let file = File::create(format!("{out_dir}/results.txt"))?;
    // 16 ranges per thread: enough that a thread slowed for a while doesn't hold the others up.
    let (range_size, claimed) = ((docs.len() / (threads * 16)).max(1), AtomicUsize::new(0));
    let scans = on_threads(threads, || scan_ranges(docs, &plan, range_size, &claimed));
    answer_queries(file, &plan, &scans, threads);
    // Nothing is buffered, and the kernel frees all the memory faster than dropping it would.
    std::process::exit(0)
}
