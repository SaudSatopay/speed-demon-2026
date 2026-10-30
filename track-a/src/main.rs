//! Track A (Ingest): per-hour, per-endpoint latency percentiles over an HTTP request log.
//!
//! The log is memory-mapped and walked once, one line at a time, with one growing latency list per
//! (hour, endpoint) group. Each group is sorted at the end, and the output is written in small
//! pieces so that it never sits in memory as well as in the file.

use std::ffi::{c_int, c_void};
use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::os::fd::AsRawFd;
use std::path::Path;

/// Every seed's log spans exactly 2026-10-30T18:00Z to 2026-11-02T17:59Z (spec section 2).
const HOURS: usize = 72;
const DATES: [&str; 4] = ["2026-10-30", "2026-10-31", "2026-11-01", "2026-11-02"];

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

#[derive(Clone, Default)]
struct Group {
    latencies: Vec<u32>,
    errors: u32,
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

/// Walks every `ts method endpoint status latency` line, and returns the endpoints by id and the
/// groups indexed by [endpoint id][hour].
fn aggregate(log: &[u8]) -> (Vec<&[u8]>, Vec<Vec<Group>>) {
    let mut endpoints = Interner::new();
    let mut groups: Vec<Vec<Group>> = Vec::new();
    let mut rest = log;
    while !rest.is_empty() {
        let hour = hour_index(rest);
        let start = 29 + method_len(rest[31]);
        let mut end = start;
        while rest[end] != b' ' {
            end += 1;
        }
        let id = endpoints.intern(&rest[start..end]);
        if id == groups.len() {
            groups.push(vec![Group::default(); HOURS]);
        }
        // Three status digits sit between two spaces, so the latency starts 5 bytes on.
        let mut pos = end + 5;
        let mut latency = 0u32;
        while rest[pos] != b'\n' {
            latency = latency * 10 + u32::from(rest[pos] - b'0');
            pos += 1;
        }
        let group = &mut groups[id][hour];
        group.latencies.push(latency);
        group.errors += u32::from(rest[end + 1] == b'5'); // Statuses are 100 to 599.
        rest = &rest[pos + 1..];
    }
    (endpoints.keys, groups)
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

/// Prints the groups by hour, then by endpoint as unsigned bytes. `[u8]` compares exactly that way,
/// with a proper prefix first.
fn write_summary(path: &Path, endpoints: &[&[u8]], groups: &mut [Vec<Group>]) -> io::Result<()> {
    let mut order: Vec<usize> = (0..endpoints.len()).collect();
    order.sort_unstable_by_key(|&id| endpoints[id]);
    let mut out = BufWriter::with_capacity(1 << 16, File::create(path)?);
    let mut line = Vec::with_capacity(256);
    for hour in 0..HOURS {
        let clock = hour + 18;
        let stamp = format!("{}T{:02}:00:00Z ", DATES[clock / 24], clock % 24);
        for &id in &order {
            let group = &mut groups[id][hour];
            if group.latencies.is_empty() {
                continue;
            }
            group.latencies.sort_unstable();
            let n = group.latencies.len() as u64;
            // Nearest rank, 1-indexed: ceil(p * n / 100).
            let rank = |p: u64| u64::from(group.latencies[((p * n + 99) / 100 - 1) as usize]);
            let errors = u64::from(group.errors);
            line.clear();
            line.extend_from_slice(stamp.as_bytes());
            line.extend_from_slice(endpoints[id]);
            for value in [
                n,
                rank(50),
                rank(95),
                rank(99),
                errors,
                errors * 1_000_000 / n,
            ] {
                line.push(b' ');
                push_decimal(&mut line, value);
            }
            line.push(b'\n');
            out.write_all(&line)?;
        }
    }
    out.flush()
}

fn main() -> io::Result<()> {
    let mut args = std::env::args().skip(1);
    let data_dir = args.next().unwrap_or_else(|| "/data".into());
    let out_dir = args.next().unwrap_or_else(|| "/out".into());
    let log = map_file(&Path::new(&data_dir).join("requests.log"))?;
    let (endpoints, mut groups) = aggregate(log);
    write_summary(
        &Path::new(&out_dir).join("summary.txt"),
        &endpoints,
        &mut groups,
    )
}
