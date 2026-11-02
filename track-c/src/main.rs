//! Track C (Join): aggregates per country, from a hostile CSV joined against a region table.
//!
//! Threads claim segments of the file from a shared cursor. Segments are cut just after a CRLF,
//! because the spec promises that a CR appears nowhere else, so that is always a record boundary, and
//! no quote counting is needed. The file is mapped, because a copy of it would take as long as much of
//! the parse, and the threads give its pages back between them when they're done, because the exit
//! would otherwise release them all on one thread, on the clock. The parser has two stages, because a
//! parser that finds each field from the end of the last one is limited by that chain of loads, not
//! by its instructions. The first stage walks fixed 64-byte blocks, and lists every comma and line end
//! outside quotes. The second reads each record's fields from its five listed positions, and no record
//! waits for the one before it. Nothing is unescaped: a quoted field's raw bytes, with its `""` pairs
//! intact, encode its value one to one, and the value's length follows from counting its quotes. Each
//! thread sums into its own rows. One shared table counts the distinct names per country, and a chunk
//! of records has its cache lines prefetched before any of it is added up.
//!
//! All the memory is one static, touched by the main thread before any other starts. The kernel
//! charges a memory group's pages to each CPU in batches, and what a CPU has charged but not used yet
//! counts in the peak: up to 252 KB per CPU that charges anything at all. So the workers, on stacks in
//! that static, never fault and never use the heap.

#![no_main]

use std::sync::atomic::{AtomicU8, AtomicU16, AtomicU64, AtomicUsize, Ordering::*};
use std::{arch::asm, arch::x86_64::*, collections::BTreeSet, fs::File, hint::black_box, thread};
use std::{io::BufWriter, io::Write, mem::offset_of, os::fd::AsRawFd, ptr::null_mut, sync::Mutex};

type Start = unsafe extern "C" fn(*mut u8) -> *mut u8;

// Declared by hand to keep the crate at zero dependencies; std already links glibc. A `pthread_attr_t`
// is 56 bytes, so the word array holds it.
unsafe extern "C" {
    fn mmap(at: *mut u8, len: usize, prot: i32, flags: i32, fd: i32, offset: i64) -> *mut u8;
    fn madvise(at: *const u8, len: usize, advice: i32) -> i32;
    fn _exit(status: i32) -> !;
    fn pthread_attr_init(attr: *mut [u64; 8]) -> i32;
    fn pthread_attr_setstack(attr: *mut [u64; 8], stack: *mut u8, len: usize) -> i32;
    fn pthread_create(id: *mut u64, attr: *const [u64; 8], start: Start, arg: *mut u8) -> i32;
    fn pthread_join(id: u64, result: *mut *mut u8) -> i32;
}

/// Zero bytes after a mapped file, so that block loads can run past its end.
const PADDING: usize = 128;

/// Maps a file for reading, followed by `PADDING` zeros. Returns the file's bytes with those after
/// them. Touches one page in every 2 MiB, so that this thread allocates the page tables: a worker
/// that did would charge a batch of pages to its CPU, and a batch counts in the peak (see above).
fn map(file: &File) -> &'static [u8] {
    let len = file.metadata().unwrap().len() as usize;
    // SAFETY: the first mapping is new, and the second replaces its start. Read-only private file
    // pages stay valid for the run, since nothing changes the data files.
    let text = unsafe {
        let base = mmap(null_mut(), len + PADDING, 1, 0x22, -1, 0); // Private and anonymous.
        assert_ne!(base as isize, -1);
        mmap(base, len, 1, 0x12, file.as_raw_fd(), 0); // Private, at this address.
        std::slice::from_raw_parts(base, len + PADDING)
    };
    let pages = (0..len + (1 << 21)).step_by(1 << 21);
    pages.for_each(|at| _ = black_box(text[at.min(len)]));
    text
}

/// Claims the next part of `parts` of a mapped file from the counter `job`, and returns where it starts
/// and ends, or nothing if none is left. Parts end at line boundaries: just past
/// the next `terminator`, or at the end of the file. In the records, a CR can only be the first byte
/// of a CRLF terminator (spec section 2.2), so it's never inside quotes, and about half the records
/// end in one, so it's never far. A file without one simply doesn't get split. The first boundary in
/// a file ends its header.
fn part(text: &[u8], job: &AtomicUsize, parts: usize, terminator: &[u8]) -> Option<(usize, usize)> {
    let at = job.fetch_add(1, Relaxed);
    let bytes = text.len() - PADDING;
    let cut = |at: usize| {
        let from = (at * bytes.div_ceil(parts)).min(bytes);
        let found = text[from..bytes].iter().position(|&c| c == terminator[0]);
        found.map_or(bytes, |to| from + to + terminator.len())
    };
    (at < parts).then(|| (cut(at), cut(at + 1)))
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
        let mask = |at: *const u8| _mm256_movemask_epi8(is(_mm256_loadu_si256(at.cast()))) as u32;
        u64::from(mask(at)) | u64::from(mask(at.add(32))) << 32
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
        let pairs = _mm256_maddubs_epi16(digits, _mm256_set_epi64x(t, 0x010A_0001_010A_010A, t, t));
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
/// Homes for a name: enough for the generator's pool of 100,000 names (spec section 2.2), at about 96%
/// full, which is as many as fit in four huge pages with the rest of `Memory`. A test build makes it
/// tiny, so that names run out of slots.
const HOMES: usize = if cfg!(sd_const_hash) { 1 << 5 } else { 104_724 };
/// Slots, with enough past the last home for its window.
const SLOTS: usize = HOMES + 32;
/// Windows a name tries before it does without a slot: more than a table this full can need, for a
/// generated file.
const TRIES: usize = 64;
/// Bytes of stack per thread, enough for its rows and lists.
const STACK: usize = 40 << 10;
/// Names longer than this get no slot. No generated name has been seen beyond 25 bytes.
const INLINE_BYTES: usize = 32;
/// A 32-byte load at `INLINE_BYTES - len` bytes in has `len` zero bytes, and the rest all ones. A `Row`
/// is the size of a cache line and aligned to one, so that the loads never split across two.
static PADS: Row = Row([0, 0, 0, 0, -1, -1, -1, -1]);

/// Distinct names per country, in one table shared by all threads, with no locks. Each slot holds a
/// name's bytes, padded with ones, and one bit per country that has seen this name. The tags, two bytes
/// per slot, zero while it's free and then 16 bits of the hash of its name, say which slot to open, so
/// a lookup costs one cache miss, with no probing through other names' lines. A name's home is a slot,
/// and one vector holds the tags of the 32 slots from there, so it costs the same in any of them, with
/// no branch. A name takes the first slot free in its window. Only when all are full does it move on,
/// by a distance its tag sets, so that names that spill don't fill the windows next door, which would
/// make long runs of them full. A home for every slot, not every few, leaves far fewer names outside
/// their first 32. Names are compared in full, as raw bytes, which
/// encode the value one to one. The slots are insert-only: a key's words go from zero to their final
/// value once, and a key has no zero bytes, so a slot that compares equal is complete. A name that gets
/// no slot goes to a locked set instead. Slots stay full, so it never gets one later.
///
/// All of it is zeros in the program's image until it's touched, and the alignment gives it huge pages.
#[repr(C, align(2097152))]
struct Memory {
    words: [[AtomicU64; 8]; SLOTS], // Each one cache line: four words of key, then 256 bits.
    tags: [AtomicU16; SLOTS],
    regions: [AtomicU8; REGION_IDS], // The join table, country indices by region id.
    index_of: [AtomicU8; 26 * 26 * 26], // Country indices by code, which takes every one in `[A-Z]{3}`.
    stacks: [[AtomicU64; STACK / 8]; 64], // More threads than the box has, or any test asks for.
}
// SAFETY: zero is a valid value of each atomic.
static MEMORY: Memory = unsafe { std::mem::zeroed() };
/// What the threads change under a lock, neither of it often. First the country codes: index 0 is
/// `_UNMATCHED`, and codes take the next as they appear, in room that `main` makes. Then the names
/// with no slot, each with a country it has seen.
static LOCKED: Mutex<(Vec<&str>, BTreeSet<(Vec<u8>, u8)>)> = Mutex::new((vec![], BTreeSet::new()));

/// Fills the join table from `len` bytes of whole region lines. Ids have at most 8 digits (spec
/// section 2.3): digits have bit 4 set, and commas don't, so the first clear bit marks the end.
fn parse_regions(text: &'static [u8], len: usize) {
    let mut pos = 0;
    while pos < len {
        // SAFETY: `pos` is inside the text, and the padding covers the load.
        let word = unsafe { load_u64(text, pos) };
        let bits = (!word & 0x1010_1010_1010_1010).trailing_zeros() & !7;
        let (id, _) = parse_three([word << (64 - bits), 0, 0]);
        pos += bits as usize / 8 + 1;
        let code = (0..3).fold(0, |code, i| code * 26 + usize::from(text[pos + i] - b'A'));
        let index = &MEMORY.index_of[code];
        if index.load(Relaxed) == 0 {
            let keys = &mut LOCKED.lock().unwrap().0;
            if index.load(Relaxed) == 0 {
                keys.push(std::str::from_utf8(&text[pos..pos + 3]).unwrap());
                index.store(keys.len() as u8, Relaxed); // At most 240 (spec section 2.3).
            }
        }
        MEMORY.regions[id as u32 as usize].store(index.load(Relaxed), Relaxed);
        pos += 4; // The three letters and the LF.
    }
}

/// A hash of the name's length and first 8 bytes, with whatever follows it masked off: the pool's
/// names rarely share them. BZHI reads only the low 8 bits of its count, so a name of 32 bytes or
/// more can keep extra bytes. That's still the same word for the same name, and the key decides
/// anyway. A test build swaps this for a constant, so that every key collides and any logic that
/// trusts a hash without comparing the key shows up as a wrong answer. Returns the name's home, and
/// the hash's low 16 bits for its tag.
fn home_and_tag(text: &[u8], start: usize, len: usize) -> (usize, u16) {
    // SAFETY: a name starts inside the text and the padding covers the load. BZHI is in BMI2 and
    // CRC32 in SSE4.2, and x86-64-v3 includes both.
    let head = unsafe { _bzhi_u64(load_u64(text, start), (len as u32) << 3) };
    // One more than the length, or the empty name would hash to zero, and its tag mean a free slot.
    let crc = unsafe { _mm_crc32_u64(len as u64 + 1, head) as u32 };
    let hash = if cfg!(sd_const_hash) { 0 } else { crc };
    ((u64::from(hash) * HOMES as u64 >> 32) as usize, hash as u16)
}

/// One bit per slot of the 32 in `window`, set where the tag is `tag`. Tags are aligned, so a vector
/// load can't tear one, and a tag of 16 bits rarely matches another name before its own.
fn slots_with(window: [__m256i; 2], tag: u16) -> u32 {
    // SAFETY: x86-64-v3 has AVX2.
    unsafe {
        let is = |tags| _mm256_cmpeq_epi16(tags, _mm256_set1_epi16(tag as i16));
        let bytes = _mm256_packs_epi16(is(window[0]), is(window[1]));
        _mm256_movemask_epi8(_mm256_permute4x64_epi64::<0xD8>(bytes)) as u32
    }
}

/// The key of a name of at most `INLINE_BYTES`.
fn key(text: &[u8], start: usize, len: usize) -> __m256i {
    // SAFETY: the name starts inside the text and the padding covers the load. Its length keeps
    // the other load inside the table. x86-64-v3 has AVX2.
    unsafe {
        let pad = _mm256_loadu_si256(PADS.0.as_ptr().byte_add(INLINE_BYTES - len).cast());
        _mm256_or_si256(_mm256_loadu_si256(text.as_ptr().add(start).cast()), pad)
    }
}

/// Whether the slot at this offset in bytes holds the name, of at most `INLINE_BYTES`. A slot whose
/// writer hasn't finished with it doesn't.
///
/// # Safety
/// `at` must be where a slot's words start.
unsafe fn holds(at: usize, text: &[u8], start: usize, len: usize) -> bool {
    // SAFETY: the slot is inside the table. x86-64-v3 has AVX2.
    unsafe {
        let stored = MEMORY.words.as_ptr().byte_add(at);
        let differ = _mm256_xor_si256(key(text, start, len), _mm256_loadu_si256(stored.cast()));
        _mm256_testz_si256(differ, differ) == 1
    }
}

/// Finds the name's slot, or takes a free one for it, and returns its offset in bytes. Only a wrong
/// guess or a new name comes here. A long name, or one that finds all its `TRIES` windows full, gets
/// no slot.
#[cold]
fn find(text: &[u8], start: usize, len: usize) -> Option<usize> {
    let (mut home, hash) = home_and_tag(text, start, len);
    let (tag, tries) = (hash.max(1), if len <= INLINE_BYTES { TRIES } else { 0 });
    // Each tag only ever changes from zero to its final tag, and a name only takes a slot when every
    // one before it on its walk is taken. The walk is the same on every thread, so two threads
    // inserting one name both come to the same free slot, one loses the exchange, and it then sees
    // the winner's tag there.
    for _ in 0..tries {
        // SAFETY: a home is below `HOMES`, so 32 below the end of the tags, and the load needs no
        // alignment.
        let window = unsafe { std::ptr::read_unaligned(MEMORY.tags.as_ptr().add(home).cast()) };
        let mut hits = slots_with(window, tag) | slots_with(window, 0);
        while hits != 0 {
            let at = home + hits.trailing_zeros() as usize;
            hits &= hits - 1;
            let (slot, words) = (&MEMORY.tags[at], &MEMORY.words[at]);
            if slot.load(Relaxed) == 0 && slot.compare_exchange(0, tag, Relaxed, Relaxed).is_ok() {
                // SAFETY: both are 32 bytes. x86-64-v3 has AVX2.
                let key: [u64; 4] = unsafe { std::mem::transmute(key(text, start, len)) };
                words.iter().zip(key).for_each(|(w, k)| w.store(k, Release));
                return Some(at * 64);
            }
            if slot.load(Relaxed) == tag {
                while words[3].load(Acquire) == 0 {}
                // SAFETY: `at * 64` is where a slot's words start.
                if unsafe { holds(at * 64, text, start, len) } {
                    return Some(at * 64);
                }
            }
        }
        home = (home + 33 + usize::from(hash)) % HOMES;
    }
    None
}

/// A country's totals, one cache line: its count; lowest amount; id sum; highest amount; amount sum;
/// name bytes; how many of its records repeat a name; and note bytes. That order keeps sums apart, or
/// the compiler pairs their additions up as vector instructions, which take more than the two would.
/// There are 256 rows, for every country index a byte can hold, so that indexing needs no check.
#[repr(align(64))]
struct Row([i64; 8]);
const EMPTY_ROW: Row = Row([0, i64::MAX, 0, i64::MIN, 0, 0, 0, 0]);

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
    let planes = black_box(&PLANES);
    // SAFETY: the blocks are inside the text, or the padding covers them. PCLMULQDQ is in the
    // build's target features. The carry-less multiply by all ones gives each bit the parity of the
    // quotes up to it, and that's odd exactly inside quotes, since a `""` pair cancels out. The
    // positions stay below `LISTED - 8`, as a chunk has `BLOCKS` blocks.
    unsafe {
        let mut cursor = out.as_mut_ptr();
        let mut base = _mm256_set1_epi32(scan as i32 + 1);
        // A byte's low half picks what the byte would have to be, to be a separator: an LF, a comma,
        // or all ones, which only a byte with the top bit set could be, and those shuffle to zero.
        let lf_comma = 0xFFFF_FF2C_FF0A_FFFFu64 as i64;
        let separator = _mm256_set_epi64x(lf_comma, -1, lf_comma, -1);
        let (quote, ones) = (_mm256_set1_epi8(b'"' as i8), _mm_set1_epi8(-1));
        for block in 0..blocks {
            let at = text.as_ptr().add(scan + block * 64);
            // Far enough ahead to pass where the chunk's own loads and a page's end stop the core.
            _mm_prefetch::<_MM_HINT_T0>(at.wrapping_add(1024).cast());
            let quotes = byte_mask(at, |block| _mm256_cmpeq_epi8(block, quote));
            let parity = _mm_clmulepi64_si128::<0>(_mm_cvtsi64_si128(quotes as i64), ones);
            let inside = _mm_cvtsi128_si64(parity) as u64 ^ *odd;
            *odd = (inside as i64 >> 63) as u64;
            let class = |block| _mm256_shuffle_epi8(separator, block);
            let mut left = byte_mask(at, |block| _mm256_cmpeq_epi8(block, class(block))) & !inside;
            // Blocks hold 5 of them on average, and a loop's exit would be a coin toss, so this
            // writes 8 whatever the number, and moves on by the right amount. The positions of up to
            // 8 set bits come out as 8 bytes at once: `PLANES[k]` has bit `k` of every position at
            // that position, and PEXT gathers it for those bits, into a byte. Those bytes are a bit
            // matrix with a row per bit of a position, and GFNI's multiply, given the columns one at
            // a time, turns it around into a byte per position.
            let matrix = planes.iter().fold(0, |m, &p| m << 8 | _pext_u64(p, left));
            let columns = _mm_cvtsi64_si128(0x8040_2010_0804_0201u64 as i64);
            let rows = _mm_cvtsi64_si128((matrix << 16) as i64);
            let bytes = _mm256_cvtepu8_epi32(_mm_gf2p8affine_epi64_epi8::<0>(columns, rows));
            _mm256_storeu_si256(cursor.cast(), _mm256_add_epi32(bytes, base));
            let total = left.count_ones() as usize;
            if total > 8 {
                for i in 0..total {
                    *cursor.add(i) = _mm256_extract_epi32::<0>(base) as u32 + left.trailing_zeros();
                    left &= left.wrapping_sub(1);
                }
            }
            cursor = cursor.add(total);
            base = _mm256_add_epi32(base, _mm256_set1_epi32(64));
        }
        cursor.offset_from(out.as_ptr()) as usize
    }
}

/// Parses the records in the first `len` bytes of `text`, adding each to its country's row.
fn parse_records(text: &[u8], len: usize, rows: &mut [Row; 256]) {
    // The positions just past commas and line ends outside quotes, after a record's start, so a
    // record's five come after the position it starts at.
    let (mut listed, mut found) = ([0u32; LISTED], [(0, 0, 0); LISTED / 5]);
    // The text starts at a record's start, so outside quotes.
    let (mut count, mut scan, mut odd_so_far) = (1, 0, 0);
    while scan < len {
        let blocks = BLOCKS.min((len - scan).div_ceil(64));
        count += separators(text, scan, blocks, &mut listed[count..], &mut odd_so_far);
        scan += blocks * 64;
        if scan >= len {
            // The last block can run on into the next segment. The file's last record always has its
            // terminator (spec section 2.2), so every record's positions are listed.
            count = listed[..count].partition_point(|&at| at as usize <= len);
        }
        let records = (count - 1) / 5;
        add_records(text, &listed[..records * 5], &mut found, rows);
        // Under 5 positions are left, and the list has 8 free past `count`, so one fixed
        // copy moves them, where a call to copy just the right number would cost more.
        listed.copy_within(records * 5..records * 5 + 8, 0);
        count -= records * 5;
    }
}

/// Adds up a chunk of whole records from `text`, given five positions for each: where it starts, and
/// just past its first four separators. `found` is room for each record's guessed slot, cents, and id
/// and region together.
fn add_records(text: &[u8], listed: &[u32], found: &mut [(u32, u32, u64)], rows: &mut [Row; 256]) {
    let (records, _) = listed.as_chunks::<5>();
    let (words, regions) = (MEMORY.words.as_ptr(), MEMORY.regions.as_ptr());
    for (positions, slot) in records.iter().zip(found.iter_mut()) {
        let [mut start, name_at, note_at, region_at, amount_at] = positions.map(|at| at as usize);
        // SAFETY: every position is inside the text, or just after it, so the padding covers the
        // loads. `region` stays inside the join table, and `home` 32 below the end of the tags.
        // Prefetching has no effect on memory.
        unsafe {
            // A byte order mark can only start a record here, and no record otherwise starts with it.
            // The black box keeps it a branch: one record in 4,000.
            (*text.get_unchecked(start) == 0xEF).then(|| start = black_box(start + 3));
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
            let (home, tag) = home_and_tag(text, name_at + 1, (note_at - name_at).wrapping_sub(3));
            // Guesses the name's slot from the tags, and asks to own its line, for a write: a locked
            // update then needs no second trip to take it from the core that wrote it last. A wrong
            // guess costs a search. A name's tag is the low 16 bits of its hash, or 1 for zero, which
            // means free; here a zero finds a free slot, which is a wrong guess too, and costs one
            // name in 65,536 less than a check.
            let window = std::ptr::read_unaligned(MEMORY.tags.as_ptr().add(home).cast());
            let hits = slots_with(window, tag);
            let guess = (home + hits.trailing_zeros() as usize) * 64;
            _mm_prefetch::<_MM_HINT_ET0>(words.byte_add(guess).cast());
            let cents = (cents as i32 ^ negative) - negative;
            *slot = (guess as u32, cents as u32, id_region);
        }
    }
    for (positions, &(guess, cents, id_region)) in records.iter().zip(found.iter()) {
        let [_, name_at, note_at, region_at, _] = positions.map(|at| at as usize);
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
        let name = (note_at - name_at - 1) as u32 as usize;
        let both = region_at - name_at - 1;
        // SAFETY: `name_at` is inside the text, and the padding covers the mask. BZHI keeps every bit
        // for a count of 64.
        let (name_quotes, quotes) = if both <= 64 {
            let is = |block| unsafe { _mm256_cmpeq_epi8(block, _mm256_set1_epi8(b'"' as i8)) };
            let mask = unsafe { byte_mask(text.as_ptr().add(name_at), is) };
            let below = |bits| unsafe { _bzhi_u64(mask, bits as u32) }.count_ones() as usize;
            (below(name), below(both))
        } else {
            let count = |to: usize| text[name_at..][..to].iter().filter(|&&c| c == b'"').count();
            (count(name), count(both))
        };
        // A value is its field, less its delimiting quotes, less one for each `""`, with empty
        // fields of no quotes or two both coming to nothing.
        row[5] += (name - name_quotes / 2).saturating_sub(1) as i64;
        row[7] += (both - name - 1 - (quotes - name_quotes) / 2).saturating_sub(1) as i64;
        let (start, len) = (name_at + 1, name.wrapping_sub(2));
        // SAFETY: `guess` came from the first pass, and `find` returns a slot too. The name starts
        // inside the text. A slot's 256 bits start 4 words in, and a byte can't index past them.
        unsafe {
            let at = if len <= INLINE_BYTES && holds(guess as usize, text, start, len) {
                guess as usize
            } else if let Some(at) = find(text, start, name.saturating_sub(2)) {
                at
            } else {
                let name = text[start..start + name.saturating_sub(2)].to_vec();
                row[6] += i64::from(!LOCKED.lock().unwrap().1.insert((name, country)));
                continue;
            };
            // Nine records in ten set a new bit, so a plain load first would rarely save the locked
            // write, and the prefetch has already asked to own the line. BTS finds the word itself, and
            // leaves the old bit in the carry, which adds one to the row's repeats if it had been set.
            asm!("lock bts [{} + {} + 32], {}", "adc qword ptr [{} + 48], 0",
                in(reg) words, in(reg) at, in(reg) usize::from(country), in(reg) row.as_mut_ptr());
        }
    }
}

/// Runs a closure, from a new thread's start.
unsafe extern "C" fn start(work: *mut u8) -> *mut u8 {
    // SAFETY: `main` passes a closure that outlives the thread.
    unsafe { (*work.cast::<&(dyn Fn() + Sync)>())() };
    null_mut()
}

/// C's entry point, not std's: std's would read `/proc/self/maps` and install handlers first, none of
/// which this program needs, and the timer counts all of it.
#[unsafe(no_mangle)]
extern "C" fn main() -> i32 {
    let arg = |at, default: &str| std::env::args().nth(at).unwrap_or_else(|| default.into());
    // `SD_THREADS` if set (the test sweep uses it), else what the container allows, capped at the
    // box's 12 threads.
    let allowed = thread::available_parallelism().map_or(1, |n| n.get().min(12));
    let threads = std::env::var("SD_THREADS").map_or(allowed, |v| v.parse().unwrap_or(allowed));
    let open = |name: &str| map(&File::open(arg(1, "/data") + "/" + name + ".csv").unwrap());
    let (records, lookup) = (open("records"), open("regions"));

    // The threads parse the whole file and merge their rows, every index's row, index 0 being
    // `_UNMATCHED`. They share the regions too, and all of those are done before any record is parsed.
    // Tying the part sizes to the thread count also makes the SD_THREADS test sweep move every cut.
    let bytes = (offset_of!(Memory, stacks) + (threads - 1) * STACK).next_multiple_of(1 << 21);
    // Whole huge pages only, even for the last part: small pages cost more time than the rest of that
    // huge page costs in memory. The size of `MEMORY` is a multiple of them, so this stays inside it.
    // SAFETY: this is only advice about pages of `MEMORY`.
    unsafe { madvise((&raw const MEMORY).cast(), bytes, 23) };
    LOCKED.lock().unwrap().0.reserve(256);
    let (lines, segments, line_parts) = (AtomicUsize::new(0), AtomicUsize::new(0), threads * 8);
    let (parsed, totals) = (AtomicUsize::new(0), Mutex::new([EMPTY_ROW; 256]));
    let released = AtomicUsize::new(0);
    let work = || {
        while let Some((pos, end)) = part(lookup, &lines, line_parts, b"\n") {
            parse_regions(&lookup[pos..], end - pos);
            parsed.fetch_add(1, Release);
        }
        // Not a barrier: nobody should wait for a thread that hasn't started, or be woken up.
        while parsed.load(Acquire) < line_parts {
            thread::yield_now();
        }
        let mut rows = [EMPTY_ROW; 256];
        // 12 segments per thread: when they run out, each thread still has its last to finish, and
        // the threads done first sit idle. Fine ones keep that short, and cost no shootdown.
        while let Some((pos, end)) = part(records, &segments, threads * 12, b"\r\n") {
            parse_records(&records[pos..], end - pos, &mut rows);
        }
        // Parsing is over, so each release can only stop threads still on their last segment, which a
        // release per segment would do all run. Threads share the releases: the exit would do them on
        // one. Any pages still in use just map again.
        let chunk = records.len().div_ceil(threads * 3).next_multiple_of(4096);
        let mut from = released.fetch_add(chunk, Relaxed);
        while from < records.len() {
            let len = chunk.min(records.len() - from);
            // SAFETY: only read pages go, and a later read maps them again. 4 is `MADV_DONTNEED`.
            unsafe { madvise(records.as_ptr().add(from), len, 4) };
            from = released.fetch_add(chunk, Relaxed);
        }
        for (Row(total), Row(row)) in totals.lock().unwrap().iter_mut().zip(&rows) {
            [0, 2, 4, 5, 6, 7].iter().for_each(|&i| total[i] += row[i]);
            (total[1], total[3]) = (total[1].min(row[1]), total[3].max(row[3]));
        }
    };
    let (work, mut ids): (&(dyn Fn() + Sync), _) = (&work, vec![0u64; threads - 1]);
    // SAFETY: each thread has its own stack, inside `MEMORY`, and is joined before `work` ends.
    unsafe {
        let mut attr = [0u64; 8];
        for (at, id) in ids.iter_mut().enumerate() {
            pthread_attr_init(&mut attr);
            pthread_attr_setstack(&mut attr, MEMORY.stacks[at].as_ptr() as *mut u8, STACK);
            assert!(pthread_create(id, &attr, start, &raw const work as *mut u8) == 0);
        }
        work();
        ids.iter().for_each(|&id| _ = pthread_join(id, null_mut()));
    }
    let totals = totals.into_inner().unwrap();
    let codes = &LOCKED.lock().unwrap().0;

    // Sorted by key, bytewise: the codes in order, then `_UNMATCHED`, because `_` is above `Z`. A
    // row's columns are already in output order, and a group without records gets no line.
    let keys = ["_UNMATCHED"].iter().chain(codes).copied();
    let lines: std::collections::BTreeMap<_, _> = keys.zip(&totals).collect();
    let mut out = BufWriter::new(File::create(arg(2, "/out") + "/aggregates.txt").unwrap());
    for (key, Row(row)) in lines.into_iter().filter(|(_, row)| row.0[0] != 0) {
        write!(out, "{key} {} {}", row[0], row[2]).unwrap();
        // Printed as `W.CC`, with the sign apart, because integer division truncates toward zero,
        // which would lose the sign of -0.75. Zero never gets a sign.
        for cents in [&row[4], &row[1], &row[3]] {
            let (sign, magnitude) = (if *cents < 0 { "-" } else { "" }, cents.unsigned_abs());
            write!(out, " {sign}{}.{:02}", magnitude / 100, magnitude % 100).unwrap();
        }
        writeln!(out, " {} {} {}", row[0] - row[6], row[5], row[7]).unwrap();
    }
    out.flush().unwrap();
    // Not a return. The handlers that `exit` runs write a byte at the start of the zeroed part of the
    // image, which is a whole 2 MiB extent with the alignment of `MEMORY`, so a huge page of its own.
    // SAFETY: the output is flushed, and nothing else needs cleaning up.
    unsafe { _exit(0) }
}
