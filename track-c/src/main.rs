//! Track C (Join): aggregates per country, from a hostile CSV joined against a region table.
//!
//! Threads claim segments of the file from a shared cursor. Segments are cut just after a CRLF,
//! because the spec promises that a CR appears nowhere else, so that is always a record boundary, and
//! no quote counting is needed. A thread reads its segment in small pieces into its own buffer, which
//! stays in its caches, needs no page mapped and is no more memory than the page tables would be.
//! Inside a piece the parser has two stages, because a parser that finds each field from the end of
//! the last one is limited by that chain of loads, not by its instructions. The first stage walks
//! fixed 64-byte blocks, and lists every comma and line end outside quotes. The second reads each
//! record's fields from its five listed positions, and no record waits for the one before it. Nothing
//! is unescaped: a quoted field's raw bytes, with its `""` pairs intact, encode its value one to one,
//! and the value's length follows from counting its quotes. Each thread sums into its own rows. One
//! shared table counts the distinct names per country, and a chunk of records has its cache lines
//! prefetched before any of it is added up.

use std::io::{self, BufWriter, Write};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, AtomicUsize, Ordering::*};
use std::{arch::x86_64::*, fs::File, os::unix::fs::FileExt, ptr::null_mut, thread};

// Declared by hand to keep the crate at zero dependencies; std already links glibc.
unsafe extern "C" {
    fn mmap(at: *mut u8, len: usize, p: i32, f: i32, fd: i32, o: i64) -> *mut u8;
    fn pthread_attr_init(attr: *mut [u64; 8]) -> i32;
    fn pthread_attr_setstack(attr: *mut [u64; 8], stack: *mut u8, len: usize) -> i32;
    fn pthread_create(
        id: *mut u64,
        attr: *const [u64; 8],
        start: unsafe extern "C" fn(*mut u8) -> *mut u8,
        arg: *mut u8,
    ) -> i32;
    fn pthread_join(id: u64, result: *mut *mut u8) -> i32;
    fn sched_getcpu() -> i32;
    fn sched_getaffinity(pid: i32, len: usize, mask: *mut [u64; 16]) -> i32;
    fn sched_setaffinity(pid: i32, len: usize, mask: *const [u64; 16]) -> i32;
}

/// Zero bytes after the text in a buffer, so that block loads can run past its end.
const PADDING: usize = 128;
/// Bytes per read: little enough to stay in the caches, enough that a read costs little per record.
const PIECE: usize = 1 << 15;

/// An input file, read in parts.
struct Input {
    file: File,
    len: usize,
}

impl Input {
    fn open(path: &str) -> io::Result<Self> {
        let file = File::open(path)?;
        let len = file.metadata()?.len() as usize;
        Ok(Input { file, len })
    }

    /// The first line boundary at or after `pos`: just past the next `terminator`, or the end of the
    /// file. In the records, a CR can only be the first byte of a CRLF terminator (spec section 2.2),
    /// so it's never inside quotes, and about half the records end in one, so it's never far. A file
    /// without one simply doesn't get split.
    fn boundary(&self, mut pos: usize, terminator: &[u8]) -> usize {
        let mut bytes = [0; 512];
        while let Some(read) = self
            .file
            .read_at(&mut bytes, pos as u64)
            .ok()
            .filter(|&n| n > 0)
        {
            if let Some(at) = bytes[..read].iter().position(|&c| c == terminator[0]) {
                return pos + at + terminator.len();
            }
            pos += read;
        }
        self.len
    }

    /// Where part `at` of `parts` starts and ends. The first boundary in a file ends its header.
    fn part(&self, parts: usize, at: usize, terminator: &[u8]) -> (usize, usize) {
        let size = self.len.div_ceil(parts).max(1);
        let cut = |at: usize| self.boundary((at * size).min(self.len), terminator);
        (cut(at), cut(at + 1))
    }

    /// Reads `pos..end` to the start of `buffer`, or of `spare` if it's too long, with `PADDING` zeros
    /// after them.
    fn read<'a>(
        &self,
        pos: usize,
        end: usize,
        buffer: &'a mut [u8],
        spare: &'a mut Vec<u8>,
    ) -> &'a [u8] {
        let len = end - pos;
        let text = if len + PADDING <= buffer.len() {
            buffer
        } else {
            spare.resize(len + PADDING, 0);
            spare.as_mut_slice()
        };
        self.file
            .read_exact_at(&mut text[..len], pos as u64)
            .unwrap();
        text[len..len + PADDING].fill(0);
        text
    }
}

/// Loads 8 bytes, little-endian, which may run into the padding.
///
/// # Safety
/// `pos + 8` must be inside `text`. The hot path can't afford a check per load, so its callers bound
/// their positions once instead.
unsafe fn load_u64(text: &[u8], pos: usize) -> u64 {
    // SAFETY: the caller keeps the load inside the buffer, and the load needs no alignment.
    unsafe { text.as_ptr().add(pos).cast::<u64>().read_unaligned() }
}

/// One bit per byte of the 64 at `pos`, set where the byte is `first` or `second`.
///
/// # Safety
/// `pos + 64` must be inside `text`.
unsafe fn byte_mask(text: &[u8], pos: usize, first: u8, second: u8) -> u64 {
    // SAFETY: the caller keeps both loads inside the buffer, they need no alignment, and the build
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
        let at = text.as_ptr().add(pos).cast::<__m256i>();
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

/// Entries in the join table. Region ids stay below a million (spec section 2.3), so the last entry
/// is past every one of them, and stays unmatched, for every id above them.
const REGION_IDS: usize = 1 << 20;
/// Slots to start with: enough for the generator's pool of 100,000 names (spec section 2.2), at about
/// 91% full. Only more names than slots cost a restart. A test build starts tiny, to exercise that.
const INITIAL_SLOTS: usize = if cfg!(sd_const_hash) { 1 << 5 } else { 110_000 };
/// Bytes of stack per thread, enough for its rows, buffer and lists.
const STACK: usize = 80 << 10;
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
/// costs the same, with no branch. Full ones spill two groups on. Names are compared in full, and
/// never unescaped: the raw bytes encode the value one to one. The slots are insert-only: a key's
/// words are written before its header is published and never again, so whoever sees the header may
/// read them as plain memory.
struct NameTable<'a> {
    records: &'a Input,
    words: &'static [AtomicU64],
    tags: &'static [AtomicU8],
    regions: &'static [AtomicU8], // The join table, country indices by region id.
    stacks: *mut u8,
    index_of: Vec<AtomicU8>, // Country indices by code, which takes every one in `[A-Z]{3}`.
    keys: Mutex<Vec<[u8; 3]>>, // Index 0 is `_UNMATCHED`, and codes take the next as they appear.
    groups: usize, // Homes. The tags and slots have one more group, for the last home's window.
    full: AtomicBool,
}

// SAFETY: only `new` and the thread that started the workers use `stacks`, before the workers run.
unsafe impl Sync for NameTable<'_> {}

impl<'a> NameTable<'a> {
    /// Maps the slots, tags, join table and threads' stacks together, and touches every page here on
    /// one thread. The kernel charges a memory group's pages to each CPU in batches, and what a CPU
    /// has charged but not used yet counts in the peak: up to 252 KB per CPU that ever takes a page
    /// fault. A thread that never faults adds none of that.
    fn new(records: &'a Input, slots: usize, threads: usize) -> Self {
        let mut allowed = [0u64; 16];
        // SAFETY: the masks are the size given. If a call fails, this thread just stays free to move.
        unsafe {
            sched_getaffinity(0, 128, &mut allowed);
            let cpu = sched_getcpu().max(0) as usize;
            let mut here = [0u64; 16];
            here[cpu / 64 % 16] = 1 << (cpu % 64);
            sched_setaffinity(0, 128, &here);
        }
        let tags = (slots / GROUP + 1) * GROUP;
        let (tags_at, regions_at) = (tags * SLOT_WORDS * 8, tags * SLOT_WORDS * 8 + tags);
        let stacks_at = (regions_at + REGION_IDS).next_multiple_of(4096);
        let bytes = stacks_at + threads * STACK;
        let (read_write, private_anonymous, huge_page) = (3, 0x22, 1 << 21);
        // SAFETY: the mapping is fresh, zeroed, never unmapped and big enough for an aligned `bytes`,
        // so the parts don't overlap, and zero is a valid value of each atomic.
        unsafe {
            let at = mmap(
                null_mut(),
                bytes + huge_page,
                read_write,
                private_anonymous,
                -1,
                0,
            );
            assert!(at as isize != -1);
            let base = at.add(at.align_offset(huge_page));
            (0..bytes)
                .step_by(4096)
                .for_each(|at| base.add(at).write_volatile(0));
            sched_setaffinity(0, 128, &allowed);
            NameTable {
                records,
                words: std::slice::from_raw_parts(base.cast(), tags * SLOT_WORDS),
                tags: std::slice::from_raw_parts(base.add(tags_at).cast(), tags),
                regions: std::slice::from_raw_parts(base.add(regions_at).cast(), REGION_IDS),
                stacks: base.add(stacks_at),
                index_of: vec![0u8; 26 * 26 * 26]
                    .into_iter()
                    .map(AtomicU8::new)
                    .collect(),
                keys: Mutex::new({
                    let mut keys = Vec::with_capacity(256);
                    keys.push(*b"___");
                    keys
                }),
                groups: slots / GROUP,
                full: AtomicBool::new(false),
            }
        }
    }

    /// Fills the join table from `len` bytes of whole region lines. Ids have at most 8 digits (spec
    /// section 2.3): digits have bit 4 set, and commas don't, so the first clear bit marks the end.
    fn parse_regions(&self, text: &[u8], len: usize) {
        let mut pos = 0;
        while pos < len {
            // SAFETY: `pos` is inside the text, and the padding covers the load.
            let word = unsafe { load_u64(text, pos) };
            let bits = (!word & 0x1010_1010_1010_1010).trailing_zeros() & !7;
            let [id, ..] = parse_three([word << (64 - bits), 0, 0]);
            pos += bits as usize / 8 + 1;
            let code = (0..3).fold(0, |code, i| code * 26 + usize::from(text[pos + i] - b'A'));
            let index = &self.index_of[code];
            if index.load(Relaxed) == 0 {
                let mut keys = self.keys.lock().unwrap();
                if index.load(Relaxed) == 0 {
                    index.store(keys.len() as u8, Relaxed); // At most 240 (spec section 2.3).
                    keys.push([text[pos], text[pos + 1], text[pos + 2]]);
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
    fn hash(text: &[u8], start: usize, len: usize) -> u32 {
        if cfg!(sd_const_hash) {
            return 0;
        }
        // SAFETY: a name starts inside the text and the padding covers the load. BZHI is in BMI2 and
        // CRC32 in SSE4.2, and x86-64-v3 includes both.
        unsafe {
            let head = _bzhi_u64(load_u64(text, start), 8 * len as u32);
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
        (group * GROUP + slot as usize) * SLOT_WORDS
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
    unsafe fn holds(&self, base: usize, text: &[u8], start: usize, len: usize) -> bool {
        // SAFETY: the slot is inside the table. Its header shows the key words were published, so they
        // no longer change. The name starts inside the text and the padding covers the load. Its
        // length is at most `INLINE_BYTES` there, which keeps the mask's load inside the table.
        // x86-64-v3 has AVX2.
        unsafe {
            let stored = self.words.as_ptr().add(base);
            if (*stored.add(3)).load(Acquire) != len as u64 + 1 {
                return false;
            }
            if len > INLINE_BYTES {
                let (at, mut other) = ((*stored).load(Relaxed), [0u8; 256]);
                return (0..len).step_by(other.len()).all(|done| {
                    let part = other.len().min(len - done);
                    let source = &self.records.file;
                    source
                        .read_exact_at(&mut other[..part], at + done as u64)
                        .unwrap();
                    other[..part] == text[start + done..start + done + part]
                });
            }
            let at = |bytes: *const u8| _mm256_loadu_si256(bytes.cast());
            let name = at(text.as_ptr().add(start));
            let mask = at(KEY_MASKS.as_ptr().cast::<u8>().add(INLINE_BYTES - len));
            _mm256_testz_si256(_mm256_xor_si256(name, at(stored.cast())), mask) == 1
        }
    }

    /// Finds the name's slot, or takes a free one for it, given where the text is in the file. Only a
    /// wrong guess or a new name comes here. When every slot is taken it gives up, and `aggregate`
    /// throws the whole pass away.
    #[cold]
    fn find(&self, text: &[u8], text_at: usize, start: usize, len: usize) -> Option<usize> {
        let hash = Self::hash(text, start, len);
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
                if unsafe { self.holds(base, text, start, len) } {
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
                // SAFETY: the name starts inside the text, and the padding covers the loads.
                let word = |i| unsafe { load_u64(text, start + 8 * i) };
                (0..3).for_each(|i| stored[i].store(word(i), Relaxed));
                if len > INLINE_BYTES {
                    stored[0].store((text_at + start) as u64, Relaxed);
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
    unsafe fn mark(
        &self,
        guess: usize,
        text: &[u8],
        text_at: usize,
        start: usize,
        len: usize,
        country: usize,
    ) -> bool {
        // SAFETY: the caller's promises, and `find` returns where a slot's words start. Its 256 bits
        // start 4 words in.
        unsafe {
            let base = if self.holds(guess, text, start, len) {
                guess
            } else if let Some(base) = self.find(text, text_at, start, len) {
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
fn parse_records(
    mut pos: usize,
    end: usize,
    names: &NameTable,
    rows: &mut Rows,
    buffer: &mut [u8],
    spare: &mut Vec<u8>,
) {
    // The positions just past commas and line ends outside quotes, after a record's start, so a
    // record's five come after the position it starts at. Positions fit 32 bits: a piece is small.
    let (mut listed, mut looked_up) = ([0u32; LISTED], [[0u32; 4]; LISTED / 5]);
    let mut piece = PIECE;
    while pos < end {
        let len = piece.min(end - pos);
        let text = names.records.read(pos, pos + len, buffer, spare);
        // A piece starts at a record's start, so outside quotes.
        let (mut count, mut scan, mut odd_so_far) = (1, 0, 0);
        listed[0] = 0;
        while scan < len {
            for _ in 0..BLOCKS.min((len - scan).div_ceil(64)) {
                // SAFETY: `scan` is inside the text and the padding covers a block. PCLMULQDQ is in
                // the build's target features. The carry-less multiply by all ones gives each bit the
                // parity of the quotes up to it, and that's odd exactly inside quotes, since a `""`
                // pair cancels out. `count` stays below `LISTED - 8`, as a chunk has `BLOCKS` blocks.
                unsafe {
                    let quotes = byte_mask(text, scan, b'"', b'"');
                    let ones = _mm_set1_epi8(-1);
                    let parity = _mm_clmulepi64_si128::<0>(_mm_cvtsi64_si128(quotes as i64), ones);
                    let odd = _mm_cvtsi128_si64(parity) as u64 ^ odd_so_far;
                    odd_so_far = (odd as i64 >> 63) as u64;
                    let mut found = byte_mask(text, scan, b',', b'\n') & !odd;
                    let total = found.count_ones() as usize;
                    // Blocks hold 5 of them on average, and a loop's exit would be a coin toss, so
                    // this writes 8 whatever the number, and moves on by the right amount.
                    let out = listed.as_mut_ptr().add(count);
                    for i in 0..8 {
                        *out.add(i) = scan as u32 + found.trailing_zeros() + 1;
                        found &= found.wrapping_sub(1);
                    }
                    for i in 8..total {
                        *out.add(i) = scan as u32 + found.trailing_zeros() + 1;
                        found &= found - 1;
                    }
                    count += total;
                }
                scan += 64;
            }
            // The final record may have no terminator (spec section 2.1).
            if scan >= len && pos + len == names.records.len && count % 5 == 0 {
                listed[count] = len as u32 + 1;
                count += 1;
            }
            let records = (count - 1) / 5;
            add_records(
                text,
                pos,
                &listed[..=records * 5],
                &mut looked_up,
                names,
                rows,
            );
            listed.copy_within(records * 5..count, 0);
            count -= records * 5;
        }
        // What's left is the start of a record the piece cut short. A record longer than a piece gets
        // a longer piece.
        let done = listed[0] as usize;
        assert!(done > 0 || pos + len < end);
        piece = if done > 0 { PIECE } else { piece * 2 };
        pos += done;
    }
}

/// Adds up a chunk of whole records from `text`, which is at `text_at` in the file, given where they
/// start and the positions just past each one's five separators.
fn add_records(
    text: &[u8],
    text_at: usize,
    listed: &[u32],
    looked_up: &mut [[u32; 4]],
    names: &NameTable,
    rows: &mut Rows,
) {
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
        let [mut start, name_at, note_at, region_at, amount_at, next] = separators(record);
        // SAFETY: every position is inside the text, or just after it, so the padding covers the
        // loads. `region` stays inside the join table. Prefetching has no effect on memory.
        unsafe {
            // Moves the digits at `start` to the top of a word, with zeros below them.
            let digits = |start, end| load_u64(text, start) << (64 - 8 * (end - start) as u32);
            // A byte order mark can only start a record here, and no record otherwise starts with it.
            if *text.get_unchecked(start) == 0xEF {
                start += 3;
            }
            let id = digits(start, name_at - 1);
            let region = digits(region_at, amount_at - 1);
            // Amounts are at most 8 bytes, and negative ones at most 7 (spec section 2.2), so one load
            // holds them. Moving the digits before the point up a byte squeezes it out, which leaves
            // 8 digits to parse. CRLF or LF, with no branch to mispredict.
            let end = next - 1 - usize::from(*text.get_unchecked(next - 2) == b'\r');
            let negative = usize::from(*text.get_unchecked(amount_at) == b'-');
            let aligned = digits(amount_at + negative, end);
            let squeezed = (aligned & 0xFFFF_0000_0000_0000) | (aligned & 0x00FF_FFFF_FFFF) << 8;
            let [id, region, cents] = parse_three([id, region, squeezed]);
            let cents = (cents as i64 ^ -(negative as i64)) + negative as i64;
            let region = (region as usize).min(REGION_IDS - 1);
            _mm_prefetch::<_MM_HINT_T0>(names.regions.as_ptr().add(region).cast());
            let raw = (note_at - name_at - 1).saturating_sub(2);
            let guess = names.guess_slot(NameTable::hash(text, name_at + 1, raw));
            *looked_up.get_unchecked_mut(record) =
                [guess as u32, region as u32, id as u32, cents as u32];
        }
    }
    for record in 0..records {
        let [_, name_at, note_at, region_at, ..] = separators(record);
        let [guess, region, id, cents] = looked_up[record];
        let (name, note) = (note_at - name_at - 1, region_at - note_at - 1);
        // SAFETY: `name_at` is inside the text, and the padding covers the mask. BZHI keeps every bit
        // for a count of 64.
        let (name_quotes, quotes) = if region_at - name_at - 1 <= 64 {
            let mask = unsafe { byte_mask(text, name_at, b'"', b'"') };
            let below = |bits| unsafe { _bzhi_u64(mask, bits as u32) }.count_ones() as usize;
            (below(name), below(region_at - name_at - 1))
        } else {
            let count = |bytes: &[u8]| bytes.iter().filter(|&&byte| byte == b'"').count();
            let name_quotes = count(&text[name_at..note_at]);
            (name_quotes, name_quotes + count(&text[note_at..region_at]))
        };
        // A value is its field, less its delimiting quotes, less one for each `""`, with empty
        // fields of no quotes or two both coming to nothing.
        let name_bytes = (name - name_quotes / 2).saturating_sub(1);
        let note_bytes = (note - (quotes - name_quotes) / 2).saturating_sub(1);
        // SAFETY: the first pass kept `region` inside the table, and `guess` came from `guess_slot`.
        let (country, is_new) = unsafe {
            let country = usize::from(names.regions.get_unchecked(region as usize).load(Relaxed));
            let raw = name.saturating_sub(2);
            let is_new = names.mark(guess as usize, text, text_at, name_at + 1, raw, country);
            (country, is_new)
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

/// Runs a closure, from a new thread's start.
unsafe extern "C" fn start<F: Fn()>(work: *mut u8) -> *mut u8 {
    // SAFETY: `aggregate` passes a closure that outlives the thread.
    unsafe { (*work.cast::<F>())() };
    null_mut()
}

/// Calls `work` on `threads` threads, this one and others on the given stacks.
fn run_threads<F: Fn() + Sync>(work: &F, threads: usize, stacks: *mut u8) {
    let mut ids = [0u64; 256];
    let others = threads.min(ids.len()) - 1;
    // SAFETY: each thread has its own stack, inside the mapping, and is joined before `work` ends.
    unsafe {
        let mut attr = [0u64; 8];
        for (at, id) in ids[..others].iter_mut().enumerate() {
            pthread_attr_init(&mut attr);
            pthread_attr_setstack(&mut attr, stacks.add(at * STACK), STACK);
            let arg = std::ptr::from_ref(work).cast_mut().cast();
            assert!(pthread_create(id, &attr, start::<F>, arg) == 0);
        }
        work();
        ids[..others]
            .iter()
            .for_each(|&id| drop(pthread_join(id, null_mut())));
    }
}

/// Parses the whole file on `threads` threads and merges their rows, returning them with their keys.
/// The threads share the regions too. Eight segments per thread balances the load. Tying the sizes to
/// the thread count also makes the SD_THREADS test sweep move every cut. The spec's pool of names is a
/// distribution, not a promise: if a file holds more names than the table, the pass is thrown away and
/// redone with a bigger table.
fn aggregate(
    records: &Input,
    lookup: &Input,
    threads: usize,
    slots: usize,
) -> (Vec<String>, Vec<Row>) {
    let names = NameTable::new(records, slots, threads);
    let (lines, segments) = (Jobs::default(), Jobs::default());
    let totals = Mutex::new(vec![EMPTY_ROW; 256]);
    let work = || {
        let mut spare = Vec::new();
        let mut buffer = [0u8; PIECE + PADDING];
        lines.run_all(threads * 16, |at| {
            let (pos, end) = lookup.part(threads * 16, at, b"\n");
            names.parse_regions(lookup.read(pos, end, &mut buffer, &mut spare), end - pos);
        });
        let mut rows = [EMPTY_ROW; 256];
        segments.run(threads * SEGMENTS, |at| {
            let (pos, end) = records.part(threads * SEGMENTS, at, b"\r\n");
            if !names.full.load(Relaxed) {
                parse_records(pos, end, &names, &mut rows, &mut buffer, &mut spare);
            }
        });
        for (total, row) in totals.lock().unwrap().iter_mut().zip(rows.iter()) {
            merge(total, row);
        }
    };
    run_threads(&work, threads, names.stacks);
    if names.full.load(Relaxed) {
        return aggregate(records, lookup, threads, slots * 4);
    }
    let keys = names.keys.into_inner().unwrap().into_iter();
    let keys = keys.map(|key| {
        if key == *b"___" {
            "_UNMATCHED".into()
        } else {
            String::from_utf8_lossy(&key).into_owned()
        }
    });
    (keys.collect(), totals.into_inner().unwrap())
}

fn main() -> io::Result<()> {
    let mut args = std::env::args().skip(1);
    let data_dir = args.next().unwrap_or_else(|| "/data".into());
    let out_dir = args.next().unwrap_or_else(|| "/out".into());
    // `SD_THREADS` if set (the test sweep uses it), else what the container allows, capped at the
    // box's 12 threads.
    let allowed = thread::available_parallelism().map_or(1, |n| n.get().min(12));
    let threads = std::env::var("SD_THREADS").map_or(allowed, |v| v.parse().unwrap_or(allowed));
    let lookup = Input::open(&format!("{data_dir}/regions.csv"))?;
    let records = Input::open(&format!("{data_dir}/records.csv"))?;
    let (keys, totals) = aggregate(&records, &lookup, threads, INITIAL_SLOTS);

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
