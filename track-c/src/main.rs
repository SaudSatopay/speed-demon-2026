//! Track C (Join): aggregates per country, from a hostile CSV joined against a region table.
//!
//! Threads claim segments of the mapped file from an atomic cursor. Segments are cut just after a
//! CRLF, because the spec promises that a CR appears nowhere else, so that is always a record
//! boundary, and no quote counting is needed. The parser follows section 2.1 of the spec, with block
//! loads in place of byte loops. It never unescapes anything: a quoted field's raw bytes, with its
//! `""` pairs intact, encode its value one to one, and the value's length is the raw length minus the
//! number of pairs. Records wait in small batches while their lookups are prefetched. Each thread
//! sums into its own rows, and one shared table counts the distinct names per country.

use std::arch::x86_64::{
    __m256i, _MM_HINT_T0, _mm_prefetch, _mm256_cmpeq_epi8, _mm256_loadu_si256,
    _mm256_movemask_epi8, _mm256_set1_epi8,
};
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

/// Readable zero bytes after each mapped file, so that block loads can run past its end.
const PADDING: usize = 4096;

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

/// Asks for the cache line holding `item`, so that a later access doesn't stall on it.
#[inline]
fn prefetch<T>(item: &T) {
    // SAFETY: prefetching has no effect on memory, and the reference is valid anyway.
    unsafe { _mm_prefetch::<_MM_HINT_T0>(std::ptr::from_ref(item).cast()) }
}

/// One bit per byte of the 64 at `pos`, set where there's a double quote.
#[inline]
fn quote_mask(file: &[u8], pos: usize) -> u64 {
    assert!(pos + 64 <= file.len() + PADDING);
    // SAFETY: the assert keeps both loads inside the mapping, they need no alignment, and the build
    // targets x86-64-v3, which has AVX2.
    unsafe {
        let quotes = _mm256_set1_epi8(b'"' as i8);
        let at = file.as_ptr().add(pos).cast::<__m256i>();
        let low = _mm256_cmpeq_epi8(_mm256_loadu_si256(at), quotes);
        let high = _mm256_cmpeq_epi8(_mm256_loadu_si256(at.add(1)), quotes);
        let (low, high) = (_mm256_movemask_epi8(low), _mm256_movemask_epi8(high));
        u64::from(low as u32) | u64::from(high as u32) << 32
    }
}

/// Parses 8 digit bytes, in three multiplications. The lowest byte is the most significant digit.
#[inline]
fn parse_eight_digits(word: u64) -> u64 {
    let digits = word & 0x0F0F_0F0F_0F0F_0F0F;
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

/// Reads the digits at `pos` and steps past the separator after them. One 8-byte load settles all but
/// the longest numbers: digits have bit 4 set, and commas and line ends don't, so the first clear bit
/// marks the end, with no byte loop to mispredict.
#[inline]
fn next_number(text: &[u8], pos: &mut usize) -> u64 {
    let word = load_u64(text, *pos);
    let ends = !word & 0x1010_1010_1010_1010;
    if ends != 0 {
        let digits = ends.trailing_zeros() as usize / 8;
        *pos += digits + 1;
        return parse_eight_digits(word << (64 - 8 * digits));
    }
    let mut value = parse_eight_digits(word);
    *pos += 8;
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
#[inline]
fn hash(key_words: &[u64; 3], len: usize) -> u64 {
    key_words.iter().fold(len as u64, |hash, &word| {
        let mixed = (hash ^ word).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        mixed ^ (mixed >> 29)
    })
}

#[cfg(sd_const_hash)]
#[inline]
fn hash(_key_words: &[u64; 3], _len: usize) -> u64 {
    0
}

/// For a name of each length up to 24 bytes, which bytes of its three words belong to it.
const KEY_MASKS: [[u64; 3]; 25] = {
    let mut masks = [[0; 3]; 25];
    let mut len = 0;
    while len <= 24 {
        let mut word = 0;
        while word < 3 {
            let bytes = if len > word * 8 { len - word * 8 } else { 0 };
            masks[len][word] = if bytes >= 8 {
                !0
            } else {
                (1 << (8 * bytes)) - 1
            };
            word += 1;
        }
        len += 1;
    }
    masks
};

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

    /// The first slot to try for a name with this hash.
    #[inline]
    fn slot_header(&self, hash: u64) -> &AtomicU64 {
        &self.words[self.first + (hash as usize & self.mask) * self.stride]
    }

    /// Marks this name as seen in `country`, and returns whether that's new for the country.
    #[inline]
    fn mark(&self, key: &[u8], key_words: &[u64; 3], hash: u64, country: usize) -> bool {
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
                        self.fill_slot(base, key, key_words);
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
            if header == published && self.key_matches(base, key, key_words) {
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

/// A text field's raw bytes, which encode its value one to one, and the length of its value.
#[derive(Clone, Copy, Default)]
struct TextField {
    start: usize,
    raw_len: usize,
    value_len: usize,
}

/// Parses the text field at `pos` and steps past the comma after it. A quoted field ends at the first
/// `"` that isn't part of a `""` pair, and inside quotes every other byte is data, commas and LFs
/// included. An empty unquoted field and `""` both give empty raw bytes, so they are one value.
#[inline]
fn text_field(file: &[u8], pos: &mut usize) -> TextField {
    let start = *pos;
    if file[start] != b'"' {
        while file[*pos] != b',' {
            *pos += 1;
        }
        *pos += 1;
        let raw_len = *pos - 1 - start;
        return TextField {
            start,
            raw_len,
            value_len: raw_len,
        };
    }
    let mut scan = start + 1;
    let mut pairs = 0;
    loop {
        let quotes = quote_mask(file, scan);
        if quotes == 0 {
            scan += 64;
            continue;
        }
        let at = scan + quotes.trailing_zeros() as usize;
        if file[at + 1] != b'"' {
            *pos = at + 2; // The closing quote and the comma.
            let raw_len = at - start - 1;
            return TextField {
                start: start + 1,
                raw_len,
                value_len: raw_len - pairs,
            };
        }
        pairs += 1;
        scan = at + 2;
    }
}

/// Reads `-?digits.dd` as exact cents, and leaves `pos` at the terminator, or at the end of the file
/// if the last one is missing. Digits and the point have bit 5 set, and line ends don't. Amounts are
/// at most 8 bytes (spec section 2.2), so one load holds them. It's aligned to the end, and the point
/// is squeezed out by moving the digits before it up a byte, which leaves 8 digits to parse.
#[inline]
fn amount_cents(file: &[u8], pos: &mut usize) -> i64 {
    let negative = file[*pos] == b'-';
    *pos += usize::from(negative);
    let word = load_u64(file, *pos);
    let ends = !word & 0x2020_2020_2020_2020;
    let chars = if ends != 0 {
        ends.trailing_zeros() as usize / 8
    } else {
        8
    };
    let cents = if ends != 0 || file[*pos + 8] & 0x20 == 0 {
        *pos += chars;
        let aligned = word << (64 - 8 * chars);
        let squeezed = (aligned & 0xFFFF_0000_0000_0000) | (aligned & 0x0000_00FF_FFFF_FFFF) << 8;
        parse_eight_digits(squeezed) as i64
    } else {
        let mut cents = 0;
        while file[*pos] & 0x20 != 0 {
            if file[*pos] != b'.' {
                cents = cents * 10 + i64::from(file[*pos] - b'0');
            }
            *pos += 1;
        }
        cents
    };
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

/// A parsed record, waiting for the cache lines its lookups need.
#[derive(Clone, Copy, Default)]
struct Pending {
    id: u64,
    cents: i64,
    region: usize,
    name: TextField,
    note_bytes: usize,
    key_words: [u64; 3],
    hash: u64,
}

/// Records parsed before any lookup is done. A batch can prefetch all its cache lines and let them
/// arrive together, instead of stalling on each in turn.
const BATCH: usize = 16;

/// Parses the records in `pos..end`, adding each to its country's row.
fn parse_records(
    file: &[u8],
    mut pos: usize,
    end: usize,
    regions: &Regions,
    names: &NameTable,
    rows: &mut [Row],
) {
    let mut batch = [Pending::default(); BATCH];
    let mut count = 0;
    while pos < end {
        // A byte order mark can only start a record here, and no record otherwise starts with it.
        if file[pos] == 0xEF {
            pos += 3;
        }
        let id = next_number(file, &mut pos);
        let name = text_field(file, &mut pos);
        let note = text_field(file, &mut pos);
        let region = next_number(file, &mut pos) as usize;
        let cents = amount_cents(file, &mut pos);
        // CRLF or LF, with no branch to mispredict, since records mix them about half and half. Past
        // the end of the file, the padding is zeros, which don't count as a CR.
        pos += 1 + usize::from(file.get(pos) == Some(&b'\r'));

        let masks = KEY_MASKS[name.raw_len.min(INLINE_BYTES)];
        let mut key_words = [0; 3];
        for (index, (word, mask)) in key_words.iter_mut().zip(masks).enumerate() {
            *word = load_u64(file, name.start + 8 * index) & mask;
        }
        let hash = hash(&key_words, name.raw_len);
        prefetch(names.slot_header(hash));
        prefetch(&regions.country_of[region.min(regions.country_of.len() - 1)]);
        batch[count] = Pending {
            id,
            cents,
            region,
            name,
            note_bytes: note.value_len,
            key_words,
            hash,
        };
        count += 1;
        if count == BATCH {
            add_records(file, &batch, regions, names, rows);
            count = 0;
        }
    }
    add_records(file, &batch[..count], regions, names, rows);
}

#[inline]
fn add_records(
    file: &[u8],
    batch: &[Pending],
    regions: &Regions,
    names: &NameTable,
    rows: &mut [Row],
) {
    let unmatched = regions.keys.len() - 1;
    for record in batch {
        let country_of = &regions.country_of;
        let country = country_of
            .get(record.region)
            .map_or(unmatched, |&c| usize::from(c));
        let row = &mut rows[country];
        row.count += 1;
        row.id_sum += record.id;
        row.amount_sum += record.cents;
        row.amount_min = row.amount_min.min(record.cents);
        row.amount_max = row.amount_max.max(record.cents);
        row.name_bytes += record.name.value_len as u64;
        row.note_bytes += record.note_bytes as u64;
        let key = &file[record.name.start..record.name.start + record.name.raw_len];
        row.names_distinct += u64::from(names.mark(key, &record.key_words, record.hash, country));
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
