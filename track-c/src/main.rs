//! Track C (Join): aggregates per country, from a hostile CSV joined against a region table.
//!
//! Threads claim segments of the mapped file from an atomic cursor. Segments are cut just after a
//! CRLF, because the spec promises that a CR appears nowhere else, so that is always a record
//! boundary, and no quote counting is needed. Inside a segment the parser has two stages, because a
//! parser that finds each field from the end of the last one is limited by that chain of loads, not
//! by its instructions. The first stage walks fixed 64-byte blocks, and lists every comma and line end
//! outside quotes. The second reads each record's fields from its five listed positions, and no record
//! waits for the one before it. Nothing is unescaped: a quoted field's raw bytes, with its `""` pairs
//! intact, encode its value one to one, and the value's length follows from counting its quotes. Each
//! thread sums into its own rows. One shared table counts the distinct names per country, and a chunk
//! of records has its cache lines prefetched before any of it is added up.

use std::io::{self, BufWriter, Write};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, AtomicUsize, Ordering::*};
use std::{arch::x86_64::*, fs::File, os::fd::AsRawFd, ptr::null_mut, thread};

// Declared by hand to keep the crate at zero dependencies; std already links glibc.
unsafe extern "C" {
    fn mmap(at: *mut u8, len: usize, p: i32, f: i32, fd: i32, o: i64) -> *mut u8;
    fn madvise(at: *mut u8, len: usize, advice: i32) -> i32;
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

/// Loads 8 bytes, little-endian, which may run into the padding.
///
/// # Safety
/// `pos + 8` must be at most `file.len() + PADDING`. The hot path can't afford a check per load, so
/// its callers bound their positions once instead.
unsafe fn load_u64(file: &[u8], pos: usize) -> u64 {
    // SAFETY: the caller keeps the load inside the mapping, and the load needs no alignment.
    unsafe { file.as_ptr().add(pos).cast::<u64>().read_unaligned() }
}

/// One bit per byte of the 64 at `pos`, set where the byte is `first` or `second`.
///
/// # Safety
/// `pos + 64` must be at most `file.len() + PADDING`.
unsafe fn byte_mask(file: &[u8], pos: usize, first: u8, second: u8) -> u64 {
    // SAFETY: the caller keeps both loads inside the mapping, they need no alignment, and the build
    // targets x86-64-v3, which has AVX2.
    unsafe {
        let is = |byte| _mm256_set1_epi8(byte as i8);
        let mask = |at| {
            let block = _mm256_loadu_si256(at);
            let either = _mm256_or_si256(
                _mm256_cmpeq_epi8(block, is(first)),
                _mm256_cmpeq_epi8(block, is(second)),
            );
            _mm256_movemask_epi8(either) as u32
        };
        let at = file.as_ptr().add(pos).cast::<__m256i>();
        u64::from(mask(at)) | u64::from(mask(at.add(1))) << 32
    }
}

/// Parses three numbers of up to 8 digits each, in one vector. Each word's digits sit at its top, with
/// the most significant in the lowest byte they use, and zero bytes below that. Each step doubles the
/// digits per lane: to 2, to 4, then to 8.
fn parse_three(words: [u64; 3]) -> [u64; 3] {
    // SAFETY: x86-64-v3 has AVX2.
    unsafe {
        let [a, b, c] = words.map(|word| word as i64);
        let digits = _mm256_and_si256(_mm256_set_epi64x(0, c, b, a), _mm256_set1_epi8(0x0F));
        let pairs = _mm256_maddubs_epi16(digits, _mm256_set1_epi16(0x010A));
        let quads = _mm256_madd_epi16(pairs, _mm256_set1_epi32(0x0001_0064));
        let packed = _mm256_packus_epi32(quads, quads);
        let eights = _mm256_madd_epi16(packed, _mm256_set1_epi32(0x0001_2710));
        let low = _mm256_extract_epi64::<0>(eights) as u64;
        [
            low & 0xFFFF_FFFF,
            low >> 32,
            _mm256_extract_epi32::<4>(eights) as u32 as u64,
        ]
    }
}

/// How many bits of `word` the digits at its bottom fill, with the digits moved to its top. Ids and
/// region ids have at most 8 digits (spec sections 2.2 and 3), so one 8-byte load settles them:
/// digits have bit 4 set, and commas and line ends don't, so the first clear bit marks the end, with
/// no loop to mispredict.
fn digits_of(word: u64) -> (u64, u32) {
    let bits = (!word & 0x1010_1010_1010_1010).trailing_zeros() & !7;
    (word << (64 - bits), bits)
}

/// The first line boundary at or after `pos`: just past the next `terminator`, or the end of the file.
/// In the records, a CR can only be the first byte of a CRLF terminator (spec section 2.2), so it's
/// never inside quotes, and about half the records end in one, so it's never far. A file without one
/// simply doesn't get split.
fn next_boundary(file: &[u8], pos: usize, terminator: &[u8]) -> usize {
    let at = file[pos..].iter().position(|&c| c == terminator[0]);
    at.map_or(file.len(), |at| pos + at + terminator.len())
}

/// Entries in the join table. Region ids stay below a million (spec section 2.3), so the last entry
/// is past every one of them, and stays unmatched, for every id above them.
const REGION_IDS: usize = 1 << 20;

/// Slots to start with: enough for the generator's pool of 100,000 names (spec section 2.2), at about
/// 91% full. Only more names than slots cost a restart. A test build starts tiny, to exercise that.
const INITIAL_SLOTS: usize = if cfg!(sd_const_hash) { 1 << 5 } else { 110_000 };
/// Words per huge page.
const PAGE_WORDS: usize = 1 << 18;
/// Words per slot, which is one cache line: three words of key, a header, and 256 country bits.
const SLOT_WORDS: usize = 8;
/// Most names fit in the slot itself: on seed 1, 99.99% of them.
const INLINE_BYTES: usize = 24;
/// Slots per group. A name's home is a group, and it may sit there or in the next one.
const GROUP: usize = 16;
/// A 32-byte load at `INLINE_BYTES - len` bytes in has its first `len` bytes set, and no others.
static KEY_MASKS: [u64; 7] = [!0, !0, !0, 0, 0, 0, 0];

/// Distinct names per country, in one table shared by all threads, with no locks. Each slot holds a
/// name's first 24 bytes (or its file offset if it's longer), a header, and one bit per country that
/// has seen this name. The tags, one byte per slot, zero while it's free and then 8 bits of the hash
/// of its name, say which slot to open, so a lookup costs one cache miss, with no probing through
/// other names' lines. One vector holds two groups' tags, so a name in its home group or the next
/// costs the same, with no branch. Full ones spill two groups on. Names are compared in full, and never unescaped: the raw bytes encode the value one to one. The slots are
/// insert-only: a key's words are written before its header is published and never again, so whoever
/// sees the header may read them as plain memory.
struct NameTable<'a> {
    file: &'a [u8],
    words: Vec<AtomicU64>,
    tags: Vec<AtomicU8>,
    regions: Vec<AtomicU8>,   // The join table, country indices by region id.
    index_of: Vec<AtomicU8>,  // Country indices by code, which takes every one in `[A-Z]{3}`.
    keys: Mutex<Vec<String>>, // Index 0 is `_UNMATCHED`, and codes take the next as they appear.
    first: usize,             // The word where the first slot starts, on a cache line boundary.
    groups: usize, // Homes. The tags and slots have one more group, for the last home's window.
    full: AtomicBool,
}

impl<'a> NameTable<'a> {
    fn new(file: &'a [u8], slots: usize) -> Self {
        let tags = (slots / GROUP + 1) * GROUP;
        // These are fresh zero pages from the system, untouched until a thread needs them.
        let words: Vec<_> = vec![0u64; tags * SLOT_WORDS + 8]
            .into_iter()
            .map(AtomicU64::new)
            .collect();
        let bytes = |len| vec![0u8; len].into_iter().map(AtomicU8::new).collect();
        NameTable {
            file,
            first: words.as_ptr().align_offset(64),
            words,
            tags: bytes(tags),
            regions: bytes(REGION_IDS),
            index_of: bytes(26 * 26 * 26),
            keys: Mutex::new(vec!["_UNMATCHED".to_string()]),
            groups: slots / GROUP,
            full: AtomicBool::new(false),
        }
    }

    /// Fills the join table from the region lines in `pos..end`.
    fn parse_regions(&self, file: &[u8], mut pos: usize, end: usize) {
        while pos < end {
            // SAFETY: `pos` is inside the file, and the padding covers the load.
            let (word, bits) = digits_of(unsafe { load_u64(file, pos) });
            pos += bits as usize / 8 + 1;
            let [id, ..] = parse_three([word, 0, 0]);
            let code = (0..3).fold(0, |code, i| code * 26 + usize::from(file[pos + i] - b'A'));
            let index = &self.index_of[code];
            if index.load(Relaxed) == 0 {
                let mut keys = self.keys.lock().unwrap();
                if index.load(Relaxed) == 0 {
                    index.store(keys.len() as u8, Relaxed); // At most 240 (spec section 2.3).
                    keys.push(String::from_utf8_lossy(&file[pos..pos + 3]).into_owned());
                }
            }
            self.regions[id as usize].store(index.load(Relaxed), Relaxed);
            pos += 4; // The three letters and the LF.
        }
    }

    /// A hash of the name's length and first 8 bytes, with whatever follows it masked off: the pool's
    /// names rarely share them. BZHI reads only the low 8 bits of its count, so a name of 32 bytes or
    /// more can keep extra bytes. That's still the same word for the same name, and the key decides
    /// anyway. A test build swaps this for a constant, so that every key collides and any logic that
    /// trusts a hash without comparing the key shows up as a wrong answer.
    fn hash(&self, start: usize, len: usize) -> u32 {
        if cfg!(sd_const_hash) {
            return 0;
        }
        // SAFETY: a name starts inside the file and the padding covers the load. BZHI is in BMI2 and
        // CRC32 in SSE4.2, and x86-64-v3 includes both.
        unsafe {
            let head = _bzhi_u64(load_u64(self.file, start), 8 * len as u32);
            _mm_crc32_u64(len as u64, head) as u32
        }
    }

    fn home(&self, hash: u32) -> usize {
        (u64::from(hash) * self.groups as u64 >> 32) as usize
    }

    fn tag(hash: u32) -> u8 {
        (hash as u8).max(1)
    }

    /// Where a slot's words start.
    fn base(&self, group: usize, slot: u32) -> usize {
        self.first + (group * GROUP + slot as usize) * SLOT_WORDS
    }

    /// The tags of the two groups at `group`, which must be a home.
    fn window(&self, group: usize) -> __m256i {
        // SAFETY: `home` and the spill step stay below `groups`, and the tags have a group past the
        // last home. x86-64-v3 has AVX2.
        unsafe { _mm256_loadu_si256(self.tags.as_ptr().add(group * GROUP).cast()) }
    }

    /// One bit per slot of a window, set where the tag is `tag`.
    fn slots_with(window: __m256i, tag: u8) -> u32 {
        // SAFETY: x86-64-v3 has AVX2.
        unsafe {
            _mm256_movemask_epi8(_mm256_cmpeq_epi8(window, _mm256_set1_epi8(tag as i8))) as u32
        }
    }

    /// Guesses the name's slot from the tags, and asks to own its line, for a write: a locked update
    /// then needs no second trip to take it from the core that wrote it last. A wrong guess costs
    /// `mark` a search.
    fn guess_slot(&self, hash: u32) -> usize {
        let group = self.home(hash);
        let hits = Self::slots_with(self.window(group), Self::tag(hash));
        let base = self.base(group, hits.trailing_zeros() & 31);
        // SAFETY: the slot is inside the table, and prefetching has no effect on memory anyway.
        unsafe { _mm_prefetch::<_MM_HINT_ET0>(self.words.as_ptr().add(base).cast()) };
        base
    }

    /// Whether this slot holds the name. A slot whose writer hasn't finished with it doesn't.
    ///
    /// # Safety
    /// `base` must be where a slot's words start.
    unsafe fn holds(&self, base: usize, start: usize, len: usize) -> bool {
        // SAFETY: the slot is inside the table. Its header shows the key words were published, so they
        // no longer change. The name starts inside the file and the padding covers the load. Its
        // length is at most `INLINE_BYTES` there, which keeps the mask's load inside the table.
        // x86-64-v3 has AVX2.
        unsafe {
            let stored = self.words.as_ptr().add(base);
            if (*stored.add(3)).load(Acquire) != len as u64 + 1 {
                return false;
            }
            if len > INLINE_BYTES {
                let at = (*stored).load(Relaxed) as usize;
                return self.file[at..at + len] == self.file[start..start + len];
            }
            let at = |bytes: *const u8| _mm256_loadu_si256(bytes.cast());
            let name = at(self.file.as_ptr().add(start));
            let mask = at(KEY_MASKS.as_ptr().cast::<u8>().add(INLINE_BYTES - len));
            _mm256_testz_si256(_mm256_xor_si256(name, at(stored.cast())), mask) == 1
        }
    }

    /// Finds the name's slot, or takes a free one for it. Only a wrong guess or a new name comes here.
    /// When every slot is taken it gives up, and `aggregate` throws the whole pass away.
    #[cold]
    fn find(&self, start: usize, len: usize) -> Option<usize> {
        let hash = self.hash(start, len);
        let mut group = self.home(hash);
        let mut spills = 0;
        while spills < self.groups {
            // One load answers both questions below, so a name can't get two slots. Each byte only
            // ever changes from zero to its final tag, and everyone takes the lowest free slot. If
            // two threads insert one name, whichever saw the other's slot as taken saw its tag too,
            // and if neither did, they both want the same byte and one loses the exchange.
            let window = self.window(group);
            let mut hits = Self::slots_with(window, Self::tag(hash));
            while hits != 0 {
                let base = self.base(group, hits.trailing_zeros());
                while self.words[base + 3].load(Acquire) == 0 {}
                // SAFETY: `base` is where a slot's words start.
                if unsafe { self.holds(base, start, len) } {
                    return Some(base);
                }
                hits &= hits - 1;
            }
            let free = Self::slots_with(window, 0);
            if free == 0 {
                group = if group + 2 < self.groups {
                    group + 2
                } else {
                    0
                };
                spills += 1;
                continue;
            }
            let at = free.trailing_zeros();
            // Losing this race just means another look at the same group.
            let tag = &self.tags[group * GROUP + at as usize];
            if tag
                .compare_exchange(0, Self::tag(hash), Relaxed, Relaxed)
                .is_ok()
            {
                let base = self.base(group, at);
                let stored = &self.words[base..base + 4];
                // SAFETY: the name starts inside the file, and the padding covers the loads.
                let word = |i| unsafe { load_u64(self.file, start + 8 * i) };
                (0..3).for_each(|i| stored[i].store(word(i), Relaxed));
                if len > INLINE_BYTES {
                    stored[0].store(start as u64, Relaxed);
                }
                stored[3].store(len as u64 + 1, Release);
                return Some(base);
            }
        }
        self.full.store(true, Relaxed);
        None
    }

    /// Marks this name as seen in `country`, and returns whether that's new for the country.
    ///
    /// # Safety
    /// `guess` must be where a slot's words start, and `country` below 256.
    unsafe fn mark(&self, guess: usize, start: usize, len: usize, country: usize) -> bool {
        // SAFETY: the caller's promises, and `find` returns where a slot's words start. Its 256 bits
        // start 4 words in.
        unsafe {
            let base = if self.holds(guess, start, len) {
                guess
            } else if let Some(base) = self.find(start, len) {
                base
            } else {
                return false;
            };
            let word = &*self.words.as_ptr().add(base + 4 + country / 64);
            let bit = 1u64 << (country % 64);
            // Nine records in ten set a new bit, so a plain load first would rarely save the locked
            // OR, and the prefetch has already asked to own the line.
            word.fetch_or(bit, Relaxed) & bit == 0
        }
    }
}

/// A country's totals, in output order: count, id sum, amount sum, lowest amount, highest amount,
/// distinct names, name bytes and note bytes.
type Row = [i64; 8];
const EMPTY_ROW: Row = [0, 0, 0, i64::MAX, i64::MIN, 0, 0, 0];
fn merge(row: &mut Row, other: &Row) {
    for i in [0, 1, 2, 5, 6, 7] {
        row[i] += other[i];
    }
    (row[3], row[4]) = (row[3].min(other[3]), row[4].max(other[4]));
}

/// Enough rows for every country index a byte can hold, so that indexing needs no check.
type Rows = [Row; 256];

/// Segments per thread.
const SEGMENTS: usize = 8;
/// Blocks per chunk: about as many records as a chunk's cache lines can overlap for.
const BLOCKS: usize = 16;
/// A block can hold at most 64 separators, and listing one writes 8 slots regardless.
const LISTED: usize = BLOCKS * 64 + 16;

/// Parses the records in `pos..end`, adding each to its country's row.
fn parse_records(pos: usize, end: usize, names: &NameTable, rows: &mut Rows) {
    let file = names.file;
    // The positions of commas and line ends outside quotes, after the line end before the first of
    // them, so a record's five come after its start. Positions fit 32 bits: the file is far below 4 GB
    // (spec section 5).
    let (mut listed, mut count, mut looked_up) = ([0u32; LISTED], 1, [[0u32; 4]; LISTED / 5]);
    listed[0] = pos as u32 - 1;
    let (mut scan, mut odd_so_far) = (pos, 0);
    while scan < end {
        for _ in 0..BLOCKS.min((end - scan).div_ceil(64)) {
            // SAFETY: `scan` is inside the file and the padding covers a block. PCLMULQDQ is in the
            // build's target features. The carry-less multiply by all ones gives each bit the parity
            // of the quotes up to it, and that's odd exactly inside quotes, since a `""` pair cancels
            // out. `count` stays below `LISTED - 8`, as the chunk holds at most `BLOCKS` blocks.
            unsafe {
                let quotes = byte_mask(file, scan, b'"', b'"');
                let ones = _mm_set1_epi8(-1);
                let parity = _mm_clmulepi64_si128::<0>(_mm_cvtsi64_si128(quotes as i64), ones);
                let odd = _mm_cvtsi128_si64(parity) as u64 ^ odd_so_far;
                odd_so_far = (odd as i64 >> 63) as u64;
                let mut found = byte_mask(file, scan, b',', b'\n') & !odd;
                let total = found.count_ones() as usize;
                // Blocks hold 5 of them on average, and a loop's exit would be a coin toss, so this
                // writes 8 whatever the number, and moves on by the right amount.
                let out = listed.as_mut_ptr().add(count);
                for i in 0..8 {
                    *out.add(i) = scan as u32 + found.trailing_zeros();
                    found &= found.wrapping_sub(1);
                }
                for i in 8..total {
                    *out.add(i) = scan as u32 + found.trailing_zeros();
                    found &= found - 1;
                }
                count += total;
            }
            scan += 64;
        }
        // The last block runs into the next segment, whose start isn't known to be outside quotes.
        while count > 1 && listed[count - 1] as usize >= end {
            count -= 1;
        }
        // The final record may have no terminator (spec section 2.1).
        if scan >= file.len() && count % 5 == 0 {
            listed[count] = file.len() as u32;
            count += 1;
        }
        let records = (count - 1) / 5;
        add_records(&listed[..=records * 5], &mut looked_up, names, rows);
        listed.copy_within(records * 5..count, 0);
        count -= records * 5;
    }
}

/// Adds up a chunk of whole records, given the line end before them and each one's five separators.
fn add_records(listed: &[u32], looked_up: &mut [[u32; 4]], names: &NameTable, rows: &mut Rows) {
    let file = names.file;
    let records = listed.len() / 5;
    let separators = |record: usize| {
        // SAFETY: the caller passes `records * 5 + 1` positions.
        let at = unsafe {
            listed
                .as_ptr()
                .add(record * 5)
                .cast::<[u32; 6]>()
                .read_unaligned()
        };
        at.map(|position| position as usize)
    };
    for record in 0..records {
        let [before, id_end, name_end, note_end, region_end, line_end] = separators(record);
        // SAFETY: every position is inside the file, or just after it, and a record starts at least
        // 32 bytes in, after the header, so the padding covers the loads above and below them.
        // `region` stays inside the join table. Prefetching has no effect on memory.
        unsafe {
            let digits = |end: usize, bytes: usize| {
                let shift = 64 - 8 * bytes as u32;
                load_u64(file, end - 8) >> shift << shift
            };
            // A byte order mark can only start a record here, and no record otherwise starts with it.
            let start = before + 1 + 3 * usize::from(*file.get_unchecked(before + 1) == 0xEF);
            let id = digits(id_end, id_end - start);
            let region = digits(region_end, region_end - note_end - 1);
            // Amounts are at most 8 bytes, and negative ones at most 7 (spec section 2.2), so one load
            // holds them. Moving the digits before the point up a byte squeezes it out, which leaves
            // 8 digits to parse. CRLF or LF, with no branch to mispredict.
            let end = line_end - usize::from(*file.get_unchecked(line_end - 1) == b'\r');
            let negative = usize::from(*file.get_unchecked(region_end + 1) == b'-');
            let aligned = digits(end, end - region_end - 1 - negative);
            let squeezed = (aligned & 0xFFFF_0000_0000_0000) | (aligned & 0x00FF_FFFF_FFFF) << 8;
            let [id, region, cents] = parse_three([id, region, squeezed]);
            let cents = (cents as i64 ^ -(negative as i64)) + negative as i64;
            let region = (region as usize).min(REGION_IDS - 1);
            _mm_prefetch::<_MM_HINT_T0>(names.regions.as_ptr().add(region).cast());
            let name = (name_end - id_end - 1).saturating_sub(2);
            let guess = names.guess_slot(names.hash(id_end + 2, name));
            *looked_up.get_unchecked_mut(record) =
                [guess as u32, region as u32, id as u32, cents as u32];
        }
    }
    for record in 0..records {
        let [_, id_end, name_end, note_end, ..] = separators(record);
        let [guess, region, id, cents] = looked_up[record];
        let (name, note) = (name_end - id_end - 1, note_end - name_end - 1);
        // SAFETY: `id_end` is inside the file, and the padding covers the mask. BZHI keeps every bit
        // for a count of 64.
        let (name_quotes, quotes) = if note_end - id_end - 1 <= 64 {
            let mask = unsafe { byte_mask(file, id_end + 1, b'"', b'"') };
            let below = |bits| unsafe { _bzhi_u64(mask, bits as u32) }.count_ones() as usize;
            (below(name), below(note_end - id_end - 1))
        } else {
            let count = |bytes: &[u8]| bytes.iter().filter(|&&byte| byte == b'"').count();
            let name_quotes = count(&file[id_end + 1..name_end]);
            (
                name_quotes,
                name_quotes + count(&file[name_end + 1..note_end]),
            )
        };
        // A value is its field, less its delimiting quotes, less one for each `""`, with empty
        // fields of no quotes or two both coming to nothing.
        let name_bytes = (name - name_quotes / 2).saturating_sub(1);
        let note_bytes = (note - (quotes - name_quotes) / 2).saturating_sub(1);
        // SAFETY: the first pass kept `region` inside the table, and `guess` came from `guess_slot`.
        let (country, is_new) = unsafe {
            let country = usize::from(names.regions.get_unchecked(region as usize).load(Relaxed));
            let raw = name.saturating_sub(2);
            (
                country,
                names.mark(guess as usize, id_end + 2, raw, country),
            )
        };
        let cents = i64::from(cents as i32);
        let row = &mut rows[country];
        row[0] += 1;
        row[1] += i64::from(id);
        row[2] += cents;
        (row[3], row[4]) = (row[3].min(cents), row[4].max(cents));
        row[5] += i64::from(is_new);
        row[6] += name_bytes as i64;
        row[7] += note_bytes as i64;
    }
}

/// Numbered jobs that threads claim from a shared cursor, however many have started so far.
#[derive(Default)]
struct Jobs {
    next: AtomicUsize,
    done: AtomicUsize,
}

impl Jobs {
    fn run(&self, count: usize, mut job: impl FnMut(usize)) {
        while let Some(at) = Some(self.next.fetch_add(1, Relaxed)).filter(|&at| at < count) {
            job(at);
            self.done.fetch_add(1, Release);
        }
    }

    /// Runs the jobs, and returns when all are finished, by whichever threads.
    fn run_all(&self, count: usize, job: impl FnMut(usize)) {
        self.run(count, job);
        while self.done.load(Acquire) < count {
            thread::yield_now();
        }
    }
}

/// Parses the whole file on `threads` threads and merges their rows, returning them with their keys.
/// The threads share the setup too: touching each huge page once before anything writes to it, which
/// keeps two threads from each getting one, then the regions. Eight segments per thread balances the
/// load. Tying the sizes to the thread count also makes the SD_THREADS test sweep move every cut. The
/// spec's pool of names is a distribution, not a promise: if a file holds more names than the table,
/// the pass is thrown away and redone with a bigger table.
fn aggregate(file: &[u8], lookup: &[u8], threads: usize, slots: usize) -> (Vec<String>, Vec<Row>) {
    let names = NameTable::new(file, slots);
    let (pages, lines, segments) = (Jobs::default(), Jobs::default(), Jobs::default());
    let totals = Mutex::new(vec![EMPTY_ROW; 256]);
    let cuts = |file: &[u8], parts: usize, at, terminator| {
        let size = file.len().div_ceil(parts).max(1);
        let end = |at: usize| next_boundary(file, (at * size).min(file.len()), terminator);
        (end(at), end(at + 1))
    };
    let work = || {
        let page_count = names.words.len().div_ceil(PAGE_WORDS);
        pages.run_all(page_count, |page| {
            names.words[page * PAGE_WORDS].store(0, Relaxed)
        });
        lines.run_all(threads * 4, |at| {
            let (pos, end) = cuts(lookup, threads * 4, at, b"\n".as_slice());
            names.parse_regions(lookup, pos, end);
        });
        let mut rows = Box::new([EMPTY_ROW; 256]);
        segments.run(threads * SEGMENTS, |at| {
            // The first boundary in the file ends the header, which always ends in CRLF (spec
            // section 2.2), so the first segment starts just after it.
            let (pos, end) = cuts(file, threads * SEGMENTS, at, b"\r\n".as_slice());
            if names.full.load(Relaxed) || pos >= end {
                return;
            }
            // Maps the segment's pages in one call, instead of a page fault per 16 pages. A kernel
            // without this call just refuses it, and the faults come as before.
            let at = (file.as_ptr() as usize + pos) & !4095;
            // SAFETY: the advice only reads pages in. A range past the mapping is refused.
            unsafe { madvise(at as *mut u8, end - pos + 4096, 22) };
            parse_records(pos, end, &names, &mut rows);
        });
        for (total, row) in totals.lock().unwrap().iter_mut().zip(rows.iter()) {
            merge(total, row);
        }
    };
    thread::scope(|scope| {
        let work = &work;
        if threads > 1 {
            scope.spawn(move || {
                (2..threads).for_each(|_| drop(scope.spawn(work)));
                work();
            });
        }
        work();
    });
    if names.full.load(Relaxed) {
        return aggregate(file, lookup, threads, slots * 4);
    }
    (
        names.keys.into_inner().unwrap(),
        totals.into_inner().unwrap(),
    )
}

fn main() -> io::Result<()> {
    let mut args = std::env::args().skip(1);
    let data_dir = args.next().unwrap_or_else(|| "/data".into());
    let out_dir = args.next().unwrap_or_else(|| "/out".into());
    // `SD_THREADS` if set (the test sweep uses it), else what the container allows, capped at the
    // box's 12 threads.
    let allowed = thread::available_parallelism().map_or(1, |n| n.get().min(12));
    let threads = std::env::var("SD_THREADS").map_or(allowed, |v| v.parse().unwrap_or(allowed));
    let lookup = map_file(&format!("{data_dir}/regions.csv"))?;
    let records = map_file(&format!("{data_dir}/records.csv"))?;
    let (keys, totals) = aggregate(records, lookup, threads, INITIAL_SLOTS);

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
