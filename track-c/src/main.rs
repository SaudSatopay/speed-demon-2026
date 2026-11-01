//! Track C (Join): aggregates per country, from a hostile CSV joined against a region table.
//!
//! Threads claim segments of the file from a shared cursor. Segments are cut just after a CRLF,
//! because the spec promises that a CR appears nowhere else, so that is always a record boundary, and
//! no quote counting is needed. A thread reads its segment in small pieces into its own buffer, which
//! stays in its caches and needs no page mapped. Inside a piece the parser has two stages, because a
//! parser that finds each field from the end of the last one is limited by that chain of loads, not
//! by its instructions. The first stage walks fixed 64-byte blocks, and lists every comma and line end
//! outside quotes. The second reads each record's fields from its five listed positions, and no record
//! waits for the one before it. Nothing is unescaped: a quoted field's raw bytes, with its `""` pairs
//! intact, encode its value one to one, and the value's length follows from counting its quotes. Each
//! thread sums into its own rows. One shared table counts the distinct names per country, and a chunk
//! of records has its cache lines prefetched before any of it is added up.
//!
//! All the memory is one mapping, touched by the main thread before any other starts. The kernel
//! charges a memory group's pages to each CPU in batches, and what a CPU has charged but not used yet
//! counts in the peak: up to 252 KB per CPU that charges anything at all. So the workers, on stacks in
//! that mapping, never fault and never use the heap.

#![no_main]

use std::arch::{asm, x86_64::*};
use std::io::{self, BufWriter, Write};
use std::sync::atomic::{AtomicU8, AtomicU64, AtomicUsize, Ordering::*};
use std::{collections::BTreeSet, fs::File, os::unix::fs::FileExt};
use std::{ptr::null_mut, sync::Mutex, thread};

type Start = unsafe extern "C" fn(*mut u8) -> *mut u8;

// Declared by hand to keep the crate at zero dependencies; std already links glibc. A `pthread_attr_t`
// is 56 bytes, so the word array holds it.
unsafe extern "C" {
    fn mmap(at: *mut u8, len: usize, p: i32, f: i32, fd: i32, o: i64) -> *mut u8;
    fn madvise(at: *mut u8, len: usize, advice: i32) -> i32;
    fn pthread_attr_init(attr: *mut [u64; 8]) -> i32;
    fn pthread_attr_setstack(attr: *mut [u64; 8], stack: *mut u8, len: usize) -> i32;
    fn pthread_create(id: *mut u64, attr: *const [u64; 8], start: Start, arg: *mut u8) -> i32;
    fn pthread_join(id: u64, result: *mut *mut u8) -> i32;
}

/// Zero bytes after the text in a buffer, so that block loads can run past its end.
const PADDING: usize = 128;
/// Bytes per read: little enough to stay in the caches, enough that a read costs little per record.
const PIECE: usize = 1 << 15;

/// An input file, read in parts: its size, and the file.
struct Input(usize, File);

impl Input {
    /// The first line boundary at or after `pos`: just past the next `terminator`, or the end of the
    /// file. In the records, a CR can only be the first byte of a CRLF terminator (spec section 2.2),
    /// so it's never inside quotes, and about half the records end in one, so it's never far. A file
    /// without one simply doesn't get split.
    fn boundary(&self, mut pos: usize, terminator: &[u8]) -> usize {
        let mut bytes = [0; 512];
        while let read @ 1.. = self.1.read_at(&mut bytes, pos as u64).unwrap() {
            if let Some(at) = bytes[..read].iter().position(|&c| c == terminator[0]) {
                return pos + at + terminator.len();
            }
            pos += read;
        }
        pos
    }

    /// Where part `at` of `parts` starts and ends. The first boundary in a file ends its header.
    fn part(&self, parts: usize, at: usize, terminator: &[u8]) -> (usize, usize) {
        let size = self.0.div_ceil(parts).max(1);
        let cut = |at: usize| self.boundary((at * size).min(self.0), terminator);
        (cut(at), cut(at + 1))
    }

    /// Reads `len` bytes at `at` to the start of `room`, or of a new buffer if they don't fit there
    /// (no generated record comes near that), with `PADDING` zeros after them.
    fn read<'a>(&self, at: usize, len: usize, room: &'a mut [u8]) -> &'a [u8] {
        // The kernel's copy runs fastest when source and destination agree within a cache line.
        let skew = at.wrapping_sub(room.as_ptr() as usize) % 64;
        let text = match room.get_mut(skew..skew + len + PADDING) {
            Some(room) => room,
            None => Vec::leak(vec![0; len + PADDING]),
        };
        self.1.read_exact_at(&mut text[..len], at as u64).unwrap();
        text[len..].fill(0);
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

/// One bit per byte of the 64 at `at`, set where `is` marks the byte in its vector.
///
/// # Safety
/// The 64 bytes at `at` must be inside a buffer.
unsafe fn byte_mask(at: *const u8, is: impl Fn(__m256i) -> __m256i) -> u64 {
    // SAFETY: the caller keeps both loads inside the buffer, they need no alignment, and the build
    // targets x86-64-v3, which has AVX2.
    unsafe {
        let mask = |at| _mm256_movemask_epi8(is(_mm256_loadu_si256(at))) as u32;
        let at = at.cast::<__m256i>();
        u64::from(mask(at)) | u64::from(mask(at.add(1))) << 32
    }
}

/// Parses three numbers of up to 8 digits each, in one vector. Each word's digits sit at its top, with
/// the most significant in the lowest byte they use. Anything below `0` counts as a zero digit. Each
/// step doubles the digits per lane: to 2, to 4, then to 8. The third is an amount, in the form `d.dd`,
/// so its weights are for cents: its point's byte has none, and the digit before the point gets the
/// weight of the point.
/// The first two come back as one word, low half first, since most callers keep them side by side.
fn parse_three(words: [u64; 3]) -> (u64, u32) {
    // SAFETY: x86-64-v3 has AVX2.
    unsafe {
        let [a, b, c] = words.map(|word| word as i64);
        let digits = _mm256_subs_epu8(_mm256_set_epi64x(0, c, b, a), _mm256_set1_epi8(b'0' as i8));
        // Weights in each word: 10 and 1, and for the steps to 8 digits 10,000 or 1,000 and 1.
        let (t, l) = (0x010A_010A_010A_010A, 0x0001_2710_0001_2710);
        let h = 0x0001_03E8_0001_03E8;
        let tens = _mm256_set_epi64x(t, 0x010A_0001_010A_010A, t, t);
        let pairs = _mm256_maddubs_epi16(digits, tens);
        let quads = _mm256_madd_epi16(pairs, _mm256_set1_epi32(0x0001_0064));
        let packed = _mm256_packus_epi32(quads, quads);
        let eights = _mm256_madd_epi16(packed, _mm256_set_epi64x(h, h, l, l));
        // The second one is a region id, and `add_records` needs it inside the join table.
        let cap = _mm256_set_epi32(-1, -1, -1, -1, -1, -1, REGION_IDS as i32 - 1, -1);
        let eights = _mm256_min_epu32(eights, cap);
        let first_two = _mm256_extract_epi64::<0>(eights) as u64;
        (first_two, _mm256_extract_epi32::<4>(eights) as u32)
    }
}

/// Entries in the join table. Region ids stay below a million (spec section 2.3), so the last entry
/// is past every one of them, and stays unmatched, for every id above them.
const REGION_IDS: usize = 1_000_001;
/// Homes for a name: enough for the generator's pool of 100,000 names (spec section 2.2), at about 91%
/// full. A test build makes it tiny, so that names run out of slots.
const HOMES: usize = if cfg!(sd_const_hash) { 1 << 5 } else { 110_000 };
/// Slots past the last home, where the names of the last homes can spill: more than a table this full
/// can need, for a generated file.
const SPARE: usize = 256;
/// Bytes of stack per thread, enough for its rows, buffer and lists.
const STACK: usize = 68 << 10;
/// Words per slot, which is one cache line: four words of key, then 256 bits.
const SLOT_WORDS: usize = 8;
/// Names longer than this get no slot. No generated name has been seen beyond 25 bytes.
const INLINE_BYTES: usize = 32;
/// A 32-byte load at `INLINE_BYTES - len` bytes in has `len` zero bytes, and the rest all ones. A `Row`
/// is the size of a cache line and aligned to one, so that the loads never split across two.
static PADS: Row = Row([0, 0, 0, 0, -1, -1, -1, -1]);

/// Distinct names per country, in one table shared by all threads, with no locks. Each slot holds a
/// name's bytes, padded with ones, and one bit per country that has seen this name. The tags, one byte
/// per slot, zero while it's free and then 8 bits of the hash of its name, say which slot to open, so
/// a lookup costs one cache miss, with no probing through other names' lines. A name's home is a slot,
/// and one vector holds the tags of the 32 slots from there, so it costs the same in any of them, with
/// no branch. Only when all are full does it move on 32 slots. A home for every slot, not every few,
/// leaves far fewer names outside their first 32. Names are compared in full, and never unescaped: the raw bytes encode the
/// value one to one. The slots are insert-only: a key's words go from zero to their final value once,
/// and a key has no zero bytes, so a slot that compares equal is complete. A name that gets no slot
/// goes to a locked set instead. A full window stays full, so that name never gets one later.
struct NameTable<'a> {
    records: &'a Input,
    words: &'static [AtomicU64],
    tags: &'static [AtomicU8],
    regions: &'static [AtomicU8], // The join table, country indices by region id.
    stacks: usize,                // Where the first worker's stack starts.
    index_of: Vec<AtomicU8>,      // Country indices by code, which takes every one in `[A-Z]{3}`.
    keys: Mutex<Vec<[u8; 3]>>, // Index 0 is `_UNMATCHED`, and codes take the next as they appear.
    spill: Mutex<BTreeSet<(Vec<u8>, u8)>>, // Names with no slot, each with a country it has seen.
}

impl<'a> NameTable<'a> {
    fn new(records: &'a Input, threads: usize) -> Self {
        let tags = HOMES + SPARE;
        let (tags_at, regions_at) = (tags * SLOT_WORDS * 8, tags * SLOT_WORDS * 8 + tags);
        let stacks_at = (regions_at + REGION_IDS).next_multiple_of(4096);
        let bytes = stacks_at + (threads - 1) * STACK;
        let (read_write, private_anon, huge) = (3, 0x22, 1 << 21);
        // SAFETY: the mapping is fresh, zeroed, never unmapped and big enough for an aligned `bytes`,
        // so the parts don't overlap, and zero is a valid value of each atomic.
        unsafe {
            let at = mmap(null_mut(), bytes + huge, read_write, private_anon, -1, 0);
            assert!(at as isize != -1);
            let base = at.add(at.align_offset(huge));
            // No huge page past the last whole one: whether one fits there depends on the address.
            madvise(base.add(bytes / huge * huge), huge, 15);
            madvise(base, bytes, 23);
            NameTable {
                records,
                words: std::slice::from_raw_parts(base.cast(), tags * SLOT_WORDS),
                tags: std::slice::from_raw_parts(base.add(tags_at).cast(), tags),
                regions: std::slice::from_raw_parts(base.add(regions_at).cast(), REGION_IDS),
                stacks: base.add(stacks_at) as usize,
                index_of: (0..26 * 26 * 26).map(|_| AtomicU8::new(0)).collect(),
                keys: Mutex::new(Vec::with_capacity(256)),
                spill: Mutex::new(BTreeSet::new()),
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
            let (id, _) = parse_three([word << (64 - bits), 0, 0]);
            pos += bits as usize / 8 + 1;
            let code = (0..3).fold(0, |code, i| code * 26 + usize::from(text[pos + i] - b'A'));
            let index = &self.index_of[code];
            if index.load(Relaxed) == 0 {
                let mut keys = self.keys.lock().unwrap();
                if index.load(Relaxed) == 0 {
                    keys.push([text[pos], text[pos + 1], text[pos + 2]]);
                    index.store(keys.len() as u8, Relaxed); // At most 240 (spec section 2.3).
                }
            }
            self.regions[id as u32 as usize].store(index.load(Relaxed), Relaxed);
            pos += 4; // The three letters and the LF.
        }
    }

    /// A hash of the name's length and first 8 bytes, with whatever follows it masked off: the pool's
    /// names rarely share them. BZHI reads only the low 8 bits of its count, so a name of 32 bytes or
    /// more can keep extra bytes. That's still the same word for the same name, and the key decides
    /// anyway. A test build swaps this for a constant, so that every key collides and any logic that
    /// trusts a hash without comparing the key shows up as a wrong answer.
    fn hash(text: &[u8], start: usize, len: usize) -> u32 {
        // SAFETY: a name starts inside the text and the padding covers the load. BZHI is in BMI2 and
        // CRC32 in SSE4.2, and x86-64-v3 includes both.
        let head = unsafe { _bzhi_u64(load_u64(text, start), (len as u32) << 3) };
        let crc = unsafe { _mm_crc32_u64(len as u64, head) as u32 };
        if cfg!(sd_const_hash) { 0 } else { crc }
    }

    fn home(hash: u32) -> usize {
        (u64::from(hash) * HOMES as u64 >> 32) as usize
    }

    /// One bit per slot of a window of tags, set where the tag is `tag`.
    fn slots_with(window: __m256i, tag: u8) -> u32 {
        // SAFETY: x86-64-v3 has AVX2.
        unsafe {
            _mm256_movemask_epi8(_mm256_cmpeq_epi8(window, _mm256_set1_epi8(tag as i8))) as u32
        }
    }

    /// The tags of the 32 slots from `home`.
    fn window(&self, home: usize) -> __m256i {
        // SAFETY: `home` stays 32 below the end of the tags.
        // x86-64-v3 has AVX2.
        unsafe { _mm256_loadu_si256(self.tags.as_ptr().add(home).cast()) }
    }

    /// Guesses the name's slot from the tags, and asks to own its line, for a write: a locked update
    /// then needs no second trip to take it from the core that wrote it last. A wrong guess costs a
    /// search. A name's tag is the low byte of its hash, or 1 for zero, which means free; here a zero
    /// finds a free slot, which is a wrong guess too, and costs one name in 256 less than a check.
    /// It returns the slot's offset in bytes.
    fn guess_slot(&self, home: usize, tag: u8) -> u32 {
        let hits = Self::slots_with(self.window(home), tag);
        let at = (home + hits.trailing_zeros() as usize) * SLOT_WORDS * 8;
        // SAFETY: the slot is inside the table, and prefetching has no effect on memory anyway.
        unsafe { _mm_prefetch::<_MM_HINT_ET0>(self.words.as_ptr().byte_add(at).cast()) };
        at as u32
    }

    /// The key of a name of at most `INLINE_BYTES`.
    fn key(text: &[u8], start: usize, len: usize) -> __m256i {
        // SAFETY: the name starts inside the text and the padding covers the load. Its length keeps
        // the other load inside the table. x86-64-v3 has AVX2.
        unsafe {
            let at = |bytes: *const u8| _mm256_loadu_si256(bytes.cast());
            let pad = at(PADS.0.as_ptr().cast::<u8>().add(INLINE_BYTES - len));
            _mm256_or_si256(at(text.as_ptr().add(start)), pad)
        }
    }

    /// Whether this slot holds the name, of at most `INLINE_BYTES`. A slot whose writer hasn't finished
    /// with it doesn't.
    ///
    /// # Safety
    /// `stored` must be where a slot's words start.
    unsafe fn holds(stored: *const AtomicU64, text: &[u8], start: usize, len: usize) -> bool {
        // SAFETY: the slot is inside the table. x86-64-v3 has AVX2.
        unsafe {
            let key = Self::key(text, start, len);
            let differ = _mm256_xor_si256(key, _mm256_loadu_si256(stored.cast()));
            _mm256_testz_si256(differ, differ) == 1
        }
    }

    /// Finds the name's slot, or takes a free one for it, and returns its offset in bytes. Only a wrong
    /// guess or a new name comes here. A long name, or one that finds every window from its home to the
    /// end of the table full, gets no slot.
    #[cold]
    fn find(&self, text: &[u8], start: usize, len: usize) -> Option<usize> {
        let hash = Self::hash(text, start, len);
        let (mut home, tag) = (Self::home(hash), (hash as u8).max(1));
        while len <= INLINE_BYTES && home <= HOMES + SPARE - 32 {
            // One load answers both questions below, so a name can't get two slots. Each byte only
            // ever changes from zero to its final tag, and everyone takes the lowest free slot. If
            // two threads insert one name, whichever saw the other's slot as taken saw its tag too,
            // and if neither did, they both want the same byte and one loses the exchange.
            let window = self.window(home);
            let mut hits = Self::slots_with(window, tag);
            while hits != 0 {
                let base = (home + hits.trailing_zeros() as usize) * SLOT_WORDS;
                while self.words[base + 3].load(Acquire) == 0 {}
                // SAFETY: `base` is where a slot's words start.
                if unsafe { Self::holds(self.words[base..].as_ptr(), text, start, len) } {
                    return Some(base * 8);
                }
                hits &= hits - 1;
            }
            let free = Self::slots_with(window, 0);
            if free == 0 {
                home += 32; // Past this window.
                continue;
            }
            // Losing this race just means another look at the same window.
            let at = home + free.trailing_zeros() as usize;
            let slot = &self.tags[at];
            if slot.compare_exchange(0, tag, Relaxed, Relaxed).is_ok() {
                // SAFETY: both are 32 bytes. x86-64-v3 has AVX2.
                let key: [u64; 4] = unsafe { std::mem::transmute(Self::key(text, start, len)) };
                let stored = &self.words[at * SLOT_WORDS..][..4];
                (0..3).for_each(|i| stored[i].store(key[i], Relaxed));
                stored[3].store(key[3], Release);
                return Some(at * SLOT_WORDS * 8);
            }
        }
        None
    }
}

/// A country's totals, one cache line: its count; lowest amount; id sum; highest amount; amount sum;
/// name bytes; how many of its records repeat a name; and note bytes. That order keeps sums apart, or
/// the compiler pairs their additions up as vector instructions, which take more than the two would.
#[repr(align(64))]
struct Row([i64; 8]);
const EMPTY_ROW: Row = Row([0, i64::MAX, 0, i64::MIN, 0, 0, 0, 0]);

/// Enough rows for every country index a byte can hold, so that indexing needs no check.
type Rows = [Row; 256];
/// A record's guessed slot, cents, and id and region together.
type Found = [(u32, u32, u64); LISTED / 5];

/// Segments per thread: when the segments run out, each thread still has its last to finish, and the
/// threads done first sit idle. Each one costs two small reads and a short piece, so not too many.
const SEGMENTS: usize = 24;
/// Blocks per chunk: about as many records as a chunk's cache lines can overlap for.
const BLOCKS: usize = 16;
/// A block can hold at most 64 separators, and listing one writes 8 slots regardless.
const LISTED: usize = BLOCKS * 64 + 16;
/// Bit `k` of each of the 64 positions in a block, at that position.
const fn plane(k: u32) -> u64 {
    !0 / ((1 << (1 << k)) + 1) << (1 << k)
}
static PLANES: [u64; 6] = [plane(0), plane(1), plane(2), plane(3), plane(4), plane(5)];

/// Lists into `out` the positions just past the separators outside quotes in `blocks` blocks of `text`
/// from `scan`, and returns how many. `odd` is all ones when the first block starts inside quotes,
/// that is after an odd number of quotes, and comes back set for the block after. A function of its
/// own, so that its constants get the registers that the loops around it would otherwise hold.
#[inline(never)]
fn separators(text: &[u8], scan: usize, blocks: usize, out: &mut [u32], odd: &mut u64) -> usize {
    // Read once here, since the compiler would otherwise rebuild them in every block.
    let planes = std::hint::black_box(&PLANES);
    // SAFETY: the blocks are inside the text, or the padding covers them. PCLMULQDQ is in the
    // build's target features. The carry-less multiply by all ones gives each bit the parity of the
    // quotes up to it, and that's odd exactly inside quotes, since a `""` pair cancels out. The
    // positions stay below `LISTED - 8`, as a chunk has `BLOCKS` blocks.
    unsafe {
        let mut cursor = out.as_mut_ptr();
        let mut base = _mm256_set1_epi32(scan as i32 + 1);
        let mut odd_so_far = *odd;
        let mut at = text.as_ptr().add(scan);
        let end = at.add(blocks * 64);
        // A byte's low half picks what the byte would have to be, to be a separator: an LF, a comma,
        // or all ones, which only a byte with the top bit set could be, and those shuffle to zero.
        let lf_comma = 0xFFFF_FF2C_FF0A_FFFFu64 as i64;
        let separator = _mm256_set_epi64x(lf_comma, -1, lf_comma, -1);
        let quote = _mm256_set1_epi8(b'"' as i8);
        while at < end {
            let quotes = byte_mask(at, |block| _mm256_cmpeq_epi8(block, quote));
            let ones = _mm_set1_epi8(-1);
            let parity = _mm_clmulepi64_si128::<0>(_mm_cvtsi64_si128(quotes as i64), ones);
            let inside = _mm_cvtsi128_si64(parity) as u64 ^ odd_so_far;
            odd_so_far = (inside as i64 >> 63) as u64;
            let shuffled = |block| _mm256_shuffle_epi8(separator, block);
            let left = byte_mask(at, |block| _mm256_cmpeq_epi8(block, shuffled(block))) & !inside;
            // Blocks hold 5 of them on average, and a loop's exit would be a coin toss, so this
            // writes 8 whatever the number, and moves on by the right amount. The positions of up to
            // 8 set bits come out as 8 bytes at once: `PLANES[k]` has bit `k` of every position at
            // that position, and PEXT gathers it for those bits, into a byte. Those bytes are a bit
            // matrix with a row per bit of a position, and GFNI's multiply, given the columns one at
            // a time, turns it around into a byte per position.
            let matrix = planes
                .iter()
                .fold(0, |matrix, &plane| matrix << 8 | _pext_u64(plane, left));
            let columns = _mm_cvtsi64_si128(0x8040_2010_0804_0201u64 as i64);
            let packed =
                _mm_gf2p8affine_epi64_epi8::<0>(columns, _mm_cvtsi64_si128((matrix << 16) as i64));
            let bytes = _mm256_cvtepu8_epi32(packed);
            _mm256_storeu_si256(cursor.cast(), _mm256_add_epi32(bytes, base));
            let total = left.count_ones() as usize;
            if total > 8 {
                let mut rest = left;
                for i in 0..total {
                    *cursor.add(i) =
                        at.offset_from(text.as_ptr()) as u32 + rest.trailing_zeros() + 1;
                    rest &= rest.wrapping_sub(1);
                }
            }
            cursor = cursor.add(total);
            at = at.add(64);
            base = _mm256_add_epi32(base, _mm256_set1_epi32(64));
        }
        *odd = odd_so_far;
        cursor.offset_from(out.as_ptr()) as usize
    }
}

/// Parses the records in `pos..end`, adding each to its country's row.
fn parse_records(mut pos: usize, end: usize, names: &NameTable, rows: &mut Rows, room: &mut [u8]) {
    // The positions just past commas and line ends outside quotes, after a record's start, so a
    // record's five come after the position it starts at. Positions fit 32 bits: a piece is small.
    let (mut listed, mut found) = ([0u32; LISTED], [(0, 0, 0); LISTED / 5]);
    let mut piece = PIECE;
    while pos < end {
        let len = piece.min(end - pos);
        let text = names.records.read(pos, len, room);
        // A piece starts at a record's start, so outside quotes.
        let (mut count, mut scan, mut odd_so_far) = (1, 0, 0);
        listed[0] = 0;
        while scan < len {
            let blocks = BLOCKS.min((len - scan).div_ceil(64));
            count += separators(text, scan, blocks, &mut listed[count..], &mut odd_so_far);
            scan += blocks * 64;
            // The final record may have no terminator (spec section 2.1). This costs little to ask.
            if scan >= len && pos + len == end && count % 5 == 0 {
                listed[count] = len as u32 + 1;
                count += 1;
            }
            let records = (count - 1) / 5;
            add_records(text, &listed[..=records * 5], &mut found, names, rows);
            // Under 5 positions are left, and the list has 8 free past `count`, so one fixed
            // copy moves them, where a call to copy just the right number would cost more.
            listed.copy_within(records * 5..records * 5 + 8, 0);
            count -= records * 5;
        }
        // What's left is the start of a record the piece cut short. A record longer than a piece gets
        // a longer piece.
        let done = listed[0] as usize;
        piece = if done > 0 { PIECE } else { 2 * piece };
        pos += done;
    }
}

/// Adds up a chunk of whole records from `text`, given where they start and the positions just past
/// each one's five separators.
fn add_records(text: &[u8], listed: &[u32], found: &mut Found, names: &NameTable, rows: &mut Rows) {
    let records = listed.len() / 5;
    let positions = |record: usize| {
        // SAFETY: the caller passes `records * 5 + 1` positions.
        let six = unsafe { listed.as_ptr().add(record * 5).cast::<[u32; 6]>() };
        unsafe { six.read_unaligned() }.map(|position| position as usize)
    };
    let (words, regions) = (names.words.as_ptr(), names.regions.as_ptr());
    for record in 0..records {
        let [mut start, name_at, note_at, region_at, amount_at, _] = positions(record);
        // SAFETY: every position is inside the text, or just after it, so the padding covers the
        // loads. `region` stays inside the join table. Prefetching has no effect on memory.
        unsafe {
            // A byte order mark can only start a record here, and no record otherwise starts with it.
            if *text.get_unchecked(start) == 0xEF {
                start = std::hint::black_box(start + 3); // Keeps it a branch: one record in 4,000.
            }
            // Moves the digits of the field at `start` to the top of a word, with zeros below them.
            let id = load_u64(text, start) << (8 * (start + 1 - name_at) as u32 & 63);
            // At most 7 digits (spec section 2.2), and the comma below them counts as a zero.
            let region = load_u64(text, region_at - 1) << (8 * (region_at - amount_at) as u32 & 63);
            // Amounts are at most 8 bytes, and negative ones at most 7 (spec section 2.2), so one load
            // holds one with what follows it. Its digits, point and sign have bit 5 set, and the CR,
            // LF or padding after it doesn't, which finds its end with no branch to mispredict.
            let word = load_u64(text, amount_at);
            let field = (!word & 0x2020_2020_2020_2020).trailing_zeros() & !7; // 8 bits per byte
            let (id_region, cents) = parse_three([id, region, word << (64 - field)]);
            // A minus sign is the one thing that sorts below a digit.
            let negative = -i32::from((word as u8) < b'0');
            _mm_prefetch::<_MM_HINT_T0>(regions.add((id_region >> 32) as usize).cast());
            let hash = NameTable::hash(text, name_at + 1, (note_at - name_at).wrapping_sub(3));
            let guess = names.guess_slot(NameTable::home(hash), hash as u8);
            let cents = (cents as i32 ^ negative) - negative;
            *found.get_unchecked_mut(record) = (guess, cents as u32, id_region);
        }
    }
    for record in 0..records {
        let [_, name_at, note_at, region_at, ..] = positions(record);
        let (guess, cents, id_region) = found[record];
        // SAFETY: the first pass kept the region inside the table, and its writes all came before.
        let country = unsafe { regions.add((id_region >> 32) as usize).cast::<u8>().read() };
        let row = &mut rows[usize::from(country)].0;
        let cents = i64::from(cents as i32);
        row[0] += 1;
        row[2] += i64::from(id_region as u32);
        row[4] += cents;
        // These hardly ever change, so branches beat conditional moves and two stores.
        (cents < row[1]).then(|| row[1] = cents);
        (cents > row[3]).then(|| row[3] = cents);
        let (name, both) = (
            (note_at - name_at - 1) as u32 as usize,
            region_at - name_at - 1,
        );
        // SAFETY: `name_at` is inside the text, and the padding covers the mask. BZHI keeps every bit
        // for a count of 64.
        let (name_quotes, quotes) = if both <= 64 {
            let quote = unsafe { _mm256_set1_epi8(b'"' as i8) };
            let is_quote = |block| unsafe { _mm256_cmpeq_epi8(block, quote) };
            let mask = unsafe { byte_mask(text.as_ptr().add(name_at), is_quote) };
            let below = |bits| unsafe { _bzhi_u64(mask, bits as u32) }.count_ones() as usize;
            (below(name), below(both))
        } else {
            let tail = &text[name_at..name_at + both];
            (count_quotes(&tail[..name]), count_quotes(tail))
        };
        // A value is its field, less its delimiting quotes, less one for each `""`, with empty
        // fields of no quotes or two both coming to nothing.
        row[5] += (name - name_quotes / 2).saturating_sub(1) as i64;
        row[7] += (both - name - 1 - (quotes - name_quotes) / 2).saturating_sub(1) as i64;
        let (start, len) = (name_at + 1, name.wrapping_sub(2));
        // SAFETY: `guess` came from `guess_slot`, and `find` returns a slot too. The name starts
        // inside the text. A slot's 256 bits start 4 words in, and a byte can't index past them.
        unsafe {
            let at = if len <= INLINE_BYTES
                && NameTable::holds(words.byte_add(guess as usize), text, start, len)
            {
                guess as usize
            } else {
                let len = name.saturating_sub(2);
                let Some(at) = names.find(text, start, len) else {
                    let pair = (text[start..start + len].to_vec(), country);
                    row[6] += i64::from(!names.spill.lock().unwrap().insert(pair));
                    continue;
                };
                at
            };
            // Nine records in ten set a new bit, so a plain load first would rarely save the locked
            // write, and the prefetch has already asked to own the line. BTS finds the word itself, and
            // leaves the old bit in the carry, which adds one to the row's repeats if it had been set.
            asm!("lock bts [{} + {} + 32], {}", "adc qword ptr [{} + 48], 0",
                in(reg) words, in(reg) at, in(reg) usize::from(country), in(reg) row.as_mut_ptr());
        }
    }
}

/// Counts the quotes in a field too long for one block's mask, which is one record in 60.
#[cold]
fn count_quotes(field: &[u8]) -> usize {
    field.iter().filter(|&&c| c == b'"').count()
}

/// Runs the numbered jobs below `count` that `next` hasn't handed out yet, sharing them with however
/// many threads call this.
fn run_jobs(next: &AtomicUsize, count: usize, mut job: impl FnMut(usize)) {
    while let Some(at) = Some(next.fetch_add(1, Relaxed)).filter(|&at| at < count) {
        job(at);
    }
}

/// Runs a closure, from a new thread's start.
unsafe extern "C" fn start<F: Fn()>(work: *mut u8) -> *mut u8 {
    // SAFETY: `run_threads` passes a closure that outlives the thread.
    unsafe { (*work.cast::<F>())() };
    null_mut()
}

/// Calls `work` on `threads` threads, this one and others on the given stacks.
fn run_threads<F: Fn() + Sync>(work: &F, threads: usize, stacks: usize) {
    let mut ids = vec![0u64; threads - 1];
    // SAFETY: each thread has its own stack, inside the mapping, and is joined before `work` ends.
    unsafe {
        let mut attr = [0u64; 8];
        for (at, id) in ids.iter_mut().enumerate() {
            pthread_attr_init(&mut attr);
            pthread_attr_setstack(&mut attr, (stacks + at * STACK) as *mut u8, STACK);
            let arg = std::ptr::from_ref(work).cast_mut().cast();
            assert!(pthread_create(id, &attr, start::<F>, arg) == 0);
        }
        work();
        for id in ids {
            pthread_join(id, null_mut());
        }
    }
}

/// Every country's code, in index order from 1, and every index's row, index 0 being `_UNMATCHED`.
type Totals = (Vec<[u8; 3]>, Rows);

/// Parses the whole file on `threads` threads and merges their rows, returning them with the codes.
/// The threads share the regions too, and all of those are done before any record is parsed. Tying
/// the part sizes to the thread count also makes the SD_THREADS test sweep move every cut.
fn aggregate(records: &Input, lookup: &Input, threads: usize) -> Totals {
    let names = NameTable::new(records, threads);
    let (lines, segments, line_parts) = (AtomicUsize::new(0), AtomicUsize::new(0), threads * 8);
    let (parsed, totals) = (AtomicUsize::new(0), Mutex::new([EMPTY_ROW; 256]));
    let work = || {
        let mut room = [0u8; PIECE + PADDING + 64];
        run_jobs(&lines, line_parts, |at| {
            let (pos, end) = lookup.part(line_parts, at, b"\n");
            names.parse_regions(lookup.read(pos, end - pos, &mut room), end - pos);
            parsed.fetch_add(1, Release);
        });
        // Not a barrier: nobody should wait for a thread that hasn't started, or be woken up.
        while parsed.load(Acquire) < line_parts {
            thread::yield_now();
        }
        let mut rows = [EMPTY_ROW; 256];
        run_jobs(&segments, threads * SEGMENTS, |at| {
            let (pos, end) = records.part(threads * SEGMENTS, at, b"\r\n");
            parse_records(pos, end, &names, &mut rows, &mut room);
        });
        for (Row(total), Row(row)) in totals.lock().unwrap().iter_mut().zip(&rows) {
            for i in [0, 2, 4, 5, 6, 7] {
                total[i] += row[i];
            }
            (total[1], total[3]) = (total[1].min(row[1]), total[3].max(row[3]));
        }
    };
    run_threads(&work, threads, names.stacks);
    let keys = names.keys.into_inner().unwrap();
    (keys, totals.into_inner().unwrap())
}

/// C's entry point, not std's: std's would read `/proc/self/maps` and install handlers first, none of
/// which this program needs, and the timer counts all of it.
#[unsafe(no_mangle)]
extern "C" fn main() -> i32 {
    run().map_or(1, |()| 0)
}

fn run() -> io::Result<()> {
    let mut args = std::env::args().skip(1);
    let data_dir = args.next().unwrap_or_else(|| "/data".into());
    let out_dir = args.next().unwrap_or_else(|| "/out".into());
    // `SD_THREADS` if set (the test sweep uses it), else what the container allows, capped at the
    // box's 12 threads.
    let allowed = thread::available_parallelism().map_or(1, |n| n.get().min(12));
    let threads = std::env::var("SD_THREADS").map_or(allowed, |v| v.parse().unwrap_or(allowed));
    let open = |name| {
        let file = File::open(format!("{data_dir}/{name}.csv"))?;
        io::Result::Ok(Input(file.metadata()?.len() as usize, file))
    };
    let (lookup, records) = (open("regions")?, open("records")?);
    let (codes, totals) = aggregate(&records, &lookup, threads);

    // Sorted by key, bytewise: the codes in order, then `_UNMATCHED`, because `_` is above `Z`. A
    // row's columns are already in output order, and a group without records gets no line.
    let keys = codes.iter().map(|code| std::str::from_utf8(code).unwrap());
    let mut lines: Vec<_> = keys.zip(&totals[1..]).collect();
    lines.sort_by_key(|&(key, _)| key);
    lines.push(("_UNMATCHED", &totals[0]));
    let mut out = BufWriter::new(File::create(format!("{out_dir}/aggregates.txt"))?);
    for (key, Row(row)) in lines.into_iter().filter(|(_, row)| row.0[0] != 0) {
        write!(out, "{key} {} {}", row[0], row[2])?;
        // Printed as `W.CC`, with the sign apart, because integer division truncates toward zero,
        // which would lose the sign of -0.75. Zero never gets a sign.
        for cents in [&row[4], &row[1], &row[3]] {
            let (sign, magnitude) = (if *cents < 0 { "-" } else { "" }, cents.unsigned_abs());
            write!(out, " {sign}{}.{:02}", magnitude / 100, magnitude % 100)?;
        }
        writeln!(out, " {} {} {}", row[0] - row[6], row[5], row[7])?;
    }
    out.flush()
}
