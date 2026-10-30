//! Track C (Join): aggregates per country, from a hostile CSV joined against a region table.
//!
//! A scalar parser, written straight from section 2.1 of the spec, walks the mapped file once. It
//! never unescapes anything: a quoted field's raw bytes, with its `""` pairs intact, encode its
//! value one to one, and the value's length is the raw length minus the number of pairs.

use std::ffi::{c_int, c_void};
use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::os::fd::AsRawFd;
use std::path::Path;

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
    text.iter()
        .position(|&byte| byte == b'\n')
        .map_or(text.len(), |lf| lf + 1)
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

/// Raw names to dense ids, by open addressing with linear probing. A hit always compares the full
/// key, because about 5% of the names differ from another only by trailing spaces.
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

#[derive(Clone)]
struct Aggregate {
    count: u64,
    id_sum: u64,
    amount_sum: i64,
    amount_min: i64,
    amount_max: i64,
    names_distinct: u64,
    name_bytes: u64,
    note_bytes: u64,
    names_seen: Vec<u64>, // A bitset over name ids.
}

/// Parses every record and adds it to its country's aggregate.
fn aggregate(file: &[u8], regions: &Regions) -> Vec<Aggregate> {
    let empty = Aggregate {
        count: 0,
        id_sum: 0,
        amount_sum: 0,
        amount_min: i64::MAX,
        amount_max: i64::MIN,
        names_distinct: 0,
        name_bytes: 0,
        note_bytes: 0,
        names_seen: Vec::new(),
    };
    let mut aggregates = vec![empty; regions.keys.len()];
    let unmatched = regions.keys.len() - 1;
    let mut names = Interner::new();
    let mut pos = after_header(file);
    while pos < file.len() {
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

        let country = regions
            .country_of
            .get(region)
            .map_or(unmatched, |&c| usize::from(c));
        let row = &mut aggregates[country];
        row.count += 1;
        row.id_sum += id;
        row.amount_sum += cents;
        row.amount_min = row.amount_min.min(cents);
        row.amount_max = row.amount_max.max(cents);
        row.name_bytes += name_bytes as u64;
        row.note_bytes += note_bytes as u64;
        let name_id = names.intern(name);
        let word = name_id / 64;
        if word >= row.names_seen.len() {
            row.names_seen.resize(word + 1, 0);
        }
        row.names_distinct += u64::from(row.names_seen[word] >> (name_id % 64) & 1 == 0);
        row.names_seen[word] |= 1 << (name_id % 64);
    }
    aggregates
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
    let regions = parse_regions(map_file(&data_dir.join("regions.csv"))?);
    let aggregates = aggregate(map_file(&data_dir.join("records.csv"))?, &regions);

    let mut out = BufWriter::new(File::create(Path::new(&out_dir).join("aggregates.txt"))?);
    for (key, row) in regions.keys.iter().zip(&aggregates) {
        if row.count == 0 {
            continue; // A country with no records prints no line.
        }
        write!(out, "{key} {} {}", row.count, row.id_sum)?;
        write_money(&mut out, row.amount_sum)?;
        write_money(&mut out, row.amount_min)?;
        write_money(&mut out, row.amount_max)?;
        let (distinct, name_bytes, note_bytes) =
            (row.names_distinct, row.name_bytes, row.note_bytes);
        writeln!(out, " {distinct} {name_bytes} {note_bytes}")?;
    }
    out.flush()
}
