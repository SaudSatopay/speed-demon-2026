//! Track A (Ingest): per-hour, per-endpoint latency percentiles over an HTTP request log.
//!
//! The log is cut into segments that threads claim from an atomic cursor, so a thread that falls behind
//! simply claims fewer, and each thread reads a segment in small pieces into its own buffer, which stays
//! in its caches and costs no page faults. Each line is read at fixed offsets where the format allows
//! it, and with block loads where it doesn't. One lock-free table, shared by all threads, turns
//! endpoints into ids, and each line becomes one 32-bit record in a shared array for its hour,
//! where each thread reserves small blocks. The log runs in rough time order, so a few threads finish
//! hours while the others still read: once the reading is well past where a thread last came to an
//! hour, a thread claims it, groups its array with three stable radix passes, and formats it over its
//! own records. Whoever can then appends the ready hours to the file, in order, and frees them. So few
//! records are in memory at any time, output never sits in memory whole, and all threads stay busy to
//! the end. No bound on the disorder is promised, so a line that comes after its hour was claimed stops
//! the round, and a new one starts with no hour due until the log is done.

use std::arch::x86_64::*;
use std::slice::{from_raw_parts, from_raw_parts_mut};
use std::sync::atomic::{AtomicU8, AtomicU16, AtomicU32, AtomicU64, AtomicUsize, Ordering::*};
use std::sync::{Arc, Mutex};
use std::{fs::File, io::Write, os::unix::fs::FileExt, path::Path, thread};

/// Every seed's log spans exactly 2026-10-30T18:00Z to 2026-11-02T17:59Z (spec section 2).
const HOURS: usize = 72;
const DATES: [&str; 4] = ["2026-10-30", "2026-10-31", "2026-11-01", "2026-11-02"];

/// A record is one request, from the top: 12 bits of endpoint id, 19 of latency, and the error flag.
/// So plain integer order sorts one endpoint's records by latency. About 3% of latencies don't fit,
/// so their records hold `BIG_LATENCY`, and the real value goes at the far end of the hour's array.
const ID_SHIFT: u32 = 20;
const BIG_LATENCY: u32 = (1 << 19) - 1;

/// Words a thread reserves at a time from an hour's array, and writes without contention: records
/// from the front, and big latencies, two words each, from the back.
const BLOCK: usize = 1024;
const BIG_BLOCK: usize = 32;

/// Bytes per read: few enough to stay in the caches, enough that a read costs little per line.
const PIECE: usize = 1 << 15;

/// Spare bytes after a piece in its buffer, so that a load that starts at a short fixed distance from a
/// line needs no check of its own.
const PADDING: usize = 192;

/// A method's length, as the nibble selected by the low nibble of its 4th byte, which tells the seven
/// apart: GET and PUT have a space there (0), POST and HEAD have T and D (4), PATCH has C (3), DELETE
/// has E (5), and OPTIONS has I (9).
const METHOD_LENS: u64 = 0x70_0064_5003;

/// Room for the spec's exactly 1,842 endpoints, at a low load.
const SLOTS: usize = 1 << 14;

/// Ids that a record can hold: the spec's endpoints have at most 2 lines each.
const IDS: usize = 1 << 12;

/// The lock-free endpoint table: slots, lines used, and lines. A slot holds its key's first line, so 0
/// means empty, as line 0 is never used, and a published slot never changes. A key has whole lines,
/// through its closing space, so no block load meets another thread's stores, and a compare touches
/// one cache line. A key's first line is its id. Then the hours claimed so far, or `usize::MAX` once a
/// line came too late for its hour, and one more than the log position where a thread last came to
/// each hour.
type Table = ([AtomicU16; SLOTS], AtomicUsize, &'static [Line], State);
type State = Mutex<(usize, [usize; HOURS])>;
type Line = [AtomicU8; 64];
type Order<'a> = (&'a [&'a [u8]], &'a [usize]);

// Declared by hand to keep the crate at zero dependencies; std already links glibc.
unsafe extern "C" {
    fn prctl(option: i32, a: u64, b: u64, c: u64, d: u64) -> i32;
    fn mallopt(param: i32, value: i32) -> i32;
    fn mmap(at: *mut u8, len: usize, p: i32, f: i32, fd: i32, o: i64) -> *mut u8;
    fn madvise(at: *mut u8, len: usize, advice: i32) -> i32;
}

/// Loads a `T` at `pos`, little-endian and with no alignment. It's a `u64` for 8 bytes, or two AVX2
/// registers for a block of 64.
///
/// # Safety
/// `pos + size_of::<T>()` must be at most `text.len() + PADDING`, and those bytes must be readable.
unsafe fn load<T: Copy>(text: &[u8], pos: usize) -> T {
    // SAFETY: the caller keeps the load inside the buffer, and the load needs no alignment.
    unsafe { text.as_ptr().add(pos).cast::<T>().read_unaligned() }
}

/// One bit per byte of two blocks, set where they're equal.
fn equal_bytes(a: [__m256i; 2], b: [__m256i; 2]) -> u64 {
    // SAFETY: the build targets x86-64-v3, which has AVX2.
    let mask = |i: usize| unsafe { _mm256_movemask_epi8(_mm256_cmpeq_epi8(a[i], b[i])) as u32 };
    u64::from(mask(0)) | u64::from(mask(1)) << 32
}

/// Mixes an endpoint's length with its first and last 8 bytes, which tell nearly all templated paths
/// apart, with one multiply, whose high bits depend on every input bit. A test build swaps it for a
/// constant, so that every key collides and any logic that trusts a hash without comparing the key
/// shows up as a wrong answer.
fn hash(first: u64, last: u64, len: usize) -> u64 {
    let hash = (first ^ last.rotate_left(29) ^ len as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    if cfg!(sd_const_hash) { 0 } else { hash }
}

/// The id of `key`, which ends in its closing space, from slot `index` on, adding it if it's new: the
/// path for the lines that `parse` can't settle in one compare. `through` marks the key's bytes among
/// the first 64. A key is always compared in full, because templated paths share prefixes, suffixes and
/// lengths.
#[cold]
fn id_of((slots, used, lines, _): &Table, key: &[u8], through: u64, mut index: usize) -> u32 {
    // SAFETY: `key` is inside a buffer, whose padding covers the loads.
    let (block, second) = unsafe { (load(key, 0), load(key, 64)) };
    // The key's bytes in the second block: at most 62, as it has at most 126 with its space.
    let rest = (1u64 << key.len().saturating_sub(64)) - 1;
    // SAFETY: a slot holds a line inside `lines`, aligned as they all are, and its bytes were stored
    // before it was published. Only a key with a second line gets its second line read here.
    let read = |at: usize| unsafe { lines.as_ptr().add(at).cast::<[__m256i; 2]>().read() };
    loop {
        index &= SLOTS - 1;
        let slot = &slots[index];
        let mut line = slot.load(Acquire) as usize;
        if line == 0 {
            let count = key.len().div_ceil(64);
            let at = used.fetch_add(count, Relaxed);
            assert!(at + count <= IDS);
            let cells = lines[at..at + count].as_flattened().iter();
            cells.zip(key).for_each(|(c, &b)| c.store(b, Relaxed));
            // If another thread's key took the slot first, the lines reserved here stay unused.
            match slot.compare_exchange(0, at as u16, Release, Acquire) {
                Ok(_) => return at as u32,
                Err(now) => line = now as usize,
            }
        }
        if !equal_bytes(read(line), block) & through == 0
            && (rest == 0 || !equal_bytes(read(line + 1), second) & rest == 0)
        {
            return line as u32;
        }
        index += 1;
    }
}

/// The ids in use and their endpoints, in the byte order of the endpoints (`[u8]` compares as unsigned
/// bytes, with a proper prefix first), after id 0, which no endpoint has: it's what the unused ends of
/// blocks hold.
fn order_ids((slots, _, lines, _): &Table) -> (Vec<&'static [u8]>, Vec<usize>) {
    // SAFETY: a published key's lines never change, and an atomic byte has the layout of a byte.
    let text = unsafe { from_raw_parts(lines.as_ptr().cast::<u8>(), lines.len() * 64) };
    let key_of = |line: usize| text[line * 64..].split(|&b| b == b' ').next().unwrap();
    let line = |slot: &AtomicU16| slot.load(Acquire) as usize;
    let ids = slots.iter().map(line).filter(|&line| line > 0);
    let mut ids: Vec<(&[u8], usize)> = ids.map(|line| (key_of(line), line)).collect();
    ids.push((&[], 0));
    ids.sort_unstable();
    ids.into_iter().unzip()
}

/// One hour's words: an array that threads reserve blocks of, where the next block of records starts
/// from the front, and how many words of big latencies are reserved at the back, each latency as two
/// words (id << 32 | latency) in the array's byte order, and one more than the length of its text, once
/// that's ready. The array has room for every line of the log,
/// since a log may be lopsided, and untouched pages cost nothing. A block's unused end stays zero, which
/// no real record is, since line 0 is never an id. All the arrays share one mapping, and each is whole
/// pages, because a mapping per hour costs a call each, and unmapping one stalls every thread that wants
/// the address space's lock.
type Hour = (&'static [AtomicU32], [AtomicUsize; 3]);

/// Hours since the start of the span, from bytes 6 to 13 of a line, M-DDTHH:, without parsing a date.
/// Oct 30, Oct 31, Nov 1 and Nov 2 differ in the last digit of the month and of the day:
/// (month & 1) + (day & 3) gives 0, 1, 2 and 3. One multiply adds the four masked bytes into the top
/// byte, weighted 24, 24, 10 and 1, and no partial product below that byte can carry into it.
fn hour_index(bytes: u64) -> usize {
    ((bytes & 0x000F_0F00_0300_0001).wrapping_mul(0x1800_0018_000A_0100) >> 56) as usize - 18
}

/// Reads the latency at `pos`, and returns it with the number of digits. Digits have bit 4 set and an
/// LF doesn't, so one load finds the end of up to 8 digits, and three vector multiplies parse them,
/// with their constants in registers that the rest of the line doesn't need. That leaves only the
/// rarest latencies, those with more than 8 digits, to the loop.
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
        let bytes = _mm_and_si128(aligned, _mm_set1_epi8(15));
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

/// Reads the segments that `go` gives. Each line is `ts method endpoint status latency`, where the
/// timestamp is always 27 bytes. A thread keeps its own unfilled blocks of each hour, so the hot path
/// reads and writes no line that other threads write, and allocates nothing.
fn parse(log: &File, table: &Table, hours: &[Hour], mut go: impl FnMut() -> Option<[usize; 2]>) {
    let (slots, _, lines, state) = table;
    let mut room = [0u8; PIECE + PADDING];
    let mut blocks = [std::ptr::null::<AtomicU32>(); HOURS];
    let mut bigs = blocks;
    // The last line's hour: its index and its place in a block, and in a segment, its bytes. An aligned
    // place means the block is used up, and hours change only between chunks of the log.
    let (mut hour, mut place) = (0, blocks[0]);
    // SAFETY: the build targets x86-64-v3, which has AVX2.
    let space = unsafe { _mm256_set1_epi8(b' ' as i8) };
    while let Some([start, end]) = go() {
        let (mut pos, mut bytes) = (start, 0);
        // A line belongs to the segment holding its first byte, so skip the one already started. The
        // log ends in LF, and a line is much shorter than this, so there's an LF to find.
        if start > 0 {
            let got = log.read_at(&mut room[..512], start as u64 - 1).unwrap();
            pos += room[..got].iter().position(|&b| b == b'\n').unwrap();
        }
        while pos < end {
            let got = log.read_at(&mut room[..PIECE], pos as u64).unwrap();
            // Whole lines only: the next read starts at the line that this piece cut short.
            let whole = room[..got].iter().rposition(|&b| b == b'\n').unwrap() + 1;
            let (stop, mut line, text) = (whole.min(end - pos), 0, &room[..]);
            while line < stop {
                // Reads at a fixed distance from `line` need no check, since `line` is inside the piece
                // and the padding is longer than any such distance, and whatever they read is safe.
                // SAFETY: both loads end at most 39 bytes after `line`.
                let (now, method): (u64, u64) =
                    unsafe { (load(text, line + 6), load(text, line + 31)) };
                if now != bytes {
                    blocks[hour] = place;
                    (bytes, hour) = (now, hour_index(now));
                    place = blocks[hour];
                    // Before any claim of this hour, this shows the claimer that this segment holds it
                    // up, as its position is at or past the low mark. After one, it's too late.
                    let mut state = state.lock().unwrap();
                    state.1[hour] = state.1[hour].max(pos + line + 1);
                    if hour < state.0 {
                        state.0 = usize::MAX;
                        return;
                    }
                }
                let at = line + 29 + (METHOD_LENS >> 4 * (method & 15) & 15) as usize;
                // SAFETY: the method's length is at most 15, so this ends within 108 bytes of `line`.
                let key = unsafe { load(text, at) };
                let spaces = equal_bytes(key, [space; 2]);
                // SAFETY: the padding covers it.
                let second = || unsafe { equal_bytes(load(text, at + 64), [space; 2]) };
                let len = match spaces {
                    // Endpoints average 34 bytes, and 94% of them end within 64.
                    0 => 64 + second().trailing_zeros() as usize,
                    _ => spaces.trailing_zeros() as usize,
                };
                // Either way `key_end` is at most 108 bytes after `line`, or inside the piece, so the
                // next two loads end at most 13 bytes later.
                let key_end = at + len;
                // Statuses are 100 to 599.
                // SAFETY: as above.
                let is_error = u32::from(unsafe { load::<u8>(text, key_end + 1) } == b'5');
                // Three status digits sit between two spaces, so the latency starts 5 bytes on.
                // SAFETY: as above.
                let (latency, digits) = unsafe { parse_latency(text, key_end + 5) };
                // Short keys hash by their length alone: a load would hold more than the key, and there
                // are very few of them.
                // SAFETY: both start inside the piece, as `at` is at least 29.
                let (first, last) = unsafe { (load(text, at), load(text, key_end - 8)) };
                let (first, last) = if len < 8 { (0, 0) } else { (first, last) };
                let index = (hash(first, last, len) >> 50) as usize;
                // One compare of a whole line of the table, through the closing space, settles nearly
                // every line of the log, with no branch on the length. An empty slot holds 0, and line 0
                // is all zeros, so it can't match.
                let kept = slots[index].load(Acquire) as usize;
                // SAFETY: a slot holds a line inside `lines`, aligned as they all are, and its bytes
                // were stored before it was published.
                let seen = unsafe { lines.as_ptr().add(kept).cast::<[__m256i; 2]>().read() };
                let through = spaces ^ spaces.wrapping_sub(1);
                let id = if !equal_bytes(seen, key) & through == 0 && spaces != 0 {
                    kept as u32
                } else {
                    id_of(table, &text[at..=key_end], through, index)
                };
                if latency >= BIG_LATENCY {
                    if bigs[hour] as usize % (BIG_BLOCK * 4) == 0 {
                        let (slots, [_, taken, _]) = &hours[hour];
                        let taken = taken.fetch_add(BIG_BLOCK, Relaxed) + BIG_BLOCK;
                        bigs[hour] = slots[slots.len() - taken..][..BIG_BLOCK].as_ptr();
                    }
                    let big = u64::from(id) << 32 | u64::from(latency);
                    // SAFETY: as for `place` below. The array is page-aligned, and blocks hold even
                    // words.
                    unsafe { (*bigs[hour].cast::<AtomicU64>()).store(big, Relaxed) };
                    bigs[hour] = bigs[hour].wrapping_add(2);
                }
                if place as usize % (BLOCK * 4) == 0 {
                    let (slots, [next, ..]) = &hours[hour];
                    place = slots[next.fetch_add(BLOCK, Relaxed)..][..BLOCK].as_ptr();
                }
                let record = id << ID_SHIFT | latency.min(BIG_LATENCY) << 1 | is_error;
                // SAFETY: `place` is inside a block this thread reserved, until it's aligned again.
                unsafe { (*place).store(record, Relaxed) };
                place = place.wrapping_add(1);
                line = key_end + 6 + digits;
            }
            pos += line;
        }
    }
}

/// The 8 decimal digits of a value below 10^8, as bytes with the most significant first, and with
/// no ASCII offset yet. Each step splits every number in its lane in two, in parallel, by multiplying
/// by a scaled reciprocal: 10^8 into two of 10^4, those into 10^2, and those into digits.
fn eight_digits(value: u64) -> u64 {
    let high_low = value / 10_000 | (value % 10_000) << 32;
    let hundreds = (high_low * 10486 >> 20) & 0x0000_007F_0000_007F;
    let pairs = (high_low - 100 * hundreds) << 16 | hundreds;
    let tens = (pairs * 103 >> 10) & 0x000F_000F_000F_000F;
    tens | (pairs - 10 * tens) << 8
}

/// Appends a number in decimal, with no loop or mispredicted branch for values below 10^8.
fn push_decimal(out: &mut Vec<u8>, value: u64) {
    if value >= 100_000_000 {
        write!(out, "{value}").unwrap();
        return;
    }
    let digits = eight_digits(value);
    let leading_zeros = (digits.trailing_zeros() as usize / 8).min(7);
    let end = out.len() + 8 - leading_zeros;
    out.extend_from_slice(&((digits | 0x3030_3030_3030_3030) >> (8 * leading_zeros)).to_le_bytes());
    out.truncate(end);
}

/// One stable pass of an LSD radix sort. `at` holds each digit's count, `order` lists the digits that
/// can occur, in the order they should come out, and `d` is a record's digit.
fn pass(from: &[u32], to: &mut [u32], at: &mut [u32], order: &[usize], d: impl Fn(u32) -> usize) {
    let mut total = 0;
    for &digit in order {
        total += std::mem::replace(&mut at[digit], total);
    }
    for &record in from {
        let place = &mut at[d(record)];
        // SAFETY: the counts are of these same records, each digit is in `order`, and `to` is as long
        // as `from`.
        unsafe { *to.get_unchecked_mut(*place as usize) = record };
        *place += 1;
    }
}

/// Appends one hour's output lines, given its records sorted by endpoint and then latency, where each
/// id's group ends, its big latencies sorted by id and then latency, and the endpoints and ids in order.
fn format_hour(h: usize, recs: &[u32], ends: &[u32], big: &[u64], order: Order, out: &mut Vec<u8>) {
    let clock = h + 18;
    let stamp = format!("{}T{:02}:00:00Z ", DATES[clock / 24], clock % 24);
    let mut start = ends[0] as usize;
    for (key, &id) in order.0.iter().zip(order.1).skip(1) {
        let group = &recs[start..ends[id] as usize];
        let Some(_) = group.first() else { continue };
        let n = group.len();
        start += n;
        let latency = |record: u32| (record >> 1) & BIG_LATENCY;
        let errors: u64 = group.iter().map(|record| u64::from(record & 1)).sum();
        let is_big = |&&r: &&u32| latency(r) == BIG_LATENCY;
        let big_count = group.iter().rev().take_while(is_big).count();
        let pct = |p: usize| {
            let index = (p * n + 99) / 100 - 1;
            match index.checked_sub(n - big_count) {
                Some(i) => big[big.partition_point(|e| e >> 32 < id as u64) + i] & 0xFFFF_FFFF,
                None => u64::from(latency(group[index])),
            }
        };
        let (p50, p99) = (pct(50), pct(99));
        let p95 = if n <= 19 { p99 } else { pct(95) };
        out.extend_from_slice(stamp.as_bytes());
        out.extend_from_slice(key);
        let count = n as u64;
        for value in [count, p50, p95, p99, errors, errors * 1_000_000 / count] {
            out.push(b' ');
            push_decimal(out, value);
        }
        out.push(b'\n');
    }
}

/// One round over the log, in which an hour is due when the reading is past where a thread last came
/// to it by this share of the log. Says whether it got to the end, as a line that comes too late stops
/// it.
fn round(log: &File, out: &Path, threads: usize, share: usize) -> std::io::Result<bool> {
    let file = File::create(out.join("summary.txt"))?;
    let end = log.metadata()?.len() as usize;
    // Room for every line: the shortest has 39 bytes, and one with a big latency has at least 45 and
    // takes 3 words. Plus each thread's unfinished blocks, and 1 MiB for text, as an hour has one line
    // per endpoint, of about 200 bytes at most.
    let len = (end / 15 + (BLOCK + BIG_BLOCK) * (threads + 1) + (1 << 18)).next_multiple_of(1024);
    let keys_at = HOURS * len * 4;
    // SAFETY: this maps fresh zero pages (PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS |
    // MAP_NORESERVE, as nearly all stay untouched), for the life of the process.
    let base = unsafe { mmap(std::ptr::null_mut(), keys_at + (1 << 22), 3, 0x4022, -1, 0) };
    // SAFETY: the mapping holds `HOURS * len` zero words, reached only through `hours`.
    let arena = unsafe { from_raw_parts(base.cast::<AtomicU32>(), HOURS * len) };
    // SAFETY: and after them 2^16 lines of zeros, page-aligned, reached only through `table`.
    let lines = unsafe { from_raw_parts(base.add(keys_at).cast::<Line>(), 1 << 16) };
    let new_hour = |slots| (slots, [const { AtomicUsize::new(0) }; 3]);
    let hours: Vec<Hour> = arena.chunks(len).map(new_hour).collect();
    let slots = [const { AtomicU16::new(0) }; SLOTS];
    let state = Mutex::new((0, [0; HOURS]));
    let table = (slots, AtomicUsize::new(1), lines, state);
    let order = Mutex::new(Arc::new(order_ids(&table)));
    let all: Vec<usize> = (0..1024).collect();
    let (cursor, written) = (AtomicUsize::new(0), Mutex::new(0));
    // A lower bound on the log position of any line each thread may yet read.
    let reached: Vec<_> = (0..threads).map(|_| AtomicUsize::new(usize::MAX)).collect();
    // Tying the size to the thread count makes the SD_THREADS test sweep move every cut, down to cuts
    // smaller than a line on tiny inputs.
    let size = (end / (threads * 64)).max(1);
    let write_hour = |hour: usize, scratch: &mut Vec<u8>| {
        let (array, [next, taken, ready]) = &hours[hour];
        // SAFETY: the hour is claimed, so no thread writes to its array any more, and what they wrote
        // is visible, as they had moved past it in `reached` by then.
        let slots = unsafe { from_raw_parts_mut(array.as_ptr() as *mut u32, array.len()) };
        let (recs, big) = slots.split_at_mut(slots.len() - taken.load(Relaxed));
        let recs = &mut recs[..next.load(Relaxed)];
        // Three stable passes sort by latency first and endpoint last.
        scratch.resize(recs.len() * 4 + 3, 0);
        // SAFETY: any bytes make a valid `u32`. Three spare bytes cover the alignment prefix, so
        // the slice holds exactly one word per record.
        let (_, tmp, _) = unsafe { scratch.align_to_mut::<u32>() };
        let (mut low, mut high, mut groups) = ([0u32; 1024], [0u32; 1024], [0u32; IDS]);
        for &record in recs.iter() {
            low[(record >> 1 & 511) as usize] += 1;
            high[(record >> 10 & 1023) as usize] += 1;
            groups[(record >> ID_SHIFT) as usize] += 1;
        }
        // An endpoint that's new since the order was made means a new order.
        let mut now = order.lock().unwrap();
        if now.1.iter().map(|&id| groups[id] as usize).sum::<usize>() != recs.len() {
            *now = Arc::new(order_ids(&table));
        }
        let order = now.clone();
        drop(now);
        pass(recs, tmp, &mut low, &all, |r| (r >> 1 & 511) as usize);
        pass(tmp, recs, &mut high, &all, |r| (r >> 10 & 1023) as usize);
        pass(recs, tmp, &mut groups, &order.1, |r| r as usize >> ID_SHIFT);
        // SAFETY: an even number of words, 8-aligned, that only this thread now touches. Unused
        // ones are zero, which sorts before id 1.
        let big = unsafe { from_raw_parts_mut(big.as_mut_ptr().cast::<u64>(), big.len() / 2) };
        big.sort_unstable();
        // SAFETY: the records are done with, and the whole array is ours. It has room for any text, so
        // this vector never reallocates, and it's leaked rather than dropped.
        let mut text = unsafe { Vec::from_raw_parts(recs.as_mut_ptr().cast(), 0, array.len() * 4) };
        format_hour(hour, tmp, &groups, big, (&order.0, &order.1), &mut text);
        ready.store(text.leak().len() + 1, Release);
    };
    // Appends the ready hours, in order, and frees them. A thread that finds this busy leaves its hour
    // to the thread in here, or to the next to finish one, or in the end to this round itself.
    let append = || -> Option<()> {
        loop {
            let mut next = written.try_lock().ok()?;
            let (array, [.., len]) = hours.get(*next).filter(|h| h.1[2].load(Acquire) > 0)?;
            // SAFETY: the text is whole, as its length is out, and nothing else refers to it.
            let text = unsafe { from_raw_parts(array.as_ptr().cast(), len.load(Relaxed) - 1) };
            // `panic = "abort"`: a failed write ends the process.
            (&file).write_all(text).expect("writing the summary");
            // SAFETY: nothing else refers to these whole pages, and nothing reads them again.
            unsafe { madvise(text.as_ptr().cast_mut(), array.len() * 4, 4) }; // MADV_DONTNEED.
            *next += 1;
        }
    };
    // The first few threads also finish hours: enough to keep up with the rest, and few, since each
    // one's scratch stays as large as the largest hour it has seen.
    let work = &|me: usize| {
        let mut scratch = Vec::with_capacity(usize::from(me < 4) << 20);
        let go = || loop {
            // Any segment that this thread takes from here on starts at or past this mark.
            let head = cursor.load(Relaxed);
            reached[me].store(head, Release);
            let low = reached.iter().map(|r| r.load(Acquire)).min().unwrap();
            // An hour with no line yet has 0, and it's due only when all reading is done.
            let due = |&seen: &usize| low >= end || seen > 0 && seen + end / share < low;
            let mut state = table.3.lock().unwrap();
            let hour = state.0;
            if me < 4 && state.1.get(hour).is_some_and(due) {
                state.0 += 1;
                drop(state);
                // While it writes, this thread holds no hour back.
                reached[me].store(usize::MAX, Release);
                write_hour(hour, &mut scratch);
                append();
                continue;
            }
            // Records pile up fast behind a stalled writer, so readers wait for it past a point.
            let held = |&s: &usize| s > 0 && s + 6 * (end / share) < head;
            if !state.1.get(hour).is_some_and(held) {
                let start = cursor.fetch_add(size, Relaxed);
                // At the end, once more with the mark at the end, which lets go of the last hours.
                if start < end || head >= end && (me >= 4 || hour >= HOURS) {
                    return (start < end).then_some([start, (start + size).min(end)]);
                }
            }
            std::hint::spin_loop();
        };
        parse(log, &table, &hours, go);
    };
    thread::scope(|scope| (0..threads).for_each(|me| drop(scope.spawn(move || work(me)))));
    append();
    Ok(table.3.lock().unwrap().0 == HOURS)
}

fn main() -> std::io::Result<()> {
    // Transparent huge pages would round each hour's sparse array up to 2 MB, so turn them off, with
    // PR_SET_THP_DISABLE. And parsing allocates nothing, so one malloc arena will do (M_ARENA_MAX), and
    // no thread maps a heap of its own. Failing is harmless.
    // SAFETY: both only set a flag on our own process.
    unsafe { (prctl(41, 1, 0, 0, 0), mallopt(-8, 1)) };
    let arg = |i, default: &str| std::env::args().nth(i).unwrap_or(default.into());
    // `SD_THREADS` if set (the test sweep uses it), else what the container allows, capped at the
    // box's 12 threads.
    let allowed = thread::available_parallelism().map_or(1, |n| n.get().min(12));
    let asked = std::env::var("SD_THREADS").map_or(0, |v| v.parse().unwrap_or(0));
    let threads = if asked > 0 { asked } else { allowed };
    let log = File::open(Path::new(&arg(1, "/data")).join("requests.log"))?;
    let round = |share| round(&log, arg(2, "/out").as_ref(), threads, share);
    // A 48th of the log is several times the disorder that the spec's shards make. If it's not enough,
    // the second round has no hour due until the log is done.
    _ = round(48)? || round(1)?;
    Ok(())
}
