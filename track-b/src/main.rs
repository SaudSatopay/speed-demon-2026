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
//!    term of such queries, and runs them in vectors when the block is full. A common term that alone makes
//!    a query match has just its own count run so, and the query's other anchors stay rare. A phrase is
//!    anchored on its rarest term.
//! 3. **Scan** (`scan_and_answer`). Threads claim ranges of the corpus in order and take a document at a
//!    time, in stages without a branch per token: `token_edges` finds the tokens, `look_up` finds their
//!    terms and marks most, `mark` the rest, and the later stages act on what those listed:
//!    `check_phrases` tries each phrase around its anchor's occurrences once the document's tokens are
//!    known, `check_bools` runs the truth tables, and `store_rows` and `answer_block` fill the columns and
//!    run the block queries.
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
//! - A *mark* is a byte per term, equal to the document in hand's stamp if it holds the term.
//! - A *check* is a BOOL query's truth table at one of its anchors.
//! - An *answer* is what a query's line prints. A *tally* is where each thread adds up the documents
//!   for one check, or for one term alone, before they go into an answer.
//! - A *block* is up to 255 documents in a row, which one thread takes together. Each has its own *stamp*,
//!   its number in the block from 1, so that its marks need no clearing: a block's end clears them all.
//! - A *column* is a term's bits over a block: which of its documents hold the term.
//! - A *row* is one document's marks of the commonest terms, as bits, which become their columns.

#![no_main]

use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering::*};
use std::{arch::x86_64::*, fs::File, io::IoSlice, io::Write, os::fd::AsRawFd, thread};

/// Term 0 is every token that no query uses, and the empty ways of the term table hold it. A truth
/// table doesn't depend on a slot that no term took, so this term also fills the unused slots of a
/// BOOL check, whatever its mark says. Real terms start here: those with rows first, then the others with
/// columns, then the rest.
const FIRST_TERM: usize = 1;
/// The low bits of a term table's word are its term's number...
const TERM_BITS: u32 = (1 << 24) - 1;
/// ...an `UNSURE` one says here which way's long key is its token's to compare, counted from the word of
/// no way, which matches no token...
const WAY_SHIFT: u32 = 26;
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
/// batches (49 at full) that a call to write takes them all at once, and each batch's text is about 30 KB.
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
    fn madvise(at: *const u8, len: usize, advice: i32) -> i32;
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
        // A thread takes long enough over a document that the hardware's prefetch rarely has the next one's
        // lines ready, so each block prefetches the line 16 blocks on.
        // SAFETY: a prefetch has no other effect, whatever the address.
        let ahead = file.as_ptr().wrapping_add(text + block + 1024);
        unsafe { _mm_prefetch(ahead.cast(), _MM_HINT_T0) };
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
        let held = (len as u8 as i8).clamp(0, 8) as u32;
        let short = _bzhi_u64(short, 8 * held) | u64::from(len > 8) << 63;
        let long = _bzhi_u32(long, 8 * len.saturating_sub(8) as u32) | u32::from(len > 12) << 31;
        (short, long)
    }
}

/// The hash of four short keys at once, whose low half's top bits say which of the term table's sets, a
/// power of two of them, a key starts at. One multiply mixes the key, as those bits depend on every input
/// bit. AVX2 only has 32-bit multiplies, so the key is first folded into 32 bits, at an offset that puts its
/// last bytes' varying bits on its first bytes' constant ones. A test build swaps it for a constant, so that
/// every key collides and any logic that trusts a hash without comparing the key shows up as a wrong answer.
fn hashes(keys: __m256i) -> __m256i {
    // SAFETY: the build targets x86-64-v3, which has AVX2.
    unsafe {
        // The multiplier is from the golden ratio.
        let folded = _mm256_xor_si256(keys, _mm256_srli_epi64(keys, 27));
        let hash = _mm256_mul_epu32(folded, _mm256_set1_epi64x(0x9E37_79B1));
        let zero = _mm256_setzero_si256();
        if cfg!(sd_const_hash) { zero } else { hash }
    }
}

/// Where a set's `rest` holds the word of no way, which comes just past the ways' words...
const NO_WAY: usize = 4;
/// The top bit of a long key, on a term longer than the keys. The rest of it says where the term is in
/// the query file.
const LONG: u32 = 1 << 31;
/// How many terms a table takes for each of its sets, whose ways with long keys are more than that, so that
/// a probe always comes to a set with room.
const TERMS_PER_SET: usize = 2;

/// Four terms in one cache line, whose keys are compared at once, so a lookup has no branch on which of
/// them it found, or whether it found one. The word of no way is what a lookup that finds none gets: 0 (no
/// query uses this token), or `UNSURE` if the probe goes on. While the queries are read, its low bit is the
/// set's lock. Only the last three ways have long keys, and terms that need one fill those in order, from
/// the last. The others take the first way free, so the first way's long key is always 0.
#[repr(align(64))]
struct TermSet {
    shorts: [u64; 4], // Short keys, 0 in the ways not used.
    rest: [u32; 8],   // The ways' words, last way first, the word of no way, then the long keys.
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
            let keys = _mm_load_si128(self.rest.as_ptr().add(NO_WAY).cast());
            let same = _mm_cmpeq_epi32(keys, _mm_set1_epi32(long_key as i32));
            // The word of no way comes first, in the place of the first way's key, which is always 0.
            _mm_movemask_ps(_mm_castsi128_ps(same)) as u32 & 14 | u32::from(long_key == 0)
        }
    }

    /// The word of the last of these ways, or else of no way: a count of zeros above four bits of ways.
    fn word(&self, ways: u32) -> u32 {
        // SAFETY: `ways` has four bits, so there are 28 to 32 of those zeros.
        unsafe { *self.rest.get_unchecked(ways.leading_zeros() as usize - 28) }
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
}

impl TermTable<'_> {
    /// The word of the term of this token, in any letter case, or else 0, from a probe that starts at
    /// this set. A set that no term was placed past ends a probe.
    #[cold]
    fn find<'s>(&'s self, token: &[u8], short: u64, mut set: &'s TermSet) -> u32 {
        loop {
            let ways = self.ways_of(set, token, short, keys_of(token).1);
            // One test, not two: that this finds a term or not is a coin toss.
            if ways | u32::from(set.rest[NO_WAY] == 0) != 0 {
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
            let long = set.rest[NO_WAY + *way as usize];
            let term = self.text.get((long & !LONG) as usize..).unwrap_or(&[]);
            ways >> way & long >> 31 != 0
                && token.eq_ignore_ascii_case(term.get(..end).unwrap_or(&[]))
                && !term.get(end).is_some_and(u8::is_ascii_alphanumeric)
        };
        (1..4).filter(is_token).map(|way| 1 << way).sum()
    }

    /// Adds this query term, if it's new, and returns its number. With no room left in the table, that
    /// number is too big for it, and `parse_queries` reads the queries again into a larger one. Threads do
    /// this at once: each holds the lock of the set it's looking at. So threads adding one term get the same number, as sets only fill, and the first set with
    /// room is where it goes. A term that's in already is found without the lock, as a way's key is stored
    /// after its word, so a key that's there has its word: most of the tokens are common terms, which
    /// every thread meets.
    fn intern(&self, token: &[u8], next_term: &AtomicUsize, own: &mut usize) -> u32 {
        let (short, long) = keys_of(token);
        // SAFETY: the build targets x86-64-v3, which has AVX2.
        let hash = unsafe { _mm256_extract_epi64::<0>(hashes(_mm256_set1_epi64x(short as i64))) };
        let top = hash as usize >> (32 - self.sets.len().trailing_zeros());
        let mut index = top & (self.sets.len() - 1);
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
                let lock = AtomicU32::from_ptr(&raw mut (*set).rest[NO_WAY]);
                let mut no_way = lock.fetch_or(1, Acquire);
                while no_way & 1 != 0 {
                    no_way = lock.fetch_or(1, Acquire);
                }
                let set = &mut *set;
                let ways = self.ways_of(set, token, short, long);
                // A token past the short key has more to compare.
                let has_more = token.len() > 8;
                let free = set.ways(0) & if has_more { 14 } else { 15 };
                let (mut term, room) = (set.word(ways) & TERM_BITS, free != 0);
                if ways == 0 && room {
                    // A thread takes numbers in batches, as that shared counter is the parse's busiest line.
                    if *own % TERM_BATCH == 0 {
                        *own = next_term.fetch_add(TERM_BATCH, Relaxed);
                    }
                    term = *own as u32;
                    *own += 1;
                    if (term as usize) < FIRST_TERM + TERMS_PER_SET * self.sets.len() {
                        let way = [free.trailing_zeros(), free.ilog2()][has_more as usize] as usize;
                        let at = (token.as_ptr() as usize - self.text.as_ptr() as usize) as u32;
                        if has_more {
                            let key = if long & LONG != 0 { LONG | at } else { long };
                            set.rest[NO_WAY + way] = key;
                        }
                        let more = UNSURE | (way as u32) << WAY_SHIFT;
                        set.rest[NO_WAY - 1 - way] = term | u32::from(has_more) * more;
                        AtomicU64::from_ptr(&raw mut set.shorts[way]).store(short, Release);
                    }
                }
                // A lookup that finds nothing in a set this term has passed has to look further.
                lock.store(no_way | u32::from(ways == 0 && !room) * UNSURE, Release);
                if ways != 0 || room {
                    return term;
                }
            }
            index = (index + 1) & (self.sets.len() - 1);
        }
    }
}

/// A thread takes term numbers up to the next multiple of this at a time. Those left unused only make the
/// table seem fuller.
const TERM_BATCH: usize = 64;

/// Reads the queries into a new plan, whose term table has this many sets: each thread the lines that
/// start in its part of the text.
fn parse_queries(text: &[u8], threads: usize, sets: usize) -> Plan<'_> {
    let mut plan = Plan::default();
    // Empty sets, from the kernel's zeros as threads first touch them. Threads that fault in the same huge
    // page each allocate one, which the memory peak counts, but below 44 MB that scores nothing.
    // SAFETY: zeros are a valid TermSet.
    let empty = unsafe { Box::<[TermSet]>::new_zeroed_slice(sets).assume_init() };
    (plan.terms.text, plan.terms.sets) = (text, empty.into_vec());
    let (next_part, next_term) = (AtomicUsize::new(0), AtomicUsize::new(FIRST_TERM));
    let mut parts = on_threads(threads, || {
        let part = claim(&next_part, text.len() / threads + 1, text.len()).unwrap_or_default();
        let (mut queries, mut steps, mut edges, mut own) = (vec![], vec![], vec![], 0);
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
                    steps.push(op | plan.terms.intern(token, &next_term, &mut own));
                }
            }
            pos = line_feed + 1;
            let to = steps.len() as u32;
            queries.push(Query(is_bool, from, to));
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
        let moved = |q: &mut Query| (q.1, q.2) = (base + q.1, base + q.2);
        queries.iter_mut().for_each(moved);
        plan.queries.extend(queries);
    }
    plan
}

/// `look_up` takes this many tokens at a time for their keys, and twice this for their words.
const LANES: usize = 4;

/// Visits the documents that start in the range `start..end` of the corpus, passing each one's id, its text
/// and what follows, and its tokens' edges in that, then `2 * LANES` empty pairs of edges, so that `look_up`
/// can read that many pairs at a time.
fn for_each_doc(docs: &[u8], start: usize, end: usize, mut visit: impl FnMut(u32, &[u8], &[u32])) {
    let mut line = start;
    while line > 0 && line < end && docs[line - 1] != b'\n' {
        line += 1;
    }
    let digits = docs[line..].iter().take_while(|&&byte| byte != b'\t');
    let mut id = digits.fold(0, |id, &digit| id * 10 + u32::from(digit - b'0'));
    let mut edges = Vec::new();
    while line < end {
        // The first token is the id, and the ids go up by one.
        let next = token_edges(docs, line, &mut edges) + 1;
        edges.extend([0; 4 * LANES]);
        visit(id, &docs[line..], &edges[2..]);
        (line, id) = (next, id + 1);
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
struct Check {
    truth: u32,
    terms: [u32; 5], // The other terms, then term 0.
}

/// A check of up to three other terms fits in one word: the low bits of its truth table, then those terms
/// here, if their numbers fit. The rest go as long checks.
const PACKED_AT: [u32; 3] = [8, 26, 44];
const PACKED_MAX: u32 = (1 << 18) - 1;

/// 1 if a document with this stamp, holding the terms marked in `present`, counts at this packed check.
fn counts_packed(check: u64, present: &[u8], stamp: __m128i) -> u64 {
    // SAFETY: `present` has a mark for every term, and the build targets x86-64-v3, which has AVX.
    unsafe {
        let term = |slot: usize| (check >> PACKED_AT[slot]) as usize & PACKED_MAX as usize;
        let mark = |slot| i32::from(*present.get_unchecked(term(slot)));
        let marks = _mm_insert_epi8::<1>(_mm_cvtsi32_si128(mark(0)), mark(1));
        let marks = _mm_insert_epi8::<2>(marks, mark(2));
        // A stamp isn't 0, which fills the rest of the vector.
        let holds = _mm_movemask_epi8(_mm_cmpeq_epi8(marks, stamp));
        u64::from(check as u32 >> holds & 1)
    }
}

/// 1 if a document with this stamp, holding the terms marked in `present`, counts at this check, else 0.
fn counts(check: &Check, present: &[u8], stamp: __m128i) -> u64 {
    // SAFETY: `present` has a mark for every term, and the build targets x86-64-v3, which has AVX.
    unsafe {
        let mark = |slot| i32::from(*present.get_unchecked(check.terms[slot] as usize));
        let marks = _mm_insert_epi8::<1>(_mm_cvtsi32_si128(mark(0)), mark(1));
        let marks = _mm_insert_epi8::<2>(marks, mark(2));
        let marks = _mm_insert_epi8::<4>(_mm_insert_epi8::<3>(marks, mark(3)), mark(4));
        // A stamp isn't 0, which fills the rest of the vector.
        let holds = _mm_movemask_epi8(_mm_cmpeq_epi8(marks, stamp));
        u64::from(check.truth >> holds & 1)
    }
}

/// The check of a query with these steps at the given anchor, after the `earlier` ones. Each term's set
/// is a mask over the 64 possible documents, so the steps give the truth table directly, and its top half
/// is what's left with the last term, the anchor, present.
fn make_check(steps: &[u32], anchor: u32, earlier: &[u32]) -> Check {
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
        // What the step makes of the set so far, given its term's set, as masks.
        table = match step & !TERM_BITS {
            AND => table & index_bits[slot],
            NOT => table & !index_bits[slot],
            _ => table | index_bits[slot],
        };
        blocked |= u64::from(earlier.contains(&term)) * index_bits[slot];
    }
    let truth = ((table & !blocked) >> 32) as u32;
    let terms = *terms.first_chunk().expect("six slots");
    Check { truth, terms }
}

/// Sorts items by their keys, below `keys`. Returns where each key's items start, then the number of items
/// twice, and the items, which stay in order within a key. A key's count goes two entries on, the sums make
/// that its start one entry on, and placing its items moves that to the next key's start.
fn group<T: Copy>(keys: usize, all: impl Iterator<Item = (u32, T)>) -> (Vec<u32>, Vec<T>) {
    let (mut from, all) = (vec![0; keys + 2], all.collect::<Vec<_>>());
    all.iter().for_each(|&(key, _)| from[key as usize + 2] += 1);
    (0..keys).for_each(|key| from[key + 2] += from[key + 1]);
    let mut grouped: Vec<T> = all.iter().map(|item| item.1).collect();
    for (key, item) in all {
        grouped[from[key as usize + 1] as usize] = item;
        from[key as usize + 1] += 1;
    }
    (from, grouped)
}

/// What a query's line prints, or what a term's tally adds to it.
#[derive(Clone, Copy)]
struct Answer {
    count: u64,           // How many documents match.
    sum: u64,             // The sum of their ids.
    ids: [u32; SMALLEST], // The smallest ids, ascending, then NONE.
}

/// An answer in a cache line of its own, which its lock and all it holds fit, as threads take it at once.
#[repr(align(64))]
struct Slot(Mutex<Answer>);

/// A query: whether it's a BOOL rather than a phrase, and the range of its steps.
struct Query(bool, u32, u32);

/// What a document calls for at a term's first occurrence in it, as places among the tallies, which
/// are in the order of the short checks, the long checks, then the terms'. A term's checks end where the
/// next term's start.
#[derive(Clone, Copy)]
struct AnchorList(u32, u32, u32); // Where its short checks start, where its long ones do, and its own tally.

/// A tally's place in the plan: the answer it goes into, and the last of that answer's ten smallest
/// documents when this last saw them, which only falls, so that the documents past it need no lock. Those
/// fill first, as threads work through the corpus together, and only a thread's own first ten ever get this
/// far. What each thread has counted is in `Scan::hits`.
struct Tally(u32, AtomicU32);

/// The queries, and what the sample decided: how each term is stored, and what to check at each term's
/// occurrences. The threads share this while they scan.
#[derive(Default)]
struct Plan<'a> {
    terms: TermTable<'a>,
    queries: Vec<Query>,
    steps: Vec<u32>,     // All the queries' steps, one query after another.
    range_size: usize,   // How many bytes of the corpus a thread claims at a time.
    rowed_count: usize,  // How many terms have rows...
    sparse_count: usize, // ...and how many have columns without them.
    // A term's phrases are `phrases[phrase_start[t].0..phrase_start[t + 1].0]`. If it's just one, of two or more
    // terms, the other number is the term after its anchor, or before it with the top bit set. Else it's 0.
    phrase_start: Vec<(u32, u32)>,
    phrases: Vec<u32>, // Each its query, the anchor's position in it, its length, its terms.
    anchor_lists: Vec<AnchorList>,
    short_checks: Vec<u64>,
    long_checks: Vec<Check>,
    block_steps: Vec<Vec<u32>>, // What goes by blocks, by how many steps: common terms alone, then queries.
    block_queries: usize,       // How many of those there are. Their tallies are the last.
    shared_answer: Vec<u32>, // For each query, the answer of the term's tally that adds to it, or NONE.
    // The answers: the queries', in order, then one for each term with a tally, then a spare.
    answers: Vec<Slot>,
    tallies: Vec<Tally>,
}

/// Puts a document among the smallest of a tally's answer. Returns `TOO_BIG` if it's past the limit, as all
/// the thread's later ones will be too. This is cold beside the check that leads here, and keeping it out
/// of line lets that check be inlined.
#[cold]
fn add_smallest(plan: &Plan, at: usize, doc: u32) -> u64 {
    let tally = &plan.tallies[at];
    if doc >= tally.1.load(Relaxed) {
        return TOO_BIG;
    }
    let mut answer = plan.answers[tally.0 as usize].0.lock().unwrap();
    insert_smallest(&mut answer.ids, doc);
    tally.1.store(answer.ids[SMALLEST - 1], Relaxed);
    0
}

/// How many times each term occurs, among pieces of the corpus 2 MiB in all, about 1%, and in place of
/// term 0, which is no real term, about how many documents those pieces have. A term's occurrences stand
/// in for the documents holding it: this only guides the plan's choices. Each thread counts by itself, as
/// the commonest terms' counts would pass between the threads at every token if they shared them.
fn sample_terms(docs: &[u8], terms: &TermTable, threads: usize) -> Vec<u32> {
    let (next_piece, piece) = (AtomicUsize::new(0), (1 << 21).min(docs.len()) / threads);
    let parts = on_threads(threads, || {
        let start = next_piece.fetch_add(1, Relaxed) * (docs.len() / threads);
        let mut lists = DocLists::default();
        lists.present = vec![0; terms.count + 1];
        let (mut df, mut sampled) = (vec![0; terms.count], 0);
        for_each_doc(docs, start, start + piece, |_, text, edges| {
            sampled += 1;
            look_up(terms, text, edges, &mut lists);
            for word in &lists.tokens[..edges.len() / 2 - 2 * LANES] {
                df[(word & TERM_BITS) as usize] += 1;
            }
        });
        df[0] = sampled;
        df
    });
    let mut df = vec![0; terms.count];
    for part in parts {
        df.iter_mut().zip(part).for_each(|(all, one)| *all += one);
    }
    df
}

/// How a term is stored, in the order of the numbers terms get.
const ROWED: usize = 0;
const COLUMN_ONLY: usize = 1;
const NEITHER: usize = 2;
/// How far the sample's counts tell the terms with neither apart, when numbering them.
const DF_CAP: u32 = 255;

/// Decides from the sample how each query is answered and how each term is stored, then lists what to
/// check at each term's occurrences.
fn make_plan(plan: &mut Plan, docs: &[u8], df: Vec<u32>) {
    // About a block's documents, a little under, as a block that a range cuts short costs a whole pass.
    let sampled = (1 << 21).min(docs.len());
    plan.range_size = sampled / df[0].max(1) as usize * (BLOCK_DOCS as usize - 8) + 1;
    // A query goes by blocks if its anchors hold about N/32 documents in all, where running it on every
    // block starts to cost less than its checks at all those documents. A term in such a query has a row
    // above about N/48, where a byte per occurrence and a bit per document, for every document, start to
    // cost less than listing its first occurrence in each document that holds it. Any choice gives
    // the same answers. `df[0]` is about how many documents the sample has.
    let sum_of = |anchors: &[u32]| anchors.iter().map(|&t| df[t as usize]).sum::<u32>();
    let rowed = |term: u32| df[term as usize] * 48 >= df[0];
    // How each term is stored, and what its word holds beyond its number.
    let (mut store_of, mut flags) = (vec![NEITHER; df.len()], vec![0; df.len()]);
    // Which of the terms' own tallies each term has, if it has one, and how many there are.
    let (mut term_tally, mut term_tallies) = (vec![NONE; df.len()], 0);
    // Each phrase and its anchor term, each check and its anchor term, and one query's anchors.
    let (mut anchored, mut checks, mut query_anchors) = (vec![], vec![], vec![]);
    // The queries that go by blocks, and their terms in order of first use. Then the common terms whose own
    // tallies go by blocks, and those tallies' answers.
    let (mut block_queries, mut used) = (vec![], vec![]);
    let (mut block_terms, mut block_term_answers) = (vec![], vec![]);
    let query_count = plan.queries.len() as u32;
    plan.shared_answer.resize(query_count as usize, NONE);

    // First, how each query is answered.
    for (query, &Query(is_bool, from, to)) in plan.queries.iter().enumerate() {
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
        // After the last AND or NOT, each OR's term alone makes a document match. The commonest
        // of them becomes the first anchor, needing no check: the term's own tally serves every
        // query so anchored, and the other anchors skip the documents that hold it. Those steps have
        // no operator, and each is an anchor. If it's common, that tally goes by blocks too, in a run of
        // its own, as long as the other anchors are rare. Then no check is anchored on it, as those are all
        // rare: a term has its own tally by marks or by blocks, not both.
        let trailing = steps.iter().rev().take_while(|step| is_or(step));
        let sure = trailing.max_by_key(|&&term| df[term as usize]);
        let (sum, sure_count) = (sum_of(&query_anchors), sure.map_or(0, |&t| df[t as usize]));
        let common_sure = sure_count * 32 >= df[0] && (sum - sure_count) * 32 < df[0];
        if sum * 32 >= df[0] && !common_sure {
            block_queries.push(query);
            for term in steps.iter().map(|step| step & TERM_BITS) {
                used.extend((store_of[term as usize] == NEITHER).then_some(term));
                store_of[term as usize] = if rowed(term) { ROWED } else { COLUMN_ONLY };
            }
            continue;
        }
        if let Some(&term) = sure {
            query_anchors.sort_by_key(|&anchor| anchor != term);
            flags[term as usize] |= u32::from(!common_sure) * BOOL_ANCHOR;
            // The term's tally if it has one, or else the next.
            let tally = &mut term_tally[term as usize];
            let first = *tally == NONE;
            *tally = (*tally).min(term_tallies);
            term_tallies += u32::from(*tally == term_tallies);
            plan.shared_answer[query as usize] = query_count + *tally;
            if first && common_sure {
                block_terms.push(term);
                block_term_answers.push(query_count + *tally);
                used.extend((store_of[term as usize] == NEITHER).then_some(term));
                store_of[term as usize] = ROWED;
            }
        }
        let skipped = usize::from(sure.is_some());
        for (at, &term) in query_anchors.iter().enumerate().skip(skipped) {
            flags[term as usize] |= BOOL_ANCHOR;
            checks.push((term, (query, make_check(steps, term, &query_anchors[..at]))));
        }
    }

    // Then the terms' numbers, which the rest of the plan is written in, and which go in the term table and
    // the steps: those with rows, then the others with columns, by first use so that a query's columns sit
    // together, then the rest, the commonest first so that what the scan touches most sits together. One
    // bucket for rows, one for the other columns, and for the rest four per count, the largest first: BOOL
    // anchors, then the phrase anchors, then the others, as only anchors use the lists they index.
    let rank = |t: usize| (DF_CAP - df[t].min(DF_CAP)) as usize;
    let unlike = |t: usize, flag: u32| usize::from(flags[t] & flag == 0);
    let kind = |t: usize| 2 * unlike(t, BOOL_ANCHOR) + unlike(t, PHRASE_ANCHOR);
    let bucket = |t: usize| store_of[t] + store_of[t] / NEITHER * (4 * rank(t) + kind(t));
    let others = (FIRST_TERM..df.len()).filter(|&t| store_of[t] == NEITHER);
    let terms = used.iter().map(|&t| t as usize).chain(others);
    let keyed = terms.map(|t| (bucket(t) as u32, t));
    let (start, ordered) = group(NEITHER + 4 * DF_CAP as usize + 4, keyed);
    (plan.rowed_count, plan.sparse_count) = (start[1] as usize, (start[2] - start[1]) as usize);
    let mut new_number = vec![0; df.len()];
    for (at, &term) in ordered.iter().enumerate() {
        new_number[term] = (FIRST_TERM + at) as u32;
        flags[term] |= new_number[term] | (u32::from(store_of[term] == COLUMN_ONLY) * SPARSE);
    }
    // Empty ways hold term 0, whose word stays 0, so this needs no branch on which ways are in use.
    for set in &mut plan.terms.sets {
        for word in &mut set.rest[..=NO_WAY] {
            *word = flags[(*word & TERM_BITS) as usize] | (*word & !TERM_BITS);
        }
    }
    for step in &mut plan.steps {
        *step = (*step & !TERM_BITS) | new_number[(*step & TERM_BITS) as usize];
    }
    // Then each anchor term's phrases.
    let keyed = anchored.iter().map(|&(t, p)| (new_number[t as usize], p));
    let (start, phrases) = group(plan.terms.count, keyed);
    // The last range is empty, past the last key, and gives the end of the last term's phrases.
    for range in start.windows(2) {
        let of_term = &phrases[range[0] as usize..range[1] as usize];
        let at = plan.phrases.len();
        for &(query, position) in of_term {
            let Query(_, from, to) = plan.queries[query as usize];
            plan.phrases.extend([query, position, to - from]);
            plan.phrases.extend(&plan.steps[from as usize..to as usize]);
        }
        let mut near = 0;
        if let ([_], &[_, position, len, ref terms @ ..]) = (of_term, &plan.phrases[at..]) {
            let before = u32::from(position + 1 == len);
            let term = terms.get((position + 1).wrapping_sub(2 * before) as usize);
            near = term.map_or(0, |term| term | before << 31);
        }
        plan.phrase_start.push((at as u32, near));
    }
    plan.phrases.extend([0; 8]);

    // Then a term's checks, the short ones of all terms first. A short check holds up to 3 other terms.
    let (term_count, answers) = (plan.terms.count, query_count + term_tallies + 1);
    for (term, (_, check)) in &mut checks {
        check.terms = check.terms.map(|term| new_number[term as usize]);
        let long = check.terms[3] != 0 || check.terms[..3].iter().any(|&t| t > PACKED_MAX);
        *term = new_number[*term as usize] + u32::from(long) * term_count as u32;
    }
    let (starts, checks) = group(2 * term_count, checks.iter().copied());
    let or = |word: u64, (at, term): (&u32, u32)| word | u64::from(term) << at;
    let pack = |&(_, Check { truth, terms }): &(u32, Check)| {
        PACKED_AT.iter().zip(terms).fold(u64::from(truth as u8), or)
    };
    let (short, long) = checks.split_at(starts[term_count] as usize);
    plan.short_checks = short.iter().map(pack).collect();
    plan.long_checks = long.iter().map(|&(_, check)| check).collect();

    // Then what goes by blocks: the common terms alone, as runs of one step, and the queries, those of the
    // same length together, so that the loop over a query's steps rarely mispredicts its exit. A step there
    // holds a column's number for its term.
    let length = |query: &u32| plan.queries[*query as usize].2 - plan.queries[*query as usize].1;
    let longest = block_queries.iter().map(length).max().unwrap_or(0) as usize;
    let keyed = block_queries.iter().map(|query| (length(query), *query));
    let (_, block_queries) = group(longest + 1, keyed);
    plan.block_steps = vec![vec![]; longest + 2];
    let column = |term: &u32| new_number[*term as usize] - FIRST_TERM as u32;
    plan.block_steps[1].extend(block_terms.iter().map(column));
    for &query in &block_queries {
        let Query(_, from, to) = plan.queries[query as usize];
        let steps = plan.steps[from as usize..to as usize].iter();
        let columns = steps.map(|step| step - FIRST_TERM as u32);
        plan.block_steps[(to - from) as usize].extend(columns);
    }
    plan.block_queries = block_terms.len() + block_queries.len();

    // Then a tally for each check, for each term with one of its own, a spare, and each run by blocks; and
    // the answers. A term whose own tally goes by blocks isn't an anchor, so its place among the others
    // stays empty.
    let checked = checks.iter().map(|&(query, _)| query);
    let blocks = block_term_answers.into_iter().chain(block_queries);
    let tally_answers = checked.chain(query_count..answers).chain(blocks);
    let tally = |answer| Tally(answer, AtomicU32::new(NONE));
    plan.tallies = tally_answers.map(tally).collect();
    let (count, sum, ids) = (0, 0, [NONE; SMALLEST]);
    let blank = |_| Slot(Mutex::new(Answer { count, sum, ids }));
    plan.answers = (0..answers).map(blank).collect();

    // Last, where each term's checks are, and its own tally, or the spare for a term without one, which
    // nothing reads.
    let list = |t| AnchorList(starts[t], starts[term_count + t], 0);
    plan.anchor_lists = (0..=term_count).map(list).collect();
    for (term, &new) in new_number.iter().enumerate() {
        plan.anchor_lists[new as usize].2 =
            checks.len() as u32 + term_tally[term].min(term_tallies);
    }
}

// ---- The scan: what each thread keeps, and the stages of a document ----

/// One thread's results, and its list of short checks for the document in hand. A thread claims ranges
/// in document order, so the documents it counts ascend, which is why only its first ten for each tally
/// can be among an answer's smallest.
#[derive(Default)]
struct Scan {
    columns: Vec<Column>, // For each term with one, which documents of the block in hand hold it. Then a spare.
    hits: Vec<u64>, // For each tally: documents, above `SUM_BITS`, and the sum of their ids, below.
    counted: Vec<u32>, // The phrases that the document in hand has counted in.
    short_list: Vec<u32>, // The short checks to run for the document in hand.
}

/// What the stages fill for a document, with room for each of its tokens and two lanes' worth more, and the
/// marks.
#[derive(Default)]
struct DocLists {
    tokens: Vec<u32>,    // Every token's word.
    unsure: Vec<u32>,    // The positions of the words that may not be the token's.
    flagged: Vec<u32>,   // The positions of the words that call for more than a mark.
    phrase_at: Vec<u32>, // The positions of phrase anchors.
    bools: Vec<u32>,     // The terms with BOOL checks, once each.
    keys: Vec<u64>,      // Every token's short key...
    places: Vec<u64>,    // ...and its set's address.
    present: Vec<u8>,    // Each term's mark, then one that means nothing.
    stamp: u8,           // The stamp of the document in hand.
}

/// Gives every list this much room. It's out of line because measurements say so: inlined with `look_up`,
/// these calls leave its loops with worse registers, for 0.9% more instructions overall.
#[inline(never)]
fn make_room(lists: &mut DocLists, room: usize) {
    lists.tokens.resize(room, 0);
    lists.unsure.resize(room, 0);
    lists.flagged.resize(room, 0);
    lists.phrase_at.resize(room, 0);
    lists.bools.resize(room, 0);
    lists.keys.resize(room, 0);
    lists.places.resize(room, 0);
}

/// How many documents a thread answers the common queries over at a time: a vector's worth, or as near as
/// a byte's stamps count.
const BLOCK_DOCS: u32 = 255;

/// A term's bits over a block of documents, in half a cache line.
#[repr(align(32))]
#[derive(Clone, Copy)]
struct Column([u64; 4]);

/// The bits of `hits` that hold the sum of ids, enough for the spec's numbers of documents.
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
/// overlap, then comparing the keys without a branch on what that finds. A word with no flags marks its
/// term there and then. The others are listed by position: those marked `UNSURE` need a full look, and few
/// do, and those that are a term's with flags go to `mark`. Returns how many it listed for that. This stage
/// stays out of line, like `mark` and `check_bools`, because measurements say so: inlining this one costs
/// 1.7% of all cycles, and those two 1.9% together.
#[inline(never)]
fn look_up(terms: &TermTable, text: &[u8], edges: &[u32], lists: &mut DocLists) -> usize {
    let (sets, count) = (terms.sets.as_ptr() as i64, edges.len() / 2 - 2 * LANES);
    if lists.keys.len() < count + 2 * LANES {
        make_room(lists, count + 2 * LANES);
    }
    let (tokens, unsure) = (&mut lists.tokens[..], &mut lists.unsure[..]);
    let (flagged, present) = (&mut lists.flagged[..], &mut lists.present[..]);
    let (keys, places) = (&mut lists.keys[..], &mut lists.places[..]);
    let (stamp, last) = (lists.stamp, terms.count as u32);
    // A set is 64 bytes, so these take a hash to a set's distance from the first.
    let shift = 32 - 6 - terms.sets.len().trailing_zeros() as i32;
    let distances = (terms.sets.len() as i64 - 1) << 6;
    // First pass: each token's key, and its set's address, prefetched, as far as the next pass reads.
    for at in (0..count.next_multiple_of(2 * LANES)).step_by(LANES) {
        // SAFETY: `edges` has eight pairs more than there are tokens, each at or before a token's start,
        // whose load stays in the mapping as in `keys_of`. Keys and places have room for seven more.
        // The set is in the table, and a prefetch has no other effect. The build targets x86-64-v3,
        // which has AVX2.
        unsafe {
            let pairs = edges.as_ptr().add(2 * at);
            let start = |i: usize| text.as_ptr().add(*pairs.add(2 * i) as usize);
            let load = |i: usize| start(i).cast::<i64>().read_unaligned();
            let bytes = _mm256_setr_epi64x(load(0), load(1), load(2), load(3));
            // Each lane of the pairs is end << 32 | start, which gives end - start in the low half, where
            // the rest of this looks.
            let pair = _mm256_loadu_si256(pairs.cast());
            let lens = _mm256_sub_epi32(_mm256_srli_epi64(pair, 32), pair);
            // The token's bytes are those whose numbers are below its length, copied to each byte as a signed
            // number, which `keys_of` follows for the very long.
            let copies = _mm256_set_epi64x(0x0808_0808_0808_0808, 0, 0x0808_0808_0808_0808, 0);
            let numbers = _mm256_set1_epi64x(0x0706_0504_0302_0100);
            let held = _mm256_cmpgt_epi8(_mm256_shuffle_epi8(lens, copies), numbers);
            let lower = _mm256_or_si256(bytes, _mm256_set1_epi8(CASE_BITS as i8));
            let more = _mm256_slli_epi64(_mm256_cmpgt_epi32(lens, _mm256_set1_epi64x(8)), 63);
            let key = _mm256_or_si256(_mm256_and_si256(held, lower), more);
            let distance = _mm256_srl_epi64(hashes(key), _mm_cvtsi32_si128(shift));
            let distance = _mm256_and_si256(distance, _mm256_set1_epi64x(distances));
            let place = _mm256_add_epi64(distance, _mm256_set1_epi64x(sets));
            _mm256_storeu_si256(keys.as_mut_ptr().add(at).cast(), key);
            _mm256_storeu_si256(places.as_mut_ptr().add(at).cast(), place);
            for i in 0..LANES {
                _mm_prefetch(*places.get_unchecked(at + i) as *const i8, _MM_HINT_T0);
            }
        }
    }
    // Second pass: compare the keys, mark the terms of the words with no flags, and list the others. Any
    // other word is above `last`, so marks the mark past the last term's.
    let (mut unsure_count, mut flagged_count) = (0, 0);
    // SAFETY: the build targets x86-64-v3, which has AVX2.
    let mut here = unsafe { _mm256_setzero_si256() };
    for at in (0..count).step_by(2 * LANES) {
        // SAFETY: the places are in the table, `present` has a mark past the last term's, and the lists have
        // room for every token and seven more. `POSITIONS` has a row for each of the masks. The build targets
        // x86-64-v3, which has AVX2.
        unsafe {
            let word = |i: usize| {
                let set = &*(*places.get_unchecked(at + i) as *const TermSet);
                set.word(set.ways(*keys.get_unchecked(at + i))) as i32
            };
            let half = |i| _mm_setr_epi32(word(i), word(i + 1), word(i + 2), word(i + 3));
            let words = _mm256_set_m128i(half(4), half(0));
            _mm256_storeu_si256(tokens.as_mut_ptr().add(at).cast(), words);
            let marked = _mm256_min_epu32(words, _mm256_set1_epi32(last as i32));
            for index in std::mem::transmute::<__m256i, [u32; 8]>(marked) {
                *present.get_unchecked_mut(index as usize) = stamp;
            }
            // The table's row moves the positions of the words in this mask to the front, and as many as
            // there are stay in the list.
            let keep = |list: *mut u32, of: __m256i| {
                let row = _mm256_movemask_ps(_mm256_castsi256_ps(of)) as usize;
                let order =
                    _mm256_cvtepu8_epi32(_mm_loadl_epi64(POSITIONS.as_ptr().add(row).cast()));
                _mm256_storeu_si256(list.cast(), _mm256_add_epi32(order, here));
                row.count_ones() as usize
            };
            // A word marked `UNSURE` is negative as a signed number.
            unsure_count += keep(unsure.as_mut_ptr().add(unsure_count), words);
            let flags = _mm256_cmpgt_epi32(words, _mm256_set1_epi32(TERM_BITS as i32));
            flagged_count += keep(flagged.as_mut_ptr().add(flagged_count), flags);
            here = _mm256_add_epi32(here, _mm256_set1_epi32(2 * LANES as i32));
        }
    }
    // Third pass: the words that may not be the token's.
    for at in unsure[..unsure_count].iter().map(|&at| at as usize) {
        // SAFETY: this is a token's position or at most seven past the last, for which `tokens` has room,
        // and `flagged` has room for every token and those seven.
        unsafe {
            let mut word = *tokens.get_unchecked(at);
            let pair = edges.as_ptr().add(2 * at);
            let (start, end) = (*pair as usize, *pair.add(1) as usize);
            let set = &*(*places.get_unchecked(at) as *const TermSet);
            // Most are terms of up to 12 bytes, whose word is right if its way's long key is the rest of the
            // token, taken here as up to 8 bytes, zero-padded, so that a longer rest is a bigger number than
            // any long key. A word of no way reads that word in place of a long key, which no token's bytes
            // equal, so those go to `find`, like tokens of over 16 bytes, and of under 8, whose count of bits
            // wraps, keeping all of them, and of 8, whose rest of 0 isn't that word. It gives 0 for the empty
            // pairs past the last token, which marks term 0, whose mark means nothing.
            let rest = text.as_ptr().add(start + 8).cast::<u64>().read_unaligned() | CASE_BITS;
            let rest = _bzhi_u64(rest, 8 * end.wrapping_sub(start + 8) as u32);
            let w = NO_WAY + (word >> WAY_SHIFT & 3) as usize;
            if rest != u64::from(*set.rest.get_unchecked(w)) {
                let token = text.get_unchecked(start..end);
                word = terms.find(token, *keys.get_unchecked(at), set);
                *tokens.get_unchecked_mut(at) = word;
            }
            let bare = word & (TERM_BITS | PHRASE_ANCHOR | BOOL_ANCHOR | SPARSE);
            *present.get_unchecked_mut(bare.min(last) as usize) = stamp;
            *flagged.get_unchecked_mut(flagged_count) = at as u32;
            flagged_count += usize::from(bare > TERM_BITS);
        }
    }
    flagged_count
}

/// Marks what the words listed in `flagged` call for, and lists what to do about it: the terms with BOOL
/// checks, once each, and the positions of phrase anchors. A term with a column takes the document's bit in
/// it here, which a term with rows gets again when its row is stored, and the last column is a spare, for
/// the other terms. Returns how long the lists are.
#[inline(never)]
fn mark(lists: &mut DocLists, flagged: usize, columns: &mut [Column]) -> [usize; 2] {
    let (tokens, present) = (&lists.tokens[..], &mut lists.present[..]);
    let (bools, phrase_at) = (&mut lists.bools[..], &mut lists.phrase_at[..]);
    let (stamp, spare) = (lists.stamp, columns.len() as u32 - 1);
    let (part, bit) = (usize::from(stamp - 1) / 64, 1 << ((stamp - 1) % 64));
    let (mut bool_count, mut phrases) = (0, 0);
    for &at in &lists.flagged[..flagged] {
        // SAFETY: this came from a token's position or at most three past the last, for which `tokens` has
        // room. `present` has a mark for every term, a column has four parts, and each list has room for
        // every token.
        unsafe {
            let word = *tokens.get_unchecked(at as usize);
            let term = word & TERM_BITS;
            let column = (term - FIRST_TERM as u32).min(spare) as usize;
            *columns.get_unchecked_mut(column).0.get_unchecked_mut(part) |= bit;
            let held = present.get_unchecked_mut(term as usize);
            let first = usize::from(*held != stamp);
            *held = stamp;
            *bools.get_unchecked_mut(bool_count) = term;
            bool_count += (word >> BOOL_ANCHOR.trailing_zeros() & 1) as usize & first;
            *phrase_at.get_unchecked_mut(phrases) = at;
            phrases += (word >> PHRASE_ANCHOR.trailing_zeros() & 1) as usize;
        }
    }
    [bool_count, phrases]
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
            column.0[word] = bits;
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
        // Most occurrences of a term with one phrase have the wrong neighbour, which this tells without the
        // look at the phrase, a cache miss.
        let (from, near) = plan.phrase_start[term];
        let held = tokens.get((at + 1).wrapping_sub(2 * (near >> 31) as usize));
        if near != 0 && held.map_or(0, |word| word & TERM_BITS) != near & TERM_BITS {
            continue;
        }
        let mut entries = &plan.phrases[from as usize..plan.phrase_start[term + 1].0 as usize];
        while let [query, position, len, rest @ ..] = entries {
            let (phrase, after) = rest.split_at(*len as usize);
            entries = after;
            // A start before the document wraps, and makes a range that isn't in it.
            let start = at.wrapping_sub(*position as usize);
            let run = tokens.get(start..start.wrapping_add(phrase.len()));
            // A phrase has at most 8 terms, so one compare does for any of them, and there's no branch on
            // which term differs.
            // SAFETY: `tokens` and `phrases` both have room for 8 past any item, and the build targets
            // x86-64-v3, which has AVX2.
            let same = |run: &[u32]| unsafe {
                let words = _mm256_loadu_si256(run.as_ptr().cast());
                let terms = _mm256_and_si256(words, _mm256_set1_epi32(TERM_BITS as i32));
                let wanted = _mm256_loadu_si256(phrase.as_ptr().cast());
                let same = _mm256_castsi256_ps(_mm256_cmpeq_epi32(terms, wanted));
                (!_mm256_movemask_ps(same)).trailing_zeros() as usize >= phrase.len()
            };
            // An anchor can occur twice in a document, but the document counts once.
            if run.is_some_and(same) && !counted.contains(query) {
                counted.push(*query);
                let mut answer = plan.answers[*query as usize].0.lock().unwrap();
                (answer.count, answer.sum) = (answer.count + 1, answer.sum + u64::from(doc));
                insert_smallest(&mut answer.ids, doc);
            }
        }
    }
}

/// Counts a document at its anchors listed so far, for each term's own tally and each check that says so.
/// Prefetching a few anchors ahead lets cache misses overlap, without a loop of its own to mispredict the
/// exit of, and listing the short checks before running them replaces an unpredictable loop exit per anchor
/// by one loop.
#[inline(never)]
fn check_bools(lists: &DocLists, bools: usize, plan: &Plan, doc: u32, scan: &mut Scan) {
    let (anchors, present) = (&lists.bools[..bools], &lists.present[..]);
    // A list has room for every token and two lanes' worth more, so this is as long as `anchors`, or more.
    let ahead = &lists.bools[LANES..];
    // SAFETY: the build targets x86-64-v3, which has AVX.
    let stamp = unsafe { _mm_set1_epi8(lists.stamp as i8) };
    let (hits, short) = (&mut scan.hits[..], &mut scan.short_list[..]);
    let checks = plan.short_checks.as_ptr();
    let (anchor_lists, this_doc) = (&plan.anchor_lists[..], (1 << SUM_BITS) + u64::from(doc));
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
    for &term in &lists.bools[..LANES] {
        prefetch(anchor_lists.as_ptr().wrapping_add(term as usize).cast());
    }
    let (short_count, mut listed) = (plan.short_checks.len(), 0);
    for (&term, &later) in anchors.iter().zip(ahead) {
        prefetch(anchor_lists.as_ptr().wrapping_add(later as usize).cast());
        // SAFETY: `anchor_lists` has one entry more than there are terms, `short` has room for all the checks
        // and four more, and an anchor's checks are all in `long_checks`.
        unsafe {
            let AnchorList(from, long_from, own_tally) = *anchor_lists.get_unchecked(term as usize);
            let AnchorList(to, long_to, _) = *anchor_lists.get_unchecked(term as usize + 1);
            prefetch(checks.wrapping_add(from as usize).cast());
            // One store lists four of the term's short checks, which is enough for most, whatever
            // `listed` then keeps of them, and the loop lists any more.
            let at = _mm_add_epi32(_mm_set1_epi32(from as i32), _mm_setr_epi32(0, 1, 2, 3));
            _mm_storeu_si128(short.as_mut_ptr().add(listed).cast(), at);
            for at in from + 4..to {
                short[listed + (at - from) as usize] = at;
            }
            listed += (to - from) as usize;
            add(1, own_tally as usize);
            for at in long_from as usize..long_to as usize {
                let check = plan.long_checks.get_unchecked(at - short_count);
                add(counts(check, present, stamp), at);
            }
        }
    }
    let mut kept = 0;
    for at in 0..listed {
        // SAFETY: the list came from `anchor_lists`, and `kept` is at most `at`.
        unsafe {
            let at = *short.get_unchecked(at);
            let check = *plan.short_checks.get_unchecked(at as usize);
            *short.get_unchecked_mut(kept) = at;
            kept += counts_packed(check, present, stamp) as usize;
        }
    }
    for &at in &short[..kept] {
        add(1, at as usize);
    }
}

/// What a step does to the set so far with its term's set, by its operator: flips that set, ands, then ors
/// that set in if asked. The step's top two bits index this: OR, AND, NOT.
#[repr(align(32))]
struct StepMasks([[u64; 8]; 3]);
static STEP_MASKS: StepMasks = StepMasks([[!0; 8], [0; 8], [!0, !0, !0, !0, 0, 0, 0, 0]]);

/// Puts documents of a block's result among the smallest of a tally's answer, up to the first past its limit.
#[cold]
fn add_smallest_of_block(plan: &Plan, at: usize, set: __m256i, first_doc: u32) -> u64 {
    // SAFETY: a vector has the layout of four words.
    let words = unsafe { std::mem::transmute::<__m256i, [u64; 4]>(set) };
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
                    let masks = STEP_MASKS.0.as_ptr().add((step >> 30) as usize).cast();
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
                let first_docs = _mm256_mul_epu32(in_word, starts);
                let sums = _mm256_add_epi64(_mm256_sad_epu8(places, zero), first_docs);
                let sums =
                    _mm256_add_epi64(sums, _mm256_slli_epi64(_mm256_sad_epu8(by_pair, zero), 3));
                let found = _mm256_add_epi64(sums, _mm256_slli_epi64(in_word, SUM_BITS as i32));
                let high = _mm256_extracti128_si256(found, 1);
                let half = _mm_add_epi64(_mm256_castsi256_si128(found), high);
                let found = _mm_add_epi64(half, _mm_unpackhi_epi64(half, half));
                let found = _mm_cvtsi128_si64(found) as u64;
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
        let with_rows = &mut scan.columns[..plan.rowed_count];
        store_rows(rows, with_rows, docs as usize / 64);
    }
    answer_block(plan, &scan.columns, &mut scan.hits, first_doc);
    scan.columns.fill(Column([0; 4]));
}

/// Claims the next `size` of the numbers below `len`, if any are left.
fn claim(next: &AtomicUsize, size: usize, len: usize) -> Option<std::ops::Range<usize>> {
    let from = next.fetch_add(size, Relaxed);
    (from < len).then(|| from..(from + size).min(len))
}

// ---- After the scan: the output ----

/// How far each job that the threads share has got: the next of each to claim, and what's printed.
#[derive(Default)]
struct Progress {
    next_range: AtomicUsize,                        // In bytes of the corpus.
    hits: Mutex<Vec<Vec<u64>>>, // Each thread's `Scan::hits`, once it has scanned...
    scanned: AtomicUsize,       // ...and how many have.
    next_total: AtomicUsize,    // The next tally to add up.
    totalled: AtomicUsize,      // How many threads have no more to add up.
    next_batch: AtomicUsize,    // In queries.
    printed: Mutex<(usize, Vec<(usize, Vec<u8>)>)>, // How many threads are done printing, and the text by batch.
    next_release: AtomicUsize,                      // In bytes of the corpus.
}

/// How much of the corpus a thread takes out of the address space at a time.
const RELEASE: usize = 1 << 24;

/// Counts this thread as here, and waits for the others. All these waits are short, as threads finish
/// about together, and a thread that sleeps through one takes far longer to wake.
fn wait_for_all(here: &AtomicUsize, threads: usize) {
    here.fetch_add(1, Release);
    while here.load(Acquire) < threads {
        thread::yield_now();
    }
}

/// One thread's part of the scan and the answers. What it counted goes where every thread can read it once
/// all are done. Then the totals, before any printing. Then threads claim batches of queries, and the last
/// to finish writes them to the file in order.
fn scan_and_answer(docs: &[u8], plan: &Plan, mut file: &File, progress: &Progress, threads: usize) {
    // It claims ranges of the corpus until none is left, and scans them.
    let mut scan = Scan::default();
    scan.hits.resize(plan.tallies.len(), 0);
    scan.columns = vec![Column([0; 4]); plan.rowed_count + plan.sparse_count + 1];
    scan.short_list = vec![0; plan.short_checks.len() + 4];
    // With a row's worth more, as a row's marks are read whole.
    let mut lists = DocLists::default();
    lists.present = vec![0; plan.terms.count + 64];
    let mut rows = vec![[0; 64]; plan.rowed_count.div_ceil(64)];
    while let Some(range) = claim(&progress.next_range, plan.range_size, docs.len()) {
        let (mut first_doc, mut in_block) = (0, 0);
        for_each_doc(docs, range.start, range.end, |doc, text, edges| {
            first_doc = if in_block == 0 { doc } else { first_doc };
            lists.stamp = in_block as u8 + 1;
            let flagged = look_up(&plan.terms, text, edges, &mut lists);
            let [bools, phrases] = mark(&mut lists, flagged, &mut scan.columns);
            // Keeps which terms with rows the document holds, each 64 of them a word of its row.
            let marks = lists.present[FIRST_TERM..].as_chunks::<64>().0;
            for (row, marks) in rows.iter_mut().zip(marks) {
                // SAFETY: a chunk has these 64 bytes, and the build targets x86-64-v3, which has AVX2.
                let half = |at: usize| unsafe {
                    let marks = _mm256_loadu_si256(marks.as_ptr().add(at).cast());
                    let held = _mm256_cmpeq_epi8(marks, _mm256_set1_epi8(lists.stamp as i8));
                    _mm256_movemask_epi8(held) as u32
                };
                row[in_block as usize % 64] = u64::from(half(0)) | u64::from(half(32)) << 32;
            }
            in_block += 1;
            if in_block % 64 == 0 {
                let part = (in_block - 1) as usize / 64;
                store_rows(&mut rows, &mut scan.columns[..plan.rowed_count], part);
            }
            let ats = &lists.phrase_at[..phrases];
            let in_doc = &lists.tokens[..edges.len() / 2 - 2 * LANES];
            check_phrases(ats, in_doc, plan, doc, &mut scan.counted);
            check_bools(&lists, bools, plan, doc, &mut scan);
            if in_block == BLOCK_DOCS {
                finish_block(plan, &mut scan, &mut rows, first_doc, in_block);
                lists.present.fill(0);
                in_block = 0;
            }
        });
        if in_block != 0 {
            finish_block(plan, &mut scan, &mut rows, first_doc, in_block);
            lists.present.fill(0);
        }
    }
    progress.hits.lock().unwrap().push(scan.hits);
    wait_for_all(&progress.scanned, threads);
    let address = |hits: &Vec<u64>| hits.as_ptr();
    let scans: Vec<_> = progress.hits.lock().unwrap().iter().map(address).collect();

    // Adds the threads' counts and ids of each tally into its answer. All threads' hits for a tally are
    // summed before its answer is locked, so an answer is visited once per tally, not once per thread as
    // well: they are many cache lines, and each thread's hits are read in order. Threads claim parts of
    // this.
    let next_tallies = || claim(&progress.next_total, BATCH, plan.tallies.len());
    for at in std::iter::from_fn(next_tallies).flatten() {
        // SAFETY: each thread's `hits` has every tally.
        let at_hits = |hits: &*const u64| unsafe { *hits.add(at) } & !TOO_BIG;
        let hits: u64 = scans.iter().map(at_hits).sum();
        let mut answer = plan.answers[plan.tallies[at].0 as usize].0.lock().unwrap();
        answer.count += hits >> SUM_BITS;
        answer.sum += hits & ((1 << SUM_BITS) - 1);
    }

    wait_for_all(&progress.totalled, threads);

    // Printing, each query's answer with what its term's tally adds to it.
    let mut pieces = vec![];
    while let Some(batch) = claim(&progress.next_batch, BATCH, plan.queries.len()) {
        let mut text = Vec::with_capacity(1 << 15);
        for query in batch.clone() {
            let mut line = *plan.answers[query].0.lock().unwrap();
            // NONE is no answer's number.
            if let Some(term_answer) = plan.answers.get(plan.shared_answer[query] as usize) {
                let Answer { count, sum, ids } = *term_answer.0.lock().unwrap();
                (line.count, line.sum) = (line.count + count, line.sum + sum);
                ids.iter().for_each(|&d| insert_smallest(&mut line.ids, d));
            }
            // Each number has a space after it, the last of which becomes the line's end.
            let Answer { count, sum, ids } = line;
            let docs = ids.iter().take(count as usize).map(|&d| u64::from(d));
            for value in [count, sum].into_iter().chain(docs) {
                // The 20 digits of the largest u64, then the space.
                let (mut digits, mut start) = ([b' '; 21], 20);
                for value in std::iter::successors(Some(value), |&v| (v >= 10).then(|| v / 10)) {
                    start -= 1;
                    digits[start] = b'0' + (value % 10) as u8;
                }
                text.extend_from_slice(&digits[start..]);
            }
            *text.last_mut().expect("a line has numbers") = b'\n';
        }
        pieces.push((batch.start, text));
    }
    // The last thread here writes it all at once, in one call to the kernel, which costs less than a call
    // per batch in turn.
    let mut printed = progress.printed.lock().unwrap();
    printed.0 += 1;
    printed.1.append(&mut pieces);
    if printed.0 == threads {
        printed.1.sort_unstable_by_key(|piece| piece.0);
        let mut slices: Vec<_> = printed
            .1
            .iter()
            .map(|piece| IoSlice::new(&piece.1))
            .collect();
        let mut rest = &mut slices[..];
        while !rest.is_empty() {
            // `panic = "abort"`: a failed write ends the process.
            let written = file.write_vectored(rest).expect("writing the results");
            IoSlice::advance_slices(&mut rest, written);
        }
    }
    drop(printed);
    // The clock runs to exit, where one thread would take the corpus's pages out of the address space, one
    // by one. All the threads share that here.
    while let Some(part) = claim(&progress.next_release, RELEASE, docs.len()) {
        // SAFETY: nothing reads the corpus again, and its pages would come back from the file if it did.
        // A part starts at a page, and a failure only leaves the pages for exit.
        unsafe { madvise(docs[part.start..].as_ptr(), part.len(), 4) };
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
    // Threads of their own map the corpus's pages ahead of the scan, whose threads would each stop for their
    // faults. They start now but wait for the parse, whose new threads need the lock that this holds. An
    // old kernel doesn't know this advice, MADV_POPULATE_READ, and the scan takes the faults as before.
    // One more thread starts them and creates the output, so that this one needn't wait for either.
    let helper = thread::spawn(move || {
        let mut mappers = vec![];
        for part in docs.chunks((docs.len() / 4 + 1).next_multiple_of(1 << 12)) {
            mappers.push(thread::spawn(move || {
                thread::park();
                // SAFETY: this only maps pages of the mapping, and a part starts at a page.
                unsafe { madvise(part.as_ptr(), part.len(), 22) };
            }));
        }
        let file = File::create(format!("{out_dir}/results.txt")).expect("creating results.txt");
        thread::park();
        mappers.iter().for_each(|mapper| mapper.thread().unpark());
        file
    });
    let mut plan = parse_queries(queries, threads, 1 << 16);
    // The mappers start now, as one thread makes the plan and the others are free for them.
    let df = sample_terms(docs, &plan.terms, threads);
    helper.thread().unpark();
    make_plan(&mut plan, docs, df);
    let file = helper.join().expect("panics abort");
    // One round of threads scans, then answers, as starting threads again costs more than a short wait.
    let progress = Progress::default();
    let work = || scan_and_answer(docs, &plan, &file, &progress, threads);
    on_threads(threads, work);
    // Nothing is buffered, and the kernel frees all the memory faster than dropping it would.
    std::process::exit(0)
}
