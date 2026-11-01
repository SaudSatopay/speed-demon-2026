//! Track B (Index): boolean and phrase queries over a 254 MB corpus.
//!
//! One scan of the corpus answers every query as it goes, so no postings are ever stored. The threads
//! share every phase but the plan's decisions, which take one thread. In order:
//!
//! 1. **Parse** (`parse_queries`). The queries are read first, into one table of their terms, so that
//!    only those terms are ever looked up.
//! 2. **Plan** (`make_plan`). A sample of about 1% of the corpus says which terms are common. Every
//!    document matching a BOOL query contains one of a few anchor terms (the rarest AND of its last
//!    run, or its OR terms). For nine queries in ten the anchors are rare: at each anchor's first
//!    occurrence in a document, the marks of what the document holds index a truth table. A query whose
//!    anchors are common goes by blocks instead: each thread keeps a bit per document of a block for each
//!    term of such queries, and runs them in vectors when the block is full. A phrase is anchored on its
//!    rarest term.
//! 3. **Scan** (`scan_ranges`). Threads claim ranges of the corpus in order and take a document at a
//!    time, in stages without a branch per token: `token_edges` finds the tokens, `look_up` finds their
//!    terms, `mark` marks them, and the later stages act on what those listed: `check_phrases` tries each
//!    phrase around its anchor's occurrences once the document's tokens are known, `check_bools` runs
//!    the truth tables, and `store_rows` and `answer_block` fill the columns and run the block queries.
//! 4. **Answer** (`scan_and_answer`). Threads add up what they counted, and print all the answers in
//!    order.
//!
//! The words used throughout:
//!
//! - A *term* is a distinct token of the queries, known by its number. Term 0 stands for every other
//!   token.
//! - A term's *word* is what the term table holds for it: its number, and flags for what its
//!   occurrences call for.
//! - A *step* of a query is a term's number and its operator.
//! - An *anchor* is a term whose occurrences trigger a check of a query.
//! - A *mark* is a byte per term, set while the document in hand holds the term.
//! - A *check* is a BOOL query's truth table at one of its anchors.
//! - An *answer* is what a query's line prints. A *tally* is where each thread adds up the documents
//!   for one check, or for one term alone, before they go into an answer.
//! - A *block* is up to 256 documents in a row, which one thread takes together.
//! - A *column* is a term's bits over a block: which of its documents hold the term.
//! - A *row* is one document's marks of the commonest terms, as bits, which become their columns.

#![no_main]

use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering::*};
use std::sync::{Barrier, Mutex, MutexGuard, OnceLock, PoisonError};
use std::{arch::x86_64::*, fs::File, io::Write, mem::zeroed, os::fd::AsRawFd, thread};

/// Term 0 is every token that no query uses, and the empty ways of the term table hold it. A truth
/// table doesn't depend on a slot that no term took, so this term also fills the unused slots of a
/// BOOL check, whatever its mark says. Real terms start here: those with rows first, then the others with
/// columns, then the rest.
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
/// ...and its column gets each document that holds it, one bit at a time.
const SPARSE: u32 = 1 << 28;
/// A step of a query is a term's number with its operator in the top bits. OR is 0, as it is for the
/// first term: starting from the empty set, "OR t1" gives S(t1). A phrase's steps have no operator.
const AND: u32 = 1 << 30;
const NOT: u32 = 2 << 30;
/// No query, document, block or answer has this number.
const NONE: u32 = u32::MAX;
/// How many queries a thread claims at a time to print, and how many tallies to add up: few enough
/// batches (49 at full) that the threads waiting for their turn to write hardly spin, and each batch's
/// text is about 30 KB.
const BATCH: usize = 1024;
/// How many of a query's matching documents are printed.
const SMALLEST: usize = 10;

/// Readable zero bytes after each mapped file, so that block loads can run past its end.
const PADDING: usize = 4096;
/// Setting this bit in every byte lowercases ASCII letters, and digits already have it set.
const CASE_BITS: u64 = 0x2020_2020_2020_2020;

// Declared by hand to keep the crate at zero dependencies; std already links glibc.
unsafe extern "C" {
    fn mmap(at: *mut u8, len: usize, p: i32, f: i32, fd: i32, o: i64) -> *mut u8;
    fn mallopt(param: i32, value: i32) -> i32;
}

/// Maps a file of this directory read-only for the life of the process, followed by `PADDING` readable
/// zero bytes. The scorer's memory peak doesn't charge us for these pages, unlike reading the file onto
/// the heap.
fn map_file(dir: &str, name: &str) -> &'static [u8] {
    let (read, private, fixed, anonymous, null) = (1, 2, 0x10, 0x20, std::ptr::null_mut());
    let file = File::open(format!("{dir}/{name}")).expect(name);
    let len = file.metadata().expect(name).len() as usize;
    // SAFETY: the first call reserves fresh zero pages, and the second maps the file over the start
    // of them, which fails only on an empty file. Neither is ever unmapped, and nothing writes to the
    // dataset while we run.
    unsafe {
        let base = mmap(null, len + PADDING, read, private | anonymous, -1, 0);
        let mapped = mmap(base, len, read, private | fixed, file.as_raw_fd(), 0);
        let mapped_all = base as isize != -1 && (mapped == base || len == 0);
        assert!(mapped_all, "mapping {name} failed");
        std::slice::from_raw_parts(base.cast(), len)
    }
}

/// One bit per byte of the 64 at `pos`, set for the token bytes, `[A-Za-z0-9]`, and likewise for the
/// LFs. Bytes of 0x80 and above compare as negative, so they are separators, even inside a letter
/// (spec section 2.2).
fn block_masks(file: &[u8], pos: usize) -> (u64, u64) {
    assert!(pos + 64 <= file.len() + PADDING, "a line has no LF");
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
            let lower = _mm256_or_si256(bytes, byte_set(CASE_BITS as u8));
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
        // Only the bits below the first LF, if there is one.
        let tokens = tokens & (line_feeds.wrapping_sub(1) & !line_feeds);
        let found = tokens ^ (tokens << 1 | carry);
        carry = tokens >> 63;
        edges.reserve(64 + 8);
        // SAFETY: the reserve leaves room for a block's at most 64 edges and the 8 that a store can
        // write past them, and the table is indexed by a byte. The build targets x86-64-v3, which has
        // AVX2.
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
        let others = handles.into_iter().map(|h| h.join().expect("panics abort"));
        std::iter::once(work()).chain(others).collect()
    })
}

// ---- The term table: query terms to their words, for every token of the corpus ----

/// A token's keys. The short key is its first 8 bytes, lowercased and zero-padded, with the top bit set
/// if there are more, which no byte of a token has. The long key is the next 4 bytes, likewise. Its top
/// bit, set if there are more still, marks a token that needs a full compare, and those are rare. For
/// a very long token, the count of bits to keep wraps, which keeps the key a function of the token, as
/// these need a full compare anyway.
fn keys_of(token: &[u8]) -> (u64, u32) {
    let (bytes, len) = (token.as_ptr(), token.len());
    // SAFETY: a token is inside a mapping that continues for `PADDING` bytes past the file. The loads
    // need no alignment, and the build targets x86-64-v3, which has BMI2, whose BZHI keeps the given
    // number of low bits, or all of them if that's more than the width.
    unsafe {
        let short = bytes.cast::<u64>().read_unaligned() | CASE_BITS;
        let long = bytes.add(8).cast::<u32>().read_unaligned() | CASE_BITS as u32;
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
        // The halves of the multiplier 0x9E37_79B9_7F4A_7C15, from the golden ratio.
        let low = _mm256_set1_epi64x(0x7F4A_7C15);
        let high = _mm256_set1_epi64x(0x9E37_79B9);
        let tops = _mm256_srli_epi64(keys, 32);
        let cross = _mm256_add_epi64(_mm256_mul_epu32(tops, low), _mm256_mul_epu32(keys, high));
        let hash = _mm256_add_epi64(_mm256_mul_epu32(keys, low), _mm256_slli_epi64(cross, 32));
        let zero = _mm256_setzero_si256();
        if cfg!(sd_const_hash) { zero } else { hash }
    }
}

/// The way of a set that never holds a term.
const UNUSED_WAY: usize = 3;
/// The top bit of a long key, on a term longer than the keys. The rest of it says where the term is in
/// the query file.
const LONG: u32 = 1 << 31;
/// How many terms a table takes for each of its sets, whose three ways are more than that, so that a
/// probe always comes to a set with room.
const TERMS_PER_SET: usize = 2;

/// Three terms in one cache line, whose keys are compared at once, so a lookup has no branch on which
/// of them it found, or whether it found one. The fourth way is never used, and its word is what a lookup
/// that finds none gets: 0 (no query uses this token), or `UNSURE` if the probe goes on. While the queries
/// are read, its short key is the set's lock.
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
    /// ways stop the count at it. The `& 3` only spares a bounds check.
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
    plain_top: i32,     // Terms up to this have rows.
}

impl TermTable<'_> {
    /// The word of the term of this token, in any letter case, or else 0, from a probe that starts at
    /// this set. A set that no term was placed past ends a probe.
    fn find<'s>(&'s self, token: &[u8], short: u64, mut set: &'s TermSet) -> u32 {
        loop {
            let ways = self.ways_of(set, token, short, keys_of(token).1);
            // One test, not two: that this finds a term or not is a coin toss.
            if ways | u32::from(set.words[UNUSED_WAY] == 0) != 0 {
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
        if long & LONG == 0 {
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
            let term = self.text.get((long & !LONG) as usize..).unwrap_or(&[]);
            ways >> way & long >> 31 != 0
                && token.eq_ignore_ascii_case(term.get(..end).unwrap_or(&[]))
                && !term.get(end).is_some_and(u8::is_ascii_alphanumeric)
        };
        (0..3).filter(is_token).map(|way| 1 << way).sum()
    }

    /// Adds this query term, if it's new, and returns its number. With no room left in the table, that
    /// number is too big for it, and `parse_queries` reads the queries again into a larger one. Threads do
    /// this at once: each holds the lock of the set it's looking at, the fourth short key, which no key
    /// equals. So threads adding one term get the same number, as sets only fill, and the first set with
    /// room is where it goes. A term that's in already is found without the lock, as a way's key is stored
    /// after its word, so a key that's there has its word: most of the tokens are common terms, which
    /// every thread meets.
    fn intern(&self, token: &[u8], next_term: &AtomicUsize) -> u32 {
        let (short, long) = keys_of(token);
        // SAFETY: the build targets x86-64-v3, which has AVX2.
        let hash = unsafe { _mm256_extract_epi64::<0>(hashes(_mm256_set1_epi64x(short as i64))) };
        let mut index = (hash as u64 >> (64 - self.sets.len().trailing_zeros())) as usize;
        loop {
            // SAFETY: the set is in the table. The look before the lock can run while another thread
            // fills the set: it acts only on a short key that's there, which an aligned load doesn't tear,
            // and whose `Release` store comes after that way's long key and word. Holding the lock,
            // this thread alone writes to the set, and only to ways that readers don't act on yet.
            unsafe {
                let set = self.sets.as_ptr().add(index).cast_mut();
                let ways = self.ways_of(&*set, token, short, long);
                if ways != 0 {
                    return (*set).word(ways) & TERM_BITS;
                }
                let lock = AtomicU64::from_ptr(&raw mut (*set).shorts[UNUSED_WAY]);
                while lock.swap(1, Acquire) != 0 {}
                let set = &mut *set;
                let ways = self.ways_of(set, token, short, long);
                // There's room if the last way is free, as they fill in order.
                let (mut term, room) = (set.word(ways) & TERM_BITS, set.shorts[2] == 0);
                if ways == 0 && room {
                    term = next_term.fetch_add(1, Relaxed) as u32;
                    if (term as usize) < FIRST_TERM + TERMS_PER_SET * self.sets.len() {
                        let way = set.ways(0).trailing_zeros() as usize;
                        let at = (token.as_ptr() as usize - self.text.as_ptr() as usize) as u32;
                        set.longs[way] = if long & LONG != 0 { LONG | at } else { long };
                        // A token past the short key has more to compare.
                        set.words[way] = term | u32::from(token.len() > 8) * UNSURE;
                        AtomicU64::from_ptr(&raw mut set.shorts[way]).store(short, Release);
                    }
                }
                // A lookup that finds nothing in a set this term has passed has to look further.
                set.words[UNUSED_WAY] |= u32::from(ways == 0 && !room) * UNSURE;
                lock.store(0, Release);
                if ways != 0 || room {
                    return term;
                }
            }
            index = (index + 1) & (self.sets.len() - 1);
        }
    }
}

/// Reads the queries into a new plan, whose term table has this many sets: each thread the lines that
/// start in its part of the text.
fn parse_queries(text: &[u8], threads: usize, sets: usize) -> Plan<'_> {
    let mut plan = Plan::default();
    // Empty sets, in place at once, as threads that fault in the same huge page each allocate one, which
    // the memory peak counts.
    // SAFETY: zeros are a valid TermSet.
    (plan.terms.text, plan.terms.sets) = (text, (0..sets).map(|_| unsafe { zeroed() }).collect());
    let (next_part, next_term) = (AtomicUsize::new(0), AtomicUsize::new(FIRST_TERM));
    let mut parts = on_threads(threads, || {
        let part = claim(&next_part, text.len() / threads + 1, text.len()).unwrap_or_default();
        let (mut queries, mut steps, mut edges) = (vec![], vec![], vec![]);
        let line_start = part.clone().find(|&at| at == 0 || text[at - 1] == b'\n');
        let mut pos = line_start.unwrap_or(part.end);
        while pos < part.end {
            let line_feed = token_edges(text, pos, &mut edges);
            // The first field is BOOL or PHRASE. Then come terms, in a BOOL query with an operator
            // between each two, in upper case, which goes in the top bits of the term after it.
            let (is_bool, from, mut op) = (text[pos] == b'B', steps.len() as u32, 0);
            for pair in &edges.as_chunks::<2>().0[1..] {
                let token = &text[pos + pair[0] as usize..pos + pair[1] as usize];
                if token[0].is_ascii_uppercase() {
                    op = u32::from(token[0] == b'A') * AND + u32::from(token[0] == b'N') * NOT;
                } else {
                    steps.push(op | plan.terms.intern(token, &next_term));
                }
            }
            pos = line_feed + 1;
            let to = steps.len() as u32;
            queries.push(Query { is_bool, from, to });
        }
        (part.start, queries, steps)
    });
    plan.terms.count = next_term.into_inner();
    // Far more terms than the table was sized for: the queries are read again, into a larger one.
    if plan.terms.count > FIRST_TERM + TERMS_PER_SET * sets {
        return parse_queries(text, threads, sets * 4);
    }
    parts.sort_unstable_by_key(|part| part.0);
    for (_, mut queries, steps) in parts {
        let base = plan.steps.len() as u32;
        plan.steps.extend(steps);
        let moved = |q: &mut Query| (q.from, q.to) = (base + q.from, base + q.to);
        queries.iter_mut().for_each(moved);
        plan.queries.extend(queries);
    }
    plan
}

/// `look_up` takes this many tokens at a time.
const LANES: usize = 4;

/// Visits the documents that start in the range `start..end` of the corpus, passing each one's id, its text
/// and what follows, and its tokens' edges in that, then `LANES` empty pairs of edges, so that `look_up` can
/// read that many pairs at a time.
fn for_each_doc(docs: &[u8], start: usize, end: usize, mut visit: impl FnMut(u32, &[u8], &[u32])) {
    let mut line = start;
    while line > 0 && line < end && docs[line - 1] != b'\n' {
        line += 1;
    }
    let mut edges = Vec::new();
    while line < end {
        // The id is the line's first token.
        let next = token_edges(docs, line, &mut edges) + 1;
        let digits = docs[line..line + edges[1] as usize].iter();
        let id = digits.fold(0, |id, &digit| id * 10 + u32::from(digit - b'0'));
        edges.extend([0; 2 * LANES]);
        visit(id, &docs[line..], &edges[2..]);
        line = next;
    }
}

// ---- The plan: how each query is answered, and how each term is stored ----

/// Whether a step has no operator: it's an OR, or a query's first term.
fn is_or(step: &u32) -> bool {
    step & !TERM_BITS == 0
}

/// Terms of which every document matching a BOOL query contains at least one, chosen rare. The
/// query's last run, from its last OR (the first term counts as an OR from the empty set), is
/// intersected with each AND in it, so any of those ANDs is in every match, and so is the first
/// term when the query is a single run. A run without an AND adds its OR term to the anchors of
/// the query before it. A term repeated among them is listed again, and `make_check` blocks its later
/// check.
fn anchors(steps: &[u32], df: &[u32], out: &mut Vec<u32>) {
    let last_or = steps.iter().rposition(is_or).unwrap_or(0);
    let ands = steps[last_or + 1..].iter().filter(|step| *step & AND != 0);
    let candidates = ands.chain(steps.first().filter(|_| last_or == 0));
    let best = candidates.min_by_key(|&step| df[(step & TERM_BITS) as usize]);
    out.push(best.unwrap_or(&steps[last_or]) & TERM_BITS);
    best.is_none().then(|| anchors(&steps[..last_or], df, out));
}

/// A BOOL query's check at one of its anchors. The anchor is present, so the marks of a document's
/// holding each of the query's other terms make a number, and that bit of `truth` says whether the
/// document counts here: whether it matches, and holds none of the query's earlier anchors, where it
/// has already counted. Most queries have few terms, and need a smaller check.
#[derive(Clone, Copy)]
#[repr(C)]
struct Check<const TERMS: usize> {
    truth: u32,
    terms: [u32; TERMS], // The other terms, then term 0.
}

/// 1 if a document holding the terms marked in `present` counts at this check, else 0.
fn counts<const TERMS: usize>(check: &Check<TERMS>, present: &[u8]) -> u64 {
    let mut holds = 0;
    for (slot, &term) in check.terms.iter().enumerate() {
        // SAFETY: `present` has a mark for every term.
        holds |= u32::from(unsafe { *present.get_unchecked(term as usize) }) << slot;
    }
    u64::from(check.truth >> holds & 1)
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
fn make_check(steps: &[u32], anchor: u32, earlier: &[u32]) -> Check<5> {
    // The positions in a word that have each of their 6 index bits set: a period of twice this width. These
    // divisions fold at compile time. (A division by a variable here would cost more than the rest of the plan.)
    let index_bits = [1, 2, 4, 8, 16, 32].map(|width| (!0u64 / ((1 << width) + 1)) << width);
    let (mut table, mut blocked, mut terms, mut others) = (0, 0, [0; 6], 0);
    for step in steps {
        // The other terms take the first five slots, and the anchor the last, which isn't kept.
        let term = step & TERM_BITS;
        let slot = if term == anchor { 5 } else { others };
        others += usize::from(term != anchor);
        terms[slot] = term;
        table = apply(step & !TERM_BITS, table, index_bits[slot]);
        blocked |= u64::from(earlier.contains(&term)) * index_bits[slot];
    }
    let truth = ((table & !blocked) >> 32) as u32;
    let [a, b, c, d, e, _] = terms;
    let terms = [a, b, c, d, e];
    Check { truth, terms }
}

/// Sorts items by their keys, below `keys`. Returns where each key's items start, then the number of items
/// twice, and the items, which stay in order within a key. A key's count goes two entries on, the sums make
/// that its start one entry on, and placing its items moves that to the next key's start.
fn group<T>(keys: usize, all: impl Clone + Iterator<Item = (u32, T)>) -> (Vec<u32>, Vec<T>) {
    let mut from = vec![0; keys + 2];
    all.clone().for_each(|(key, _)| from[key as usize + 2] += 1);
    (0..keys).for_each(|key| from[key + 2] += from[key + 1]);
    let mut grouped: Vec<T> = all.clone().map(|item| item.1).collect();
    for (key, item) in all {
        grouped[from[key as usize + 1] as usize] = item;
        from[key as usize + 1] += 1;
    }
    (from, grouped)
}

/// What a query's line prints, or what a term's tally adds to it.
#[derive(Clone, Copy)]
#[repr(C)]
struct Answer {
    count: u64,           // How many documents match.
    sum: u64,             // The sum of their ids.
    ids: [u32; SMALLEST], // The smallest ids, ascending, then NONE.
}

/// A query: whether it's a BOOL rather than a phrase, and the range of its steps.
struct Query {
    is_bool: bool,
    from: u32,
    to: u32,
}

/// What a document calls for at a term's first occurrence in it, as places among the tallies, which
/// are in the order of the short checks, the long checks, then the terms'. A term's checks end where the
/// next term's start.
#[derive(Clone, Copy)]
#[repr(C)]
struct AnchorList {
    short_from: u32,
    long_from: u32,
    own_tally: u32,
}

/// A tally's place in the plan. What each thread has counted is in `Scan::hits`.
struct Tally {
    answer: u32, // The answer it goes into.
    // The last of that answer's ten smallest documents when this last saw them, which only falls, so
    // that the documents past it need no lock. Those fill first, as threads work through the corpus
    // together, and only a thread's own first ten ever get this far.
    limit: AtomicU32,
}

/// The queries, and what the sample decided: how each term is stored, and what to check at each term's
/// occurrences. The threads share this while they scan.
#[derive(Default)]
struct Plan<'a> {
    terms: TermTable<'a>,
    queries: Vec<Query>,
    steps: Vec<u32>,        // All the queries' steps, one query after another.
    rowed_count: usize,     // How many terms have rows...
    sparse_count: usize,    // ...and how many have columns without them.
    phrase_start: Vec<u32>, // A term's phrases are `phrases[phrase_start[t]..phrase_start[t + 1]]`...
    phrases: Vec<u32>, // ...each its query, the anchor's position in it, its length, its terms.
    anchor_lists: Vec<AnchorList>,
    short_checks: Vec<Check<3>>,
    long_checks: Vec<Check<5>>,
    block_steps: Vec<Vec<u32>>, // The steps of the queries that go by blocks, by how many steps they have.
    block_queries: usize,       // How many there are. Their tallies are the last.
    shared_answer: Vec<u32>, // For each query, the answer of the term's tally that adds to it, or NONE.
    // The answers: the queries', in order, then one for each term with a tally, then a spare.
    answers: Vec<Mutex<Answer>>,
    tallies: Vec<Tally>,
}

/// Locks an answer. Only a panicking thread could poison the lock, and a panic aborts the process here,
/// so there's no error to handle.
fn lock(answer: &Mutex<Answer>) -> MutexGuard<'_, Answer> {
    answer.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Puts a document among the smallest of a tally's answer. Returns `TOO_BIG` if it's past the limit, as all
/// the thread's later ones will be too. This is cold beside the check that leads here, and keeping it out
/// of line lets that check be inlined.
#[cold]
fn add_smallest(plan: &Plan, at: usize, doc: u32) -> u64 {
    let tally = &plan.tallies[at];
    if doc >= tally.limit.load(Relaxed) {
        return TOO_BIG;
    }
    let mut answer = lock(&plan.answers[tally.answer as usize]);
    insert_smallest(&mut answer.ids, doc);
    tally.limit.store(answer.ids[SMALLEST - 1], Relaxed);
    0
}

/// How many times each term occurs, among pieces of the corpus 2 MiB in all, about 1%, and in place of
/// term 0, which is no real term, about how many documents those pieces have. A term's occurrences stand
/// in for the documents holding it: this only guides the plan's choices. No term has a row yet, so
/// `look_up` lists every term it finds in `marks`.
fn sample_terms(docs: &[u8], terms: &TermTable, threads: usize) -> Vec<u32> {
    let df: Vec<AtomicU32> = (0..terms.count).map(|_| AtomicU32::new(0)).collect();
    let (next_piece, piece) = (AtomicUsize::new(0), (1 << 21).min(docs.len()) / threads);
    on_threads(threads, || {
        let start = next_piece.fetch_add(1, Relaxed) * (docs.len() / threads);
        let mut lists = DocLists::default();
        for_each_doc(docs, start, start + piece, |_, text, edges| {
            df[0].fetch_add(1, Relaxed);
            let [marked, ..] = look_up(terms, text, edges, &mut lists);
            for word in &lists.marks[..marked] {
                df[(word & TERM_BITS) as usize].fetch_add(1, Relaxed);
            }
        });
    });
    df.into_iter().map(AtomicU32::into_inner).collect()
}

/// How a term is stored, in the order of the numbers terms get.
const ROWED: usize = 0;
const COLUMN_ONLY: usize = 1;
const NEITHER: usize = 2;
/// How far the sample's counts tell the terms with neither apart, when numbering them.
const DF_CAP: u32 = 255;

/// Numbers the terms: those with rows, then the others with columns, then the rest, the commonest first
/// so that what the scan touches most sits together. Puts the numbers in the term table and the steps,
/// and returns them.
fn number_terms(plan: &mut Plan, df: &[u32], store_of: &[usize], flags: &mut [u32]) -> Vec<u32> {
    // One bucket for rows, one for the other columns, and for the rest one per count, the largest first.
    let rank = |t: usize| (DF_CAP - df[t].min(DF_CAP)) as usize;
    let bucket = |t: usize| store_of[t] + store_of[t] / NEITHER * rank(t);
    let keyed = (FIRST_TERM..df.len()).map(|t| (bucket(t) as u32, t));
    let (start, ordered) = group(NEITHER + DF_CAP as usize + 1, keyed);
    (plan.rowed_count, plan.sparse_count) = (start[1] as usize, (start[2] - start[1]) as usize);
    let mut new_number = vec![0; df.len()];
    for (at, &term) in ordered.iter().enumerate() {
        new_number[term] = (FIRST_TERM + at) as u32;
        flags[term] |= new_number[term] | (u32::from(store_of[term] == COLUMN_ONLY) * SPARSE);
    }
    // Empty ways hold term 0, whose word stays 0, so this needs no branch on which ways are in use.
    for word in plan.terms.sets.iter_mut().flat_map(|set| &mut set.words) {
        *word = flags[(*word & TERM_BITS) as usize] | (*word & UNSURE);
    }
    for step in &mut plan.steps {
        *step = (*step & !TERM_BITS) | new_number[(*step & TERM_BITS) as usize];
    }
    plan.terms.plain_top = (FIRST_TERM + plan.rowed_count) as i32 - 1;
    new_number
}

/// Lists each anchor term's phrases, given (term, (query, the anchor's position in it)).
fn list_phrases(plan: &mut Plan, anchored: &[(u32, (u32, u32))], new_number: &[u32]) {
    let keyed = anchored.iter().map(|&(t, p)| (new_number[t as usize], p));
    let (start, phrases) = group(plan.terms.count, keyed);
    // The last range is empty, past the last key, and gives the end of the last term's phrases.
    for range in start.windows(2) {
        plan.phrase_start.push(plan.phrases.len() as u32);
        for &(query, position) in &phrases[range[0] as usize..range[1] as usize] {
            let Query { from, to, .. } = plan.queries[query as usize];
            plan.phrases.extend([query, position, to - from]);
            plan.phrases.extend(&plan.steps[from as usize..to as usize]);
        }
    }
}

/// Decides from the sample how each query is answered and how each term is stored, then lists what to
/// check at each term's occurrences.
fn make_plan(plan: &mut Plan, docs: &[u8], threads: usize) {
    let df = sample_terms(docs, &plan.terms, threads);
    // A query goes by blocks if its anchors hold about N/48 documents in all, where running it on every
    // block starts to cost less than its checks at all those documents. A term in such a query has a row
    // above about N/48, where a byte per occurrence and a bit per document, for every document, start to
    // cost less than listing its first occurrence in each document that holds it. Any choice gives
    // the same answers. `df[0]` is about how many documents the sample has.
    let by_blocks =
        |anchors: &[u32]| anchors.iter().map(|&t| df[t as usize]).sum::<u32>() * 48 >= df[0];
    let rowed = |term: u32| df[term as usize] * 48 >= df[0];
    // How each term is stored, and what its word holds beyond its number.
    let (mut store_of, mut flags) = (vec![NEITHER; df.len()], vec![0; df.len()]);
    // Which of the terms' own tallies each term has, if it has one, and how many there are.
    let (mut term_tally, mut term_tallies) = (vec![NONE; df.len()], 0);
    // Each phrase and its anchor term, each check and its anchor term, and one query's anchors.
    let (mut anchored, mut checks, mut query_anchors) = (vec![], vec![], vec![]);
    let mut block_queries = vec![];
    let query_count = plan.queries.len() as u32;
    plan.shared_answer.resize(query_count as usize, NONE);

    // First, how each query is answered.
    for (query, &Query { is_bool, from, to }) in plan.queries.iter().enumerate() {
        let (query, steps) = (query as u32, &plan.steps[from as usize..to as usize]);
        if !is_bool {
            // Any term of a phrase would be correct as its anchor. The rarest triggers the fewest checks.
            let rarest = (0..steps.len()).min_by_key(|&at| df[steps[at] as usize]);
            let rarest = rarest.unwrap_or(0);
            flags[steps[rarest] as usize] |= PHRASE_ANCHOR;
            anchored.push((steps[rarest], (query, rarest as u32)));
            continue;
        }
        query_anchors.clear();
        anchors(steps, &df, &mut query_anchors);
        if by_blocks(&query_anchors) {
            block_queries.push(query);
            for term in steps.iter().map(|step| step & TERM_BITS) {
                store_of[term as usize] = if rowed(term) { ROWED } else { COLUMN_ONLY };
            }
            continue;
        }
        // After the last AND or NOT, each OR's term alone makes a document match. The commonest
        // of them becomes the first anchor, needing no check: the term's own tally serves every
        // query so anchored, and the other anchors skip the documents that hold it. Those steps have
        // no operator, and each is an anchor.
        let trailing = steps.iter().rev().take_while(|step| is_or(step));
        let sure = trailing.max_by_key(|&&term| df[term as usize]);
        if let Some(&term) = sure {
            query_anchors.sort_by_key(|&anchor| anchor != term);
            flags[term as usize] |= BOOL_ANCHOR;
            // The term's tally if it has one, or else the next.
            let tally = &mut term_tally[term as usize];
            *tally = (*tally).min(term_tallies);
            term_tallies += u32::from(*tally == term_tallies);
            plan.shared_answer[query as usize] = query_count + *tally;
        }
        let skipped = usize::from(sure.is_some());
        for (at, &term) in query_anchors.iter().enumerate().skip(skipped) {
            flags[term as usize] |= BOOL_ANCHOR;
            checks.push((term, (query, make_check(steps, term, &query_anchors[..at]))));
        }
    }

    // Then the terms' numbers, which the rest of the plan is written in.
    let new_number = number_terms(plan, &df, &store_of, &mut flags);
    list_phrases(plan, &anchored, &new_number);

    // Then a term's checks, the short ones of all terms first. A short check holds up to 3 other terms.
    let (term_count, answers) = (plan.terms.count, query_count + term_tallies + 1);
    for (term, (_, check)) in &mut checks {
        *term = new_number[*term as usize] + u32::from(check.terms[3] != 0) * term_count as u32;
        check.terms = check.terms.map(|term| new_number[term as usize]);
    }
    let (starts, checks) = group(2 * term_count, checks.iter().copied());
    let narrow = |&(_, Check { truth, terms }): &(u32, Check<5>)| {
        let terms = [terms[0], terms[1], terms[2]];
        Check { truth, terms }
    };
    let (short, long) = checks.split_at(starts[term_count] as usize);
    plan.short_checks = short.iter().map(narrow).collect();
    plan.long_checks = long.iter().map(|&(_, check)| check).collect();

    // Then the queries that go by blocks, those of the same length together, so that the loop over a
    // query's steps rarely mispredicts its exit. A step there holds a column's number for its term.
    let length = |query: &u32| {
        let Query { from, to, .. } = plan.queries[*query as usize];
        to - from
    };
    let longest = block_queries.iter().map(length).max().unwrap_or(0) as usize;
    let keyed = block_queries.iter().map(|query| (length(query), *query));
    let (_, block_queries) = group(longest + 1, keyed);
    plan.block_steps = vec![vec![]; longest + 1];
    for &query in &block_queries {
        let Query { from, to, .. } = plan.queries[query as usize];
        let steps = plan.steps[from as usize..to as usize].iter();
        let columns = steps.map(|step| step - FIRST_TERM as u32);
        plan.block_steps[(to - from) as usize].extend(columns);
    }
    plan.block_queries = block_queries.len();

    // Then a tally for each check, for each term with one of its own, a spare, and each block query; and
    // the answers.
    let check_queries = checks.iter().map(|&(query, _)| query);
    let tally_answers = check_queries
        .chain(query_count..answers)
        .chain(block_queries);
    let tally = |answer| Tally {
        answer,
        limit: AtomicU32::new(NONE),
    };
    plan.tallies = tally_answers.map(tally).collect();
    let (count, sum, ids) = (0, 0, [NONE; SMALLEST]);
    let blank = |_| Mutex::new(Answer { count, sum, ids });
    plan.answers = (0..answers).map(blank).collect();

    // Last, where each term's checks are, and its own tally, or the spare for a term without one, which
    // nothing reads.
    let list = |t| AnchorList {
        short_from: starts[t],
        long_from: starts[term_count + t],
        own_tally: 0,
    };
    plan.anchor_lists = (0..=term_count).map(list).collect();
    for (term, &new) in new_number.iter().enumerate() {
        plan.anchor_lists[new as usize].own_tally =
            checks.len() as u32 + term_tally[term].min(term_tallies);
    }
}

// ---- The scan: what each thread keeps, and the stages of a document ----

// What the comments and the scan's cache behaviour rely on.
const _: () = assert!(size_of::<TermSet>() == 64);

/// One thread's results, and its list of short checks for the document in hand. A thread claims ranges
/// in document order, so the documents it counts ascend, which is why only its first ten for each tally
/// can be among an answer's smallest.
#[derive(Default)]
struct Scan {
    columns: Vec<Column>, // For each term with one, which documents of the block in hand hold it.
    hits: Vec<u64>, // For each tally: documents, above `SUM_BITS`, and the sum of their ids, below.
    counted: Vec<u32>, // The phrases that the document in hand has counted in.
    short_list: Vec<u32>, // The short checks to run for the document in hand.
}

/// What the stages fill for a document, with room for each of its tokens and `LANES` more.
#[derive(Default)]
struct DocLists {
    tokens: Vec<u32>,    // Every token's word.
    later: Vec<u32>,     // The positions of those to look at again.
    marks: Vec<u32>,     // The words to mark one at a time: terms with flags or with no row.
    phrase_at: Vec<u32>, // The positions of phrase anchors.
    bools: Vec<u32>,     // The terms with BOOL checks, once each.
    sparse: Vec<u32>,    // The terms with columns and no rows, once each.
    plain: Vec<u32>,     // The words of terms with rows and no flags.
    keys: Vec<u64>,      // Every token's short key...
    places: Vec<u64>,    // ...and its set's address.
}

/// Gives every list this much room. It's out of line because measurements say so: inlined with `look_up`,
/// these calls leave its loops with worse registers, for 0.9% more instructions overall.
#[inline(never)]
fn make_room(lists: &mut DocLists, room: usize) {
    lists.tokens.resize(room, 0);
    lists.later.resize(room, 0);
    lists.marks.resize(room, 0);
    lists.phrase_at.resize(room, 0);
    lists.bools.resize(room, 0);
    lists.sparse.resize(room, 0);
    lists.plain.resize(room, 0);
    lists.keys.resize(room, 0);
    lists.places.resize(room, 0);
}

/// How many documents a thread answers the common queries over at a time: a vector's worth.
const BLOCK_DOCS: u32 = 256;

/// A term's bits over a block of documents.
type Column = [u64; 4];

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
/// needs a full look, and few are. Lists the words that are terms, those of terms with rows and no
/// flags apart, and the positions of phrase anchors, and returns how long those lists are. This stage stays
/// out of line, like `mark` and `check_bools`, because measurements say so: inlining this one costs 1.7%
/// of all cycles, and those two 1.9% together.
#[inline(never)]
fn look_up(terms: &TermTable, text: &[u8], edges: &[u32], lists: &mut DocLists) -> [usize; 3] {
    let (sets, count) = (terms.sets.as_ptr() as i64, edges.len() / 2 - LANES);
    if lists.keys.len() < count + LANES {
        make_room(lists, count + LANES);
    }
    let (tokens, later) = (&mut lists.tokens[..], &mut lists.later[..]);
    let (marks, phrase_at) = (&mut lists.marks[..], &mut lists.phrase_at[..]);
    let plain = &mut lists.plain[..];
    let (keys, places) = (&mut lists.keys[..], &mut lists.places[..]);
    let shift = 64 - terms.sets.len().trailing_zeros() as i32;
    // First pass: each token's key, and its set's address, prefetched.
    for at in (0..count).step_by(LANES) {
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
            let lower = _mm256_or_si256(bytes, _mm256_set1_epi8(CASE_BITS as i8));
            let more = _mm256_slli_epi64(_mm256_cmpgt_epi32(lens, _mm256_set1_epi64x(8)), 63);
            let key = _mm256_or_si256(_mm256_and_si256(held, lower), more);
            // A set is 64 bytes.
            let index = _mm256_srl_epi64(hashes(key), _mm_cvtsi32_si128(shift));
            let place = _mm256_add_epi64(_mm256_slli_epi64(index, 6), _mm256_set1_epi64x(sets));
            _mm256_storeu_si256(keys.as_mut_ptr().add(at).cast(), key);
            _mm256_storeu_si256(places.as_mut_ptr().add(at).cast(), place);
            for i in 0..LANES {
                _mm_prefetch(*places.get_unchecked(at + i) as *const i8, _MM_HINT_T0);
            }
        }
    }
    // Second pass: compare the keys, and list the words by what they call for.
    let (mut later_count, mut marked, mut phrases, mut plains) = (0, 0, 0, 0);
    for at in (0..count).step_by(LANES) {
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
            // row and no flags is also below the others. The table's row moves such words to the
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
    // Third pass: the words that may not be the token's, and the positions of phrase anchors.
    for at in later[..later_count].iter().map(|&at| at as usize) {
        // SAFETY: this is a token's position or at most three past the last, for which `tokens` has
        // room, and the other lists have room for every token and those three.
        unsafe {
            let mut word = *tokens.get_unchecked(at);
            // Past the last token, the pairs are empty, whose words are 0 or the unused way's, as `find`
            // returns too, which marks term 0, whose mark means nothing.
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
            phrases += (word >> PHRASE_ANCHOR.trailing_zeros() & 1) as usize;
        }
    }
    [marked, phrases, plains]
}

/// Marks what a document holds, with a byte per term, and lists what to do about it: the terms with BOOL
/// checks or sparse columns, once each. Returns how long the lists are.
#[inline(never)]
fn mark(words: &[u32], present: &mut [u8], bools: &mut [u32], sparse: &mut [u32]) -> [usize; 2] {
    let (mut bool_count, mut sparse_count) = (0, 0);
    for &word in words {
        let term = word & TERM_BITS;
        // SAFETY: `present` has a mark for every term, and each list has room for every token.
        unsafe {
            let held = present.get_unchecked_mut(term as usize);
            let first = usize::from(*held ^ 1);
            *held = 1;
            *bools.get_unchecked_mut(bool_count) = term;
            bool_count += (word >> BOOL_ANCHOR.trailing_zeros() & 1) as usize & first;
            *sparse.get_unchecked_mut(sparse_count) = term;
            sparse_count += (word >> SPARSE.trailing_zeros() & 1) as usize & first;
        }
    }
    [bool_count, sparse_count]
}

/// Keeps which terms with rows a document holds, each 64 of them a word of its row.
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

/// Turns 64 documents' rows into these documents' word of each column, the `word`th, by swapping ever
/// smaller squares across the diagonal. A last row's marks of terms past those with rows have no column to
/// go to.
fn store_rows(rows: &mut [[u64; 64]], columns: &mut [Column], word: usize) {
    for (index, row) in rows.iter_mut().enumerate() {
        for width in [32, 16, 8, 4, 2, 1] {
            let low_bits = !0u64 / ((1 << width) + 1);
            for square in row.chunks_exact_mut(2 * width) {
                let (first, second) = square.split_at_mut(width);
                for (a, b) in first.iter_mut().zip(second) {
                    let swapped = (*a >> width ^ *b) & low_bits;
                    (*a, *b) = (*a ^ swapped << width, *b ^ swapped);
                }
            }
        }
        for (&bits, column) in row.iter().zip(&mut columns[index * 64..]) {
            column[word] = bits;
        }
        *row = [0; 64];
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
                let mut answer = lock(&plan.answers[*query as usize]);
                (answer.count, answer.sum) = (answer.count + 1, answer.sum + u64::from(doc));
                insert_smallest(&mut answer.ids, doc);
            }
        }
    }
}

/// Counts a document at these anchors of it, for each term's own tally and each check that says so.
/// Prefetching first lets cache misses overlap, and listing the short checks before running them
/// replaces an unpredictable loop exit per anchor by one loop.
#[inline(never)]
fn check_bools(anchors: &[u32], present: &[u8], plan: &Plan, doc: u32, scan: &mut Scan) {
    let (hits, short) = (&mut scan.hits[..], &mut scan.short_list[..]);
    let (checks, short_hits) = (plan.short_checks.as_ptr(), hits.as_ptr());
    let (lists, this_doc) = (&plan.anchor_lists[..], (1 << SUM_BITS) + u64::from(doc));
    // SAFETY: `hits` has every tally. A thread's hits ascend, so only its first ten can be among the
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
            let list = *lists.get_unchecked(term as usize);
            let next = *lists.get_unchecked(term as usize + 1);
            let (from, to) = (list.short_from, next.short_from);
            prefetch(checks.wrapping_add(from as usize).cast());
            prefetch(short_hits.wrapping_add(from as usize).cast());
            // One store lists four of the term's short checks, which is enough for most, whatever
            // `listed` then keeps of them, and the loop lists any more.
            let at = _mm_add_epi32(_mm_set1_epi32(from as i32), _mm_setr_epi32(0, 1, 2, 3));
            _mm_storeu_si128(short.as_mut_ptr().add(listed).cast(), at);
            for at in from + 4..to {
                short[listed + (at - from) as usize] = at;
            }
            listed += (to - from) as usize;
            add(1, list.own_tally as usize);
            for at in list.long_from as usize..next.long_from as usize {
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

/// What a step does to the set so far with its term's set, by its operator: flips that set, ands, then ors
/// that set in if asked. The step's top two bits index this: OR, AND, NOT.
#[repr(align(32))]
struct StepMasks([[u64; 8]; 3]);
static STEP_MASKS: StepMasks = StepMasks([
    [!0, !0, !0, !0, !0, !0, !0, !0],
    [0; 8],
    [!0, !0, !0, !0, 0, 0, 0, 0],
]);

/// Puts documents of a block's result among the smallest of a tally's answer, up to the first past its limit.
#[cold]
fn add_smallest_of_block(plan: &Plan, at: usize, set: __m256i, first_doc: u32) -> u64 {
    // SAFETY: a vector has the layout of four words.
    let words = unsafe { std::mem::transmute::<__m256i, Column>(set) };
    for (word, &bits) in words.iter().enumerate() {
        let mut rest = bits;
        while rest != 0 {
            let doc = first_doc + 64 * word as u32 + rest.trailing_zeros();
            if add_smallest(plan, at, doc) != 0 {
                return TOO_BIG;
            }
            rest &= rest - 1;
        }
    }
    0
}

/// Runs each query that goes by blocks over one block of documents, in vectors, and adds what it finds to
/// the query's tally. Each query's steps apply in order from the first term's set. Then each nibble's count
/// of set bits and sum of their positions are table lookups, so a byte gets them at once. A position is the
/// word's first document, plus 8 times the byte's number in the word, plus its place in the byte. The
/// bytes' numbers times their counts stay below 256 for each pair of bytes, so that SAD can add those too.
#[inline(never)]
fn answer_block(plan: &Plan, columns: &[Column], hits: &mut [u64], first_doc: u32) {
    let mut at = plan.tallies.len() - plan.block_queries;
    // SAFETY: a step's number is a column's, its top two bits index `STEP_MASKS`, the tallies include `at`,
    // and the build targets x86-64-v3, which has AVX2.
    unsafe {
        let table = |t: [u8; 16]| _mm256_broadcastsi128_si256(_mm_loadu_si128(t.as_ptr().cast()));
        let count_of = table([0, 1, 1, 2, 1, 2, 2, 3, 1, 2, 2, 3, 2, 3, 3, 4]);
        let low_sum = table([0, 0, 1, 1, 2, 2, 3, 3, 3, 3, 4, 4, 5, 5, 6, 6]);
        let high_sum = table([0, 4, 5, 9, 6, 10, 11, 15, 7, 11, 12, 16, 13, 17, 18, 22]);
        let (nibble, zero) = (_mm256_set1_epi8(15), _mm256_setzero_si256());
        let look = |table, at| _mm256_shuffle_epi8(table, at);
        let byte_numbers = _mm256_set1_epi64x(0x0706_0504_0302_0100);
        let doc = i64::from(first_doc);
        let starts = _mm256_setr_epi64x(doc, doc + 64, doc + 128, doc + 192);
        let column = |step: u32| {
            let at = (step & TERM_BITS) as usize;
            _mm256_loadu_si256(columns.as_ptr().add(at).cast())
        };
        for (length, steps) in plan.block_steps.iter().enumerate().skip(1) {
            for query in steps.chunks_exact(length) {
                let mut set = column(*query.get_unchecked(0));
                for &step in query.get_unchecked(1..) {
                    let with = column(step);
                    let masks = STEP_MASKS
                        .0
                        .as_ptr()
                        .add((step >> 30) as usize)
                        .cast::<__m256i>();
                    let kept = _mm256_xor_si256(with, _mm256_load_si256(masks));
                    let added = _mm256_and_si256(with, _mm256_load_si256(masks.add(1)));
                    set = _mm256_or_si256(_mm256_and_si256(set, kept), added);
                }
                let low = _mm256_and_si256(set, nibble);
                let high = _mm256_and_si256(_mm256_srli_epi16(set, 4), nibble);
                let ones = _mm256_add_epi8(look(count_of, low), look(count_of, high));
                let places = _mm256_add_epi8(look(low_sum, low), look(high_sum, high));
                let in_word = _mm256_sad_epu8(ones, zero);
                let by_pair = _mm256_maddubs_epi16(ones, byte_numbers);
                let sums = _mm256_add_epi64(
                    _mm256_add_epi64(
                        _mm256_sad_epu8(places, zero),
                        _mm256_mul_epu32(in_word, starts),
                    ),
                    _mm256_slli_epi64(_mm256_sad_epu8(by_pair, zero), 3),
                );
                let found = _mm256_add_epi64(sums, _mm256_slli_epi64(in_word, SUM_BITS as i32));
                let half = _mm_add_epi64(
                    _mm256_castsi256_si128(found),
                    _mm256_extracti128_si256(found, 1),
                );
                let found =
                    _mm_cvtsi128_si64(_mm_add_epi64(half, _mm_unpackhi_epi64(half, half))) as u64;
                let hit = hits.get_unchecked_mut(at);
                // Fewer than ten so far, and more now, in a way the compiler can't turn into a coin-toss
                // branch. A thread's hits ascend, so only its first ten can be among the smallest.
                let open = (*hit).wrapping_sub((SMALLEST as u64) << SUM_BITS) >> 63;
                if open & u64::from(found != 0) != 0 {
                    *hit |= add_smallest_of_block(plan, at, set, first_doc);
                }
                *hit += found;
                at += 1;
            }
        }
    }
}

/// Finishes a block: the last row if it's part full, its queries, and clean columns for the next.
fn finish_block(plan: &Plan, scan: &mut Scan, rows: &mut [[u64; 64]], first_doc: u32, docs: u32) {
    if docs % 64 != 0 {
        store_rows(
            rows,
            &mut scan.columns[..plan.rowed_count],
            docs as usize / 64,
        );
    }
    answer_block(plan, &scan.columns, &mut scan.hits, first_doc);
    scan.columns.fill([0; 4]);
}

/// Claims the next `size` of the numbers below `len`, if any are left.
fn claim(next: &AtomicUsize, size: usize, len: usize) -> Option<std::ops::Range<usize>> {
    let from = next.fetch_add(size, Relaxed);
    (from < len).then(|| from..(from + size).min(len))
}

/// Claims ranges of the corpus until none is left, and scans them.
fn scan_ranges(docs: &[u8], plan: &Plan, range_size: usize, next_range: &AtomicUsize) -> Scan {
    let mut scan = Scan::default();
    scan.hits.resize(plan.tallies.len(), 0);
    scan.columns = vec![[0; 4]; plan.rowed_count + plan.sparse_count];
    scan.short_list = vec![0; plan.short_checks.len() + 4];
    // With a row's worth more, as `set_row` reads whole rows of marks.
    let mut present = vec![0u8; plan.terms.count + 64];
    let sparse_from = FIRST_TERM + plan.rowed_count;
    let mut lists = DocLists::default();
    let mut rows = vec![[0; 64]; plan.rowed_count.div_ceil(64)];
    while let Some(range) = claim(next_range, range_size, docs.len()) {
        let (mut first_doc, mut in_block) = (0, 0);
        for_each_doc(docs, range.start, range.end, |doc, text, edges| {
            first_doc = if in_block == 0 { doc } else { first_doc };
            let [marked, phrases, plains] = look_up(&plan.terms, text, edges, &mut lists);
            for &word in &lists.plain[..plains] {
                // SAFETY: `present` has a mark for every term, and these words have no flags.
                unsafe { *present.get_unchecked_mut(word as usize) = 1 };
            }
            let in_doc = &lists.tokens[..edges.len() / 2 - LANES];
            let marked = &lists.marks[..marked];
            let [bools, sparse] = mark(marked, &mut present, &mut lists.bools, &mut lists.sparse);
            set_row(&present[FIRST_TERM..], &mut rows, in_block);
            in_block += 1;
            let word = (in_block - 1) as usize / 64;
            if in_block % 64 == 0 {
                store_rows(&mut rows, &mut scan.columns[..plan.rowed_count], word);
            }
            for &term in &lists.sparse[..sparse] {
                // SAFETY: a term with this list's flag has a column, and a block has four words.
                unsafe {
                    let column = scan.columns.get_unchecked_mut(term as usize - FIRST_TERM);
                    *column.get_unchecked_mut(word) |= 1 << ((in_block - 1) % 64);
                }
            }
            let ats = &lists.phrase_at[..phrases];
            check_phrases(ats, in_doc, plan, doc, &mut scan.counted);
            check_bools(&lists.bools[..bools], &present, plan, doc, &mut scan);
            for &word in marked {
                // SAFETY: `present` has a mark for every term.
                unsafe { *present.get_unchecked_mut((word & TERM_BITS) as usize) = 0 };
            }
            present[FIRST_TERM..sparse_from].fill(0);
            if in_block == BLOCK_DOCS {
                finish_block(plan, &mut scan, &mut rows, first_doc, in_block);
                in_block = 0;
            }
        });
        if in_block != 0 {
            finish_block(plan, &mut scan, &mut rows, first_doc, in_block);
        }
    }
    scan
}

// ---- After the scan: the output ----

/// Prints a query's line: each number with a space after it, the last of which becomes the line's end.
fn print_line(out: &mut Vec<u8>, answer: &Answer) {
    let Answer { count, sum, ids } = answer;
    let docs = ids.iter().take(*count as usize).map(|&d| u64::from(d));
    for value in [*count, *sum].into_iter().chain(docs) {
        // The 20 digits of the largest u64, then the space.
        let (mut digits, mut start) = ([b' '; 21], 20);
        for value in std::iter::successors(Some(value), |&v| (v >= 10).then(|| v / 10)) {
            start -= 1;
            digits[start] = b'0' + (value % 10) as u8;
        }
        out.extend_from_slice(&digits[start..]);
    }
    *out.last_mut().expect("a line has numbers") = b'\n';
}

/// How far each job that the threads share has got: the next of each to claim, and how far the file has
/// got.
#[derive(Default)]
struct Progress {
    next_range: AtomicUsize,     // In bytes of the corpus.
    next_scan_slot: AtomicUsize, // In `Shared::scans`.
    next_total: AtomicUsize,     // The next tally to add up.
    next_batch: AtomicUsize,     // In queries, as is...
    written: AtomicUsize,        // ...how many of them have reached the file.
}

/// What the threads of the scan and answer round share.
#[repr(C)]
struct Shared {
    progress: Progress,
    barrier: Barrier,           // Between phases.
    scans: Vec<OnceLock<Scan>>, // A slot for each thread's scan.
}

/// One thread's part of the scan and the answers. Its scan goes into its slot, for every thread to read
/// once all are done. Then the totals, before any printing. Then threads claim batches of queries in order,
/// and each waits for the batch before it to reach the file. The wait is short, because batches cost about
/// the same.
fn scan_and_answer(docs: &[u8], plan: &Plan, mut file: &File, shared: &Shared) {
    let (progress, barrier) = (&shared.progress, &shared.barrier);
    // 48 ranges per thread: enough that a thread slowed for a while holds no one up, and that all
    // finish within a small part of the scan.
    let range_size = docs.len() / (shared.scans.len() * 48) + 1;
    let scan = scan_ranges(docs, plan, range_size, &progress.next_range);
    let slot = progress.next_scan_slot.fetch_add(1, Relaxed);
    assert!(shared.scans[slot].set(scan).is_ok(), "slot taken twice");
    barrier.wait();
    let scan_in = |slot| OnceLock::get(slot).expect("every scan is stored by now");
    let scans: Vec<&Scan> = shared.scans.iter().map(scan_in).collect();

    // Adds the threads' counts and ids of each tally into its answer. All threads' hits for a tally are
    // summed before its answer is locked, so an answer is visited once per tally, not once per thread as
    // well: they are many cache lines, and each thread's hits are read in order. Threads claim parts of
    // this.
    let next_tallies = || claim(&progress.next_total, BATCH, plan.tallies.len());
    for at in std::iter::from_fn(next_tallies).flatten() {
        let hits: u64 = scans.iter().map(|scan| scan.hits[at] & !TOO_BIG).sum();
        let mut answer = lock(&plan.answers[plan.tallies[at].answer as usize]);
        answer.count += hits >> SUM_BITS;
        answer.sum += hits & ((1 << SUM_BITS) - 1);
    }

    barrier.wait();

    // Printing, each query's answer with what its term's tally adds to it.
    let mut text = Vec::new();
    while let Some(batch) = claim(&progress.next_batch, BATCH, plan.queries.len()) {
        text.clear();
        for query in batch.clone() {
            let mut line = *lock(&plan.answers[query]);
            // NONE is no answer's number.
            if let Some(term_answer) = plan.answers.get(plan.shared_answer[query] as usize) {
                let Answer { count, sum, ids } = *lock(term_answer);
                (line.count, line.sum) = (line.count + count, line.sum + sum);
                ids.iter().for_each(|&d| insert_smallest(&mut line.ids, d));
            }
            print_line(&mut text, &line);
        }
        while progress.written.load(Acquire) != batch.start {
            thread::yield_now();
        }
        // `panic = "abort"`: a failed write ends the process rather than leaving the other
        // threads waiting for their turn.
        file.write_all(&text).expect("writing the results");
        progress.written.store(batch.end, Release);
    }
}

/// `#![no_main]`: without std's runtime setup, which reads /proc/self/maps and gives every thread a
/// signal stack (an mmap, an mprotect and a sigaltstack each, and again at exit). `env::args` still works.
#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    // All of the heap comes from one contiguous region, grown a lot at a time, so that the kernel can
    // back it with huge pages, in far fewer page faults than threads' arenas and small mappings take.
    // These are M_ARENA_MAX (1), M_MMAP_THRESHOLD (1 GiB) and M_TOP_PAD (64 MiB).
    let (arenas, mmap_above, top_pad) = (-8, -3, -2);
    for (param, value) in [(arenas, 1), (mmap_above, 1 << 30), (top_pad, 1 << 26)] {
        // SAFETY: no thread has started, and these only tune the allocator.
        unsafe { mallopt(param, value) };
    }
    let arg = |at, default: &str| std::env::args().nth(at).unwrap_or(default.into());
    let (data, out_dir) = (arg(1, "/data"), arg(2, "/out"));
    // `SD_THREADS` if set (the test sweep uses it), else what the container allows, capped at the
    // box's 12 threads.
    let asked = std::env::var("SD_THREADS").map_or(0, |v| v.parse().unwrap_or(0));
    let allowed = thread::available_parallelism().map_or(1, |n| n.get().min(12));
    let threads = if asked > 0 { asked } else { allowed };
    let (docs, queries) = (map_file(&data, "docs.txt"), map_file(&data, "queries.txt"));
    // Sized for the spec's 50,000 queries, whose text holds about one distinct term per 23 bytes: at
    // `TERMS_PER_SET`, the table has room for all of their 75,000 or so.
    let mut plan = parse_queries(queries, threads, 1 << 16);
    make_plan(&mut plan, docs, threads);
    // The last document's id starts the last line, which trimming only ever shortens past the id.
    let last_line = docs.trim_ascii_end().rsplit(|&byte| byte == b'\n').next();
    let last_line = last_line.unwrap_or_default();
    let digits = last_line.iter().take_while(|&&byte| byte != b'\t');
    let last_doc = digits.fold(0, |id, &digit| id * 10 + u32::from(digit - b'0'));
    // The ids sum to under N * N / 2, which `SUM_BITS` must hold.
    assert!((last_doc as usize) < 1 << (SUM_BITS / 2), "too many ids");
    let file = File::create(format!("{out_dir}/results.txt")).expect("creating results.txt");
    // One round of threads scans, then answers, as starting threads again costs more than a barrier.
    let shared = Shared {
        progress: Progress::default(),
        barrier: Barrier::new(threads),
        scans: (0..threads).map(|_| OnceLock::new()).collect(),
    };
    on_threads(threads, || scan_and_answer(docs, &plan, &file, &shared));
    // Nothing is buffered, and the kernel frees all the memory faster than dropping it would.
    std::process::exit(0)
}
