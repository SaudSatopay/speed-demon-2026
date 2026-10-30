//! Track A (Ingest): per-hour, per-endpoint latency percentiles over an HTTP request log.
//!
//! The log is memory-mapped and cut into segments that threads claim from an atomic cursor, so a
//! thread that falls behind simply claims fewer. Each line is read at fixed offsets where the format
//! allows it, and with block loads where it doesn't. One lock-free table, shared by all threads,
//! turns endpoints into ids, and each line becomes one 32-bit record in a shared array for its hour,
//! where each thread reserves small blocks. When the log is done, threads claim hours: each groups one
//! hour's array in place, selects its percentiles, formats it, frees it, takes the next stretch of the output file in hour
//! order, and copies its text there. So the output is in a fixed order whatever the scheduling, never
//! sits in memory whole, and is written by all threads at once.

use std::arch::x86_64::{
    __m256i, _mm256_cmpeq_epi8, _mm256_loadu_si256, _mm256_movemask_epi8, _mm256_set1_epi8,
};
use std::ffi::{c_int, c_void};
use std::fs::File;
use std::hint;
use std::io;
use std::os::fd::AsRawFd;
use std::path::Path;
use std::ptr::null_mut;
use std::sync::Mutex;
use std::sync::atomic::{
    AtomicU32, AtomicU64, AtomicUsize,
    Ordering::{Acquire, Relaxed, Release},
};
use std::thread;

/// Every seed's log spans exactly 2026-10-30T18:00Z to 2026-11-02T17:59Z (spec section 2).
const HOURS: usize = 72;
const DATES: [&str; 4] = ["2026-10-30", "2026-10-31", "2026-11-01", "2026-11-02"];

/// A record is one request, from the top: 12 bits of endpoint id, 19 of latency, and the error flag.
/// Once the id is a byte-order rank, plain integer order sorts records by endpoint and then by
/// latency. About 3% of latencies don't fit, so their records hold `BIG_LATENCY`, and the real value
/// goes in a side list.
const ID_SHIFT: u32 = 20;
const BIG_LATENCY: u32 = (1 << 19) - 1;

/// Records a thread reserves at a time from an hour's array, and writes without contention.
const BLOCK_RECORDS: usize = 256;

/// Readable zero bytes after the log, so that block loads can run past its last line.
const PADDING: usize = 4096;

/// A method's length, from its 4th byte, which tells the seven apart: GET and PUT have a space there,
/// POST and HEAD have T and D, PATCH has C, DELETE has E, and OPTIONS has I.
const METHOD_LEN: [u8; 256] = {
    let mut table = [0; 256];
    table[b' ' as usize] = 3;
    (table[b'T' as usize], table[b'D' as usize]) = (4, 4);
    (
        table[b'C' as usize],
        table[b'E' as usize],
        table[b'I' as usize],
    ) = (5, 6, 7);
    table
};

// Declared by hand to keep the crate at zero dependencies; std already links glibc.
unsafe extern "C" {
    fn prctl(option: c_int, a: u64, b: u64, c: u64, d: u64) -> c_int;
    fn mmap(at: *mut c_void, len: usize, p: c_int, f: c_int, fd: c_int, o: i64) -> *mut c_void;
}

/// Calls mmap, turning its failure value into an error.
fn map(at: *mut c_void, len: usize, prot: c_int, flags: c_int, fd: c_int) -> io::Result<*mut u8> {
    // SAFETY: every caller passes either a null address, or the start of its own fresh mapping.
    let mapped = unsafe { mmap(at, len, prot, flags, fd, 0) };
    if mapped as isize == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(mapped.cast())
}

/// Maps a file read-only for the life of the process, followed by `PADDING` readable zero bytes. The
/// scorer's memory peak doesn't charge us for these pages, unlike reading the log onto the heap.
fn map_file(path: &Path) -> io::Result<&'static [u8]> {
    let (read, private, fixed, anonymous) = (1, 2, 0x10, 0x20);
    let file = File::open(path)?;
    let len = file.metadata()?.len() as usize;
    let base = map(null_mut(), len + PADDING, read, private | anonymous, -1)?;
    if len > 0 {
        map(base.cast(), len, read, private | fixed, file.as_raw_fd())?;
    }
    // SAFETY: the file is mapped over fresh zero pages. Neither is ever unmapped, and nothing writes
    // to the dataset while we run.
    Ok(unsafe { std::slice::from_raw_parts(base, len) })
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
    let asked = std::env::var("SD_THREADS")
        .ok()
        .and_then(|v| v.parse().ok());
    let allowed = || thread::available_parallelism().map_or(1, |n| n.get().min(12));
    asked.filter(|&threads| threads > 0).unwrap_or_else(allowed)
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

/// Room for the spec's 1,842 endpoints per seed, at a low load.
const SLOTS: usize = 1 << 12;
const EMPTY: u64 = 0;
const BUSY: u64 = 1;

/// A slot: a header holding part of the hash and the key's length, and where the key first occurred.
#[derive(Default)]
struct Slot {
    header: AtomicU64,
    start: AtomicU64,
}

/// Endpoints to ids, shared by all threads with no locks. A key is always compared in full, against
/// its first occurrence in the log, because templated paths share prefixes, suffixes and lengths. A
/// slot's number is the endpoint's id, and a published slot never changes.
struct EndpointTable<'a> {
    log: &'a [u8],
    slots: Vec<Slot>,
    used: AtomicUsize,
}

impl<'a> EndpointTable<'a> {
    fn new(log: &'a [u8]) -> Self {
        EndpointTable {
            log,
            slots: (0..SLOTS).map(|_| Slot::default()).collect(),
            used: AtomicUsize::new(0),
        }
    }

    /// The id of the endpoint at `start..end`, adding it if it's new.
    #[inline]
    fn id_of(&self, start: usize, end: usize) -> usize {
        let len = end - start;
        let words = if len >= 8 {
            let word_at = |pos| load_u64(self.log, pos);
            [
                word_at(start),
                word_at(start + (len - 8) / 2),
                word_at(end - 8),
            ]
        } else {
            [load_u64(self.log, end - 8) >> (64 - 8 * len); 3]
        };
        let hash = hash(words, len);
        // The top half of the hash and the length, plus 2 to stay clear of EMPTY and BUSY.
        let published = (hash & 0xFFFF_FFFF_0000_0000) | (len as u64 + 2);
        let mut index = (hash as usize & (SLOTS - 1)).max(1); // Slot 0 stays free: see `new_hours`.
        loop {
            let slot = &self.slots[index];
            let mut header = slot.header.load(Acquire);
            if header == EMPTY
                && slot
                    .header
                    .compare_exchange(EMPTY, BUSY, Acquire, Acquire)
                    .is_ok()
            {
                assert!(
                    self.used.fetch_add(1, Relaxed) < SLOTS / 8 * 7,
                    "more endpoints than the spec's"
                );
                slot.start.store(start as u64, Relaxed);
                slot.header.store(published, Release);
                return index;
            }
            while header == EMPTY || header == BUSY {
                hint::spin_loop();
                header = slot.header.load(Acquire);
            }
            if header == published {
                let first = slot.start.load(Relaxed) as usize;
                if self.log[first..first + len] == self.log[start..end] {
                    return index;
                }
            }
            index = ((index + 1) & (SLOTS - 1)).max(1);
        }
    }

    /// Every endpoint in byte order (`[u8]` compares as unsigned bytes, with a proper prefix first),
    /// and each slot's position in that order, counting from 1.
    fn sorted_keys(&self) -> (Vec<&'a [u8]>, Vec<u32>) {
        let mut keys: Vec<(&[u8], usize)> = Vec::new();
        for (index, slot) in self.slots.iter().enumerate() {
            let header = slot.header.load(Acquire);
            if header != EMPTY {
                let len = (header & 0xFFFF_FFFF) as usize - 2;
                let start = slot.start.load(Relaxed) as usize;
                keys.push((&self.log[start..start + len], index));
            }
        }
        keys.sort_unstable();
        let mut rank = vec![0; SLOTS];
        for (position, &(_, slot)) in keys.iter().enumerate() {
            rank[slot] = position as u32 + 1;
        }
        (keys.into_iter().map(|(key, _)| key).collect(), rank)
    }
}

/// All the records of one hour, in one array that threads reserve blocks of.
struct HourRecords {
    slots: Vec<AtomicU32>,
    next: AtomicUsize,
}

/// Each hour's array has room for every line of the log, since a log may be lopsided. That's fine,
/// because untouched pages cost nothing. A block's unused end stays zero, which no real record is,
/// since slot 0 is never an id. So after sorting, the zeros are at the front and easy to skip.
fn new_hours(log: &[u8], threads: usize) -> Vec<HourRecords> {
    const SHORTEST_LINE: usize = 40;
    let len = log.len() / SHORTEST_LINE + BLOCK_RECORDS * (threads + 1);
    let new_hour = |_: usize| HourRecords {
        slots: vec![0u32; len].into_iter().map(AtomicU32::new).collect(),
        next: AtomicUsize::new(0),
    };
    (0..HOURS).map(new_hour).collect()
}

/// A thread's unfilled block of one hour's array, and its latencies too big for a record.
#[derive(Clone, Default)]
struct HourWriter {
    next: usize,
    end: usize,
    big_latencies: Vec<u64>, // (id << 32 | latency), for the records holding `BIG_LATENCY`.
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

/// Reads the latency at `pos`, and returns it with the number of digits. Up to 8 digits and their LF
/// fit in one load, which covers all but the rarest latencies.
#[inline]
fn parse_latency(log: &[u8], pos: usize) -> (u64, usize) {
    let word = load_u64(log, pos);
    let line_feeds = word ^ 0x0A0A_0A0A_0A0A_0A0A;
    let ones = line_feeds.wrapping_sub(0x0101_0101_0101_0101);
    let found = ones & !line_feeds & 0x8080_8080_8080_8080;
    if found != 0 {
        let digits = found.trailing_zeros() as usize / 8;
        return (parse_digits(word, digits), digits);
    }
    let (mut value, mut digits) = (parse_digits(word, 8), 8);
    while log[pos + digits] != b'\n' {
        value = value * 10 + u64::from(log[pos + digits] - b'0');
        digits += 1;
    }
    (value, digits)
}

/// Claims segments until the log runs out, and returns this thread's big latencies for each hour.
/// Each line is `ts method endpoint status latency`, where the timestamp is always 27 bytes.
fn parse_segments(
    log: &[u8],
    table: &EndpointTable,
    hours: &[HourRecords],
    cursor: &AtomicUsize,
    segment: usize,
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
            // Three status digits sit between two spaces, so the latency starts 5 bytes on.
            let (latency, digits) = parse_latency(log, key_end + 5);
            let hour = hour_index(log, line);
            let writer = &mut writers[hour];
            let mut stored = latency as u32;
            if stored >= BIG_LATENCY {
                writer.big_latencies.push(u64::from(id) << 32 | latency);
                stored = BIG_LATENCY;
            }
            if writer.next == writer.end {
                writer.next = hours[hour].next.fetch_add(BLOCK_RECORDS, Relaxed);
                writer.end = writer.next + BLOCK_RECORDS;
            }
            let record = id << ID_SHIFT | stored << 1 | is_error;
            hours[hour].slots[writer.next].store(record, Relaxed);
            writer.next += 1;
            line = key_end + 5 + digits + 1;
        }
    }
}

/// The 8 decimal digits of a value below 10^8, as bytes with the most significant first, and with
/// no ASCII offset yet. Each step splits every number in its lane in two, in parallel, by multiplying
/// by a scaled reciprocal: 10^8 into two of 10^4, those into 10^2, and those into digits.
#[inline]
fn eight_digits(value: u64) -> u64 {
    let high_low = value / 10_000 | (value % 10_000) << 32;
    let hundreds = (high_low * 10486 >> 20) & 0x0000_007F_0000_007F;
    let pairs = (high_low - 100 * hundreds) << 16 | hundreds;
    let tens = (pairs * 103 >> 10) & 0x000F_000F_000F_000F;
    tens | (pairs - 10 * tens) << 8
}

/// Appends a number in decimal, with no loop or mispredicted branch for values below 10^8.
#[inline]
fn push_decimal(out: &mut Vec<u8>, value: u64) {
    const ASCII: u64 = 0x3030_3030_3030_3030;
    if value >= 100_000_000 {
        push_decimal(out, value / 100_000_000);
        out.extend_from_slice(&(eight_digits(value % 100_000_000) | ASCII).to_le_bytes());
        return;
    }
    let digits = eight_digits(value);
    let leading_zeros = (digits.trailing_zeros() as usize / 8).min(7);
    let end = out.len() + 8 - leading_zeros;
    out.extend_from_slice(&((digits | ASCII) >> (8 * leading_zeros)).to_le_bytes());
    out.truncate(end);
}

/// Moves an hour's records into runs by endpoint rank, in place: each record swaps straight into its
/// endpoint's next free position. Nearly all groups are small, so this one pass does most of the work
/// of a full sort, and only `select_nth_unstable` remains per group.
fn group_by_rank(records: &mut [u32], counts: &mut Vec<u32>, rank_count: usize) {
    counts.clear();
    counts.resize(2 * rank_count + 2, 0);
    let (next, ends) = counts.split_at_mut(rank_count + 1);
    for record in records.iter() {
        ends[(record >> ID_SHIFT) as usize] += 1;
    }
    let mut total = 0;
    for (start, end) in next.iter_mut().zip(ends.iter_mut()) {
        *start = total;
        total += *end;
        *end = total;
    }
    for rank in 0..=rank_count {
        while next[rank] < ends[rank] {
            let here = next[rank] as usize;
            let target = (records[here] >> ID_SHIFT) as usize;
            records.swap(here, next[target] as usize);
            next[target] += 1;
        }
    }
}

/// Appends one hour's output lines, given its records in runs by endpoint rank, and its big latencies
/// sorted by endpoint rank and then value.
fn format_hour(
    hour: usize,
    records: &mut [u32],
    big_latencies: &[u64],
    endpoints: &[&[u8]],
    text: &mut Vec<u8>,
) {
    let clock = hour + 18;
    let stamp = format!("{}T{:02}:00:00Z ", DATES[clock / 24], clock % 24);
    let mut big = big_latencies.iter().map(|&entry| entry & 0xFFFF_FFFF);
    let mut bigs = Vec::new();
    for group in records.chunk_by_mut(|a, b| a >> ID_SHIFT == b >> ID_SHIFT) {
        let n = group.len();
        let latency = |record: u32| (record >> 1) & BIG_LATENCY;
        let rank = group[0] >> ID_SHIFT;
        let errors: u64 = group.iter().map(|record| u64::from(record & 1)).sum();
        // A group's big latencies sort last, and are next in the side list, which is in the same order.
        let big_count = group.iter().filter(|&&r| latency(r) == BIG_LATENCY).count();
        bigs.clear();
        bigs.extend(big.by_ref().take(big_count));
        // Nearest rank, 1-indexed: ceil(p * n / 100). Each selection only searches below the last.
        let mut below = n;
        let mut pct = |p: usize| {
            let index = (p * n + 99) / 100 - 1;
            if let Some(&value) = bigs.get(index.wrapping_sub(n - big_count)) {
                return value;
            }
            if index < below {
                group[..below].select_nth_unstable(index);
                below = index;
            }
            u64::from(latency(group[index]))
        };
        let (p99, p95, p50) = (pct(99), pct(95), pct(50));
        text.extend_from_slice(stamp.as_bytes());
        text.extend_from_slice(endpoints[rank as usize - 1]);
        let count = n as u64;
        for value in [count, p50, p95, p99, errors, errors * 1_000_000 / count] {
            text.push(b' ');
            push_decimal(text, value);
        }
        text.push(b'\n');
    }
}

/// The output file, mapped so that threads can copy their text into it at the same time. Writing
/// with `write` would queue them on the file's lock. It starts long enough for any output, which
/// costs nothing until written, and is cut to its true length at the end.
struct OutputFile {
    file: File,
    base: *mut u8,
    /// `(hours done << 40) | bytes so far`, so a thread claims its stretch of the file in hour order.
    progress: AtomicU64,
}

// SAFETY: threads write through `base` only into stretches that `claim` hands out, which never overlap.
unsafe impl Sync for OutputFile {}

impl OutputFile {
    fn create(path: &Path, longest: usize) -> io::Result<Self> {
        let file = File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)?;
        let (read_write, shared) = (3, 1);
        file.set_len(longest as u64)?;
        // An empty log gets an empty file, and mmap rejects an empty range.
        let base = if longest == 0 {
            null_mut()
        } else {
            map(null_mut(), longest, read_write, shared, file.as_raw_fd())?
        };
        let progress = AtomicU64::new(0);
        Ok(OutputFile {
            file,
            base,
            progress,
        })
    }

    /// Waits for the hours before this one to take their stretches, which is short because each only
    /// has to add a length, then copies this hour's text to its own stretch.
    fn write_hour(&self, hour: usize, text: &[u8]) {
        let bytes_so_far = loop {
            let progress = self.progress.load(Acquire);
            if (progress >> 40) as usize == hour {
                break progress & ((1 << 40) - 1);
            }
            thread::yield_now();
        };
        let next = ((hour as u64 + 1) << 40) | (bytes_so_far + text.len() as u64);
        self.progress.store(next, Release);
        if !text.is_empty() {
            // SAFETY: this stretch is this thread's alone, and `create` made the mapping long enough
            // for any output.
            unsafe {
                let to = self.base.add(bytes_so_far as usize);
                std::ptr::copy_nonoverlapping(text.as_ptr(), to, text.len());
            }
        }
    }

    fn finish(self) -> io::Result<()> {
        self.file
            .set_len(self.progress.load(Acquire) & ((1 << 40) - 1))
    }
}

/// Threads claim hours in order. Each hour's array is freed once formatted, so the output on tmpfs
/// grows as the records shrink, and the two don't add up in the memory peak.
fn write_summary(
    path: &Path,
    table: &EndpointTable,
    hours: Vec<HourRecords>,
    big_lists: &[Vec<Vec<u64>>],
    threads: usize,
) -> io::Result<()> {
    let (endpoints, rank) = table.sorted_keys();
    let longest_key = endpoints.iter().map(|key| key.len()).max().unwrap_or(0);
    let lines: usize = hours
        .iter()
        .map(|hour| hour.next.load(Relaxed).min(endpoints.len()))
        .sum();
    // The stamp, the key, and six numbers of at most 10 digits, each with its space.
    let output = OutputFile::create(path, lines * (21 + longest_key + 6 * 11 + 1))?;
    let hours: Vec<Mutex<Option<HourRecords>>> = hours
        .into_iter()
        .map(|hour| Mutex::new(Some(hour)))
        .collect();
    let next_hour = AtomicUsize::new(0);
    thread::scope(|scope| {
        for _ in 0..threads {
            scope.spawn(|| {
                let (mut big_latencies, mut text, mut counts) =
                    (Vec::new(), Vec::new(), Vec::new());
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
                    // SAFETY: all threads have joined, this thread owns the array, and an AtomicU32
                    // has the same size and alignment as a u32.
                    let sorted = unsafe {
                        std::slice::from_raw_parts_mut(
                            records.slots.as_mut_ptr().cast::<u32>(),
                            used,
                        )
                    };
                    let id_mask = (1u32 << ID_SHIFT) - 1;
                    for record in sorted.iter_mut().filter(|record| **record != 0) {
                        *record =
                            rank[(*record >> ID_SHIFT) as usize] << ID_SHIFT | *record & id_mask;
                    }
                    group_by_rank(sorted, &mut counts, endpoints.len());
                    big_latencies.clear();
                    big_latencies.extend(big_lists.iter().flat_map(|lists| &lists[hour]).map(
                        |entry| u64::from(rank[(entry >> 32) as usize]) << 32 | entry & 0xFFFF_FFFF,
                    ));
                    big_latencies.sort_unstable();
                    let zeros = sorted.iter().take_while(|&&record| record == 0).count();
                    text.clear();
                    format_hour(
                        hour,
                        &mut sorted[zeros..],
                        &big_latencies,
                        &endpoints,
                        &mut text,
                    );
                    drop(records);
                    output.write_hour(hour, &text);
                }
            });
        }
    });
    output.finish()
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
    let table = EndpointTable::new(log);
    let hours = new_hours(log, threads);
    let cursor = AtomicUsize::new(0);
    let big_lists: Vec<Vec<Vec<u64>>> = thread::scope(|scope| {
        let handles: Vec<_> = (0..threads)
            .map(|_| scope.spawn(|| parse_segments(log, &table, &hours, &cursor, segment)))
            .collect();
        let joined = handles.into_iter().map(|handle| handle.join());
        joined
            .map(|lists| lists.expect("panic = \"abort\" means a join can't fail"))
            .collect()
    });
    write_summary(
        &Path::new(&out_dir).join("summary.txt"),
        &table,
        hours,
        &big_lists,
        threads,
    )
}
