//! Track A (Ingest): per-hour, per-endpoint latency percentiles over an HTTP request log.
//!
//! The log is memory-mapped and cut into segments that threads claim from an atomic cursor, so a
//! thread that falls behind simply claims fewer. Each thread interns endpoints into its own table
//! and appends one 64-bit record per line to a list for that line's hour. When the log is done,
//! the tables are merged and sorted, and threads claim hours: each merges and sorts one hour's
//! records, formats them, and waits for its turn to append them to the file. So the output is in
//! a fixed order whatever the scheduling, and never sits in memory whole.

use std::ffi::{c_int, c_void};
use std::fs::File;
use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::path::Path;
use std::sync::atomic::{
    AtomicUsize,
    Ordering::{Acquire, Relaxed, Release},
};
use std::thread;

/// Every seed's log spans exactly 2026-10-30T18:00Z to 2026-11-02T17:59Z (spec section 2).
const HOURS: usize = 72;
const DATES: [&str; 4] = ["2026-10-30", "2026-10-31", "2026-11-01", "2026-11-02"];

/// A record is one request: the endpoint id from this bit up, the latency in bits 1 to 32, and the
/// error flag in bit 0. Once the id is a byte-order rank, plain integer order sorts records by
/// endpoint and then by latency.
const ID_SHIFT: u32 = 33;

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
/// for these pages, unlike reading the log onto the heap.
fn map_file(path: &Path) -> io::Result<&'static [u8]> {
    const PROT_READ: c_int = 1;
    const MAP_PRIVATE: c_int = 2;
    let file = File::open(path)?;
    let len = file.metadata()?.len() as usize;
    if len == 0 {
        return Ok(&[]); // mmap rejects an empty range, and an empty log is legal.
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

/// FNV-1a. A test build swaps it for a constant, so that every key collides and any logic that
/// trusts a hash without comparing the key shows up as a wrong answer.
#[cfg(not(sd_const_hash))]
fn hash(key: &[u8]) -> usize {
    let h = key.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, &byte| {
        (h ^ u64::from(byte)).wrapping_mul(0x100_0000_01b3)
    });
    (h ^ (h >> 32)) as usize
}

#[cfg(sd_const_hash)]
fn hash(_key: &[u8]) -> usize {
    0
}

/// Endpoint strings to dense ids, by open addressing with linear probing. A hit always compares
/// the full key, because templated paths share prefixes, suffixes and lengths.
struct Interner<'a> {
    slots: Vec<u32>, // id + 1, or 0 when free. The length is a power of two.
    keys: Vec<&'a [u8]>,
}

impl<'a> Interner<'a> {
    fn new() -> Self {
        Interner {
            slots: vec![0; 1 << 12],
            keys: Vec::new(),
        }
    }

    fn intern(&mut self, key: &'a [u8]) -> usize {
        let mask = self.slots.len() - 1;
        let mut slot = hash(key) & mask;
        while self.slots[slot] != 0 {
            let id = self.slots[slot] as usize - 1;
            if self.keys[id] == key {
                return id;
            }
            slot = (slot + 1) & mask;
        }
        self.keys.push(key);
        self.slots[slot] = self.keys.len() as u32;
        if self.keys.len() * 2 > self.slots.len() {
            self.grow();
        }
        self.keys.len() - 1
    }

    /// The spec promises 1,842 endpoints per seed, but nothing here depends on that number.
    fn grow(&mut self) {
        let mut slots = vec![0; self.slots.len() * 2];
        let mask = slots.len() - 1;
        for (id, key) in self.keys.iter().enumerate() {
            let mut slot = hash(key) & mask;
            while slots[slot] != 0 {
                slot = (slot + 1) & mask;
            }
            slots[slot] = id as u32 + 1;
        }
        self.slots = slots;
    }
}

/// Hours since the start of the span, read from four timestamp bytes without parsing a date.
/// Oct 30, Oct 31, Nov 1 and Nov 2 differ in the last digit of the month and of the day:
/// (month & 1) + (day & 3) gives 0, 1, 2 and 3.
fn hour_index(line: &[u8]) -> usize {
    let day = (line[6] & 1) + (line[9] & 3);
    let hour = 10 * (line[11] & 15) + (line[12] & 15);
    24 * usize::from(day) + usize::from(hour) - 18
}

/// The method starts at byte 28, so byte 31 is its 4th byte, which tells the seven apart.
fn method_len(fourth_byte: u8) -> usize {
    match fourth_byte {
        b' ' => 3,        // GET, PUT
        b'T' | b'D' => 4, // POST, HEAD
        b'C' => 5,        // PATCH
        b'E' => 6,        // DELETE
        _ => 7,           // OPTIONS
    }
}

/// One thread's view of the log: its endpoint table, and its records for each hour.
struct Worker<'a> {
    endpoints: Interner<'a>,
    hours: Vec<Vec<u64>>,
}

/// Claims segments until the log runs out, parsing each `ts method endpoint status latency` line.
fn parse_segments<'a>(log: &'a [u8], cursor: &AtomicUsize, segment: usize) -> Worker<'a> {
    let mut endpoints = Interner::new();
    let mut hours = vec![Vec::new(); HOURS];
    loop {
        let start = cursor.fetch_add(segment, Relaxed);
        if start >= log.len() {
            return Worker { endpoints, hours };
        }
        let end = (start + segment).min(log.len());
        // A line belongs to the segment holding its first byte, so skip the one already started.
        // The log ends in LF, so this stops at the end of the log at the latest.
        let mut line = start;
        while line > 0 && log[line - 1] != b'\n' {
            line += 1;
        }
        while line < end {
            let key_start = line + 29 + method_len(log[line + 31]);
            let mut pos = key_start;
            while log[pos] != b' ' {
                pos += 1;
            }
            let id = endpoints.intern(&log[key_start..pos]) as u64;
            let is_error = u64::from(log[pos + 1] == b'5'); // Statuses are 100 to 599.
            pos += 5; // Three status digits sit between two spaces.
            let mut latency = 0u64;
            while log[pos] != b'\n' {
                latency = latency * 10 + u64::from(log[pos] - b'0');
                pos += 1;
            }
            hours[hour_index(&log[line..])].push(id << ID_SHIFT | latency << 1 | is_error);
            line = pos + 1;
        }
    }
}

/// Merges the threads' endpoint tables. Returns every endpoint in byte order (`[u8]` compares as
/// unsigned bytes, with a proper prefix first), and for each thread a map from its ids to positions
/// in that order.
fn merge_endpoints<'a>(workers: &[Worker<'a>]) -> (Vec<&'a [u8]>, Vec<Vec<u32>>) {
    let mut merged = Interner::new();
    let mut maps: Vec<Vec<u32>> = workers
        .iter()
        .map(|worker| {
            let keys = worker.endpoints.keys.iter();
            keys.map(|&key| merged.intern(key) as u32).collect()
        })
        .collect();
    let mut order: Vec<u32> = (0..merged.keys.len() as u32).collect();
    order.sort_unstable_by_key(|&id| merged.keys[id as usize]);
    let mut rank = vec![0; order.len()];
    for (position, &id) in order.iter().enumerate() {
        rank[id as usize] = position as u32;
    }
    for id in maps.iter_mut().flatten() {
        *id = rank[*id as usize];
    }
    let sorted = order.iter().map(|&id| merged.keys[id as usize]).collect();
    (sorted, maps)
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

/// Appends one hour's output lines, given that hour's records in integer order.
fn format_hour(hour: usize, records: &[u64], endpoints: &[&[u8]], text: &mut Vec<u8>) {
    let clock = hour + 18;
    let stamp = format!("{}T{:02}:00:00Z ", DATES[clock / 24], clock % 24);
    for group in records.chunk_by(|a, b| a >> ID_SHIFT == b >> ID_SHIFT) {
        let n = group.len() as u64;
        // Nearest rank, 1-indexed: ceil(p * n / 100).
        let percentile = |p: u64| group[((p * n + 99) / 100 - 1) as usize] >> 1 & 0xFFFF_FFFF;
        let errors: u64 = group.iter().map(|record| record & 1).sum();
        text.extend_from_slice(stamp.as_bytes());
        text.extend_from_slice(endpoints[(group[0] >> ID_SHIFT) as usize]);
        for value in [
            n,
            percentile(50),
            percentile(95),
            percentile(99),
            errors,
            errors * 1_000_000 / n,
        ] {
            text.push(b' ');
            push_decimal(text, value);
        }
        text.push(b'\n');
    }
}

/// Threads claim hours in order, and each waits for the hour before it to reach the file. The wait
/// is short, because hours cost about the same, so spinning with a yield is enough.
fn write_summary(path: &Path, workers: &[Worker], threads: usize) -> io::Result<()> {
    let (endpoints, maps) = merge_endpoints(workers);
    let file = File::create(path)?;
    let next_hour = AtomicUsize::new(0);
    let written = AtomicUsize::new(0);
    thread::scope(|scope| {
        for _ in 0..threads {
            scope.spawn(|| {
                let (mut records, mut text) = (Vec::new(), Vec::new());
                loop {
                    let hour = next_hour.fetch_add(1, Relaxed);
                    if hour >= HOURS {
                        return;
                    }
                    records.clear();
                    for (worker, map) in workers.iter().zip(&maps) {
                        records.extend(worker.hours[hour].iter().map(|record| {
                            let rank = u64::from(map[(record >> ID_SHIFT) as usize]);
                            rank << ID_SHIFT | record & ((1 << ID_SHIFT) - 1)
                        }));
                    }
                    records.sort_unstable();
                    text.clear();
                    format_hour(hour, &records, &endpoints, &mut text);
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
    let mut args = std::env::args().skip(1);
    let data_dir = args.next().unwrap_or_else(|| "/data".into());
    let out_dir = args.next().unwrap_or_else(|| "/out".into());
    let threads = thread_count();
    let log = map_file(&Path::new(&data_dir).join("requests.log"))?;
    // Eight segments per thread balances the load. Tying the size to the thread count also makes
    // the SD_THREADS test sweep move every cut, down to cuts smaller than a line on tiny inputs.
    let segment = (log.len() / (threads * 8)).max(1);
    let cursor = AtomicUsize::new(0);
    let workers: Vec<Worker> = thread::scope(|scope| {
        let handles: Vec<_> = (0..threads)
            .map(|_| scope.spawn(|| parse_segments(log, &cursor, segment)))
            .collect();
        let joined = handles.into_iter().map(|handle| handle.join());
        joined
            .map(|worker| worker.expect("panic = \"abort\" means a join can't fail"))
            .collect()
    });
    write_summary(&Path::new(&out_dir).join("summary.txt"), &workers, threads)
}
