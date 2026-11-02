//! Track A (Ingest): per-hour, per-endpoint latency percentiles over an HTTP request log.
//!
//! The log is cut into segments that threads claim from an atomic cursor, so a thread that falls behind
//! simply claims fewer, and each thread reads a segment in small pieces into its own buffer, which stays
//! in its caches and, once touched, costs no page faults. Each line is read at fixed offsets where the
//! format allows it, and with block loads where it doesn't. One lock-free table, shared by all threads,
//! turns endpoints into ids, and each line becomes one 32-bit record in a shared array for its hour,
//! where each thread reserves small blocks. The log runs in rough time order, so a few threads finish
//! hours while the others still read: once the reading is well past where a thread last came to an
//! hour, one of those few claims it, groups its array with three stable radix passes, and formats it
//! over its own records. Whichever of them can then appends the ready hours to the file, in order, and
//! frees them. So few records are in memory at any time, output never sits in memory whole, and all
//! threads stay busy until the reading ends, when those few finish what's left. No bound on the disorder
//! is promised, so a line that comes after its hour was claimed stops the round, and a new one starts
//! with no hour due until the log is done.

// `main` below is the C entry point. T counts from exec, and before its own `main` std reads
// /proc/self/maps and installs signal handlers, and then gives each thread a signal stack, all for a
// stack overflow message this program doesn't need. `std::env::args` still works, through glibc's
// init array.
#![no_main]

use std::arch::x86_64::*;
use std::sync::atomic::{AtomicU8, AtomicU16, AtomicU32, AtomicU64, AtomicUsize, Ordering::*};
use std::{fs::File, io, io::Write, mem::replace, mem::transmute, os::fd::AsRawFd, path::Path};
use std::{slice::from_raw_parts, slice::from_raw_parts_mut, sync::Arc, sync::Mutex, thread};

/// Every seed's log spans exactly 2026-10-30T18:00Z to 2026-11-02T17:59Z (spec section 2).
const HOURS: usize = 72;
const DATES: [&str; 4] = ["2026-10-30", "2026-10-31", "2026-11-01", "2026-11-02"];

/// A record is one request, from the top: 12 bits of endpoint id, 19 of latency, and the error flag,
/// so the id is the record shifted down by `ID_SHIFT`. Plain integer order sorts one endpoint's records
/// by latency. About 3% of latencies don't fit, so their records hold `BIG_LATENCY`, and the real value
/// goes at the far end of the hour's array.
const BIG_LATENCY: u32 = (1 << 19) - 1;
const ID_SHIFT: u32 = 20;

/// Words a thread reserves at a time from an hour's array, and writes without contention: records
/// from the front, a page at a time, and big latencies, two words each, from the back.
const BLOCK: usize = 1024;
const BIG_BLOCK: usize = 32;

/// Bytes of the log in a segment. Threads faulting in the same large folio of the page cache sleep on
/// its lock, so they stay out of each other's folios.
const SEGMENT: usize = 1 << 20;

/// Bytes of the log that a thread takes out of the address space at a time, at the end.
const RELEASE: usize = 1 << 23;

/// Spare bytes after the log, so that a load at a short fixed distance from a line needs no check of
/// its own. Whatever a line holds, the farthest load ends 185 bytes after its start: at most 44 to the
/// endpoint, two blocks to find its closing space, and 13 past that for the latency's load.
const PADDING: usize = 192;

/// Room for the spec's exactly 1,842 endpoints, at a low load.
const SLOTS: usize = 1 << 14;

/// Ids that a record can hold: the spec's endpoints have at most 2 lines each. As a key's first line is
/// its id, this is also how many lines the table has.
const IDS: usize = 1 << 12;

/// A method's length is the nibble of this selected by the low nibble of its 4th byte, which tells the
/// seven apart: GET and PUT have a space there (0), POST and HEAD have T and D (4), PATCH has C (3),
/// DELETE has E (5), and OPTIONS has I (9).
const METHOD_LENGTHS: u64 = 0x70_0064_5003;

/// The lock-free endpoint table, and what the threads share about the hours, here to spare `parse` a
/// fifth argument. `repr(C)` keeps the fields in the order they were timed in, as that decides which of
/// them share a cache line.
#[repr(C)]
struct Table {
    /// The keys. A key has whole lines, through its closing space, so a block load of a published key
    /// meets no other thread's stores, and a compare touches one cache line. A key's first line is its
    /// id.
    lines: &'static [[AtomicU8; 64]],
    /// Each holds its key's first line, so 0 means empty, as line 0 is never used, and a published slot
    /// never changes.
    slots: [AtomicU16; SLOTS],
    /// Lines reserved so far.
    used: AtomicUsize,
    /// One more than the log positions where threads first and last came to each hour, the last 0 if
    /// none has and `usize::MAX` once the hour is claimed. Once a line came too late for its hour, all
    /// are `[0, usize::MAX]`. `panic = "abort"` means no thread dies holding a lock, so the `expect` on a
    /// lock never fires.
    spans: Mutex<[[usize; 2]; HOURS]>,
    /// The latest order of the ids, kept for the hours to come.
    order: Mutex<Arc<Order>>,
}

/// One hour's words. All the arrays share one mapping, and each is whole pages, because a mapping per
/// hour costs a call each, and unmapping one stalls every thread that wants the address space's lock.
#[derive(Default)]
struct Hour {
    /// An array that threads reserve blocks of. It has room for every line of the log, since a log may
    /// be lopsided, and untouched pages cost nothing. A block's unused end stays zero, which no real
    /// record is, since line 0 is never an id.
    words: &'static [AtomicU32],
    /// Words reserved at the front, for records: where the next block of records starts.
    front: AtomicUsize,
    /// Words reserved at the back, for big latencies, each as two words (id << 32 | latency) in the
    /// array's byte order.
    back: AtomicUsize,
    /// One more than the length of the hour's text, once that's ready.
    ready: AtomicUsize,
}

/// The endpoints in use and their ids, in the byte order of the endpoints (`[u8]` compares as unsigned
/// bytes, with a proper prefix first), after id 0, which no endpoint has: it's what the unused ends of
/// blocks hold.
type Order = (Vec<&'static [u8]>, Vec<usize>);

// Declared by hand to keep the crate at zero dependencies; std already links glibc.
unsafe extern "C" {
    fn prctl(option: i32, a: u64, b: u64, c: u64, d: u64) -> i32;
    fn mmap(addr: usize, len: usize, prot: i32, flags: i32, fd: i32, offset: i64) -> *mut u8;
    fn madvise(addr: *mut u8, len: usize, advice: i32) -> i32;
}

// Their arguments, as Linux on x86-64 defines them.
/// `MAP_PRIVATE | MAP_ANONYMOUS | MAP_NORESERVE`.
const MAP_SPARSE: i32 = 0x02 | 0x20 | 0x4000;
const MADV_DONTNEED: i32 = 4;

/// Loads a `T` at element `pos`, little-endian and with no alignment. It's a `u8`, a `u64` for 8 bytes,
/// or two AVX2 registers for a block of 64, from the text or from a line of the table.
///
/// # Safety
/// The loaded bytes must be inside the buffer that `text` is part of. They may be past `text`'s own
/// end: a key, a short slice of a piece, loads two whole blocks.
unsafe fn load<T: Copy>(text: &[impl Sized], pos: usize) -> T {
    // SAFETY: the caller keeps the load inside the buffer, and the load needs no alignment.
    unsafe { text.as_ptr().add(pos).cast::<T>().read_unaligned() }
}

/// A call to this marks the branch it's on as rare, so that the per-line path gets the registers and the
/// straight run of code.
#[cold]
fn cold() {}

/// One bit per byte of two blocks, set where they're equal.
fn equal_bytes(a: [__m256i; 2], b: [__m256i; 2]) -> u64 {
    // SAFETY: the build targets x86-64-v3, which has AVX2.
    let mask = |i: usize| unsafe { _mm256_movemask_epi8(_mm256_cmpeq_epi8(a[i], b[i])) as u32 };
    u64::from(mask(0)) | u64::from(mask(1)) << 32
}

/// Mixes an endpoint's length with its first and last 8 bytes, which tell nearly all templated paths
/// apart, with one multiply, whose high bits depend on every input bit. Short keys hash by their length
/// alone: a load would hold more than the key, and there are very few of them. A test build swaps the
/// mix for a constant, so that every key collides and any logic that trusts a hash without comparing
/// the key shows up as a wrong answer.
fn hash(first: u64, last: u64, len: usize) -> u64 {
    let mix = (first ^ last.rotate_left(29)) * u64::from(len >= 8) ^ len as u64;
    // The constant is 2^64 over the golden ratio, as in Fibonacci hashing.
    (if cfg!(sd_const_hash) { 0 } else { mix }).wrapping_mul(0x9E37_79B9_7F4A_7C15)
}

/// The id of `key`, which ends in its closing space, from slot `index` on, adding it if it's new: the
/// path for the lines that `parse` can't settle in one compare. `mask` marks the key's bytes among the
/// first 64. A key is always compared in full, because templated paths share prefixes, suffixes and
/// lengths.
#[cold]
fn id_of(table: &Table, key: &[u8], mask: u64, mut index: usize) -> u32 {
    // SAFETY: `key` is inside a buffer, whose padding covers the loads.
    let (block, second) = unsafe { (load(key, 0), load(key, 64)) };
    // The key's bytes in the second block: at most 62, as it has at most 126 with its space.
    let rest = (1u64 << key.len().saturating_sub(64)) - 1;
    // SAFETY: a slot holds a line inside `lines`, and its bytes were stored
    // before it was published. The line after a key's last is inside `lines` too, as a key is given
    // room for 2. Another thread may be storing that line right now, which makes this read a race
    // that x86 tolerates, and nothing depends on what it reads: `rest` masks it off for a key of one
    // line, and a longer key's first compare fails unless the slot's key has a second line too.
    let differ = |at: usize, b, m: u64| !equal_bytes(unsafe { load(table.lines, at) }, b) & m;
    loop {
        let slot = &table.slots[index & (SLOTS - 1)];
        if slot.load(Acquire) == 0 {
            // A key takes at most 2 lines.
            let at = table.used.fetch_add(key.len().div_ceil(64), Relaxed);
            assert!(at + 2 <= IDS, "more keys than a record's id can hold");
            let cells = table.lines[at..].as_flattened().iter();
            cells.zip(key).for_each(|(c, &b)| c.store(b, Relaxed));
            // If another thread's key took the slot first, the lines reserved here stay unused, and
            // the compare below tells whose key is in the slot.
            _ = slot.compare_exchange(0, at as u16, Release, Relaxed);
        }
        let line = slot.load(Acquire) as usize;
        if differ(line, block, mask) | differ(line + 1, second, rest) == 0 {
            return line as u32;
        }
        index += 1;
    }
}

/// Hours since the start of the span, from bytes 6 to 13 of a line, M-DDTHH:, without parsing a date.
/// Oct 30, Oct 31, Nov 1 and Nov 2 differ in the last digit of the month and of the day:
/// (month & 1) + (day & 3) gives 0, 1, 2 and 3. One multiply adds the four masked bytes into the top
/// byte, weighted 24, 24, 10 and 1, and no partial product below that byte can carry into it. The span
/// starts at 18:00, hence the 18.
fn hour_index(stamp: u64) -> usize {
    ((stamp & 0x000F_0F00_0300_0001).wrapping_mul(0x1800_0018_000A_0100) >> 56) as usize - 18
}

/// Reads the latency at `pos`, and returns it with the number of digits. Digits have bit 4 set and an
/// LF doesn't, so one load finds the end of up to 8 digits, and three vector multiplies parse them.
/// That leaves only the rarest latencies, those with more than 8 digits, to the loop.
///
/// # Safety
/// `pos + 8` must be at most `text.len()`.
unsafe fn parse_latency(text: &[u8], pos: usize) -> (u32, usize) {
    // SAFETY: the caller vouches for this load, and the loop below uses checked indexing.
    let word = unsafe { load::<u64>(text, pos) };
    let mut digits = (!word & 0x1010_1010_1010_1010).trailing_zeros() as usize / 8;
    // SAFETY: the build targets x86-64-v3, which has SSE4.1.
    let mut value = unsafe {
        let aligned = _mm_cvtsi64_si128((word << (64 - 8 * digits)) as i64);
        let bytes = _mm_subs_epu8(aligned, _mm_set1_epi8(48));
        let pairs = _mm_maddubs_epi16(bytes, _mm_set1_epi16(10 + (1 << 8)));
        let quads = _mm_madd_epi16(pairs, _mm_set1_epi32(100 + (1 << 16)));
        let eight = _mm_packus_epi32(quads, quads);
        _mm_cvtsi128_si32(_mm_madd_epi16(eight, _mm_set1_epi32(10000 + (1 << 16)))) as u64
    };
    while digits >= 8 && text[pos + digits] != b'\n' {
        value = value * 10 + u64::from(text[pos + digits] - b'0');
        digits += 1;
    }
    (value as u32, digits)
}

/// Reads the segments that `next` gives. Each line is `ts method endpoint status latency`, where the
/// timestamp is always 27 bytes. A thread keeps its own unfilled blocks of each hour, so the hot path
/// reads and writes no line that other threads write, and allocates nothing.
fn parse(text: &[u8], table: &Table, hours: &[Hour], mut next: impl FnMut() -> Option<[usize; 2]>) {
    // This thread's place in each hour's block of records. An aligned place, as a null is, means the
    // block is used up.
    let mut blocks = [std::ptr::null::<AtomicU32>(); HOURS];
    // Its place in each hour's block of big latencies, then the last line's hour: its index and its
    // place in a block, and in a segment, its stamp. Hours change only between chunks of the log, so
    // most lines keep to these.
    let (mut bigs, mut hour, mut place) = ([std::ptr::null::<AtomicU64>(); HOURS], 0, blocks[0]);
    // A block of spaces, to find where an endpoint ends.
    // SAFETY: the build targets x86-64-v3, which has AVX2.
    let gap = unsafe { [_mm256_set1_epi8(b' ' as i8); 2] };
    while let Some([start, end]) = next() {
        // A line belongs to the segment holding its first byte, so a segment after the first starts a
        // byte early, and skips the line already started. The log ends in LF, so there's an LF to find.
        let mut last_stamp = 0;
        let skip = text[start.max(1) - 1..].iter().position(|&b| b == b'\n');
        let mut line = if start > 0 { start + skip.unwrap() } else { 0 };
        while line < end {
            // Prefetch past the page's end, where the L2 streamer stops.
            // SAFETY: a prefetch needs no valid address, and never faults.
            unsafe { _mm_prefetch::<_MM_HINT_T0>(text.as_ptr().wrapping_add(line + 1536).cast()) };
            // Reads at a fixed distance from `line` need no check, since `line` is inside the log
            // and the padding is longer than any such distance, and whatever they read is safe.
            // The hour's bytes of the timestamp, and the method's 4th byte, whose low nibble picks
            // its length.
            // SAFETY: both loads end at most 32 bytes after `line`.
            let stamp: u64 = unsafe { load(text, line + 6) };
            let method: u8 = unsafe { load(text, line + 31) };
            if stamp != last_stamp {
                cold();
                // The old hour's place goes back, and the new hour's comes out.
                (blocks[hour], last_stamp, hour) = (place, stamp, hour_index(stamp));
                place = blocks[hour];
                // Before any claim of this hour, this shows the claimer that this segment holds it
                // up, as its position is at or past the low mark. After one, it's too late, which
                // ends the round.
                let mut spans = table.spans.lock().expect("poisoned");
                if spans[hour][1] == usize::MAX {
                    spans.fill([0, usize::MAX]);
                    return;
                }
                spans[hour] = [spans[hour][0].min(line + 1), spans[hour][1].max(line + 1)];
            }
            // The endpoint starts after the timestamp, the method and their two spaces.
            let at = line + 29 + (METHOD_LENGTHS >> 4 * (method & 15) & 15) as usize;
            // SAFETY: the method's length is at most 15, so this ends within 108 bytes of `line`.
            let key = unsafe { load(text, at) };
            let spaces = equal_bytes(key, gap);
            // Endpoints average 34 bytes, and 94% of them end within 64, so the second block is
            // read only when the first has no space: there, the first's trailing zeros are 64.
            let mut len = spaces.trailing_zeros() as usize;
            if spaces == 0 {
                // SAFETY: the padding covers it.
                len += unsafe { equal_bytes(load(text, at + 64), gap) }.trailing_zeros() as usize;
            }
            // Whatever the line holds, the closing space, `close`, is at most 172 bytes after
            // `line`, so the loads of its status and latency end at most 13 bytes later, inside
            // the padding.
            let close = at + len;
            // Three status digits sit between two spaces, so the latency starts 5 bytes on.
            // SAFETY: as above.
            let (mut latency, digits) = unsafe { parse_latency(text, close + 5) };
            // SAFETY: `at` is at least 29, so `close - 8` is inside the buffer, and both loads end
            // inside the padding, as above.
            let (first, last) = unsafe { (load(text, at), load(text, close - 8)) };
            // The hash's top bits pick the slot.
            let index = (hash(first, last, len) >> (u64::BITS - SLOTS.ilog2())) as usize;
            // One compare of a whole line of the table, through the closing space, settles 92% of
            // the log's lines, with no branch on the length of a key that fits. An empty slot
            // holds 0, and line 0 is all zeros, so it can't match.
            let kept = table.slots[index].load(Acquire) as usize;
            // SAFETY: a slot holds a line inside the table's lines, and its
            // bytes were stored before it was published.
            let seen = unsafe { load(table.lines, kept) };
            // The bits up to and including the lowest one set: the key's bytes through its space, or
            // all 64 if it has no space in this block, which then leaves it to `id_of`.
            let mask = spaces ^ spaces.wrapping_sub(1);
            let miss = || id_of(table, &text[at..=close], mask, index);
            let hit = !equal_bytes(seen, key) & mask == 0 && spaces != 0;
            let id = if hit { kept as u32 } else { miss() };
            if latency >= BIG_LATENCY {
                // A fresh block of big latencies, from the back of the hour's array.
                if bigs[hour] as usize % (BIG_BLOCK * 4) == 0 {
                    let Hour { words, back, .. } = &hours[hour];
                    let taken = back.fetch_add(BIG_BLOCK, Relaxed) + BIG_BLOCK;
                    bigs[hour] = words[words.len() - taken..].as_ptr().cast();
                }
                // SAFETY: as for `place` below. The array is page-aligned, and blocks hold even
                // words.
                unsafe { (*bigs[hour]).store(u64::from(id) << 32 | latency as u64, Relaxed) };
                bigs[hour] = bigs[hour].wrapping_add(1);
                latency = BIG_LATENCY;
            }
            // A fresh block of records, from the front.
            if place as usize % (BLOCK * 4) == 0 {
                place = hours[hour].words[hours[hour].front.fetch_add(BLOCK, Relaxed)..].as_ptr();
            }
            // Statuses are 100 to 599.
            // SAFETY: as above, for `close`.
            let err = u32::from(unsafe { load::<u8>(text, close + 1) } == b'5');
            // SAFETY: `place` is inside a block this thread reserved, until it's aligned again.
            unsafe { (*place).store(id << ID_SHIFT | latency << 1 | err, Relaxed) };
            place = place.wrapping_add(1);
            line = close + 6 + digits;
        }
    }
}

/// The 8 decimal digits of each of four values below 10^8, as bytes with the most significant first, and
/// with no ASCII offset yet. Each step splits every number in its lane in two, in parallel, by multiplying
/// by a scaled reciprocal: 10^8 into two of 10^4, those into 10^2, and those into digits. The
/// reciprocals are 2^45 / 10^4, 2^20 / 100 and 2^10 / 10, rounded up.
fn eight_digits(values: [u64; 4]) -> [u64; 4] {
    // SAFETY: the build targets x86-64-v3, which has AVX2, and both types are 32 bytes of plain data.
    unsafe {
        let value = transmute::<_, __m256i>(values);
        let high = _mm256_srli_epi64(_mm256_mul_epu32(value, _mm256_set1_epi64x(0xD1B7_1759)), 45);
        let low = _mm256_sub_epi64(value, _mm256_mul_epu32(high, _mm256_set1_epi64x(10_000)));
        let fours = _mm256_or_si256(high, _mm256_slli_epi64(low, 32));
        let hundreds = _mm256_srli_epi32(_mm256_mullo_epi32(fours, _mm256_set1_epi32(10486)), 20);
        let rest = _mm256_sub_epi32(fours, _mm256_mullo_epi32(hundreds, _mm256_set1_epi32(100)));
        let pairs = _mm256_or_si256(hundreds, _mm256_slli_epi32(rest, 16));
        let tens = _mm256_srli_epi16(_mm256_mullo_epi16(pairs, _mm256_set1_epi16(103)), 10);
        let ones = _mm256_sub_epi16(pairs, _mm256_mullo_epi16(tens, _mm256_set1_epi16(10)));
        transmute(_mm256_or_si256(tens, _mm256_slli_epi16(ones, 8)))
    }
}

/// One stable pass of an LSD radix sort. `at` holds each digit's count, `order` lists the digits that
/// can occur, in the order they should come out, and `d` is a record's digit. The counts become the
/// digits' starting places, then each record goes to its digit's next place, so `at` ends up holding
/// where each digit's records end.
fn pass(from: &[u32], to: &mut [u32], at: &mut [u32], order: &[usize], d: impl Fn(u32) -> usize) {
    order.iter().fold(0, |t, &d| t + replace(&mut at[d], t));
    for &record in from {
        let place = &mut at[d(record)];
        // SAFETY: the counts are of these same records, each digit is in `order`, and `to` is as long
        // as `from`.
        unsafe { *to.get_unchecked_mut(*place as usize) = record };
        *place += 1;
    }
}

/// Appends hour `h`'s output lines to `to`, given its records sorted by endpoint and then latency, where
/// each id's group ends, its big latencies sorted by id and then latency, and the endpoints and ids in
/// order.
fn format_hour(h: usize, recs: &[u32], ends: &[u32], big: &[u64], order: &Order, to: &mut Vec<u8>) {
    let stamp = format!("{}T{:02}:00:00Z ", DATES[(h + 18) / 24], (h + 18) % 24);
    // Each group starts where the one before it ends, and id 0's holds the unused ends of blocks.
    let mut start = ends[0] as usize;
    for (key, &id) in order.0.iter().zip(&order.1).skip(1) {
        let group = &recs[replace(&mut start, ends[id] as usize)..ends[id] as usize];
        // An endpoint with no line in this hour prints nothing.
        let n @ 1.. = group.len() else { continue };
        let errors = group.iter().map(|r| r & 1).sum::<u32>() as u64;
        // Nearest rank, as the spec defines it. A group's big latencies are its last records, as they
        // sort highest, so a rank among them reads the real value from `big`, as many places from the end
        // of the id's as it is from the end of the group.
        let pct = |p: usize| {
            let rank = (p * n + 99) / 100;
            let latency = group[rank - 1] >> 1 & BIG_LATENCY;
            if latency < BIG_LATENCY {
                return u64::from(latency);
            }
            big[big.partition_point(|e| e >> 32 <= id as u64) - (n + 1 - rank)] & 0xFFFF_FFFF
        };
        let (p50, p95, p99) = (pct(50), pct(95), pct(99));
        let rate = errors * 1_000_000 / n as u64;
        // The digits of the six numbers, in the order they print.
        let [a, b, c, d] = eight_digits([n as u64, p50, p95, p99]);
        let [e, f, ..] = eight_digits([errors, rate, 0, 0]);
        // SAFETY: the hour's array has room for 256 bytes past any line. A key is in the table's lines,
        // followed by its closing space, and a copy of fixed size has no branch on its length. The stamp
        // is 21 bytes with its space, and the 22 is that and the key's closing space.
        unsafe {
            let mut at = to.as_mut_ptr().add(to.len());
            at.copy_from_nonoverlapping(stamp.as_ptr(), 21);
            at.add(21).copy_from_nonoverlapping(key.as_ptr(), 128);
            at = at.add(22 + key.len());
            let numbers = at;
            for word in [a, b, c, d, e, f] {
                let zeros = (word.trailing_zeros() as usize / 8).min(7);
                std::ptr::write_unaligned(at.cast(), (word | 0x3030_3030_3030_3030) >> (8 * zeros));
                at.add(8 - zeros).write(b' ');
                at = at.add(9 - zeros);
            }
            at.sub(1).write(b'\n');
            // A latency of 10^8 or more is rare, and isn't printed by the above: the numbers start again.
            let end = if p99 < 100_000_000 { at } else { numbers };
            to.set_len(end.offset_from(to.as_ptr()) as usize);
        }
        if p99 >= 100_000_000 {
            writeln!(to, "{n} {p50} {p95} {p99} {errors} {rate}").expect("writing to a Vec");
        }
    }
}

/// Finishes hour `h`, which this thread has claimed: sorts its records and formats them over their own
/// words. `digits` lists every value a latency digit can take. It's out of line because, inlined, its
/// values took registers from the per-line loop in `parse`, which cost that loop an instruction a line.
#[inline(never)]
fn write_hour(h: usize, hour: &Hour, table: &Table, digits: &[usize], tmp: &mut Vec<u32>) {
    let words = hour.words;
    // SAFETY: the hour is claimed, so no thread writes to its array any more, and what they wrote
    // is visible, as they had moved past it in `round`'s `reached` by then. The big latencies are an
    // even number of words, 8-aligned, and the unused ones are zero, which sorts before id 1.
    let recs = unsafe { from_raw_parts_mut(words.as_ptr() as *mut u32, hour.front.load(Relaxed)) };
    let tail = &words[words.len() - hour.back.load(Relaxed)..];
    // SAFETY: as above.
    let big = unsafe { from_raw_parts_mut(tail.as_ptr() as *mut u64, tail.len() / 2) };
    // Three stable passes sort by latency first and endpoint last, through one word of scratch per
    // record. Bit 0 is the error flag, so the digits are 9 bits from bit 1, 10 from bit 10, and the id.
    // These arrays start as counts, and `pass` leaves them holding where each digit's records end.
    tmp.resize(recs.len(), 0);
    let (mut low, mut high, mut ends) = ([0u32; 1024], [0u32; 1024], [0u32; IDS]);
    for &record in recs.iter() {
        low[(record >> 1 & 511) as usize] += 1;
        high[(record >> 10 & 1023) as usize] += 1;
        ends[(record >> ID_SHIFT) as usize] += 1;
    }
    // An endpoint that's new since the last order was made means a new order, which is kept for
    // the hours to come. It's made from the table as it is now, so it has this hour's endpoints.
    let mut order = table.order.lock().expect("poisoned").clone();
    if order.1.iter().map(|&id| ends[id] as usize).sum::<usize>() != recs.len() {
        // The table's ids as they are now, in the order of their endpoints.
        // SAFETY: an atomic byte has the layout of a byte, and only published keys are read through
        // this, whose lines never change.
        let text = unsafe { from_raw_parts(table.lines.as_ptr().cast::<u8>(), IDS * 64) };
        let key_of = |line: usize| text[line * 64..].split(|&b| b == b' ').next().unwrap();
        let line = |slot: &AtomicU16| slot.load(Acquire) as usize;
        let ids = table.slots.iter().map(line).filter(|&line| line > 0);
        let mut ids: Vec<_> = ids.map(|l| (key_of(l), l)).chain([(&[][..], 0)]).collect();
        ids.sort_unstable();
        order = Arc::new(ids.into_iter().unzip());
        *table.order.lock().expect("poisoned") = order.clone();
    }
    pass(recs, tmp, &mut low, digits, |r| (r >> 1 & 511) as usize);
    pass(tmp, recs, &mut high, digits, |r| (r >> 10 & 1023) as usize);
    pass(recs, tmp, &mut ends, &order.1, |r| r as usize >> ID_SHIFT);
    big.sort_unstable();
    // SAFETY: the records are done with, and the whole array is ours. It has room for any text, so
    // this vector never reallocates, and it's leaked rather than dropped.
    let mut text = unsafe { Vec::from_raw_parts(recs.as_mut_ptr().cast(), 0, words.len() * 4) };
    format_hour(h, tmp, &ends, big, &order, &mut text);
    hour.ready.store(text.leak().len() + 1, Release);
}

/// Appends the ready hours, in order, and frees them: `next` is the hour to write next. A thread that
/// finds this busy leaves its hour to the thread in here, or to the next to finish one, or in the end
/// to `round` itself.
fn append(hours: &[Hour], next: &Mutex<usize>, mut file: &File) {
    let Ok(mut n) = next.try_lock() else { return };
    while let Some(h) = hours.get(*n).filter(|h| h.ready.load(Acquire) > 0) {
        // SAFETY: the text is whole, as its length is out, and nothing else refers to it.
        let text = unsafe { from_raw_parts(h.words.as_ptr().cast(), h.ready.load(Relaxed) - 1) };
        // `panic = "abort"`: a failed write ends the process.
        file.write_all(text).expect("writing the summary");
        // SAFETY: nothing else refers to these whole pages, and nothing reads them again. A failure
        // would only keep them in memory.
        unsafe { madvise(text.as_ptr().cast_mut(), h.words.len() * 4, MADV_DONTNEED) };
        *n += 1;
    }
}

/// One round over the log, in which an hour is due when the reading is past where a thread last came
/// to it by `margin` bytes. Says whether it got to the end, as a line that comes too late stops
/// it.
fn round(log: &[u8], out: &Path, threads: usize, margin: usize) -> bool {
    let file = File::create(out.join("summary.txt")).expect("summary.txt");
    let end = log.len() - PADDING;
    // Each hour's room, in words, as whole pages. Enough for every line: the shortest has 39 bytes, and
    // one with a big latency has at least 45 and takes 3 words. Plus each thread's unfinished blocks,
    // and 1 MiB for text, as an hour has one line per endpoint, of about 200 bytes at most.
    let room = (end / 15 + (BLOCK + BIG_BLOCK) * (threads + 1) + (1 << 18)).next_multiple_of(BLOCK);
    // The hours' words, then the table's lines. Nearly all stay untouched, hence MAP_NORESERVE.
    // SAFETY: this maps fresh zero pages, readable and writable (1 | 2), for the life of the process.
    let base = unsafe { mmap(0, HOURS * room * 4 + IDS * 64, 1 | 2, MAP_SPARSE, -1, 0) };
    assert_ne!(base as isize, -1, "mmap: {}", io::Error::last_os_error());
    // SAFETY: the mapping holds `HOURS * room` zero words, reached only through `hours`.
    let arena = unsafe { from_raw_parts(base.cast::<AtomicU32>(), HOURS * room) }.chunks(room);
    let hours = Vec::from_iter(arena.map(|words| Hour {
        words,
        ..Default::default()
    }));
    let table = Table {
        // SAFETY: after the hours' words come the lines of zeros, page-aligned, reached only from here.
        lines: unsafe { from_raw_parts(base.add(HOURS * room * 4).cast(), IDS) },
        slots: [const { AtomicU16::new(0) }; SLOTS],
        used: AtomicUsize::new(1),
        spans: Mutex::new([[usize::MAX, 0]; HOURS]),
        order: Mutex::default(),
    };
    let digits: Vec<usize> = (0..1024).collect();
    let (cursor, next_out, released) = (AtomicUsize::new(0), Mutex::new(0), AtomicUsize::new(0));
    // A lower bound on the log position of any line each thread may yet read.
    let reached: Vec<_> = (0..threads).map(|_| AtomicUsize::new(usize::MAX)).collect();
    // What each thread does: finish an hour whenever one is due, if it's a finisher, and read.
    let worker = &|me: usize| {
        // The first few threads also finish hours: enough to keep up with the rest, and few, since each
        // one's scratch stays as large as the largest hour it has seen. It has room for 1 MiB of
        // records from the start, in pages that stay untouched until a large hour needs them.
        let finisher = me < 4;
        let mut scratch = Vec::with_capacity(usize::from(finisher) << 18);
        let next_segment = || loop {
            // Any segment that this thread takes from here on starts at or past this mark.
            reached[me].store(cursor.load(Relaxed), Release);
            // Only finishers take the lock here, as a thread put to sleep on it wakes up late and maybe on
            // another CPU, and its segment holds up the rest.
            let left = finisher && {
                let mut spans = table.spans.lock().expect("poisoned");
                // A thread past the end has no line left: only that lets go of an hour with no line.
                let due = |&[first, last]: &[usize; 2]| {
                    let clear = |r| r >= last + margin || r + SEGMENT + margin < first;
                    let free = |r| r >= end || last > 0 && clear(r);
                    // A segment not yet taken could still hold the hour.
                    last < usize::MAX
                        && cursor.load(Relaxed) >= (last + margin).min(end)
                        && reached.iter().all(|r| free(r.load(Acquire)))
                };
                // An hour with no line yet has 0, and it's due only when all reading is done. Nothing
                // waits for a thread that fell behind: on a shared machine, the others would then sit out
                // its whole turn off the core, and that happens all the time, so a wait here turned a busy
                // box into a 4 times slower run. Its records pile up meanwhile, which costs memory only
                // while the box is busy.
                if let Some(h) = spans.iter().position(due) {
                    spans[h][1] = usize::MAX;
                    drop(spans);
                    // While it writes, this thread holds no hour back.
                    reached[me].store(usize::MAX, Release);
                    write_hour(h, &hours[h], &table, &digits, &mut scratch);
                    append(&hours, &next_out, &file);
                    continue;
                }
                spans.iter().any(|s| s[1] < usize::MAX)
            };
            let start = cursor.fetch_add(SEGMENT, Relaxed);
            // Exactly where this thread is now, which a claim can then trust, even once past the end.
            reached[me].store(start, Release);
            if start < end || !left {
                return (start < end).then_some([start, (start + SEGMENT).min(end)]);
            }
            // The wait gives up its core: a spinning thread would keep the one it waits for off a shared core.
            thread::yield_now();
        };
        parse(log, &table, &hours, next_segment);
        // Exit would take the log's pages out of the address space one by one on the last thread, and
        // the clock counts that, so whichever threads are done share the work first. From the log's
        // start on, as threads still reading are near its end, and a page taken from them just comes back.
        while let Some(at) = Some(released.fetch_add(RELEASE, Relaxed)).filter(|&at| at < end) {
            let page = log[at..].as_ptr().cast_mut();
            // SAFETY: the log's pages come back from the file on the next read, with the same bytes. `at`
            // is a multiple of a page, and a failure would only leave them for exit.
            unsafe { madvise(page, RELEASE.min(end - at), MADV_DONTNEED) };
        }
    };
    // This thread is one of them, so that no more threads than CPUs ever want to run: one that starts
    // on a busy CPU can wait there for a whole tick, and then it holds every hour up.
    thread::scope(|scope| {
        (1..threads).for_each(|me| drop(scope.spawn(move || worker(me))));
        worker(0);
    });
    append(&hours, &next_out, &file);
    table.spans.lock().expect("poisoned")[0][0] > 0
}

/// The C entry point: see `#![no_main]` at the top.
#[unsafe(no_mangle)]
extern "C" fn main() -> i32 {
    // Transparent huge pages would round each hour's sparse array up to 2 MB, so turn them off. Failing
    // is harmless.
    // SAFETY: 41 is `PR_SET_THP_DISABLE`, which only sets a flag on our own process.
    unsafe { prctl(41, 1, 0, 0, 0) };
    let arg = |i, default: &str| std::env::args().nth(i).unwrap_or(default.into());
    // `SD_THREADS` if set (the test sweep uses it), else what the container allows, capped at the
    // box's 12 threads.
    let cap = thread::available_parallelism().map_or(1, |n| n.get().min(12));
    let threads = std::env::var("SD_THREADS").map_or(cap, |v| v.parse().unwrap_or(cap).max(1));
    let log = File::open(Path::new(&arg(1, "/data")).join("requests.log")).expect("requests.log");
    let end = log.metadata().expect("the log's size").len() as usize;
    // SAFETY: fresh zero pages for the padding and past it, then the log over their start. Nothing writes
    // to the log while this runs, as it's on a read-only mount, and only `text` reaches the mapping.
    let text = unsafe {
        // Protection 1 is `PROT_READ`.
        let base = mmap(0, end + PADDING + (1 << 12), 1, MAP_SPARSE, -1, 0);
        // Flags: `MAP_PRIVATE | MAP_FIXED`. If the first call failed, this one does too, harmlessly.
        let mapped = mmap(base as usize, end, 1, 0x02 | 0x10, log.as_raw_fd(), 0);
        assert!(base as isize != -1 && (end == 0 || mapped == base), "mmap");
        from_raw_parts(base, end + PADDING)
    };
    let round = |share| round(text, arg(2, "/out").as_ref(), threads, end / share);
    // A 48th of the log is several times the disorder that the spec's shards make. If it's not enough,
    // the second round has no hour due until the log is done.
    _ = round(48) || round(1);
    0
}
