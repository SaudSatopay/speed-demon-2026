//! Track B (Index): boolean and phrase queries over a 254 MB corpus.
//!
//! The queries are read first, so only their terms are ever looked up. Every document matching a
//! BOOL query contains one of a few anchor terms (the rarest AND of its last run, or its OR terms),
//! and for nine queries in ten the anchors are rare, so the query is answered during the scan: at
//! each anchor's first occurrence in a document, the document's term stamps give the whole formula
//! in a few loads, and no posting of any of its terms is ever stored. Only queries anchored on a
//! common term read postings afterwards: bitmaps for their common terms, set during the scan, and
//! chains of 2-byte offsets for their other terms. Each occurrence of a phrase's rarest term checks
//! the phrase around it when the document ends. Threads scan contiguous ranges of the corpus in
//! 32-byte blocks, then answer the queries in batches, appending their answers to the file in order.

use std::arch::x86_64::*;
use std::fs::File;
use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering::*};
use std::thread;

/// A token that no query uses.
const NONE: u32 = u32::MAX;
/// How a term is stored: this value if no query reads its postings, so only its stamp is kept...
const STAMP_ONLY: u32 = NONE;
/// ...this flag plus a number if it has a bitmap, and otherwise the number of its posting list.
const BITMAP: u32 = 1 << 31;
/// A term's phrases are checked around each of its occurrences...
const PHRASE_ANCHOR: u32 = 1;
/// ...and its BOOL queries are evaluated for each document that contains it.
const BOOL_ANCHOR: u32 = 2;
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
        let base = mmap(
            std::ptr::null_mut(),
            len + PADDING,
            read,
            private | anonymous,
            -1,
            0,
        );
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

/// One bit per byte of the 32 at `pos`, set for the token bytes, `[A-Za-z0-9]`. Bytes of 0x80 and
/// above compare as negative, so they are separators, even inside a letter (spec section 2.2).
fn token_mask(file: &[u8], pos: usize) -> u32 {
    assert!(pos + 32 <= file.len() + PADDING);
    // SAFETY: the assert keeps the load inside the mapping, it needs no alignment, and the build
    // targets x86-64-v3, which has AVX2.
    unsafe {
        let byte_set = |byte: u8| _mm256_set1_epi8(byte as i8);
        let within = |value, low: u8, high: u8| {
            let above = _mm256_cmpgt_epi8(value, byte_set(low - 1));
            _mm256_and_si256(above, _mm256_cmpgt_epi8(byte_set(high + 1), value))
        };
        let bytes = _mm256_loadu_si256(file.as_ptr().add(pos).cast());
        let lower = _mm256_or_si256(bytes, byte_set(0x20));
        let tokens = _mm256_or_si256(within(lower, b'a', b'z'), within(bytes, b'0', b'9'));
        _mm256_movemask_epi8(tokens) as u32
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

/// A token's key: its first 16 bytes, lowercased and zero-padded, from one load and a mask. Setting
/// this bit in every byte lowercases ASCII letters, and digits already have it set.
fn token_key(file: &[u8], start: usize, len: usize) -> u128 {
    assert!(start + 16 <= file.len() + PADDING);
    // SAFETY: the assert keeps the load inside the mapping, and the load needs no alignment.
    let bytes = unsafe { file.as_ptr().add(start).cast::<u128>().read_unaligned() };
    (bytes | 0x2020_2020_2020_2020_2020_2020_2020_2020) & (u128::MAX >> (128 - 8 * len.min(16)))
}

/// Mixes a key and its length with one multiply, whose high bits depend on every input bit. A test
/// build swaps it for a constant, so that every key collides and any logic that trusts a hash without
/// comparing the key shows up as a wrong answer.
fn hash(key: u128, len: usize) -> u64 {
    let (low, high) = (key as u64, (key >> 64) as u64);
    let hash = (low ^ high.rotate_left(29) ^ len as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    if cfg!(sd_const_hash) { 0 } else { hash }
}

/// The slot a key's probe starts at, in a table of `slots` slots, a power of two: the hash's top bits.
fn home(slots: usize, key: u128, len: usize) -> usize {
    (hash(key, len) >> (64 - slots.trailing_zeros())) as usize
}

/// A slot of the term table: half a cache line holding all that a token's lookup needs, so one memory
/// access answers it. On seed 1, nearly all terms fit in the 16 bytes of the key.
#[derive(Clone, Copy, Default)]
#[repr(align(32))]
struct TermSlot {
    key: u128,
    len: u32, // 0 when the slot is free.
    term: u32,
    store: u32, // How the term is stored: STAMP_ONLY, BITMAP | number, or a list number.
    flags: u32, // PHRASE_ANCHOR and BOOL_ANCHOR, for what to check when the term occurs.
}

/// Query terms to dense ids, by open addressing with linear probing. A hit always compares the full
/// key: terms longer than the slot's 16 bytes are checked against the query file.
struct TermTable<'a> {
    slots: Vec<TermSlot>, // The length is a power of two.
    keys: Vec<&'a [u8]>,
}

impl<'a> TermTable<'a> {
    /// Sized for `query_bytes` of query text, where 16 bytes hold about one distinct term.
    fn new(query_bytes: usize) -> Self {
        TermTable {
            slots: vec![TermSlot::default(); (query_bytes / 16).next_power_of_two().max(1 << 12)],
            keys: Vec::new(),
        }
    }

    /// The slot of the term for the token at `start..end` of `file`, in any letter case, or else the
    /// empty slot that ends its probe, whose term is the one no query uses. The compares are joined
    /// without short-circuits, so a probe has one branch, the exit, which is nearly always taken.
    #[inline(always)]
    fn find(&self, file: &[u8], start: usize, end: usize) -> &TermSlot {
        let len = end - start;
        let key = token_key(file, start, len);
        let mask = self.slots.len() - 1;
        let mut index = home(self.slots.len(), key, len);
        loop {
            let slot = &self.slots[index];
            let differs = (slot.key ^ key) | u128::from(slot.len ^ len as u32);
            // The term, or the empty slot, unless a term longer than the key disagrees past it.
            if differs.min(u128::from(slot.len)) == 0
                && (len <= 16 || slot.len == 0 || self.long_term_is(slot, &file[start..end]))
            {
                return slot;
            }
            index = (index + 1) & mask;
        }
    }

    /// Whether a term longer than the key's 16 bytes, whose key and length match, is this token.
    #[cold]
    fn long_term_is(&self, slot: &TermSlot, token: &[u8]) -> bool {
        token.eq_ignore_ascii_case(self.keys[slot.term as usize])
    }

    /// Adds the query term at `start..end` of `file`, if it's new, and returns its id.
    fn intern(&mut self, file: &'a [u8], start: usize, end: usize) -> u32 {
        let slot = self.find(file, start, end);
        if slot.len != 0 {
            return slot.term;
        }
        let len = end - start;
        let term = self.keys.len() as u32;
        self.keys.push(&file[start..end]);
        let slot = TermSlot {
            key: token_key(file, start, len),
            len: len as u32,
            term,
            store: STAMP_ONLY,
            flags: 0,
        };
        place_slot(&mut self.slots, slot);
        if self.keys.len() * 4 > self.slots.len() * 3 {
            let mut bigger = vec![TermSlot::default(); self.slots.len() * 2];
            for &slot in self.slots.iter().filter(|slot| slot.len != 0) {
                place_slot(&mut bigger, slot);
            }
            self.slots = bigger;
        }
        term
    }
}

fn place_slot(slots: &mut [TermSlot], slot: TermSlot) {
    let mask = slots.len() - 1;
    let mut index = home(slots.len(), slot.key, slot.len as usize);
    while slots[index].len != 0 {
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

/// Visits the documents whose lines start in `start..end`, passing each one's id, where its text
/// starts, and its LF. A line belongs to the range holding its first byte.
fn for_each_doc(docs: &[u8], start: usize, end: usize, mut visit: impl FnMut(u32, usize, usize)) {
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
        let lf = pos + 1 + offset_of_line_feed(docs, pos + 1);
        if aligned {
            visit(id, pos + 1, lf);
        }
        line = lf + 1;
    }
}

/// Visits each token of the text in `start..lf`, passing where it starts and ends. Each 32-byte block
/// gives a mask of its token bytes, and the set bits of the mask XORed with itself shifted are where
/// tokens start and end, so a token costs a few bit operations and no byte loop.
fn for_each_token(docs: &[u8], start: usize, lf: usize, mut visit: impl FnMut(usize, usize)) {
    let mut block = start;
    let mut token_start = start;
    let mut in_token = false;
    while block <= lf {
        let mut mask = token_mask(docs, block);
        if lf - block < 32 {
            mask &= (1 << (lf - block)) - 1;
        }
        let mut edges = mask ^ (mask << 1 | u32::from(in_token));
        while edges != 0 {
            let at = block + edges.trailing_zeros() as usize;
            if in_token {
                visit(token_start, at);
            } else {
                token_start = at;
            }
            in_token = !in_token;
            edges &= edges - 1;
        }
        block += 32;
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
        Some(step) => out.push(step.term),
        None => {
            out.push(steps[last_or].term);
            anchors(&steps[..last_or], sample_df, out);
        }
    }
}

/// Groups `(term, item)` pairs sorted by term into one list per term: the items of `term` are
/// `items[start[term]..start[term + 1]]`.
fn group_by_term<T: Copy>(pairs: &[(u32, T)], term_count: usize) -> (Vec<u32>, Vec<T>) {
    let mut start = vec![0u32; term_count + 1];
    for &(term, _) in pairs {
        start[term as usize + 1] += 1;
    }
    for term in 0..term_count {
        start[term + 1] += start[term];
    }
    (start, pairs.iter().map(|&(_, item)| item).collect())
}

/// A BOOL query packed for evaluation against one document's stamps: the terms carry their operator
/// in the top two bits (OR 0, AND 1, NOT 2), and unused slots hold `OR` of a term no document has.
#[derive(Clone, Copy)]
#[repr(align(32))]
struct Packed {
    query: u32,
    terms: [u32; 6],
}

/// The term bits of a packed step.
const TERM_BITS: u32 = (1 << 30) - 1;

fn op_code(op: u8) -> u32 {
    match op {
        b'A' => 1,
        b'N' => 2,
        _ => 0,
    }
}

/// What the sample decided: how each term is stored, and what to check at each term's occurrences.
struct Plan {
    store_of: Vec<u32>,
    bitmap_count: usize,
    list_count: usize,
    phrase_start: Vec<u32>, // A term's phrases are `phrases[phrase_start[t]..phrase_start[t + 1]]`.
    phrases: Vec<(u32, u32)>, // (query, position of this term in the phrase).
    bool_start: Vec<u32>,   // Likewise the BOOL queries anchored on a term.
    bools: Vec<Packed>,
    scan_count: usize, // How many BOOL queries the scan answers.
}

fn make_plan(docs: &[u8], queries: &mut Queries) -> Plan {
    // Two terms follow the query terms: the one every other token maps to, and one no token maps to.
    let term_count = queries.terms.keys.len();
    for slot in queries.terms.slots.iter_mut().filter(|slot| slot.len == 0) {
        slot.term = term_count as u32;
        slot.store = STAMP_ONLY;
    }
    let (mut last_doc, mut sample_df) = (vec![NONE; term_count + 2], vec![0u32; term_count + 2]);
    let mut sampled_docs = 0;
    for_each_doc(docs, 0, docs.len().min(SAMPLE_BYTES), |doc, text, lf| {
        sampled_docs += 1;
        for_each_token(docs, text, lf, |start, end| {
            let term = queries.terms.find(docs, start, end).term as usize;
            if last_doc[term] != doc {
                last_doc[term] = doc;
                sample_df[term] += 1;
            }
        });
    });

    // A query anchored on a common term, with df above about N/48, is answered from postings: bitmaps
    // for its common terms and chains for the others. On memory alone, a bitmap (N/8 bytes) and a
    // chain (about 3.3 bytes per posting) cross near df = N/26, but setting a bit is cheaper than
    // extending a chain, and measuring put the best K near N/48. Any choice gives the same answers.
    let common = |term: u32| sample_df[term as usize] * 32 >= sampled_docs;
    let mut store_of = vec![STAMP_ONLY; term_count];
    let (mut bitmap_count, mut list_count, mut scan_count) = (0, 0, 0);
    let (mut phrase_anchors, mut bool_anchors, mut anchor_terms) = (vec![], vec![], vec![]);
    let Queries { list, steps, terms } = queries;
    for (index, query) in list.iter_mut().enumerate() {
        let steps = &steps[query.start as usize..query.end as usize];
        if query.is_phrase {
            // Any term of the phrase would be correct. The rarest triggers the fewest checks.
            let rarest = (0..steps.len()).min_by_key(|&i| sample_df[steps[i].term as usize]);
            let position = rarest.unwrap_or(0);
            phrase_anchors.push((steps[position].term, (index as u32, position as u32)));
            continue;
        }
        anchor_terms.clear();
        anchors(steps, &sample_df, &mut anchor_terms);
        if anchor_terms.iter().any(|&term| common(term)) {
            for step in steps {
                let store = &mut store_of[step.term as usize];
                if *store != STAMP_ONLY {
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
            let mut packed = Packed {
                query: query.slot,
                terms: [term_count as u32 + 1; 6], // The term no token maps to, in unused slots.
            };
            for (slot, step) in packed.terms.iter_mut().zip(steps) {
                *slot = step.term | op_code(step.op) << 30;
            }
            bool_anchors.extend(anchor_terms.iter().map(|&term| (term, packed)));
        }
    }
    phrase_anchors.sort_unstable();
    bool_anchors.sort_unstable_by_key(|&(term, packed)| (term, packed.query));
    bool_anchors.dedup_by_key(|&mut (term, packed)| (term, packed.query));
    let (phrase_start, phrases) = group_by_term(&phrase_anchors, term_count);
    let (bool_start, bools) = group_by_term(&bool_anchors, term_count);
    for slot in terms.slots.iter_mut().filter(|slot| slot.len != 0) {
        let term = slot.term as usize;
        slot.store = store_of[term];
        slot.flags = u32::from(phrase_start[term] != phrase_start[term + 1]) * PHRASE_ANCHOR
            | u32::from(bool_start[term] != bool_start[term + 1]) * BOOL_ANCHOR;
    }
    Plan {
        store_of,
        bitmap_count: bitmap_count as usize,
        list_count: list_count as usize,
        phrase_start,
        phrases,
        bool_start,
        bools,
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

/// What one part of the corpus holds of a BOOL query answered in the scan.
#[derive(Clone, Copy)]
struct Hits {
    last: u16,  // The offset of the last matching document, to count a document once.
    count: u16, // How many documents of the part match...
    sum: u32,   // ...and the sum of their offsets.
}

const NO_HITS: Hits = Hits {
    last: u16::MAX,
    count: 0,
    sum: 0,
};

/// A run of up to 65,535 documents, whose postings and sums are 2-byte offsets from its first one.
struct Part {
    first_doc: u32,
    lists: Vec<ListState>,
    hits: Vec<Hits>,
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

fn scan_range(
    docs: &[u8],
    range: (usize, usize),
    queries: &Queries,
    plan: &Plan,
    bitmaps: &[AtomicU64],
    words: usize,
    spare: usize, // This thread's own word after the bitmaps, where stamp-only terms set bits.
    smallest: &[Mutex<[u32; SMALLEST]>],
) -> ScanResult {
    let term_count = queries.terms.keys.len();
    let mut result = ScanResult {
        blocks: Vec::new(),
        parts: Vec::new(),
        phrase_hits: Vec::new(),
    };
    let new_part = |first_doc| Part {
        first_doc,
        lists: vec![EMPTY_LIST; plan.list_count],
        hits: vec![NO_HITS; plan.scan_count],
    };
    let mut part = new_part(NONE);
    // The offset of the last document of the part holding each term, and the two terms after them.
    let mut stamps = vec![u16::MAX; term_count + 2];
    let (mut tokens, mut phrase_anchors, mut bool_anchors) = (Vec::new(), Vec::new(), Vec::new());
    for_each_doc(docs, range.0, range.1, |doc, text, lf| {
        if part.first_doc == NONE {
            part.first_doc = doc;
        } else if doc - part.first_doc >= u32::from(u16::MAX) {
            let full = std::mem::replace(&mut part, new_part(doc));
            result.parts.push(full);
            stamps.fill(u16::MAX);
        }
        let offset = (doc - part.first_doc) as u16;
        let (doc_word, doc_bit) = (doc as usize / 64, 1u64 << (doc % 64));
        tokens.clear();
        phrase_anchors.clear();
        bool_anchors.clear();
        for_each_token(docs, text, lf, |start, end| {
            let slot = queries.terms.find(docs, start, end);
            let term = slot.term as usize;
            tokens.push(slot.term);
            if slot.flags & PHRASE_ANCHOR != 0 {
                phrase_anchors.push((tokens.len() - 1, term));
            }
            // Only a term's first occurrence in a document counts.
            let first = stamps[term] != offset;
            stamps[term] = offset;
            if first & (slot.flags & BOOL_ANCHOR != 0) {
                bool_anchors.push(term);
            }
            if slot.store & BITMAP != 0 {
                // Setting a bit again is harmless, and a stamp-only term's bit lands in the spare
                // word, so this takes no branch. And no other thread writes this document's words,
                // so it takes no atomic either.
                let bitmap = (slot.store & !BITMAP) as usize;
                let at = if slot.store == STAMP_ONLY {
                    spare
                } else {
                    bitmap * words + doc_word
                };
                bitmaps[at].store(bitmaps[at].load(Relaxed) | doc_bit, Relaxed);
            } else if first {
                let list = &mut part.lists[slot.store as usize];
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
        });
        for &(position, term) in &phrase_anchors {
            let from = plan.phrase_start[term] as usize;
            let to = plan.phrase_start[term + 1] as usize;
            for &(query, offset) in &plan.phrases[from..to] {
                let steps = queries.steps_of(&queries.list[query as usize]);
                let Some(start) = position.checked_sub(offset as usize) else {
                    continue;
                };
                let run = tokens.get(start..start + steps.len());
                let matches = run.is_some_and(|w| w.iter().zip(steps).all(|(&t, s)| t == s.term));
                // An anchor can occur twice in a document, but the document counts once.
                let hits = &mut result.phrase_hits;
                let mut same_doc = hits.iter().rev().take_while(|hit| hit.1 == doc);
                if matches && !same_doc.any(|hit| hit.0 == query) {
                    hits.push((query, doc));
                }
            }
        }
        // The document holds a term exactly when the term's stamp is this document, so a BOOL query
        // is its steps applied to those bits, left to right from the empty set. All six slots are
        // applied without a branch: OR keeps `matches | has`, and AND and NOT keep `matches & has`
        // with `has` flipped for NOT.
        for &term in &bool_anchors {
            let from = plan.bool_start[term] as usize;
            let to = plan.bool_start[term + 1] as usize;
            for packed in &plan.bools[from..to] {
                let mut matches = false;
                for &coded in &packed.terms {
                    let has = stamps[(coded & TERM_BITS) as usize] == offset;
                    let (is_or, is_not) = (coded >> 30 == 0, coded >> 30 == 2);
                    matches = (matches & (has ^ is_not)) | (is_or & (matches | has));
                }
                // Two of a query's anchors can share a document, but the document counts once. A
                // thread's hits ascend, so only its first ten can be among the smallest.
                let hits = &mut part.hits[packed.query as usize];
                if matches && hits.last != offset {
                    hits.last = offset;
                    if usize::from(hits.count) < SMALLEST {
                        add_smallest(&smallest[packed.query as usize], doc);
                    }
                    hits.count += 1;
                    hits.sum += u32::from(offset);
                }
            }
        }
    });
    result.parts.push(part);
    result
}

/// Everything the queries read.
struct Index<'a> {
    store_of: &'a [u32],
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
fn total_hits(results: &[ScanResult], scan_count: usize) -> (Vec<u64>, Vec<u64>) {
    let (mut counts, mut sums) = (vec![0u64; scan_count], vec![0u64; scan_count]);
    for part in results.iter().flat_map(|result| &result.parts) {
        for (slot, hits) in part.hits.iter().enumerate() {
            let count = u64::from(hits.count);
            counts[slot] += count;
            sums[slot] += count * u64::from(part.first_doc) + u64::from(hits.sum);
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
            let store = index.store_of[step.term as usize];
            let other = if store & BITMAP != 0 {
                let from = (store & !BITMAP) as usize * index.words;
                &index.bitmaps[from..from + index.words]
            } else {
                index.read_list(store as usize, &mut self.list);
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
        const INDEX_BIT_MASKS: [u64; 6] = [
            0xAAAA_AAAA_AAAA_AAAA,
            0xCCCC_CCCC_CCCC_CCCC,
            0xF0F0_F0F0_F0F0_F0F0,
            0xFF00_FF00_FF00_FF00,
            0xFFFF_0000_FFFF_0000,
            0xFFFF_FFFF_0000_0000,
        ];
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
    // After the bitmaps, a spare word on its own cache line for each thread.
    let bitmap_words = plan.bitmap_count * words;
    let bitmaps: Vec<AtomicU64> = (0..bitmap_words + threads * 8)
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
            let spare = bitmap_words + i * 8;
            scope.spawn(move || {
                scan_range(docs, range, queries, plan, bitmaps, words, spare, smallest)
            })
        };
        let handles: Vec<_> = (0..threads).map(scan).collect();
        handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect()
    });

    let (hit_start, hits) = collect_hits(&results, queries.list.len());
    let (counts, sums) = total_hits(&results, plan.scan_count);
    let index = Index {
        store_of: &plan.store_of,
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
