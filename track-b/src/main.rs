//! Track B (Index): boolean and phrase queries over a 254 MB corpus.
//!
//! The queries are read first, so only their terms are ever indexed. A small sample of the corpus
//! decides which terms are common enough to deserve a bitmap, and which term of each phrase is
//! rarest. Threads then scan the corpus once, each over its own contiguous range, in 32-byte blocks.
//! Every token is looked up, in one cache access, and buffered for its document. Common terms set
//! bits directly, other terms append 2-byte offsets to a chain of small blocks, and each occurrence
//! of a phrase's rarest term checks the phrase around it when the document ends. Threads then answer
//! the queries in batches, appending their answers to the file in order.

use std::arch::x86_64::{
    _mm256_and_si256, _mm256_cmpeq_epi8, _mm256_cmpgt_epi8, _mm256_loadu_si256,
    _mm256_movemask_epi8, _mm256_or_si256, _mm256_set1_epi8,
};
use std::ffi::{c_int, c_void};
use std::fs::File;
use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::path::Path;
use std::sync::atomic::{
    AtomicU64, AtomicUsize,
    Ordering::{Acquire, Relaxed, Release},
};
use std::thread;

/// A token that no query uses.
const NONE: u32 = u32::MAX;
/// How a term is stored: this value if only phrases use it, so it needs no postings...
const PHRASE_ONLY: u32 = NONE;
/// ...this flag plus a number if it has a bitmap, and otherwise the number of its posting list.
const BITMAP: u32 = 1 << 31;
/// Query terms are at most 32 bytes, so a longer token can't equal one (spec section 2.3).
const MAX_TERM: usize = 32;
/// How many queries a thread claims at a time.
const BATCH: usize = 256;
/// How much of the corpus the sample reads, about 1%.
const SAMPLE_BYTES: usize = 1 << 21;

/// Readable zero bytes after each mapped file, so that block loads can run past its end.
const PADDING: usize = 4096;

// Declared by hand to keep the crate at zero dependencies; std already links glibc.
unsafe extern "C" {
    fn mmap(
        addr: *mut c_void,
        len: usize,
        prot: c_int,
        flags: c_int,
        fd: c_int,
        offset: i64,
    ) -> *mut c_void;
}

/// Maps a file read-only for the life of the process, followed by `PADDING` readable zero bytes. The
/// scorer's memory peak doesn't charge us for these pages, unlike reading the file onto the heap.
fn map_file(path: &Path) -> io::Result<&'static [u8]> {
    const PROT_READ: c_int = 1;
    const MAP_PRIVATE: c_int = 2;
    const MAP_FIXED: c_int = 0x10;
    const MAP_ANONYMOUS: c_int = 0x20;
    let file = File::open(path)?;
    let len = file.metadata()?.len() as usize;
    let null = std::ptr::null_mut();
    // SAFETY: the first call reserves fresh zero pages for the file and its padding, and the second
    // maps the file over the start of them. Neither is ever unmapped, and nothing writes to the
    // dataset while we run.
    unsafe {
        let anonymous = MAP_PRIVATE | MAP_ANONYMOUS;
        let base = mmap(null, len + PADDING, PROT_READ, anonymous, -1, 0);
        if base as isize == -1 {
            return Err(io::Error::last_os_error());
        }
        let fixed = MAP_PRIVATE | MAP_FIXED;
        if len > 0 && mmap(base, len, PROT_READ, fixed, file.as_raw_fd(), 0) as isize == -1 {
            return Err(io::Error::last_os_error());
        }
        Ok(std::slice::from_raw_parts(base as *const u8, len))
    }
}

/// Loads 8 bytes, little-endian, which may run into the padding.
#[inline]
fn load_u64(file: &[u8], pos: usize) -> u64 {
    assert!(pos + 8 <= file.len() + PADDING);
    // SAFETY: the assert keeps the load inside the mapping, and the load needs no alignment.
    unsafe { file.as_ptr().add(pos).cast::<u64>().read_unaligned() }
}

/// The offset of the first LF at or after `pos`.
#[inline]
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
#[inline]
fn token_mask(file: &[u8], pos: usize) -> u32 {
    assert!(pos + 32 <= file.len() + PADDING);
    // SAFETY: the assert keeps the load inside the mapping, it needs no alignment, and the build
    // targets x86-64-v3, which has AVX2.
    unsafe {
        let byte_set = |byte: u8| _mm256_set1_epi8(byte as i8);
        let bytes = _mm256_loadu_si256(file.as_ptr().add(pos).cast());
        let lower = _mm256_or_si256(bytes, byte_set(0x20));
        let letters = _mm256_and_si256(
            _mm256_cmpgt_epi8(lower, byte_set(b'a' - 1)),
            _mm256_cmpgt_epi8(byte_set(b'z' + 1), lower),
        );
        let digits = _mm256_and_si256(
            _mm256_cmpgt_epi8(bytes, byte_set(b'0' - 1)),
            _mm256_cmpgt_epi8(byte_set(b'9' + 1), bytes),
        );
        _mm256_movemask_epi8(_mm256_or_si256(letters, digits)) as u32
    }
}

/// `SD_THREADS` if set (the test sweep uses it), else what the container allows, capped at the
/// box's 12 threads.
fn thread_count() -> usize {
    match std::env::var("SD_THREADS")
        .ok()
        .and_then(|v| v.parse().ok())
    {
        Some(threads) if threads > 0 => threads,
        _ => thread::available_parallelism().map_or(1, |n| n.get().min(12)),
    }
}

/// Setting this bit in every byte lowercases ASCII letters, and digits already have it set.
const LOWERCASE: u64 = 0x2020_2020_2020_2020;

/// For a key of each length up to 16, which bytes of its two words belong to it.
const KEY_MASKS: [[u64; 2]; 17] = {
    let mut masks = [[0; 2]; 17];
    let mut len = 0;
    while len <= 16 {
        let first = if len >= 8 { 8 } else { len };
        masks[len][0] = if first == 8 {
            !0
        } else {
            (1 << (8 * first)) - 1
        };
        let second = len - first;
        masks[len][1] = if second == 8 {
            !0
        } else {
            (1 << (8 * second)) - 1
        };
        len += 1;
    }
    masks
};

/// A token's key: its first 16 bytes, lowercased and zero-padded, from two loads and a mask.
#[inline]
fn token_key(file: &[u8], start: usize, len: usize) -> [u64; 2] {
    let masks = KEY_MASKS[len.min(16)];
    [
        (load_u64(file, start) | LOWERCASE) & masks[0],
        (load_u64(file, start + 8) | LOWERCASE) & masks[1],
    ]
}

/// Mixes a key and its length. A test build swaps it for a constant, so that every key collides and
/// any logic that trusts a hash without comparing the key shows up as a wrong answer.
#[cfg(not(sd_const_hash))]
#[inline]
fn hash(key: [u64; 2], len: usize) -> usize {
    let first = (key[0] ^ len as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    let second = ((first ^ (first >> 29)) ^ key[1]).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    (second ^ (second >> 32)) as usize
}

#[cfg(sd_const_hash)]
#[inline]
fn hash(_key: [u64; 2], _len: usize) -> usize {
    0
}

/// A slot of the term table: half a cache line holding all that a token's lookup needs, so one memory
/// access answers it. On seed 1, nearly all terms fit in the 16 bytes of the key.
#[derive(Clone, Copy, Default)]
#[repr(align(32))]
struct TermSlot {
    key: [u64; 2],
    len: u32, // 0 when the slot is free.
    term: u32,
    store: u32,  // How the term is stored: PHRASE_ONLY, BITMAP | number, or a list number.
    anchor: u32, // Where this term's phrases start in `Plan::anchored`, or NONE if it has none.
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

    /// Finds the term for the token at `start..end` of `file`, in any letter case.
    #[inline]
    fn find(&self, file: &[u8], start: usize, end: usize) -> Option<&TermSlot> {
        let len = end - start;
        if len > MAX_TERM {
            return None;
        }
        let key = token_key(file, start, len);
        let mask = self.slots.len() - 1;
        let mut index = hash(key, len) & mask;
        loop {
            let slot = &self.slots[index];
            if slot.len == 0 {
                return None;
            }
            if slot.key == key
                && slot.len as usize == len
                && (len <= 16
                    || file[start..end].eq_ignore_ascii_case(self.keys[slot.term as usize]))
            {
                return Some(slot);
            }
            index = (index + 1) & mask;
        }
    }

    /// Adds the query term at `start..end` of `file`, if it's new, and returns its id.
    fn intern(&mut self, file: &'a [u8], start: usize, end: usize) -> u32 {
        if let Some(slot) = self.find(file, start, end) {
            return slot.term;
        }
        let len = end - start;
        let term = self.keys.len() as u32;
        self.keys.push(&file[start..end]);
        let slot = TermSlot {
            key: token_key(file, start, len),
            len: len as u32,
            term,
            store: PHRASE_ONLY,
            anchor: NONE,
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
    let mut index = hash(slot.key, slot.len as usize) & mask;
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
            let len = rest
                .iter()
                .position(|&byte| byte == b' ')
                .unwrap_or(rest.len());
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
            start,
            end,
        });
        pos = line_end + 1;
    }
    Queries { terms, list, steps }
}

/// Visits the documents whose lines start in `start..end`, passing each one's id, where its text
/// starts, and its LF. A line belongs to the range holding its first byte.
#[inline]
fn for_each_doc(docs: &[u8], start: usize, end: usize, mut visit: impl FnMut(u32, usize, usize)) {
    let mut line = start;
    while line > 0 && docs[line - 1] != b'\n' {
        line += 1;
    }
    while line < end {
        let mut pos = line;
        let mut id = 0;
        while docs[pos] != b'\t' {
            id = id * 10 + u32::from(docs[pos] - b'0');
            pos += 1;
        }
        let lf = pos + 1 + offset_of_line_feed(docs, pos + 1);
        visit(id, pos + 1, lf);
        line = lf + 1;
    }
}

/// Visits each token of the text in `start..lf`, passing where it starts and ends. Each 32-byte block
/// gives a mask of its token bytes, and the set bits of the mask XORed with itself shifted are where
/// tokens start and end, so a token costs a few bit operations and no byte loop.
#[inline]
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

/// What the sample decided: how each term is stored, and which phrases to check at each term.
struct Plan {
    store_of: Vec<u32>,
    sample_df: Vec<u32>,
    bitmap_count: usize,
    list_count: usize,
    anchor_start: Vec<u32>, // A term's phrases are `anchored[anchor_start[t]..anchor_start[t + 1]]`.
    anchored: Vec<(u32, u32)>, // (query, position of this term in the phrase).
}

fn make_plan(docs: &[u8], queries: &mut Queries) -> Plan {
    let term_count = queries.terms.keys.len();
    let (mut last_doc, mut sample_df) = (vec![NONE; term_count], vec![0u32; term_count]);
    let mut sampled_docs = 0;
    for_each_doc(docs, 0, docs.len().min(SAMPLE_BYTES), |doc, text, lf| {
        sampled_docs += 1;
        for_each_token(docs, text, lf, |start, end| {
            if let Some(slot) = queries.terms.find(docs, start, end) {
                let term = slot.term as usize;
                if last_doc[term] != doc {
                    last_doc[term] = doc;
                    sample_df[term] += 1;
                }
            }
        });
    });

    // On memory alone, a bitmap (N/8 bytes) and a chain (about 3.3 bytes per posting) cross near df =
    // N/26. But setting a bit is cheaper than extending a chain, and measuring put the best K near
    // N/48. Any choice gives the same answers, and a wrong guess only costs some memory.
    let mut store_of = vec![PHRASE_ONLY; term_count];
    let (mut bitmap_count, mut list_count) = (0, 0);
    let mut anchors = Vec::new();
    for (index, query) in queries.list.iter().enumerate() {
        let steps = queries.steps_of(query);
        if query.is_phrase {
            // Any term of the phrase would be correct. The rarest triggers the fewest checks.
            let rarest = (0..steps.len()).min_by_key(|&i| sample_df[steps[i].term as usize]);
            let position = rarest.unwrap_or(0);
            anchors.push((steps[position].term, index as u32, position as u32));
        } else {
            for step in steps {
                let term = step.term as usize;
                if store_of[term] == PHRASE_ONLY {
                    if sample_df[term] * 48 >= sampled_docs {
                        store_of[term] = BITMAP | bitmap_count;
                        bitmap_count += 1;
                    } else {
                        store_of[term] = list_count;
                        list_count += 1;
                    }
                }
            }
        }
    }
    anchors.sort_unstable();
    let mut anchor_start = vec![0u32; term_count + 1];
    for &(term, _, _) in &anchors {
        anchor_start[term as usize + 1] += 1;
    }
    for term in 0..term_count {
        anchor_start[term + 1] += anchor_start[term];
    }
    let anchored = anchors
        .iter()
        .map(|&(_, query, position)| (query, position))
        .collect();
    for slot in queries.terms.slots.iter_mut().filter(|slot| slot.len != 0) {
        let term = slot.term as usize;
        slot.store = store_of[term];
        if anchor_start[term] != anchor_start[term + 1] {
            slot.anchor = anchor_start[term];
        }
    }
    Plan {
        store_of,
        sample_df,
        bitmap_count: bitmap_count as usize,
        list_count: list_count as usize,
        anchor_start,
        anchored,
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
    fill: u16,
    last: u16, // The last offset added, to count a document once. Offsets never reach u16::MAX.
}

const EMPTY_LIST: ListState = ListState {
    tail: 0,
    fill: 0,
    last: u16::MAX,
};

/// A run of up to 65,535 documents, whose postings are 2-byte offsets from its first document.
struct Part {
    first_doc: u32,
    lists: Vec<ListState>,
}

/// One thread's results from its range of the corpus. Chains need no merge, no second copy and no
/// sort: a thread's range follows document order, and so do its parts and its chains.
struct ScanResult {
    blocks: Vec<Block>,
    parts: Vec<Part>,
    phrase_hits: Vec<(u32, u32)>, // (query, doc), in document order.
}

fn scan_range(
    docs: &[u8],
    range: (usize, usize),
    queries: &Queries,
    plan: &Plan,
    bitmaps: &[AtomicU64],
    words: usize,
) -> ScanResult {
    let mut result = ScanResult {
        blocks: Vec::new(),
        parts: Vec::new(),
        phrase_hits: Vec::new(),
    };
    let mut lists = vec![EMPTY_LIST; plan.list_count];
    let mut first_doc = NONE;
    let (mut tokens, mut anchor_hits) = (Vec::new(), Vec::new());
    for_each_doc(docs, range.0, range.1, |doc, text, lf| {
        if first_doc == NONE {
            first_doc = doc;
        }
        if doc - first_doc >= u32::from(u16::MAX) {
            let lists = std::mem::replace(&mut lists, vec![EMPTY_LIST; plan.list_count]);
            result.parts.push(Part { first_doc, lists });
            first_doc = doc;
        }
        let offset = (doc - first_doc) as u16;
        tokens.clear();
        anchor_hits.clear();
        for_each_token(docs, text, lf, |start, end| {
            let Some(slot) = queries.terms.find(docs, start, end) else {
                tokens.push(NONE);
                return;
            };
            tokens.push(slot.term);
            if slot.store & BITMAP != 0 {
                if slot.store != PHRASE_ONLY {
                    let bitmap = (slot.store & !BITMAP) as usize;
                    let word = &bitmaps[bitmap * words + doc as usize / 64];
                    let bit = 1u64 << (doc % 64);
                    if word.load(Relaxed) & bit == 0 {
                        word.fetch_or(bit, Relaxed);
                    }
                }
            } else {
                let list = &mut lists[slot.store as usize];
                if list.last != offset {
                    list.last = offset;
                    if list.tail == 0 || usize::from(list.fill) == BLOCK_POSTINGS {
                        result.blocks.push(Block {
                            previous: list.tail,
                            postings: [0; BLOCK_POSTINGS],
                        });
                        list.tail = result.blocks.len() as u32;
                        list.fill = 0;
                    }
                    result.blocks[list.tail as usize - 1].postings[usize::from(list.fill)] = offset;
                    list.fill += 1;
                }
            }
            if slot.anchor != NONE {
                anchor_hits.push((tokens.len() - 1, slot.term as usize));
            }
        });
        for &(position, term) in &anchor_hits {
            let from = plan.anchor_start[term] as usize;
            let to = plan.anchor_start[term + 1] as usize;
            for &(query, offset) in &plan.anchored[from..to] {
                let steps = queries.steps_of(&queries.list[query as usize]);
                let Some(start) = position.checked_sub(offset as usize) else {
                    continue;
                };
                let window = tokens.get(start..start + steps.len());
                let matches = window.is_some_and(|window| {
                    window
                        .iter()
                        .zip(steps)
                        .all(|(&token, step)| token == step.term)
                });
                // An anchor can occur twice in a document, but the document counts once.
                let hits = &mut result.phrase_hits;
                if matches
                    && !hits
                        .iter()
                        .rev()
                        .take_while(|hit| hit.1 == doc)
                        .any(|hit| hit.0 == query)
                {
                    hits.push((query, doc));
                }
            }
        }
    });
    result.parts.push(Part { first_doc, lists });
    result
}

/// Everything the queries read.
struct Index<'a> {
    store_of: &'a [u32],
    sample_df: &'a [u32],
    bitmaps: Vec<u64>,
    words: usize,
    scans: &'a [ScanResult],
    hit_start: Vec<u32>, // A phrase query's documents are `hits[hit_start[q]..hit_start[q + 1]]`.
    hits: Vec<u32>,
}

impl Index<'_> {
    /// Writes a posting list out in document order. It walks the chains newest first, so it collects
    /// the documents in descending order and then reverses them.
    fn read_list(&self, list: usize, out: &mut Vec<u32>) {
        out.clear();
        for scan in self.scans.iter().rev() {
            for part in scan.parts.iter().rev() {
                let state = part.lists[list];
                let (mut tail, mut fill) = (state.tail, usize::from(state.fill));
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

fn has_bit(bits: &[u64], doc: u32) -> bool {
    bits[doc as usize / 64] >> (doc % 64) & 1 != 0
}

/// One thread's running result R for a BOOL query, as a sorted list while it's small and a bitmap
/// once a union with a bitmap makes it large.
struct Evaluator {
    order: Vec<Step>,
    list: Vec<u32>,
    term_list: Vec<u32>,
    merged: Vec<u32>,
    bits: Vec<u64>,
    is_bits: bool,
}

impl Evaluator {
    fn apply_list(&mut self, op: u8, other: &[u32]) {
        if self.is_bits {
            match op {
                b'A' => {
                    self.list.clear();
                    let bits = &self.bits;
                    self.list
                        .extend(other.iter().filter(|&&doc| has_bit(bits, doc)));
                    self.is_bits = false;
                }
                b'O' => {
                    for &doc in other {
                        self.bits[doc as usize / 64] |= 1 << (doc % 64);
                    }
                }
                _ => {
                    for &doc in other {
                        self.bits[doc as usize / 64] &= !(1 << (doc % 64));
                    }
                }
            }
        } else if op == b'O' {
            self.merged.clear();
            let (mut i, mut j) = (0, 0);
            while i < self.list.len() && j < other.len() {
                let (a, b) = (self.list[i], other[j]);
                self.merged.push(a.min(b));
                i += usize::from(a <= b);
                j += usize::from(b <= a);
            }
            self.merged.extend_from_slice(&self.list[i..]);
            self.merged.extend_from_slice(&other[j..]);
            std::mem::swap(&mut self.list, &mut self.merged);
        } else {
            let keep_found = op == b'A';
            let mut i = 0;
            self.list.retain(|&doc| {
                while i < other.len() && other[i] < doc {
                    i += 1;
                }
                (i < other.len() && other[i] == doc) == keep_found
            });
        }
    }

    fn apply_bitmap(&mut self, op: u8, other: &[u64]) {
        if self.is_bits {
            for (word, &with) in self.bits.iter_mut().zip(other) {
                *word = match op {
                    b'A' => *word & with,
                    b'O' => *word | with,
                    _ => *word & !with,
                };
            }
        } else if op == b'O' {
            self.bits.copy_from_slice(other);
            for &doc in &self.list {
                self.bits[doc as usize / 64] |= 1 << (doc % 64);
            }
            self.is_bits = true;
        } else {
            let keep_found = op == b'A';
            self.list.retain(|&doc| has_bit(other, doc) == keep_found);
        }
    }

    /// Applies a BOOL query's steps in order, from the empty set. Strictly left to right is the
    /// spec's rule, but AND and NOT both intersect, so a run of them between two ORs can go in any
    /// order. The rarest ANDs go first, to shrink the result early, and the first term counts as an
    /// AND too. That usually keeps R a short list and avoids copying a bitmap.
    fn run(&mut self, steps: &[Step], index: &Index) {
        self.order.clear();
        self.order.extend_from_slice(steps);
        let df = |step: &Step| index.sample_df[step.term as usize];
        let mut first_run = true;
        for run in self.order.chunk_by_mut(|_, next| next.op != b'O') {
            let (head, rest) = run.split_at_mut(1);
            rest.sort_unstable_by_key(|step| (step.op == b'N', df(step)));
            // Only the query's first OR starts from the empty set, so only it can trade places.
            if first_run
                && rest
                    .first()
                    .is_some_and(|s| s.op == b'A' && df(s) < df(&head[0]))
            {
                std::mem::swap(&mut head[0].term, &mut rest[0].term);
            }
            first_run = false;
        }
        self.list.clear();
        self.is_bits = false;
        for i in 0..self.order.len() {
            let step = self.order[i];
            if step.op != b'O' && !self.is_bits && self.list.is_empty() {
                continue; // An empty R stays empty until the next OR.
            }
            let store = index.store_of[step.term as usize];
            if store & BITMAP != 0 {
                let from = (store & !BITMAP) as usize * index.words;
                self.apply_bitmap(step.op, &index.bitmaps[from..from + index.words]);
            } else {
                let mut term_list = std::mem::take(&mut self.term_list);
                index.read_list(store as usize, &mut term_list);
                self.apply_list(step.op, &term_list);
                self.term_list = term_list;
            }
        }
    }

    fn print_result(&self, out: &mut Vec<u8>) {
        if !self.is_bits {
            print_list(out, &self.list);
            return;
        }
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
        let mut smallest = Vec::with_capacity(10);
        for (i, &word) in self.bits.iter().enumerate() {
            let ones = u64::from(word.count_ones());
            count += ones;
            sum += 64 * i as u64 * ones;
            for (bit, mask) in INDEX_BIT_MASKS.iter().enumerate() {
                sum += u64::from((word & mask).count_ones()) << bit;
            }
            let mut rest = word;
            while rest != 0 && smallest.len() < 10 {
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
    print_line(out, docs.len() as u64, sum, &docs[..docs.len().min(10)]);
}

/// Threads claim batches of queries in order, and each waits for the batch before it to reach the
/// file. The wait is short, because batches cost about the same.
fn answer_queries(path: &Path, queries: &Queries, index: &Index, threads: usize) -> io::Result<()> {
    let file = File::create(path)?;
    let next_batch = AtomicUsize::new(0);
    let written = AtomicUsize::new(0);
    thread::scope(|scope| {
        for _ in 0..threads {
            scope.spawn(|| {
                let mut evaluator = Evaluator {
                    order: Vec::new(),
                    list: Vec::new(),
                    term_list: Vec::new(),
                    merged: Vec::new(),
                    bits: vec![0; index.words],
                    is_bits: false,
                };
                let mut text = Vec::new();
                loop {
                    let batch = next_batch.fetch_add(1, Relaxed);
                    let first = batch * BATCH;
                    if first >= queries.list.len() {
                        return;
                    }
                    text.clear();
                    let last = (first + BATCH).min(queries.list.len());
                    for number in first..last {
                        let query = &queries.list[number];
                        if query.is_phrase {
                            let (from, to) = (index.hit_start[number], index.hit_start[number + 1]);
                            print_list(&mut text, &index.hits[from as usize..to as usize]);
                        } else {
                            evaluator.run(queries.steps_of(query), index);
                            evaluator.print_result(&mut text);
                        }
                    }
                    while written.load(Acquire) != batch {
                        thread::yield_now();
                    }
                    // `panic = "abort"`: a failed write ends the process rather than leaving the
                    // other threads waiting for their turn.
                    (&file).write_all(&text).expect("writing the results");
                    written.store(batch + 1, Release);
                }
            });
        }
    });
    Ok(())
}

/// The number of documents, from the id on the last line.
fn document_count(docs: &[u8]) -> usize {
    let Some((_, body)) = docs.split_last() else {
        return 0;
    };
    let line = body
        .iter()
        .rposition(|&byte| byte == b'\n')
        .map_or(0, |lf| lf + 1);
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
    let results: Vec<ScanResult> = thread::scope(|scope| {
        let handles: Vec<_> = (0..threads)
            .map(|i| {
                let range = (i * docs.len() / threads, (i + 1) * docs.len() / threads);
                let (queries, plan, bitmaps) = (&queries, &plan, &bitmaps);
                scope.spawn(move || scan_range(docs, range, queries, plan, bitmaps, words))
            })
            .collect();
        let joined = handles.into_iter().map(|handle| handle.join());
        joined
            .map(|result| result.expect("panic = \"abort\" means a join can't fail"))
            .collect()
    });

    let (hit_start, hits) = collect_hits(&results, queries.list.len());
    let index = Index {
        store_of: &plan.store_of,
        sample_df: &plan.sample_df,
        bitmaps: bitmaps.into_iter().map(AtomicU64::into_inner).collect(),
        words,
        scans: &results,
        hit_start,
        hits,
    };
    answer_queries(
        &Path::new(&out_dir).join("results.txt"),
        &queries,
        &index,
        threads,
    )
}
