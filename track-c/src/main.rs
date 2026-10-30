//! Track C (Join): aggregates per country, from a hostile CSV joined against a region table.
//!
//! Threads claim segments of the mapped file from an atomic cursor. Segments are cut just after a
//! CRLF, because the spec promises that a CR appears nowhere else, so that is always a record
//! boundary, and no quote counting is needed. The parser follows section 2.1 of the spec, with block
//! loads in place of byte loops. It never unescapes anything: a quoted field's raw bytes, with its
//! `""` pairs intact, encode its value one to one, and the value's length is the raw length minus the
//! number of pairs. Records wait in small batches while their lookups are prefetched. Each thread
//! sums into its own rows, and one shared table counts the distinct names per country.

use std::arch::x86_64::*;
use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::os::fd::AsRawFd;
use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering::*};
use std::thread;

/// Number of possible country codes, `[A-Z]{3}` (spec section 2.3).
const CODES: usize = 26 * 26 * 26;

// Declared by hand to keep the crate at zero dependencies; std already links glibc.
unsafe extern "C" {
    fn mmap(at: *mut u8, len: usize, p: i32, f: i32, fd: i32, o: i64) -> *mut u8;
}

/// Readable zero bytes after each mapped file, so that block loads can run past its end.
const PADDING: usize = 4096;

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
        let null = std::ptr::null_mut();
        let base = mmap(null, len + PADDING, read, private | anonymous, -1, 0);
        let mapped = mmap(base, len, read, private | fixed, file.as_raw_fd(), 0);
        assert!(base as isize != -1 && (mapped == base || len == 0));
        Ok(std::slice::from_raw_parts(base.cast(), len))
    }
}

/// Loads 8 bytes, little-endian, which may run into the padding.
fn load_u64(file: &[u8], pos: usize) -> u64 {
    assert!(pos + 8 <= file.len() + PADDING);
    // SAFETY: the assert keeps the load inside the mapping, and the load needs no alignment.
    unsafe { file.as_ptr().add(pos).cast::<u64>().read_unaligned() }
}

/// Asks for the cache line holding `item`, so that a later access doesn't stall on it.
fn prefetch<T>(item: &T) {
    // SAFETY: prefetching has no effect on memory, and the reference is valid anyway.
    unsafe { _mm_prefetch::<_MM_HINT_T0>(std::ptr::from_ref(item).cast()) }
}

/// One bit per byte of the 64 at `pos`, set where there's a double quote.
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
fn parse_eight_digits(word: u64) -> u64 {
    let digits = word & 0x0F0F_0F0F_0F0F_0F0F;
    let pairs = digits.wrapping_mul((10 << 8) + 1) >> 8 & 0x00FF_00FF_00FF_00FF;
    let quads = pairs.wrapping_mul((100 << 16) + 1) >> 16 & 0x0000_FFFF_0000_FFFF;
    quads.wrapping_mul((10000 << 32) + 1) >> 32
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

/// Reads the digits at `pos` and steps past the separator after them. Ids and region ids have at most
/// 8 digits (spec sections 2.2 and 3), so one 8-byte load settles them: digits have bit 4 set, and
/// commas and line ends don't, so the first clear bit marks the end, with no loop to mispredict.
fn next_number(text: &[u8], pos: &mut usize) -> u64 {
    let word = load_u64(text, *pos);
    let digits = (!word & 0x1010_1010_1010_1010).trailing_zeros() as usize / 8;
    *pos += digits + 1;
    parse_eight_digits(word << (64 - 8 * digits))
}

/// The position just past the first LF, where the first data line starts.
fn after_header(text: &[u8]) -> usize {
    text.iter()
        .position(|&c| c == b'\n')
        .map_or(text.len(), |lf| lf + 1)
}

/// The first record boundary at or after `pos`: just past the next CRLF, or the end of the file.
/// A CR can only be the first byte of a CRLF terminator (spec section 2.2), so this is never inside
/// quotes. About half the records end in CRLF, so it's never far. A file without one simply doesn't
/// get split.
fn next_boundary(file: &[u8], pos: usize) -> usize {
    file[pos..]
        .iter()
        .position(|&c| c == b'\r')
        .map_or(file.len(), |cr| pos + cr + 2)
}

/// The join table: country indices by region id, and each index's key. Index 0 is `_UNMATCHED`, and
/// the codes take the next indices as they first appear, so the output is sorted when it's written.
/// The table's last entry is past every region id, and stays unmatched, for every id above them.
struct Regions {
    country_of: Vec<u8>,
    keys: Vec<String>,
}

fn parse_regions(file: &[u8]) -> Regions {
    let mut pos = after_header(file);
    let (mut country_of, mut index_of) = (vec![0u8], vec![0u8; CODES]);
    let mut keys = vec!["_UNMATCHED".to_string()];
    while pos < file.len() {
        let id = next_number(file, &mut pos) as usize;
        let letter = |i: usize| usize::from(file[pos + i] - b'A');
        let code = letter(0) * 26 * 26 + letter(1) * 26 + letter(2);
        if index_of[code] == 0 {
            assert!(keys.len() < 256, "the spec has 240 countries");
            index_of[code] = keys.len() as u8;
            keys.push(String::from_utf8_lossy(&file[pos..pos + 3]).into_owned());
        }
        pos += 4; // The three letters and the LF.
        // Grown in doubling steps, so the table takes 1 MB for ids up to a million.
        if country_of.len() <= id + 1 {
            country_of.resize((id + 2).next_power_of_two(), 0);
        }
        country_of[id] = index_of[code];
    }
    Regions { country_of, keys }
}

/// Mixes a name's first 24 bytes and its length. A test build swaps it for a constant, so that every
/// key collides and any logic that trusts a hash without comparing the key shows up as a wrong
/// answer.
fn hash(key_words: &[u64; 3], len: usize) -> u64 {
    let hash = key_words.iter().fold(len as u64, |hash, &word| {
        let mixed = (hash ^ word).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        mixed ^ (mixed >> 29)
    });
    if cfg!(sd_const_hash) { 0 } else { hash }
}

/// Slots to start with: 2^17 for the generator's pool of 100,000 names. A test build starts tiny, to
/// exercise the restart that a larger pool would need.
const INITIAL_SLOTS: usize = if cfg!(sd_const_hash) { 1 << 4 } else { 1 << 17 };
/// Words per slot, which is one cache line: a header, three words of key, and 256 country bits.
const SLOT_WORDS: usize = 8;
/// Most names fit in the slot itself: on seed 1, 99.99% of them.
const INLINE_BYTES: usize = 24;
const EMPTY: u64 = 0;
const BUSY: u64 = 1;

/// Distinct names per country, in one table shared by all threads, with no locks. Each slot holds a
/// header, the name's first 24 bytes (or its file offset if it's longer), and one bit per country
/// that has seen this name. So a lookup costs one cache miss, with no second memory access to compare
/// the key. Names are compared in full, and never unescaped: the raw bytes encode the value one to
/// one. The slots are insert-only, so a published slot never changes its key.
struct NameTable<'a> {
    file: &'a [u8],
    words: Vec<AtomicU64>,
    first: usize, // The word where the first slot starts, on a cache line boundary.
    mask: usize,
    used: AtomicUsize,
}

impl<'a> NameTable<'a> {
    fn new(file: &'a [u8], slots: usize) -> Self {
        // An untouched allocation stays out of the memory peak until a thread writes to it.
        let zeroed = vec![0u64; slots * SLOT_WORDS + 8];
        let words: Vec<AtomicU64> = zeroed.into_iter().map(AtomicU64::new).collect();
        NameTable {
            file,
            first: words.as_ptr().align_offset(64),
            words,
            mask: slots - 1,
            used: AtomicUsize::new(0),
        }
    }

    fn is_full(&self) -> bool {
        self.used.load(Relaxed) * 8 >= (self.mask + 1) * 7
    }

    /// Marks this name as seen in `country`, and returns whether that's new for the country. When the
    /// table is full it gives up, and `aggregate` throws the whole pass away.
    fn mark(&self, key: &[u8], key_words: &[u64; 3], hash: u64, country: usize) -> bool {
        // The top half of the hash and the length, plus 2 to stay clear of EMPTY and BUSY.
        let published = (hash & 0xFFFF_FFFF_0000_0000) | (key.len() as u64 + 2);
        let mut slot = hash as usize & self.mask;
        let base = loop {
            let base = self.first + slot * SLOT_WORDS;
            let (header, stored) = (&self.words[base], &self.words[base + 1..base + 4]);
            if header.load(Acquire) == EMPTY {
                if self.is_full() {
                    return false;
                }
                if header
                    .compare_exchange(EMPTY, BUSY, Acquire, Acquire)
                    .is_ok()
                {
                    self.used.fetch_add(1, Relaxed);
                    if key.len() <= INLINE_BYTES {
                        (0..3).for_each(|i| stored[i].store(key_words[i], Relaxed));
                    } else {
                        let offset = key.as_ptr() as usize - self.file.as_ptr() as usize;
                        stored[0].store(offset as u64, Relaxed);
                    }
                    header.store(published, Release);
                    break base;
                }
            }
            while header.load(Acquire) == BUSY {
                std::hint::spin_loop();
            }
            if header.load(Acquire) == published {
                let found = if key.len() <= INLINE_BYTES {
                    (0..3).all(|i| stored[i].load(Relaxed) == key_words[i])
                } else {
                    let offset = stored[0].load(Relaxed) as usize;
                    &self.file[offset..offset + key.len()] == key
                };
                if found {
                    break base;
                }
            }
            slot = (slot + 1) & self.mask;
        };
        let word = &self.words[base + 4 + country / 64];
        let bit = 1u64 << (country % 64);
        word.load(Relaxed) & bit == 0 && word.fetch_or(bit, Relaxed) & bit == 0
    }
}

/// A text field: where its raw bytes start, their length, and the length of its value. The raw bytes
/// encode the value one to one.
#[derive(Clone, Copy, Default)]
struct TextField(usize, usize, usize);

/// Parses the text field at `pos` and steps past the comma after it. A quoted field ends at the first
/// `"` that isn't part of a `""` pair, and inside quotes every other byte is data, commas and LFs
/// included. An empty unquoted field and `""` both give empty raw bytes, so they are one value.
fn text_field(file: &[u8], pos: &mut usize) -> TextField {
    let start = *pos;
    if file[start] != b'"' {
        while file[*pos] != b',' {
            *pos += 1;
        }
        *pos += 1;
        return TextField(start, *pos - 1 - start, *pos - 1 - start);
    }
    let mut scan = start + 1;
    let mut pairs = 0;
    loop {
        let quotes = quote_mask(file, scan);
        let at = scan + quotes.trailing_zeros() as usize;
        if quotes == 0 {
            scan += 64;
        } else if file[at + 1] == b'"' {
            pairs += 1;
            scan = at + 2;
        } else {
            *pos = at + 2; // The closing quote and the comma.
            return TextField(start + 1, at - start - 1, at - start - 1 - pairs);
        }
    }
}

/// Reads `-?digits.dd` as exact cents, and leaves `pos` at the terminator, or at the end of the file
/// if the last one is missing. Digits and the point have bit 5 set, and line ends don't. Amounts are
/// at most 8 bytes (spec section 2.2), so one load holds them. It's aligned to the end, and the point
/// is squeezed out by moving the digits before it up a byte, which leaves 8 digits to parse.
fn amount_cents(file: &[u8], pos: &mut usize) -> i64 {
    let negative = file[*pos] == b'-';
    *pos += usize::from(negative);
    let word = load_u64(file, *pos);
    let chars = (!word & 0x2020_2020_2020_2020).trailing_zeros() as usize / 8;
    *pos += chars;
    let aligned = word << (64 - 8 * chars);
    let squeezed = (aligned & 0xFFFF_0000_0000_0000) | (aligned & 0x0000_00FF_FFFF_FFFF) << 8;
    let cents = parse_eight_digits(squeezed) as i64;
    if negative { -cents } else { cents }
}

/// A country's totals, in output order: count, id sum, amount sum, lowest amount, highest amount,
/// distinct names, name bytes and note bytes. A single record is a row too, so one `merge` does both
/// jobs: adding a record to its country, and adding up the threads' rows.
type Row = [i64; 8];
const EMPTY_ROW: Row = [0, 0, 0, i64::MAX, i64::MIN, 0, 0, 0];
fn merge(row: &mut Row, other: &Row) {
    for (i, (total, &value)) in row.iter_mut().zip(other).enumerate() {
        *total = match i {
            3 => (*total).min(value),
            4 => (*total).max(value),
            _ => *total + value,
        };
    }
}

/// A parsed record, waiting for the cache lines its lookups need: id, cents, region, name, note
/// bytes, the name's first 24 bytes, and their hash.
type Pending = (i64, i64, usize, TextField, usize, [u64; 3], u64);

/// Records parsed before any lookup is done. A batch can prefetch all its cache lines and let them
/// arrive together, instead of stalling on each in turn.
const BATCH: usize = 16;

/// Parses the records in `pos..end`, adding each to its country's row.
fn parse_records(
    mut pos: usize,
    end: usize,
    regions: &Regions,
    names: &NameTable,
    rows: &mut [Row],
) {
    let file = names.file;
    let last_region = regions.country_of.len() - 1;
    let mut batch = [Pending::default(); BATCH];
    let mut count = 0;
    while pos < end {
        // A byte order mark can only start a record here, and no record otherwise starts with it.
        pos += 3 * usize::from(file[pos] == 0xEF);
        let id = next_number(file, &mut pos) as i64;
        let name = text_field(file, &mut pos);
        let note = text_field(file, &mut pos);
        let region = (next_number(file, &mut pos) as usize).min(last_region);
        let cents = amount_cents(file, &mut pos);
        // CRLF or LF, with no branch to mispredict, since records mix them about half and half. Past
        // the end of the file, the padding is zeros, which don't count as a CR.
        pos += 1 + usize::from(file.get(pos) == Some(&b'\r'));

        // The name's first 24 bytes, with whatever follows it masked off. BZHI reads only the low 8
        // bits of its count, so a name of 32 bytes or more can lose a word here. That's still the same
        // words for the same name, and only the hash uses them: longer names are compared in full.
        let mut key_words = [0; 3];
        for (word, key_word) in key_words.iter_mut().enumerate() {
            let bytes = name.1.saturating_sub(8 * word) as u32;
            // SAFETY: BZHI is in BMI2, which x86-64-v3 includes, and it only touches registers.
            *key_word = unsafe { _bzhi_u64(load_u64(file, name.0 + 8 * word), 8 * bytes) };
        }
        let hash = hash(&key_words, name.1);
        prefetch(&names.words[names.first + (hash as usize & names.mask) * SLOT_WORDS]);
        prefetch(&regions.country_of[region]);
        batch[count] = (id, cents, region, name, note.2, key_words, hash);
        count += 1;
        if count == BATCH {
            add_records(&batch, regions, names, rows);
            count = 0;
        }
    }
    add_records(&batch[..count], regions, names, rows);
}

fn add_records(batch: &[Pending], regions: &Regions, names: &NameTable, rows: &mut [Row]) {
    for &(id, cents, region, name, note_bytes, key_words, hash) in batch {
        let country = usize::from(regions.country_of[region]);
        let key = &names.file[name.0..name.0 + name.1];
        let is_new = i64::from(names.mark(key, &key_words, hash, country));
        let (name_bytes, note_bytes) = (name.2 as i64, note_bytes as i64);
        merge(
            &mut rows[country],
            &[1, id, cents, cents, cents, is_new, name_bytes, note_bytes],
        );
    }
}

/// Parses the whole file on `threads` threads and merges their rows. Eight segments per thread
/// balances the load. Tying the size to the thread count also makes the SD_THREADS test sweep move
/// every cut. The spec's pool of names is a distribution, not a promise: if a file holds more names
/// than the table, the pass is thrown away and redone with a bigger table.
fn aggregate(file: &[u8], regions: &Regions, threads: usize, slots: usize) -> Vec<Row> {
    let names = NameTable::new(file, slots);
    let segment = (file.len() / (threads * 8)).max(1);
    let cursor = AtomicUsize::new(0);
    let totals = Mutex::new(vec![EMPTY_ROW; regions.keys.len()]);
    on_threads(threads, || {
        let mut rows = vec![EMPTY_ROW; regions.keys.len()];
        let claim = || Some(cursor.fetch_add(segment, Relaxed)).filter(|&s| s < file.len());
        while let Some(start) = claim() {
            let first = (start > 0).then(|| next_boundary(file, start)).unwrap_or(0);
            let last = next_boundary(file, (start + segment).min(file.len()));
            parse_records(first, last, regions, &names, &mut rows);
        }
        for (total, row) in totals.lock().unwrap().iter_mut().zip(&rows) {
            merge(total, row);
        }
    });
    if names.is_full() {
        return aggregate(file, regions, threads, slots * 4);
    }
    totals.into_inner().unwrap()
}

fn main() -> io::Result<()> {
    let mut args = std::env::args().skip(1);
    let data_dir = args.next().unwrap_or_else(|| "/data".into());
    let out_dir = args.next().unwrap_or_else(|| "/out".into());
    let data_dir = Path::new(&data_dir);
    let threads = thread_count();
    let regions = parse_regions(map_file(&data_dir.join("regions.csv"))?);
    let records = map_file(&data_dir.join("records.csv"))?;
    let totals = aggregate(
        &records[after_header(records)..],
        &regions,
        threads,
        INITIAL_SLOTS,
    );

    // Sorted by key, bytewise: `_UNMATCHED` lands last, because `_` is above `Z`. A row's columns are
    // already in output order, and a group without records gets no line.
    let mut lines: Vec<_> = regions.keys.iter().zip(&totals).collect();
    lines.sort();
    let mut out = BufWriter::new(File::create(Path::new(&out_dir).join("aggregates.txt"))?);
    for (key, row) in lines.into_iter().filter(|(_, row)| row[0] > 0) {
        write!(out, "{key} {} {}", row[0], row[1])?;
        // Printed as `W.CC`, with the sign apart, because integer division truncates toward zero,
        // which would lose the sign of -0.75. Zero never gets a sign.
        for cents in &row[2..5] {
            let sign = if *cents < 0 { "-" } else { "" };
            let magnitude = cents.unsigned_abs();
            write!(out, " {sign}{}.{:02}", magnitude / 100, magnitude % 100)?;
        }
        writeln!(out, " {} {} {}", row[5], row[6], row[7])?;
    }
    out.flush()
}
