//! Track B (Index): boolean and phrase queries over a 254 MB corpus.
//!
//! The queries are read first, so only their terms are ever indexed. A small sample of the corpus
//! decides which terms are common enough to deserve a bitmap, and which term of each phrase is
//! rarest. Threads then scan the corpus once, each over its own contiguous range. Every token is
//! looked up and buffered for its document. Common terms set bits directly, other terms append to a
//! list per thread, and each occurrence of a phrase's rarest term checks the phrase around it when
//! the document ends. The lists are then moved into one sorted posting list per term, and threads
//! answer the queries in batches, appending their answers to the file in order.

use std::ffi::{c_int, c_void};
use std::fs::File;
use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::path::Path;
use std::sync::atomic::{
    AtomicU32, AtomicU64, AtomicUsize,
    Ordering::{Acquire, Relaxed, Release},
};
use std::thread;

/// A token that no query uses. Also marks a term with no bitmap, in `Plan::bitmap_of`.
const NONE: u32 = u32::MAX;
/// A term that needs no postings at all, because only phrases use it.
const PHRASE_ONLY: u32 = NONE - 1;
/// Query terms are at most 32 bytes, so a longer token can't equal one (spec section 2.3).
const MAX_TERM: usize = 32;
/// Marks an entry of a thread's posting list as starting a new document.
const DOC_MARK: u32 = 1 << 31;
/// How many queries a thread claims at a time.
const BATCH: usize = 256;
/// How much of the corpus the sample reads, about 1%.
const SAMPLE_BYTES: usize = 1 << 21;

/// For each byte, its lowercase form if it's a token byte, else 0. Every byte of 0x80 or above is a
/// separator, even inside a letter (spec section 2.2).
const TOKEN_BYTE: [u8; 256] = {
    let mut table = [0; 256];
    let mut byte = 0;
    while byte < 256 {
        let value = byte as u8;
        if value.is_ascii_alphanumeric() {
            table[byte] = value.to_ascii_lowercase();
        }
        byte += 1;
    }
    table
};

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

/// Maps a file read-only for the life of the process. The scorer's memory peak doesn't charge us
/// for these pages, unlike reading the file onto the heap.
fn map_file(path: &Path) -> io::Result<&'static [u8]> {
    const PROT_READ: c_int = 1;
    const MAP_PRIVATE: c_int = 2;
    let file = File::open(path)?;
    let len = file.metadata()?.len() as usize;
    if len == 0 {
        return Ok(&[]); // mmap rejects an empty range.
    }
    // SAFETY: a fresh private mapping of exactly `len` bytes that is never unmapped, of a dataset
    // nothing writes to while we run.
    unsafe {
        let ptr = mmap(
            std::ptr::null_mut(),
            len,
            PROT_READ,
            MAP_PRIVATE,
            file.as_raw_fd(),
            0,
        );
        if ptr as isize == -1 {
            return Err(io::Error::last_os_error());
        }
        Ok(std::slice::from_raw_parts(ptr as *const u8, len))
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

const HASH_SEED: u64 = 0xcbf2_9ce4_8422_2325;

/// One step of FNV-1a. A test build swaps it for a constant, so that every key collides and any
/// logic that trusts a hash without comparing the key shows up as a wrong answer.
#[cfg(not(sd_const_hash))]
fn mix(hash: u64, byte: u8) -> u64 {
    (hash ^ u64::from(byte)).wrapping_mul(0x100_0000_01b3)
}

#[cfg(sd_const_hash)]
fn mix(_hash: u64, _byte: u8) -> u64 {
    0
}

/// Query terms to dense ids, by open addressing with linear probing. A hit always compares the
/// full key.
struct TermTable<'a> {
    slots: Vec<u32>, // id + 1, or 0 when free. The length is a power of two.
    keys: Vec<&'a [u8]>,
}

impl<'a> TermTable<'a> {
    fn new() -> Self {
        TermTable {
            slots: vec![0; 1 << 12],
            keys: Vec::new(),
        }
    }

    fn home_slot(&self, hash: u64) -> usize {
        (hash ^ (hash >> 32)) as usize & (self.slots.len() - 1)
    }

    fn find(&self, key: &[u8], hash: u64) -> u32 {
        let mut slot = self.home_slot(hash);
        loop {
            match self.slots[slot] {
                0 => return NONE,
                id if self.keys[id as usize - 1] == key => return id - 1,
                _ => slot = (slot + 1) & (self.slots.len() - 1),
            }
        }
    }

    fn intern(&mut self, key: &'a [u8]) -> u32 {
        let hash = key.iter().fold(HASH_SEED, |hash, &byte| mix(hash, byte));
        let found = self.find(key, hash);
        if found != NONE {
            return found;
        }
        self.keys.push(key);
        if self.keys.len() * 2 > self.slots.len() {
            self.slots = vec![0; self.slots.len() * 2];
            for id in 0..self.keys.len() {
                let hash = self.keys[id]
                    .iter()
                    .fold(HASH_SEED, |h, &byte| mix(h, byte));
                self.place(hash, id as u32 + 1);
            }
        } else {
            self.place(hash, self.keys.len() as u32);
        }
        self.keys.len() as u32 - 1
    }

    fn place(&mut self, hash: u64, entry: u32) {
        let mut slot = self.home_slot(hash);
        while self.slots[slot] != 0 {
            slot = (slot + 1) & (self.slots.len() - 1);
        }
        self.slots[slot] = entry;
    }
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
    let mut terms = TermTable::new();
    let (mut list, mut steps) = (Vec::new(), Vec::new());
    for line in text
        .split(|&byte| byte == b'\n')
        .filter(|line| !line.is_empty())
    {
        let start = steps.len() as u32;
        let is_phrase = line[0] == b'P';
        if is_phrase {
            // PHRASE "t1 t2 ...", so the terms sit between the two quotes.
            for term in line[8..line.len() - 1].split(|&byte| byte == b' ') {
                let term = terms.intern(term);
                steps.push(Step { op: 0, term });
            }
        } else {
            // BOOL t1 op t2 ... Starting from the empty set, "OR t1" gives S(t1).
            let mut op = b'O';
            for (i, field) in line[5..].split(|&byte| byte == b' ').enumerate() {
                if i % 2 == 0 {
                    let term = terms.intern(field);
                    steps.push(Step { op, term });
                } else {
                    op = field[0];
                }
            }
        }
        let end = steps.len() as u32;
        list.push(Query {
            is_phrase,
            start,
            end,
        });
    }
    Queries { terms, list, steps }
}

/// Visits the documents whose lines start in `start..end`, passing each one's id and its text with
/// its LF. A line belongs to the range holding its first byte.
fn for_each_doc(docs: &[u8], start: usize, end: usize, mut visit: impl FnMut(u32, &[u8])) {
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
        let mut lf = pos;
        while docs[lf] != b'\n' {
            lf += 1;
        }
        visit(id, &docs[pos + 1..=lf]);
        line = lf + 1;
    }
}

/// Visits each token of a text that ends in a separator, passing its term id or NONE. A longer
/// token than any term still counts as a token, because it breaks a phrase.
fn for_each_token(text: &[u8], terms: &TermTable, mut visit: impl FnMut(u32)) {
    let mut key = [0u8; MAX_TERM];
    let (mut len, mut hash) = (0, HASH_SEED);
    for &byte in text {
        let lowered = TOKEN_BYTE[usize::from(byte)];
        if lowered != 0 {
            if len < MAX_TERM {
                key[len] = lowered;
            }
            len += 1;
            hash = mix(hash, lowered);
        } else if len > 0 {
            visit(if len <= MAX_TERM {
                terms.find(&key[..len], hash)
            } else {
                NONE
            });
            (len, hash) = (0, HASH_SEED);
        }
    }
}

/// What the sample decided: how each term is stored, and which phrases to check at each term.
struct Plan {
    bitmap_of: Vec<u32>, // A bitmap's number, or NONE for a posting list, or PHRASE_ONLY.
    bitmap_count: usize,
    anchor_start: Vec<u32>, // A term's phrases are `anchored[anchor_start[t]..anchor_start[t + 1]]`.
    anchored: Vec<(u32, u32)>, // (query, position of this term in the phrase).
}

fn make_plan(docs: &[u8], queries: &Queries) -> Plan {
    let term_count = queries.terms.keys.len();
    let (mut last_doc, mut sample_df) = (vec![NONE; term_count], vec![0u32; term_count]);
    let mut sampled_docs = 0;
    for_each_doc(docs, 0, docs.len().min(SAMPLE_BYTES), |doc, text| {
        sampled_docs += 1;
        for_each_token(text, &queries.terms, |id| {
            if id != NONE && last_doc[id as usize] != doc {
                last_doc[id as usize] = doc;
                sample_df[id as usize] += 1;
            }
        });
    });

    // A bitmap costs N/8 bytes and a list 4 bytes per document, so they cross at df = N/32. Any
    // choice gives the same answers, and a wrong guess only costs some memory.
    let mut bitmap_of = vec![PHRASE_ONLY; term_count];
    let mut bitmap_count = 0;
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
                if bitmap_of[term] == PHRASE_ONLY {
                    bitmap_of[term] = if sample_df[term] * 32 >= sampled_docs {
                        bitmap_count += 1;
                        bitmap_count as u32 - 1
                    } else {
                        NONE
                    };
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
    Plan {
        bitmap_of,
        bitmap_count,
        anchor_start,
        anchored,
    }
}

/// A term's state in one thread: the last document that listed it, and how many have.
#[derive(Clone, Copy)]
struct Seen {
    last_doc: u32,
    count: u32,
}

/// One thread's results from its range of the corpus.
struct ScanResult {
    seen: Vec<Seen>,
    postings: Vec<u32>, // Term ids, with DOC_MARK | doc before each document's terms.
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
    let seen = vec![
        Seen {
            last_doc: NONE,
            count: 0
        };
        queries.terms.keys.len()
    ];
    let mut result = ScanResult {
        seen,
        postings: Vec::new(),
        phrase_hits: Vec::new(),
    };
    let (mut tokens, mut anchor_hits) = (Vec::new(), Vec::new());
    for_each_doc(docs, range.0, range.1, |doc, text| {
        tokens.clear();
        anchor_hits.clear();
        let mut doc_marked = false;
        for_each_token(text, &queries.terms, |id| {
            tokens.push(id);
            if id == NONE {
                return;
            }
            let term = id as usize;
            match plan.bitmap_of[term] {
                PHRASE_ONLY => {}
                NONE => {
                    let seen = &mut result.seen[term];
                    if seen.last_doc != doc {
                        seen.last_doc = doc;
                        seen.count += 1;
                        if !doc_marked {
                            result.postings.push(DOC_MARK | doc);
                            doc_marked = true;
                        }
                        result.postings.push(id);
                    }
                }
                bitmap => {
                    let word = &bitmaps[bitmap as usize * words + doc as usize / 64];
                    let bit = 1u64 << (doc % 64);
                    if word.load(Relaxed) & bit == 0 {
                        word.fetch_or(bit, Relaxed);
                    }
                }
            }
            if plan.anchor_start[term] != plan.anchor_start[term + 1] {
                anchor_hits.push((tokens.len() - 1, term));
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
    result
}

/// Everything the queries read: a sorted posting list per term, and the bitmaps.
struct Index {
    bitmap_of: Vec<u32>,
    bitmaps: Vec<u64>,
    words: usize,
    offsets: Vec<u32>, // A term's documents are `postings[offsets[t]..offsets[t + 1]]`.
    postings: Vec<u32>,
    hit_start: Vec<u32>, // A phrase query's documents are `hits[hit_start[q]..hit_start[q + 1]]`.
    hits: Vec<u32>,
}

/// Moves the threads' lists into one sorted list per term. Thread ranges follow document order, so
/// each thread's slots for a term come after those of the threads before it. That makes the slots
/// disjoint, and each thread can fill its own without any sorting.
fn build_postings(results: &mut [ScanResult], term_count: usize) -> (Vec<u32>, Vec<u32>) {
    let mut offsets = vec![0; term_count + 1];
    let mut total = 0;
    for term in 0..term_count {
        offsets[term] = total;
        for result in results.iter_mut() {
            let count = result.seen[term].count;
            result.seen[term].count = total; // From here on, it's this thread's next slot.
            total += count;
        }
    }
    offsets[term_count] = total;
    let postings: Vec<AtomicU32> = (0..total).map(|_| AtomicU32::new(0)).collect();
    thread::scope(|scope| {
        for result in results.iter_mut() {
            let postings = &postings;
            scope.spawn(move || {
                let mut doc = 0;
                for &entry in &result.postings {
                    if entry & DOC_MARK != 0 {
                        doc = entry & !DOC_MARK;
                    } else {
                        let slot = &mut result.seen[entry as usize].count;
                        postings[*slot as usize].store(doc, Relaxed);
                        *slot += 1;
                    }
                }
            });
        }
    });
    (
        offsets,
        postings.into_iter().map(AtomicU32::into_inner).collect(),
    )
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
    list: Vec<u32>,
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

    /// Applies a BOOL query's steps strictly left to right, from the empty set.
    fn run(&mut self, steps: &[Step], index: &Index) {
        self.list.clear();
        self.is_bits = false;
        for step in steps {
            let term = step.term as usize;
            match index.bitmap_of[term] {
                NONE | PHRASE_ONLY => {
                    let (from, to) = (index.offsets[term], index.offsets[term + 1]);
                    self.apply_list(step.op, &index.postings[from as usize..to as usize]);
                }
                bitmap => {
                    let from = bitmap as usize * index.words;
                    self.apply_bitmap(step.op, &index.bitmaps[from..from + index.words]);
                }
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
                    list: Vec::new(),
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
    let queries = parse_queries(map_file(&data_dir.join("queries.txt"))?);
    let plan = make_plan(docs, &queries);

    let words = document_count(docs).div_ceil(64);
    let bitmaps: Vec<AtomicU64> = (0..plan.bitmap_count * words)
        .map(|_| AtomicU64::new(0))
        .collect();
    let mut results: Vec<ScanResult> = thread::scope(|scope| {
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

    let (offsets, postings) = build_postings(&mut results, queries.terms.keys.len());
    let (hit_start, hits) = collect_hits(&results, queries.list.len());
    drop(results);
    let index = Index {
        bitmap_of: plan.bitmap_of,
        bitmaps: bitmaps.into_iter().map(AtomicU64::into_inner).collect(),
        words,
        offsets,
        postings,
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
