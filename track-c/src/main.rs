//! Track C (Join): aggregates per country, from a hostile CSV joined against a region table.
//!
//! Threads claim segments of the mapped file from an atomic cursor. Segments are cut just after a
//! CRLF, because the spec promises that a CR appears nowhere else, so that is always a record
//! boundary, and no quote counting is needed. A scalar parser, written straight from section 2.1 of
//! the spec, walks each segment. It never unescapes anything: a quoted field's raw bytes, with its
//! `""` pairs intact, encode its value one to one, and the value's length is the raw length minus
//! the number of pairs. Each thread sums into its own rows, and one shared table counts the
//! distinct names per country.

use std::ffi::{c_int, c_void};
use std::fs::File;
use std::hint;
use std::io::{self, BufWriter, Write};
use std::os::fd::AsRawFd;
use std::path::Path;
use std::sync::atomic::{
    AtomicU64, AtomicUsize,
    Ordering::{Acquire, Relaxed, Release},
};
use std::thread;

/// Number of possible country codes, `[A-Z]{3}` (spec section 2.3).
const CODES: usize = 26 * 26 * 26;

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

/// Reads the digits at `pos` and steps past the separator after them. Commas and line ends both
/// sort below the digits, so one comparison ends the number.
fn next_number(text: &[u8], pos: &mut usize) -> u64 {
    let mut value = 0;
    while text[*pos] >= b'0' {
        value = value * 10 + u64::from(text[*pos] - b'0');
        *pos += 1;
    }
    *pos += 1;
    value
}

/// The position just past the first LF, where the first data line starts.
fn after_header(text: &[u8]) -> usize {
    let lf = text.iter().position(|&byte| byte == b'\n');
    lf.map_or(text.len(), |lf| lf + 1)
}

/// The first record boundary at or after `pos`: just past the next CRLF, or the end of the file.
/// A CR can only be the first byte of a CRLF terminator (spec section 2.2), so this is never inside
/// quotes. About half the records end in CRLF, so it's never far. A file without one simply doesn't
/// get split.
fn next_boundary(file: &[u8], pos: usize) -> usize {
    let cr = file[pos..].iter().position(|&byte| byte == b'\r');
    cr.map_or(file.len(), |cr| pos + cr + 2)
}

/// The join table: country indices by region id, and the codes in byte order. Indices follow that
/// order, so printing by index sorts the output. The last index stands for `_UNMATCHED`, which sorts
/// after every code because `_` is above `Z`.
struct Regions {
    country_of: Vec<u16>,
    keys: Vec<String>,
}

fn parse_regions(file: &[u8]) -> Regions {
    let lines = || {
        let mut pos = after_header(file);
        std::iter::from_fn(move || {
            (pos < file.len()).then(|| {
                let id = next_number(file, &mut pos) as usize;
                let letter = |i: usize| usize::from(file[pos + i] - b'A');
                let code = letter(0) * 26 * 26 + letter(1) * 26 + letter(2);
                pos += 4; // The three letters and the LF.
                (id, code)
            })
        })
    };
    let mut seen = vec![false; CODES];
    let mut highest_id = 0;
    for (id, code) in lines() {
        seen[code] = true;
        highest_id = highest_id.max(id);
    }
    let mut index_of = vec![0u16; CODES];
    let mut keys = Vec::new();
    for code in (0..CODES).filter(|&code| seen[code]) {
        index_of[code] = keys.len() as u16;
        let letter = |value: usize| char::from(b'A' + (value % 26) as u8);
        keys.push(String::from_iter([
            letter(code / 26 / 26),
            letter(code / 26),
            letter(code),
        ]));
    }
    let unmatched = keys.len() as u16;
    keys.push("_UNMATCHED".to_string());
    let mut country_of = vec![unmatched; highest_id + 1];
    for (id, code) in lines() {
        country_of[id] = index_of[code];
    }
    Regions { country_of, keys }
}

/// Mixes a name's first 24 bytes and its length. A test build swaps it for a constant, so that every
/// key collides and any logic that trusts a hash without comparing the key shows up as a wrong
/// answer.
#[cfg(not(sd_const_hash))]
fn hash(key_words: &[u64; 3], len: usize) -> u64 {
    key_words.iter().fold(len as u64, |hash, &word| {
        let mixed = (hash ^ word).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        mixed ^ (mixed >> 29)
    })
}

#[cfg(sd_const_hash)]
fn hash(_key_words: &[u64; 3], _len: usize) -> u64 {
    0
}

/// Slots to start with: 2^17 for the generator's pool of 100,000 names. A test build starts tiny, to
/// exercise the restart that a larger pool would need.
const INITIAL_SLOTS: usize = if cfg!(sd_const_hash) { 1 << 4 } else { 1 << 17 };
/// Most names fit in the slot itself: on seed 1, 99.99% of them.
const INLINE_BYTES: usize = 24;
const EMPTY: u64 = 0;
const BUSY: u64 = 1;

/// Distinct names per country, in one table shared by all threads, with no locks. Each slot is one
/// cache line: a header, the name's first 24 bytes (or its file offset if it's longer), and one bit
/// per country that has seen this name. So a lookup costs one cache miss, with no second memory
/// access to compare the key. Names are compared in full, and never unescaped: the raw bytes encode
/// the value one to one. The slots are insert-only, so a published slot never changes its key.
struct NameTable<'a> {
    file: &'a [u8],
    words: Vec<AtomicU64>,
    first: usize,  // The word where the first slot starts, on a cache line boundary.
    stride: usize, // Words per slot.
    mask: usize,
    used: AtomicUsize,
    overflowed: AtomicU64,
}

impl<'a> NameTable<'a> {
    fn new(file: &'a [u8], slots: usize, countries: usize) -> Self {
        let stride = (4 + countries.div_ceil(64)).next_multiple_of(8);
        // An untouched allocation stays out of the memory peak until a thread writes to it.
        let zeroed = vec![0u64; slots * stride + 8];
        let words: Vec<AtomicU64> = zeroed.into_iter().map(AtomicU64::new).collect();
        NameTable {
            file,
            first: words.as_ptr().align_offset(64),
            words,
            stride,
            mask: slots - 1,
            used: AtomicUsize::new(0),
            overflowed: AtomicU64::new(0),
        }
    }

    /// Marks this name as seen in `country`, and returns whether that's new for the country.
    fn mark(&self, key: &[u8], country: usize) -> bool {
        let mut key_words = [0u64; 3];
        let inline = &key[..key.len().min(INLINE_BYTES)];
        for (word, chunk) in key_words.iter_mut().zip(inline.chunks(8)) {
            let mut bytes = [0; 8];
            bytes[..chunk.len()].copy_from_slice(chunk);
            *word = u64::from_le_bytes(bytes);
        }
        let hash = hash(&key_words, key.len());
        // The top half of the hash and the length, plus 2 to stay clear of EMPTY and BUSY.
        let published = (hash & 0xFFFF_FFFF_0000_0000) | (key.len() as u64 + 2);
        let mut slot = hash as usize & self.mask;
        loop {
            let base = self.first + slot * self.stride;
            let mut header = self.words[base].load(Acquire);
            if header == EMPTY {
                if self.used.load(Relaxed) * 8 >= (self.mask + 1) * 7 {
                    self.overflowed.store(1, Relaxed);
                    return false; // This whole pass will be thrown away.
                }
                let claim = self.words[base].compare_exchange(EMPTY, BUSY, Acquire, Acquire);
                match claim {
                    Ok(_) => {
                        self.fill_slot(base, key, &key_words);
                        self.words[base].store(published, Release);
                        return self.mark_country(base, country);
                    }
                    Err(current) => header = current,
                }
            }
            while header == BUSY {
                hint::spin_loop();
                header = self.words[base].load(Acquire);
            }
            if header == published && self.key_matches(base, key, &key_words) {
                return self.mark_country(base, country);
            }
            slot = (slot + 1) & self.mask;
        }
    }

    fn fill_slot(&self, base: usize, key: &[u8], key_words: &[u64; 3]) {
        self.used.fetch_add(1, Relaxed);
        if key.len() <= INLINE_BYTES {
            for (i, &word) in key_words.iter().enumerate() {
                self.words[base + 1 + i].store(word, Relaxed);
            }
        } else {
            let offset = key.as_ptr() as usize - self.file.as_ptr() as usize;
            self.words[base + 1].store(offset as u64, Relaxed);
        }
    }

    fn key_matches(&self, base: usize, key: &[u8], key_words: &[u64; 3]) -> bool {
        if key.len() <= INLINE_BYTES {
            let stored = |i: usize| self.words[base + 1 + i].load(Relaxed);
            key_words
                .iter()
                .enumerate()
                .all(|(i, &word)| stored(i) == word)
        } else {
            let offset = self.words[base + 1].load(Relaxed) as usize;
            &self.file[offset..offset + key.len()] == key
        }
    }

    fn mark_country(&self, base: usize, country: usize) -> bool {
        let word = &self.words[base + 4 + country / 64];
        let bit = 1u64 << (country % 64);
        word.load(Relaxed) & bit == 0 && word.fetch_or(bit, Relaxed) & bit == 0
    }
}

/// Parses the text field at `pos` and steps past the comma after it. Returns its raw bytes (which
/// encode the value one to one) and the length of its value. A quoted field ends at the first `"`
/// that isn't part of a `""` pair, and inside quotes every other byte is data, commas and LFs
/// included. An empty unquoted field and `""` both give empty raw bytes, so they are one value.
fn text_field<'a>(file: &'a [u8], pos: &mut usize) -> (&'a [u8], usize) {
    let start = *pos;
    if file[start] != b'"' {
        while file[*pos] != b',' {
            *pos += 1;
        }
        *pos += 1;
        return (&file[start..*pos - 1], *pos - 1 - start);
    }
    let mut end = start + 1;
    let mut pairs = 0;
    loop {
        while file[end] != b'"' {
            end += 1;
        }
        if file[end + 1] != b'"' {
            break;
        }
        end += 2;
        pairs += 1;
    }
    *pos = end + 2; // The closing quote and the comma.
    (&file[start + 1..end], end - start - 1 - pairs)
}

/// Reads `-?digits.dd` as exact cents, from the digits alone, since the fraction is always two
/// digits. Leaves `pos` at the terminator, or at the end of the file if the last one is missing.
fn amount_cents(file: &[u8], pos: &mut usize) -> i64 {
    let negative = file[*pos] == b'-';
    *pos += usize::from(negative);
    let mut cents = 0;
    while *pos < file.len() && file[*pos] >= b'.' {
        if file[*pos] != b'.' {
            cents = cents * 10 + i64::from(file[*pos] - b'0');
        }
        *pos += 1;
    }
    if negative { -cents } else { cents }
}

#[derive(Clone, Copy)]
struct Row {
    count: u64,
    id_sum: u64,
    amount_sum: i64,
    amount_min: i64,
    amount_max: i64,
    names_distinct: u64,
    name_bytes: u64,
    note_bytes: u64,
}

impl Row {
    const EMPTY: Row = Row {
        count: 0,
        id_sum: 0,
        amount_sum: 0,
        amount_min: i64::MAX,
        amount_max: i64::MIN,
        names_distinct: 0,
        name_bytes: 0,
        note_bytes: 0,
    };

    fn merge(&mut self, other: &Row) {
        self.count += other.count;
        self.id_sum += other.id_sum;
        self.amount_sum += other.amount_sum;
        self.amount_min = self.amount_min.min(other.amount_min);
        self.amount_max = self.amount_max.max(other.amount_max);
        self.names_distinct += other.names_distinct;
        self.name_bytes += other.name_bytes;
        self.note_bytes += other.note_bytes;
    }
}

/// Parses the records in `pos..end`, adding each to its country's row.
fn parse_records(
    file: &[u8],
    mut pos: usize,
    end: usize,
    regions: &Regions,
    names: &NameTable,
    rows: &mut [Row],
) {
    let unmatched = regions.keys.len() - 1;
    while pos < end {
        // A byte order mark can only start a record here, and no record otherwise starts with it.
        if file[pos] == 0xEF {
            pos += 3;
        }
        let id = next_number(file, &mut pos);
        let (name, name_bytes) = text_field(file, &mut pos);
        let (_, note_bytes) = text_field(file, &mut pos);
        let region = next_number(file, &mut pos) as usize;
        let cents = amount_cents(file, &mut pos);
        // CRLF or LF. The spec lets the last record end at the end of the file instead.
        pos += usize::from(pos < file.len() && file[pos] == b'\r');
        pos += 1;

        let country_of = &regions.country_of;
        let country = country_of
            .get(region)
            .map_or(unmatched, |&c| usize::from(c));
        let row = &mut rows[country];
        row.count += 1;
        row.id_sum += id;
        row.amount_sum += cents;
        row.amount_min = row.amount_min.min(cents);
        row.amount_max = row.amount_max.max(cents);
        row.name_bytes += name_bytes as u64;
        row.note_bytes += note_bytes as u64;
        row.names_distinct += u64::from(names.mark(name, country));
    }
}

/// Parses the whole file on `threads` threads and merges their rows. Eight segments per thread
/// balances the load. Tying the size to the thread count also makes the SD_THREADS test sweep move
/// every cut. Returns None if the name table filled up.
fn aggregate(file: &[u8], regions: &Regions, threads: usize, slots: usize) -> Option<Vec<Row>> {
    let names = NameTable::new(file, slots, regions.keys.len());
    let segment = (file.len() / (threads * 8)).max(1);
    let cursor = AtomicUsize::new(0);
    let worker_rows: Vec<Vec<Row>> = thread::scope(|scope| {
        let handles: Vec<_> = (0..threads)
            .map(|_| {
                scope.spawn(|| {
                    let mut rows = vec![Row::EMPTY; regions.keys.len()];
                    loop {
                        let start = cursor.fetch_add(segment, Relaxed);
                        if start >= file.len() {
                            return rows;
                        }
                        let first = match start {
                            0 => after_header(file),
                            _ => next_boundary(file, start),
                        };
                        let cut = start + segment;
                        let last = if cut >= file.len() {
                            file.len()
                        } else {
                            next_boundary(file, cut)
                        };
                        parse_records(file, first, last, regions, &names, &mut rows);
                    }
                })
            })
            .collect();
        let joined = handles.into_iter().map(|handle| handle.join());
        joined
            .map(|rows| rows.expect("panic = \"abort\" means a join can't fail"))
            .collect()
    });
    if names.overflowed.load(Relaxed) != 0 {
        return None;
    }
    let mut totals = vec![Row::EMPTY; regions.keys.len()];
    for rows in &worker_rows {
        for (total, row) in totals.iter_mut().zip(rows) {
            total.merge(row);
        }
    }
    Some(totals)
}

/// Prints cents as `W.CC`. The sign and the absolute value are separate because integer division
/// truncates toward zero, which would lose the sign of -0.75. Zero never gets a sign.
fn write_money(out: &mut impl Write, cents: i64) -> io::Result<()> {
    let sign = if cents < 0 { "-" } else { "" };
    let magnitude = cents.unsigned_abs();
    write!(out, " {sign}{}.{:02}", magnitude / 100, magnitude % 100)
}

fn main() -> io::Result<()> {
    let mut args = std::env::args().skip(1);
    let data_dir = args.next().unwrap_or_else(|| "/data".into());
    let out_dir = args.next().unwrap_or_else(|| "/out".into());
    let data_dir = Path::new(&data_dir);
    let threads = thread_count();
    let regions = parse_regions(map_file(&data_dir.join("regions.csv"))?);
    let file = map_file(&data_dir.join("records.csv"))?;

    // The spec's pool of names is a distribution, not a promise. If a file holds more names than the
    // table, the pass is thrown away and redone with a bigger table.
    let mut slots = INITIAL_SLOTS;
    let totals = loop {
        if let Some(totals) = aggregate(file, &regions, threads, slots) {
            break totals;
        }
        slots *= 4;
    };

    let mut out = BufWriter::new(File::create(Path::new(&out_dir).join("aggregates.txt"))?);
    for (key, row) in regions.keys.iter().zip(&totals) {
        if row.count == 0 {
            continue; // A country with no records prints no line.
        }
        write!(out, "{key} {} {}", row.count, row.id_sum)?;
        write_money(&mut out, row.amount_sum)?;
        write_money(&mut out, row.amount_min)?;
        write_money(&mut out, row.amount_max)?;
        let (distinct, name_bytes) = (row.names_distinct, row.name_bytes);
        writeln!(out, " {distinct} {name_bytes} {}", row.note_bytes)?;
    }
    out.flush()
}
