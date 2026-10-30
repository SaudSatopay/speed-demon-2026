//! Track A (Ingest): per-hour, per-endpoint latency percentiles over an HTTP request log.
//!
//! The log is memory-mapped and cut into segments that threads claim from an atomic cursor, so a
//! thread that falls behind simply claims fewer. Each line is read at fixed offsets where the format
//! allows it, and with block loads where it doesn't. One lock-free table, shared by all threads,
//! turns endpoints into ids, and each line becomes one 32-bit record in a shared array for its hour,
//! where each thread reserves small blocks. When the log is done, threads claim hours: each sorts one
//! hour's array in place, formats it, drops it, and waits for its turn to append to the file. So the
//! output is in a fixed order whatever the scheduling, and never sits in memory whole.

use std::arch::x86_64::{
    __m256i, _mm256_cmpeq_epi8, _mm256_loadu_si256, _mm256_movemask_epi8, _mm256_set1_epi8,
};
use std::ffi::{c_int, c_void};
use std::fs::File;
use std::hint;
use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{
    AtomicBool, AtomicU32, AtomicU64, AtomicUsize,
    Ordering::{Acquire, Relaxed, Release},
};
use std::thread;

/// Every seed's log spans exactly 2026-10-30T18:00Z to 2026-11-02T17:59Z (spec section 2).
const HOURS: usize = 72;
const DATES: [&str; 4] = ["2026-10-30", "2026-10-31", "2026-11-01", "2026-11-02"];

/// Records a thread reserves at a time from an hour's array, and writes without contention.
const BLOCK_RECORDS: usize = 256;

/// A 32-bit record is one request, from the top: the endpoint's id, the latency, and the error flag
/// in bit 0. Once the id is a byte-order rank, plain integer order sorts records by endpoint and then
/// by latency. About 3% of latencies don't fit, so their records hold the highest latency value, and
/// the real one goes in a side list. These widths suit a table of `slots` endpoints.
#[derive(Clone, Copy)]
struct RecordFormat {
    id_shift: u32,
    big_latency: u32,
}

impl RecordFormat {
    fn new(slots: usize) -> Self {
        let id_shift = 32 - slots.trailing_zeros();
        RecordFormat {
            id_shift,
            big_latency: (1 << (id_shift - 1)) - 1,
        }
    }

    #[inline]
    fn latency(self, record: u32) -> u32 {
        (record >> 1) & self.big_latency
    }
}

/// All the records of one hour, in one array that threads reserve blocks of. Its length is a generous
/// guess, because untouched pages cost nothing, and if a log beats the guess, the pass restarts with
/// a longer array. A block's unused end stays zero, which no real record is, since slot 0 is never an
/// id. So after sorting, the zeros are at the front and easy to skip.
struct HourRecords {
    slots: Vec<AtomicU32>,
    next: AtomicUsize,
}

/// A thread's unfilled block of one hour's array, and its latencies too big for a record.
#[derive(Clone, Default)]
struct HourWriter {
    next: usize,
    end: usize,
    big_latencies: Vec<u64>, // (id << 32 | latency), for the records holding `big_latency`.
}

/// Readable zero bytes after the log, so that block loads can run past its last line.
const PADDING: usize = 4096;

/// A method's length, from its 4th byte, which tells the seven apart: GET and PUT have a space there,
/// POST and HEAD have T and D, PATCH has C, DELETE has E, and OPTIONS has I.
const METHOD_LEN: [u8; 256] = {
    let mut table = [0; 256];
    table[b' ' as usize] = 3;
    table[b'T' as usize] = 4;
    table[b'D' as usize] = 4;
    table[b'C' as usize] = 5;
    table[b'E' as usize] = 6;
    table[b'I' as usize] = 7;
    table
};

// Declared by hand to keep the crate at zero dependencies; std already links glibc.
unsafe extern "C" {
    fn prctl(option: c_int, arg2: u64, arg3: u64, arg4: u64, arg5: u64) -> c_int;
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
/// scorer's memory peak doesn't charge us for these pages, unlike reading the log onto the heap.
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
fn load_u64(log: &[u8], pos: usize) -> u64 {
    assert!(pos + 8 <= log.len() + PADDING);
    // SAFETY: the assert keeps the load inside the mapping, and the load needs no alignment.
    unsafe { log.as_ptr().add(pos).cast::<u64>().read_unaligned() }
}

/// The offset of the next space at or after `pos`. Endpoints average 34 bytes, and 94% of them end
/// within 64, so two 32-byte loads almost always settle it, with no byte loop to mispredict.
#[inline]
fn offset_of_space(log: &[u8], pos: usize) -> usize {
    let mut offset = 0;
    loop {
        assert!(pos + offset + 64 <= log.len() + PADDING);
        // SAFETY: the assert keeps both loads inside the mapping, they need no alignment, and the
        // build targets x86-64-v3, which has AVX2.
        let mask = unsafe {
            let spaces = _mm256_set1_epi8(b' ' as i8);
            let at = log.as_ptr().add(pos + offset).cast::<__m256i>();
            let low = _mm256_cmpeq_epi8(_mm256_loadu_si256(at), spaces);
            let high = _mm256_cmpeq_epi8(_mm256_loadu_si256(at.add(1)), spaces);
            let (low, high) = (_mm256_movemask_epi8(low), _mm256_movemask_epi8(high));
            u64::from(low as u32) | u64::from(high as u32) << 32
        };
        if mask != 0 {
            return offset + mask.trailing_zeros() as usize;
        }
        offset += 64;
    }
}

/// Parses the digits in the low `count` bytes of `word` (1 to 8 of them), in three multiplications.
#[inline]
fn parse_digits(word: u64, count: usize) -> u64 {
    let digits = (word << (64 - 8 * count)) & 0x0F0F_0F0F_0F0F_0F0F;
    let pairs = digits.wrapping_mul((10 << 8) + 1) >> 8 & 0x00FF_00FF_00FF_00FF;
    let quads = pairs.wrapping_mul((100 << 16) + 1) >> 16 & 0x0000_FFFF_0000_FFFF;
    quads.wrapping_mul((10000 << 32) + 1) >> 32
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

/// Mixes an endpoint's length and three words of it: its start, middle and end, because templated
/// paths share long prefixes. A test build swaps it for a constant, so that every key collides and
/// any logic that trusts a hash without comparing the key shows up as a wrong answer.
#[cfg(not(sd_const_hash))]
#[inline]
fn hash(words: [u64; 3], len: usize) -> u64 {
    words.iter().fold(len as u64, |hash, &word| {
        let mixed = (hash ^ word).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        mixed ^ (mixed >> 29)
    })
}

#[cfg(sd_const_hash)]
#[inline]
fn hash(_words: [u64; 3], _len: usize) -> u64 {
    0
}

/// Slots to start with: the spec promises 1,842 endpoints per seed. A test build starts tiny, to
/// exercise the restart that more endpoints would need.
const INITIAL_SLOTS: usize = if cfg!(sd_const_hash) { 1 << 4 } else { 1 << 12 };
const STRIDE: usize = 8; // Words per slot, which is one cache line.
const INLINE_BYTES: usize = 56;
const EMPTY: u64 = 0;
const BUSY: u64 = 1;

/// Endpoints to ids, shared by all threads with no locks. Each slot is a cache line: a header, then
/// the first 56 bytes of the key, which is all of it for 94% of the endpoints, so a hit usually needs
/// no access to the log. Keys are always compared in full, because templated paths share prefixes,
/// suffixes and lengths. A slot's number is the endpoint's id, and a published slot never changes.
struct EndpointTable<'a> {
    log: &'a [u8],
    words: Vec<AtomicU64>,
    first: usize, // The word where the first slot starts, on a cache line boundary.
    offsets: Vec<AtomicU64>, // Where each slot's key first occurred in the log.
    mask: usize,
    used: AtomicUsize,
    overflowed: AtomicBool,
}

impl<'a> EndpointTable<'a> {
    fn new(log: &'a [u8], slots: usize) -> Self {
        let zeroed = vec![0u64; slots * STRIDE + STRIDE];
        let words: Vec<AtomicU64> = zeroed.into_iter().map(AtomicU64::new).collect();
        EndpointTable {
            log,
            first: words.as_ptr().align_offset(64),
            words,
            offsets: (0..slots).map(|_| AtomicU64::new(0)).collect(),
            mask: slots - 1,
            used: AtomicUsize::new(0),
            overflowed: AtomicBool::new(false),
        }
    }

    /// The id of the endpoint at `start..end`, adding it if it's new.
    #[inline]
    fn id_of(&self, start: usize, end: usize) -> usize {
        let len = end - start;
        let words = if len >= 8 {
            [
                load_u64(self.log, start),
                load_u64(self.log, start + (len - 8) / 2),
                load_u64(self.log, end - 8),
            ]
        } else {
            [load_u64(self.log, end - 8) >> (64 - 8 * len); 3]
        };
        let hash = hash(words, len);
        // The top half of the hash and the length, plus 2 to stay clear of EMPTY and BUSY.
        let published = (hash & 0xFFFF_FFFF_0000_0000) | (len as u64 + 2);
        let mut slot = (hash as usize & self.mask).max(1); // Slot 0 stays free: see HourRecords.
        loop {
            let base = self.first + slot * STRIDE;
            let mut header = self.words[base].load(Acquire);
            if header == EMPTY {
                if self.used.load(Relaxed) * 8 >= (self.mask + 1) * 7 {
                    self.overflowed.store(true, Relaxed);
                    return 0; // This whole pass will be thrown away.
                }
                let claim = self.words[base].compare_exchange(EMPTY, BUSY, Acquire, Acquire);
                match claim {
                    Ok(_) => {
                        self.fill_slot(slot, base, start, len);
                        self.words[base].store(published, Release);
                        return slot;
                    }
                    Err(current) => header = current,
                }
            }
            while header == BUSY {
                hint::spin_loop();
                header = self.words[base].load(Acquire);
            }
            if header == published && self.key_matches(slot, base, start, len) {
                return slot;
            }
            slot = ((slot + 1) & self.mask).max(1);
        }
    }

    /// One inline word of a key: the bytes past the key's end are masked off, so that a key's words
    /// don't depend on what follows it in the log.
    #[inline]
    fn key_word(&self, start: usize, len: usize, word: usize) -> u64 {
        let loaded = load_u64(self.log, start + 8 * word);
        let bytes = len - 8 * word;
        if bytes >= 8 {
            loaded
        } else {
            loaded & ((1 << (8 * bytes)) - 1)
        }
    }

    fn fill_slot(&self, slot: usize, base: usize, start: usize, len: usize) {
        self.used.fetch_add(1, Relaxed);
        self.offsets[slot].store(start as u64, Relaxed);
        for word in 0..len.min(INLINE_BYTES).div_ceil(8) {
            self.words[base + 1 + word].store(self.key_word(start, len, word), Relaxed);
        }
    }

    #[inline]
    fn key_matches(&self, slot: usize, base: usize, start: usize, len: usize) -> bool {
        if len > INLINE_BYTES {
            let first = self.offsets[slot].load(Relaxed) as usize;
            return self.log[first..first + len] == self.log[start..start + len];
        }
        (0..len.div_ceil(8)).all(|word| {
            self.words[base + 1 + word].load(Relaxed) == self.key_word(start, len, word)
        })
    }

    /// Every endpoint in byte order (`[u8]` compares as unsigned bytes, with a proper prefix first),
    /// and each slot's position in that order, counting from 1.
    fn sorted_keys(&self) -> (Vec<&'a [u8]>, Vec<u32>) {
        let mut keys: Vec<(&[u8], usize)> = Vec::new();
        for slot in 0..=self.mask {
            let header = self.words[self.first + slot * STRIDE].load(Acquire);
            if header != EMPTY {
                let len = (header & 0xFFFF_FFFF) as usize - 2;
                let start = self.offsets[slot].load(Relaxed) as usize;
                keys.push((&self.log[start..start + len], slot));
            }
        }
        keys.sort_unstable();
        let mut rank = vec![0; self.mask + 1];
        for (position, &(_, slot)) in keys.iter().enumerate() {
            rank[slot] = position as u32 + 1;
        }
        (keys.into_iter().map(|(key, _)| key).collect(), rank)
    }
}

/// A zeroed array of atomics, whose pages stay untouched, and out of the memory peak, until written.
fn zeroed_atomics(len: usize) -> Vec<AtomicU32> {
    let mut zeroed = std::mem::ManuallyDrop::new(vec![0u32; len]);
    let (pointer, capacity) = (zeroed.as_mut_ptr(), zeroed.capacity());
    // SAFETY: the allocation moves to the new Vec, and an AtomicU32 has the same size and alignment
    // as a u32, so its layout is the same.
    unsafe { Vec::from_raw_parts(pointer.cast::<AtomicU32>(), len, capacity) }
}

/// Hours since the start of the span, read from four timestamp bytes without parsing a date.
/// Oct 30, Oct 31, Nov 1 and Nov 2 differ in the last digit of the month and of the day:
/// (month & 1) + (day & 3) gives 0, 1, 2 and 3.
#[inline]
fn hour_index(log: &[u8], line: usize) -> usize {
    let bytes = load_u64(log, line + 6).to_le_bytes(); // Bytes 6 to 13: M-DDTHH:
    let day = (bytes[0] & 1) + (bytes[3] & 3);
    let hour = 10 * (bytes[5] & 15) + (bytes[6] & 15);
    24 * usize::from(day) + usize::from(hour) - 18
}

/// Claims segments until the log runs out, and returns this thread's big latencies for each hour.
/// Each line is `ts method endpoint status latency`, where the timestamp is always 27 bytes.
fn parse_segments(
    log: &[u8],
    table: &EndpointTable,
    hours: &[HourRecords],
    cursor: &AtomicUsize,
    segment: usize,
    format: RecordFormat,
) -> Vec<Vec<u64>> {
    let mut writers = vec![HourWriter::default(); HOURS];
    loop {
        let start = cursor.fetch_add(segment, Relaxed);
        if start >= log.len() {
            return writers
                .into_iter()
                .map(|writer| writer.big_latencies)
                .collect();
        }
        let end = (start + segment).min(log.len());
        // A line belongs to the segment holding its first byte, so skip the one already started.
        // The log ends in LF, so this stops at the end of the log at the latest.
        let mut line = start;
        while line > 0 && log[line - 1] != b'\n' {
            line += 1;
        }
        while line < end {
            let key_start = line + 29 + usize::from(METHOD_LEN[usize::from(log[line + 31])]);
            let key_end = key_start + offset_of_space(log, key_start);
            let id = table.id_of(key_start, key_end) as u32;
            let is_error = u32::from(log[key_end + 1] == b'5'); // Statuses are 100 to 599.
            // Three status digits sit between two spaces, so the latency starts 5 bytes on. Up to 8
            // digits and their LF fit in one load, which covers all but the rarest latencies.
            let digits_at = key_end + 5;
            let word = load_u64(log, digits_at);
            let line_feeds = word ^ 0x0A0A_0A0A_0A0A_0A0A;
            let found = line_feeds.wrapping_sub(0x0101_0101_0101_0101)
                & !line_feeds
                & 0x8080_8080_8080_8080;
            let (latency, digits) = if found != 0 {
                let digits = found.trailing_zeros() as usize / 8;
                (parse_digits(word, digits), digits)
            } else {
                let mut digits = 8;
                let mut value = parse_digits(word, 8);
                while log[digits_at + digits] != b'\n' {
                    value = value * 10 + u64::from(log[digits_at + digits] - b'0');
                    digits += 1;
                }
                (value, digits)
            };
            let hour = hour_index(log, line);
            let writer = &mut writers[hour];
            let mut stored = latency as u32;
            if stored >= format.big_latency {
                writer.big_latencies.push(u64::from(id) << 32 | latency);
                stored = format.big_latency;
            }
            if writer.next == writer.end {
                writer.next = hours[hour].next.fetch_add(BLOCK_RECORDS, Relaxed);
                writer.end = writer.next + BLOCK_RECORDS;
                if writer.end > hours[hour].slots.len() {
                    table.overflowed.store(true, Relaxed);
                    (writer.next, writer.end) = (0, BLOCK_RECORDS); // This pass will be thrown away.
                }
            }
            let record = id << format.id_shift | stored << 1 | is_error;
            hours[hour].slots[writer.next].store(record, Relaxed);
            writer.next += 1;
            line = digits_at + digits + 1;
        }
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

/// Appends one hour's output lines, given its records in integer order and its big latencies sorted
/// by endpoint rank and then value.
fn format_hour(
    hour: usize,
    records: &[u32],
    big_latencies: &[u64],
    endpoints: &[&[u8]],
    format: RecordFormat,
    text: &mut Vec<u8>,
) {
    let clock = hour + 18;
    let stamp = format!("{}T{:02}:00:00Z ", DATES[clock / 24], clock % 24);
    let mut big = big_latencies.iter().map(|&entry| entry & 0xFFFF_FFFF);
    let mut bigs = Vec::new();
    for group in records.chunk_by(|a, b| a >> format.id_shift == b >> format.id_shift) {
        let n = group.len();
        // A group's big latencies sort last, and are next in the side list, which is in the same order.
        let is_big = |&&record: &&u32| format.latency(record) == format.big_latency;
        let big_count = group.iter().rev().take_while(is_big).count();
        bigs.clear();
        bigs.extend(big.by_ref().take(big_count));
        // Nearest rank, 1-indexed: ceil(p * n / 100).
        let percentile = |p: usize| {
            let index = (p * n + 99) / 100 - 1;
            match bigs.get(index.wrapping_sub(n - big_count)) {
                Some(&value) => value,
                None => u64::from(format.latency(group[index])),
            }
        };
        let errors: u64 = group.iter().map(|record| u64::from(record & 1)).sum();
        text.extend_from_slice(stamp.as_bytes());
        text.extend_from_slice(endpoints[(group[0] >> format.id_shift) as usize - 1]);
        for value in [
            n as u64,
            percentile(50),
            percentile(95),
            percentile(99),
            errors,
            errors * 1_000_000 / n as u64,
        ] {
            text.push(b' ');
            push_decimal(text, value);
        }
        text.push(b'\n');
    }
}

/// Threads claim hours in order, and each waits for the hour before it to reach the file. The wait
/// is short, because hours cost about the same, so spinning with a yield is enough. Each hour's array
/// is dropped once formatted, so the output on tmpfs grows as the records shrink, and the two don't
/// add up in the memory peak.
fn write_summary(
    path: &Path,
    table: &EndpointTable,
    hours: Vec<HourRecords>,
    big_lists: &[Vec<Vec<u64>>],
    format: RecordFormat,
    threads: usize,
) -> io::Result<()> {
    let (endpoints, rank) = table.sorted_keys();
    let hours: Vec<Mutex<Option<HourRecords>>> = hours
        .into_iter()
        .map(|hour| Mutex::new(Some(hour)))
        .collect();
    let file = File::create(path)?;
    let next_hour = AtomicUsize::new(0);
    let written = AtomicUsize::new(0);
    thread::scope(|scope| {
        for _ in 0..threads {
            scope.spawn(|| {
                let (mut big_latencies, mut text) = (Vec::new(), Vec::new());
                loop {
                    let hour = next_hour.fetch_add(1, Relaxed);
                    if hour >= HOURS {
                        return;
                    }
                    let taken = hours[hour].lock().map(|mut slot| slot.take());
                    let taken = taken.expect("panic = \"abort\" means a lock can't be poisoned");
                    let Some(mut records) = taken else {
                        return;
                    };
                    let used = (*records.next.get_mut()).min(records.slots.len());
                    let mask = (1u32 << format.id_shift) - 1;
                    // SAFETY: all threads have joined, this thread owns the array, and an AtomicU32
                    // has the same size and alignment as a u32.
                    let sorted = unsafe {
                        std::slice::from_raw_parts_mut(
                            records.slots.as_mut_ptr().cast::<u32>(),
                            used,
                        )
                    };
                    for record in sorted.iter_mut() {
                        if *record != 0 {
                            *record = rank[(*record >> format.id_shift) as usize]
                                << format.id_shift
                                | *record & mask;
                        }
                    }
                    sorted.sort_unstable();
                    big_latencies.clear();
                    for lists in big_lists {
                        big_latencies.extend(lists[hour].iter().map(|entry| {
                            u64::from(rank[(entry >> 32) as usize]) << 32 | entry & 0xFFFF_FFFF
                        }));
                    }
                    big_latencies.sort_unstable();
                    let real = sorted.partition_point(|&record| record == 0);
                    text.clear();
                    format_hour(
                        hour,
                        &sorted[real..],
                        &big_latencies,
                        &endpoints,
                        format,
                        &mut text,
                    );
                    drop(records);
                    while written.load(Acquire) != hour {
                        thread::yield_now();
                    }
                    // `panic = "abort"`: a failed write ends the process rather than leaving the
                    // other threads waiting for their turn.
                    (&file).write_all(&text).expect("writing the summary");
                    written.store(hour + 1, Release);
                }
            });
        }
    });
    Ok(())
}

fn main() -> io::Result<()> {
    // Transparent huge pages would round each hour's sparse array up to 2 MB. Failing is harmless.
    const PR_SET_THP_DISABLE: c_int = 41;
    // SAFETY: this only sets a flag on our own process.
    unsafe { prctl(PR_SET_THP_DISABLE, 1, 0, 0, 0) };
    let mut args = std::env::args().skip(1);
    let data_dir = args.next().unwrap_or_else(|| "/data".into());
    let out_dir = args.next().unwrap_or_else(|| "/out".into());
    let threads = thread_count();
    let log = map_file(&Path::new(&data_dir).join("requests.log"))?;
    // Eight segments per thread balances the load. Tying the size to the thread count also makes
    // the SD_THREADS test sweep move every cut, down to cuts smaller than a line on tiny inputs.
    let segment = (log.len() / (threads * 8)).max(1);
    // A line is at least 40 bytes. An hour's array starts at eight times its fair share of the lines.
    let mut hour_slots = (log.len() / 40 / HOURS * 8).max(BLOCK_RECORDS * threads) + BLOCK_RECORDS;
    let mut slots = INITIAL_SLOTS;
    loop {
        let table = EndpointTable::new(log, slots);
        let format = RecordFormat::new(slots);
        let hours: Vec<HourRecords> = (0..HOURS)
            .map(|_| HourRecords {
                slots: zeroed_atomics(hour_slots),
                next: AtomicUsize::new(0),
            })
            .collect();
        let cursor = AtomicUsize::new(0);
        let big_lists: Vec<Vec<Vec<u64>>> = thread::scope(|scope| {
            let handles: Vec<_> = (0..threads)
                .map(|_| {
                    scope.spawn(|| parse_segments(log, &table, &hours, &cursor, segment, format))
                })
                .collect();
            let joined = handles.into_iter().map(|handle| handle.join());
            joined
                .map(|lists| lists.expect("panic = \"abort\" means a join can't fail"))
                .collect()
        });
        if !table.overflowed.load(Relaxed) {
            let path = Path::new(&out_dir).join("summary.txt");
            return write_summary(&path, &table, hours, &big_lists, format, threads);
        }
        // More endpoints than the spec promises, or a very lopsided log: redo it with more room.
        slots *= 4;
        hour_slots *= 4;
    }
}
