//! Track C (Join): aggregates per country, from a hostile CSV joined against a region table.
//!
//! Threads claim segments of the mapped file from an atomic cursor. Segments are cut just after a
//! CRLF, because the spec promises that a CR appears nowhere else, so that is always a record
//! boundary, and no quote counting is needed. The parser follows section 2.1 of the spec, with block
//! loads in place of byte loops. It never unescapes anything: a quoted field's raw bytes, with its
//! `""` pairs intact, encode its value one to one, and the value's length is the raw length minus the
//! number of pairs. Records wait in small batches while their lookups are prefetched. Each thread
//! sums into its own rows, and one shared table counts the distinct names per country.

use std::io::{self, BufWriter, Write};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering::*};
use std::{arch::x86_64::*, fs::File, os::fd::AsRawFd, ptr::null_mut, sync::Mutex, thread};

// Declared by hand to keep the crate at zero dependencies; std already links glibc.
unsafe extern "C" {
    fn mmap(at: *mut u8, len: usize, p: i32, f: i32, fd: i32, o: i64) -> *mut u8;
}

/// Readable zero bytes after each mapped file, so that block loads can run past its end.
const PADDING: usize = 4096;

/// Maps a file read-only for the life of the process, followed by `PADDING` readable zero bytes. The
/// scorer's memory peak doesn't charge us for these pages, unlike reading the file onto the heap.
fn map_file(path: &str) -> io::Result<&'static [u8]> {
    let (read, private, fixed, anonymous) = (1, 2, 0x10, 0x20);
    let file = File::open(path)?;
    let len = file.metadata()?.len() as usize;
    // SAFETY: the first call reserves fresh zero pages, and the second maps the file over the start
    // of them, which fails only on an empty file. Neither is ever unmapped, and nothing writes to the
    // dataset while we run.
    unsafe {
        let base = mmap(null_mut(), len + PADDING, read, private | anonymous, -1, 0);
        let mapped = mmap(base, len, read, private | fixed, file.as_raw_fd(), 0);
        assert!(base as isize != -1 && (mapped == base || len == 0));
        Ok(std::slice::from_raw_parts(base.cast(), len))
    }
}

/// Loads 8 bytes, little-endian, which may run into the padding. Every position here comes from
/// stepping through the file, which indexes it first and panics past its end, so a load starts at
/// most 16 bytes past the end, well inside the padding.
fn load_u64(file: &[u8], pos: usize) -> u64 {
    debug_assert!(pos + 8 <= file.len() + PADDING);
    // SAFETY: the padding keeps the load inside the mapping (see above), and it needs no alignment.
    unsafe { file.as_ptr().add(pos).cast::<u64>().read_unaligned() }
}

/// Asks for the cache line holding `item`, so that a later access doesn't stall on it. `_MM_HINT_ET0`
/// asks to own the line, for a write: a locked update then needs no second trip to take it from the
/// core that wrote it last.
fn prefetch<const HINT: i32, T>(item: &T) {
    // SAFETY: prefetching has no effect on memory, and the reference is valid anyway.
    unsafe { _mm_prefetch::<HINT>(std::ptr::from_ref(item).cast()) }
}

/// One bit per byte of the 64 at `pos`, set where there's a double quote.
fn quote_mask(file: &[u8], pos: usize) -> u64 {
    assert!(pos + 64 <= file.len() + PADDING);
    // SAFETY: the assert keeps both loads inside the mapping, they need no alignment, and the build
    // targets x86-64-v3, which has AVX2.
    unsafe {
        let quotes = _mm256_set1_epi8(b'"' as i8);
        let mask = |at| _mm256_movemask_epi8(_mm256_cmpeq_epi8(_mm256_loadu_si256(at), quotes));
        let at = file.as_ptr().add(pos).cast::<__m256i>();
        u64::from(mask(at) as u32) | u64::from(mask(at.add(1)) as u32) << 32
    }
}

/// The parity of the bits up to and including each bit, which is a carry-less multiplication by
/// all ones.
fn prefix_xor(bits: u64) -> u64 {
    // SAFETY: PCLMULQDQ is on for every build (`.cargo/config.toml`), and these intrinsics only
    // touch registers.
    unsafe {
        let (bits, ones) = (_mm_cvtsi64_si128(bits as i64), _mm_set1_epi64x(-1));
        _mm_cvtsi128_si64(_mm_clmulepi64_si128::<0>(bits, ones)) as u64
    }
}

/// Parses 8 digit bytes, in three multiplications. The lowest byte is the most significant digit.
/// The first multiplier, 0xA01, is `(10 << 8) + 1`.
fn parse_eight_digits(word: u64) -> u64 {
    let pairs = (word & 0x0F0F_0F0F_0F0F_0F0F).wrapping_mul(0xA01) >> 8 & 0x00FF_00FF_00FF_00FF;
    let quads = pairs.wrapping_mul((100 << 16) + 1) >> 16 & 0x0000_FFFF_0000_FFFF;
    quads.wrapping_mul((10000 << 32) + 1) >> 32
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

/// The first record boundary at or after `pos`: just past the next CRLF, or the end of the file.
/// A CR can only be the first byte of a CRLF terminator (spec section 2.2), so this is never inside
/// quotes. About half the records end in CRLF, so it's never far. A file without one simply doesn't
/// get split.
fn next_boundary(file: &[u8], pos: usize) -> usize {
    let cr = file[pos..].iter().position(|&c| c == b'\r');
    cr.map_or(file.len(), |cr| pos + cr + 2)
}

/// Returns the join table, country indices by region id, and each index's key. Index 0 is
/// `_UNMATCHED`, and the codes take the next indices as they first appear, so the output is sorted
/// when it's written. The table's last entry is past every region id, and stays unmatched, for every
/// id above them.
fn parse_regions(file: &[u8]) -> (Vec<u8>, Vec<String>) {
    let mut pos = b"region_id,country\n".len(); // The header never varies (spec section 2.3).
    // Region ids stay below a million (spec section 2.3). `index_of` takes every code in `[A-Z]{3}`.
    let (mut country_of, mut index_of) = (vec![0u8; 1 << 20], vec![0u8; 26 * 26 * 26]);
    let mut keys = vec!["_UNMATCHED".to_string()];
    while pos < file.len() {
        let id = next_number(file, &mut pos) as usize;
        let code = (0..3).fold(0, |code, i| code * 26 + usize::from(file[pos + i] - b'A'));
        if index_of[code] == 0 {
            index_of[code] = keys.len() as u8; // At most 240 countries (spec section 2.3).
            keys.push(String::from_utf8_lossy(&file[pos..pos + 3]).into_owned());
        }
        pos += 4; // The three letters and the LF.
        country_of[id] = index_of[code];
    }
    (country_of, keys)
}

/// Slots to start with: enough for the generator's pool of 100,000 names (spec section 2.2), at about
/// 91% full. Only more names than slots cost a restart. A test build starts tiny, to exercise that.
const INITIAL_SLOTS: usize = if cfg!(sd_const_hash) { 1 << 4 } else { 110_000 };
/// Words per slot, which is one cache line: a header, three words of key, and 256 country bits.
const SLOT_WORDS: usize = 8;
/// Most names fit in the slot itself: on seed 1, 99.99% of them.
const INLINE_BYTES: usize = 24;
/// Slots per word of tags. A slot's tag is a byte: zero while it's free, then 7 bits of the hash of
/// its name with the top bit set.
const GROUP: usize = 8;
const LOW_BITS: u64 = 0x0101_0101_0101_0101;
const HIGH_BITS: u64 = 0x8080_8080_8080_8080;

/// Distinct names per country, in one table shared by all threads, with no locks. Each slot holds a
/// header, the name's first 24 bytes (or its file offset if it's longer), and one bit per country
/// that has seen this name. The tags, one word per group of slots, say which slot to open, so a
/// lookup costs one cache miss, with no probing through other names' lines and no second memory
/// access to compare the key. A full group spills into the next. Names are compared in full, and
/// never unescaped: the raw bytes encode the value one to one. The slots are insert-only, so a
/// published slot never changes its key.
struct NameTable<'a> {
    file: &'a [u8],
    words: Vec<AtomicU64>,
    tags: Vec<AtomicU64>,
    first: usize, // The word where the first slot starts, on a cache line boundary.
    full: AtomicBool,
}

impl<'a> NameTable<'a> {
    fn new(file: &'a [u8], slots: usize) -> Self {
        // Prime the table's huge-page extents on this thread before parallel writes begin.
        let zeroed = vec![0u64; slots * SLOT_WORDS + 8];
        let words: Vec<AtomicU64> = zeroed.into_iter().map(AtomicU64::new).collect();
        let extents = words.iter().step_by(1 << 18);
        extents.for_each(|word| word.store(0, Relaxed));
        NameTable {
            file,
            first: words.as_ptr().align_offset(64),
            words,
            tags: (0..slots / GROUP).map(|_| AtomicU64::new(0)).collect(),
            full: AtomicBool::new(false),
        }
    }

    /// A hash's home group, which is never the last group: that one only takes spills, so the group
    /// after any home needs no wrap.
    fn home(&self, hash: u64) -> usize {
        (u64::from(hash as u32) * (self.tags.len() - 1) as u64 >> 32) as usize
    }

    /// The bytes of `tags` that hold this hash's tag. A neighbour above a match can show up too, which
    /// does no harm: the key decides.
    fn matches(tags: u64, hash: u64) -> u64 {
        let differ = tags ^ (hash >> 56 | 0x80) * LOW_BITS;
        differ.wrapping_sub(LOW_BITS) & !differ & HIGH_BITS
    }

    /// Asks for the line of the slot the tags point to: the lowest match in the home group, or in the
    /// next group when the home has none. Both words are read whatever the home says, and the choice
    /// is arithmetic, since a branch on it would mispredict on every spilled name. Returns the hash
    /// with that slot packed into bits no lookup reads, as its place past the home group's first
    /// slot (16 when nothing matched), so that `mark` can start there without reading the tags.
    fn prefetch_slot(&self, hash: u64) -> u64 {
        let home = self.home(hash);
        let at_home = Self::matches(self.tags[home].load(Relaxed), hash);
        let spill = 0u64.wrapping_sub(u64::from(at_home == 0));
        let hits = at_home | Self::matches(self.tags[home + 1].load(Relaxed), hash) & spill;
        let hint = hits.trailing_zeros() as usize / 8 + (spill as usize & GROUP);
        let slot = home * GROUP + (hint & 2 * GROUP - 1);
        prefetch::<_MM_HINT_ET0, _>(&self.words[self.first + slot * SLOT_WORDS]);
        hash & !(0xFF << 32) | (hint as u64) << 32
    }

    /// Whether this taken slot holds the name. Its writer may not have finished with it yet.
    fn holds(&self, base: usize, key: &[u8], key_words: &[u64; 3]) -> bool {
        let (header, stored) = (&self.words[base], &self.words[base + 1..base + 4]);
        while header.load(Acquire) == 0 {}
        header.load(Relaxed) == key.len() as u64 + 1
            && if key.len() <= INLINE_BYTES {
                (0..3).all(|i| stored[i].load(Relaxed) == key_words[i])
            } else {
                key == &self.file[stored[0].load(Relaxed) as usize..][..key.len()]
            }
    }

    /// Marks this name as seen in `country`, and returns whether that's new for the country. When
    /// every slot is taken it gives up, and `aggregate` throws the whole pass away. `hash` is what
    /// `prefetch_slot` returned for the name.
    fn mark(&self, key: &[u8], key_words: &[u64; 3], hash: u64, country: usize) -> bool {
        let mut group = self.home(hash);
        let base = 'search: {
            // Nearly every name sits in the slot the tags pointed to when the record was parsed, so
            // that slot is checked without a loop. A taken slot stays taken, with the same tag, so
            // it's still safe to read. The loop below sees the rest: new names, colliding tags, and
            // names published after the prefetch.
            let hint = (hash >> 32 & 0x1F) as usize;
            let base = self.first + (group * GROUP + hint) * SLOT_WORDS;
            if hint < 2 * GROUP && self.holds(base, key, key_words) {
                break 'search base;
            }
            for _ in 0..self.tags.len() {
                let word = &self.tags[group];
                let tags = word.load(Relaxed);
                let mut hits = Self::matches(tags, hash);
                while hits != 0 {
                    let slot = group * GROUP + hits.trailing_zeros() as usize / 8;
                    let base = self.first + slot * SLOT_WORDS;
                    if self.holds(base, key, key_words) {
                        break 'search base;
                    }
                    hits &= hits - 1;
                }
                let free = !tags & HIGH_BITS;
                if free == 0 {
                    group = (group + 1) * usize::from(group + 1 < self.tags.len());
                    continue;
                }
                let at = free.trailing_zeros() as usize / 8;
                let taken = tags | (hash >> 56 | 0x80) << (8 * at);
                // Losing this race just means another look at the same group.
                if word.compare_exchange(tags, taken, Relaxed, Relaxed).is_ok() {
                    let base = self.first + (group * GROUP + at) * SLOT_WORDS;
                    let stored = &self.words[base + 1..base + 4];
                    if key.len() <= INLINE_BYTES {
                        (0..3).for_each(|i| stored[i].store(key_words[i], Relaxed));
                    } else {
                        stored[0].store(key.as_ptr() as u64 - self.file.as_ptr() as u64, Relaxed);
                    }
                    self.words[base].store(key.len() as u64 + 1, Release);
                    break 'search base;
                }
            }
            self.full.store(true, Relaxed);
            return false;
        };
        let word = &self.words[base + 4 + country / 64];
        let bit = 1u64 << (country % 64);
        // Nine records in ten set a new bit, so a plain load first would rarely save the locked OR,
        // and the prefetch has already asked to own the line.
        word.fetch_or(bit, Relaxed) & bit == 0
    }
}

/// A text field: where its raw bytes start, their length, and the length of its value. The raw bytes
/// encode the value one to one.
type TextField = (usize, usize, usize);

/// Parses the text field at `pos` and steps past the comma after it. An unquoted name or note is
/// always empty (spec section 2.2), and it gives the same empty raw bytes as `""`, so they are one
/// value. In a quoted field, the parity of the quotes up to and including each byte is odd at the
/// first quote of a `""` pair, at the closing quote and after it, so the closing quote is the first
/// odd byte followed by another, with no branch per pair to mispredict. A window of 64 bytes holds
/// nearly every field. The next window takes the parity over, and finds a closing quote at the last
/// byte of the one before.
fn text_field(file: &[u8], pos: &mut usize) -> TextField {
    let start = *pos;
    if file[start] != b'"' {
        *pos += 1;
        return (start, 0, 0);
    }
    let (mut scan, mut quotes, mut carry) = (start + 1, 0, 0);
    loop {
        let mask = quote_mask(file, scan);
        let parity = prefix_xor(mask) ^ carry;
        let closing = parity & (parity << 1 | carry & 1); // Bit `i` means the quote before it.
        let at = closing.trailing_zeros() as usize;
        if closing != 0 {
            let end = scan + at - 1;
            quotes += (mask & ((1u64 << at) - 1) >> 1).count_ones();
            *pos = end + 2; // The closing quote and the comma.
            // The quotes before the closing one come in pairs, each one byte of value. The closing
            // quote is counted too when it ended the window before, which the division drops.
            let raw = end - start - 1;
            return (start + 1, raw, raw - quotes as usize / 2);
        }
        (quotes, carry) = (quotes + mask.count_ones(), (parity as i64 >> 63) as u64);
        scan += 64;
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
    for i in [0, 1, 2, 5, 6, 7] {
        row[i] += other[i];
    }
    (row[3], row[4]) = (row[3].min(other[3]), row[4].max(other[4]));
}

/// A parsed record, waiting for the cache lines its lookups need: id, cents, region, name, note
/// bytes, the name's first 24 bytes, and their hash with the slot `prefetch_slot` chose packed in.
type Pending = (i64, i64, usize, TextField, usize, [u64; 3], u64);

/// Parses the records in `pos..end`, adding each to its country's row.
fn parse_records(mut pos: usize, end: usize, regions: &[u8], names: &NameTable, rows: &mut [Row]) {
    let file = names.file;
    // Records parsed before any lookup is done. A batch can prefetch all its cache lines and let
    // them arrive together, instead of stalling on each in turn.
    let (mut batch, mut count) = ([Pending::default(); 16], 0);
    while pos < end {
        // A byte order mark can only start a record here, and no record otherwise starts with it.
        pos += 3 * usize::from(file[pos] == 0xEF);
        let id = next_number(file, &mut pos) as i64;
        let name = text_field(file, &mut pos);
        let note = text_field(file, &mut pos);
        let region = (next_number(file, &mut pos) as usize).min(regions.len() - 1);
        let cents = amount_cents(file, &mut pos);
        // CRLF or LF, with no branch to mispredict, since records mix them about half and half. Past
        // the end of the file, the padding is zeros, which don't count as a CR.
        pos += 1 + usize::from(file.get(pos) == Some(&b'\r'));

        // The name's first 24 bytes, with whatever follows it masked off. BZHI reads only the low 8
        // bits of its count, so a name of 32 bytes or more can lose a word here. That's still the same
        // words for the same name, and only the hash uses them: longer names are compared in full.
        let key_words: [u64; 3] = std::array::from_fn(|word| {
            let bytes = name.1.saturating_sub(8 * word) as u32;
            // SAFETY: BZHI is in BMI2, which x86-64-v3 includes, and it only touches registers.
            unsafe { _bzhi_u64(load_u64(file, name.0 + 8 * word), 8 * bytes) }
        });
        // Mixes those bytes and the name's length. A test build swaps this for a constant, so that
        // every key collides and any logic that trusts a hash without comparing the key shows up
        // as a wrong answer.
        let hash = key_words.iter().fold(name.1 as u64, |hash, &word| {
            let mixed = (hash ^ word).wrapping_mul(0x9E37_79B9_7F4A_7C15);
            mixed ^ (mixed >> 29)
        });
        let hash = names.prefetch_slot(if cfg!(sd_const_hash) { 0 } else { hash });
        prefetch::<_MM_HINT_T0, _>(&regions[region]);
        batch[count] = (id, cents, region, name, note.2, key_words, hash);
        count += 1;
        if count == batch.len() {
            add_records(&batch, regions, names, rows);
            count = 0;
        }
    }
    add_records(&batch[..count], regions, names, rows);
}

fn add_records(batch: &[Pending], regions: &[u8], names: &NameTable, rows: &mut [Row]) {
    for &(id, cents, region, name, note_bytes, key_words, hash) in batch {
        let country = usize::from(regions[region]);
        let key = &names.file[name.0..name.0 + name.1];
        let is_new = i64::from(names.mark(key, &key_words, hash, country));
        let (name_bytes, note_bytes) = (name.2 as i64, note_bytes as i64);
        let record = [1, id, cents, cents, cents, is_new, name_bytes, note_bytes];
        merge(&mut rows[country], &record);
    }
}

/// Parses the whole file on `threads` threads and merges their rows. Eight segments per thread
/// balances the load. Tying the size to the thread count also makes the SD_THREADS test sweep move
/// every cut. The spec's pool of names is a distribution, not a promise: if a file holds more names
/// than the table, the pass is thrown away and redone with a bigger table.
fn aggregate(file: &[u8], regions: &[u8], groups: usize, threads: usize, slots: usize) -> Vec<Row> {
    let names = NameTable::new(file, slots);
    let segment = (file.len() / (threads * 8)).max(1);
    let (cursor, totals) = (AtomicUsize::new(0), Mutex::new(vec![EMPTY_ROW; groups]));
    let work = || {
        let mut rows = vec![EMPTY_ROW; groups];
        let claim = || Some(cursor.fetch_add(segment, Relaxed)).filter(|&s| s < file.len());
        while let Some(start) = claim().filter(|_| !names.full.load(Relaxed)) {
            // The first boundary in the file ends the header, which always ends in CRLF (spec
            // section 2.2), so the first segment starts just after it.
            let last = next_boundary(file, (start + segment).min(file.len()));
            parse_records(next_boundary(file, start), last, regions, &names, &mut rows);
        }
        for (total, row) in totals.lock().unwrap().iter_mut().zip(&rows) {
            merge(total, row);
        }
    };
    thread::scope(|scope| (0..threads).for_each(|_| drop(scope.spawn(&work))));
    if names.full.load(Relaxed) {
        return aggregate(file, regions, groups, threads, slots * 4);
    }
    totals.into_inner().unwrap()
}

fn main() -> io::Result<()> {
    let mut args = std::env::args().skip(1);
    let data_dir = args.next().unwrap_or_else(|| "/data".into());
    let out_dir = args.next().unwrap_or_else(|| "/out".into());
    // `SD_THREADS` if set (the test sweep uses it), else what the container allows, capped at the
    // box's 12 threads.
    let allowed = thread::available_parallelism().map_or(1, |n| n.get().min(12));
    let threads = std::env::var("SD_THREADS").map_or(allowed, |v| v.parse().unwrap_or(allowed));
    let (regions, keys) = parse_regions(map_file(&format!("{data_dir}/regions.csv"))?);
    let records = map_file(&format!("{data_dir}/records.csv"))?;
    let totals = aggregate(records, &regions, keys.len(), threads, INITIAL_SLOTS);

    // Sorted by key, bytewise: `_UNMATCHED` lands last, because `_` is above `Z`. A row's columns are
    // already in output order, and a group without records gets no line.
    let mut lines: Vec<_> = keys.iter().zip(&totals).collect();
    lines.sort();
    let mut out = BufWriter::new(File::create(format!("{out_dir}/aggregates.txt"))?);
    for (key, row) in lines.into_iter().filter(|(_, row)| row[0] > 0) {
        write!(out, "{key} {} {}", row[0], row[1])?;
        // Printed as `W.CC`, with the sign apart, because integer division truncates toward zero,
        // which would lose the sign of -0.75. Zero never gets a sign.
        for cents in &row[2..5] {
            let (sign, magnitude) = (if *cents < 0 { "-" } else { "" }, cents.unsigned_abs());
            write!(out, " {sign}{}.{:02}", magnitude / 100, magnitude % 100)?;
        }
        writeln!(out, " {} {} {}", row[5], row[6], row[7])?;
    }
    out.flush()
}
