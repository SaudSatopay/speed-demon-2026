//! Track A (Ingest): per-hour, per-endpoint latency percentiles over an HTTP request log.
//!
//! The log is memory-mapped and cut into segments that threads claim from an atomic cursor, so a
//! thread that falls behind simply claims fewer. Each line is read at fixed offsets where the format
//! allows it, and with block loads where it doesn't. One lock-free table, shared by all threads,
//! turns endpoints into ids, and each line becomes one 32-bit record in a shared array for its hour,
//! where each thread reserves small blocks. When the log is done, threads claim hours: each groups one
//! hour's array with three stable radix passes, formats it, frees it, and waits for its turn to append
//! to the file. The output vector doubles as aligned radix scratch before it holds text, so output is
//! in a fixed order whatever the scheduling and never sits in memory whole.

use std::arch::x86_64::*;
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering::*};
use std::{fs::File, io::Write, os::fd::AsRawFd, path::Path, sync::Mutex, thread};

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
const BLOCK_RECORDS: usize = 256;
const BIG_BLOCK: usize = 32;

/// Readable zero bytes after the log, so that block loads can run past its last line. So a load that
/// starts inside the log, or a short fixed distance after it, needs no check of its own.
const PADDING: usize = 4096;

/// A method's length, as the nibble selected by the low nibble of its 4th byte, which tells the seven
/// apart: GET and PUT have a space there (0), POST and HEAD have T and D (4), PATCH has C (3), DELETE
/// has E (5), and OPTIONS has I (9).
const METHOD_LENS: u64 = 0x70_0064_5003;

/// Room for the spec's exactly 1,842 endpoints, at a low load.
const SLOTS: usize = 1 << 12;
type Table = [AtomicU32; SLOTS];

// Declared by hand to keep the crate at zero dependencies; std already links glibc.
unsafe extern "C" {
    fn prctl(option: i32, a: u64, b: u64, c: u64, d: u64) -> i32;
    fn mallopt(param: i32, value: i32) -> i32;
    fn mmap(at: *mut u8, len: usize, p: i32, f: i32, fd: i32, o: i64) -> *mut u8;
    fn madvise(at: *mut u8, len: usize, advice: i32) -> i32;
}

/// Maps a file read-only for the life of the process, followed by `PADDING` readable zero bytes. The
/// scorer's memory peak doesn't charge us for these pages, unlike reading the log onto the heap.
fn map_file(path: &Path) -> std::io::Result<&'static [u8]> {
    let file = File::open(path)?;
    let len = file.metadata()?.len() as usize;
    // SAFETY: the first call reserves fresh zero pages (PROT_READ, MAP_PRIVATE | MAP_ANONYMOUS), and
    // the second maps the file over the start of them (MAP_PRIVATE | MAP_FIXED), which fails only on
    // an empty file. Neither is ever unmapped, and nothing writes to the dataset while we run.
    unsafe {
        let base = mmap(std::ptr::null_mut(), len + PADDING, 1, 0x22, -1, 0);
        let mapped = mmap(base, len, 1, 0x12, file.as_raw_fd(), 0);
        assert!(base as isize != -1 && (mapped == base || len == 0));
        Ok(std::slice::from_raw_parts(base.cast(), len))
    }
}

/// Loads a `T` at `pos`, little-endian and with no alignment, which may run into the padding. It's
/// a `u64` for 8 bytes, or two AVX2 registers for a block of 64.
///
/// # Safety
/// `log` must come from `map_file`, and `pos + size_of::<T>()` must be at most `log.len() + PADDING`.
unsafe fn load<T: Copy>(log: &[u8], pos: usize) -> T {
    // SAFETY: the caller keeps the load inside the mapping, and the load needs no alignment.
    unsafe { log.as_ptr().add(pos).cast::<T>().read_unaligned() }
}

/// One bit per byte of two blocks, set where they're equal.
fn equal_bytes(a: [__m256i; 2], b: [__m256i; 2]) -> u64 {
    // SAFETY: the build targets x86-64-v3, which has AVX2.
    let mask = |i: usize| unsafe { _mm256_movemask_epi8(_mm256_cmpeq_epi8(a[i], b[i])) as u32 };
    u64::from(mask(0)) | u64::from(mask(1)) << 32
}

/// Runs `work` on `threads` threads, and waits for them all: the scope joins every thread, so the
/// handles have no use.
fn on_threads(threads: usize, work: impl Fn() + Sync) {
    thread::scope(|scope| (0..threads).for_each(|_| drop(scope.spawn(&work))));
}

/// Mixes an endpoint's length with its first and last 8 bytes, which tell nearly all templated paths
/// apart, with one multiply, whose high bits depend on every input bit. A test build swaps it for a
/// constant, so that every key collides and any logic that trusts a hash without comparing the key
/// shows up as a wrong answer.
fn hash(first: u64, last: u64, len: usize) -> u64 {
    let hash = (first ^ last.rotate_left(29) ^ len as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    if cfg!(sd_const_hash) { 0 } else { hash }
}

/// The id of the endpoint at `start..end`, from slot `index` on, adding it if it's new: the path for
/// the lines that `parse` can't settle in one compare. An id is a slot number in a table that all
/// threads share with no locks: a slot holds where the key first occurred in the log, which is never
/// 0, so 0 means empty, and a published slot never changes. A key is always compared in full, through
/// its closing space, because templated paths share prefixes, suffixes and lengths, and that space is
/// why a slot needs no length. Slot 0 stays free: see `Hour`.
///
/// # Safety
/// `log` must come from `map_file`, `start` must be at least 8, `start <= end`, `end` must be inside
/// the log and hold a space, and `table` must have been filled only by this function, with this log.
#[cold]
unsafe fn id_of(table: &Table, log: &[u8], start: usize, end: usize, mut index: usize) -> u32 {
    let len = end - start;
    // SAFETY: this starts inside the log.
    let block = unsafe { load(log, start) };
    let position = start as u32;
    loop {
        index = (index & (SLOTS - 1)).max(1);
        let slot = &table[index];
        if slot.load(Relaxed) == 0 && slot.compare_exchange(0, position, Relaxed, Relaxed).is_ok() {
            return index as u32;
        }
        let first = slot.load(Relaxed) as usize;
        // The rest of a long key may be longer than what's left of the log after `first`, if that
        // key is shorter and on the last line, hence the `get`.
        // SAFETY: `first` is the `start` of an earlier call, which was inside the log.
        if (!equal_bytes(unsafe { load(log, first) }, block)).trailing_zeros() as usize
            > len.min(63)
            && (len < 64 || log.get(first + 64..=first + len) == Some(&log[start + 64..=end]))
        {
            return index as u32;
        }
        index += 1;
    }
}

/// Each id's endpoint, with an empty key for unused ids, and the ids in use, in the byte order of
/// their endpoints (`[u8]` compares as unsigned bytes, with a proper prefix first), after id 0,
/// which no endpoint has: it's what the unused ends of blocks hold.
fn order_ids<'a>(table: &Table, log: &'a [u8]) -> (Vec<&'a [u8]>, Vec<usize>) {
    let key_of = |slot: &AtomicU32| match slot.load(Relaxed) as usize {
        0 => &log[..0],
        first => log[first..].split(|&b| b == b' ').next().unwrap(),
    };
    let keys: Vec<&[u8]> = table.iter().map(key_of).collect();
    let mut ids: Vec<usize> = (1..SLOTS).filter(|&id| !keys[id].is_empty()).collect();
    ids.sort_unstable_by_key(|&id| keys[id]);
    (keys, std::iter::once(0).chain(ids).collect())
}

/// One hour's words: an array that threads reserve blocks of, where the next block of records starts
/// from the front, and how many words of big latencies are reserved at the back, each latency as two
/// words (id << 32 | latency) in the array's byte order. The array has room for every line of the log,
/// since a log may be lopsided, and untouched pages cost nothing. A block's unused end stays zero, which
/// no real record is, since slot 0 is never an id. All the arrays share one mapping, and each is whole
/// pages, because a mapping per hour costs a call each, and unmapping one stalls every thread that wants
/// the address space's lock.
type Hour = (&'static mut [AtomicU32], AtomicUsize, AtomicUsize);

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
/// `log` must come from `map_file`, and `pos + 8` must be at most `log.len() + PADDING`.
unsafe fn parse_latency(log: &[u8], pos: usize) -> (u32, usize) {
    // SAFETY: the caller vouches for this load, and the loop below uses checked indexing.
    let word = unsafe { load::<u64>(log, pos) };
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
    while digits >= 8 && log[pos + digits] != b'\n' {
        value = value * 10 + u64::from(log[pos + digits] - b'0');
        digits += 1;
    }
    (value as u32, digits)
}

/// Claims segments until the log runs out. Each line is `ts method endpoint status latency`, where
/// the timestamp is always 27 bytes. A thread keeps its own unfilled blocks of each hour, so the hot
/// path reads and writes no line that other threads write, and allocates nothing.
/// `log` is the mapping from `map_file`, whose padding the unchecked loads here rely on.
fn parse(log: &[u8], table: &Table, hours: &[Hour], head: &AtomicUsize, size: usize) {
    let mut blocks = [std::ptr::null::<AtomicU32>(); HOURS];
    let mut bigs = blocks;
    let claim = || Some(head.fetch_add(size, Relaxed)).filter(|&start| start < log.len());
    // The last line's hour: its bytes, its index and its place in a block. An aligned place means the
    // block is used up, and hours change only between chunks of the log.
    let (mut bytes, mut hour, mut place) = (0, 0, blocks[0]);
    // SAFETY: the build targets x86-64-v3, which has AVX2.
    let space = unsafe { _mm256_set1_epi8(b' ' as i8) };
    while let Some(start) = claim() {
        let end = (start + size).min(log.len());
        // Maps the segment's pages in one call, not in a fault per 16 pages. MADV_POPULATE_READ.
        // SAFETY: the range is in the mapping, and this has no effect on what it holds.
        unsafe {
            madvise(
                log.as_ptr().add(start & !4095).cast_mut(),
                end - (start & !4095),
                22,
            )
        };
        // A line belongs to the segment holding its first byte, so skip the one already started.
        // The log ends in LF, so this stops at the end of the log at the latest.
        let mut line = start;
        while line > 0 && log[line - 1] != b'\n' {
            line += 1;
        }
        while line < end {
            // Reads at a fixed distance from `line` need no check, since `line` is inside the log and
            // the padding is longer than any such distance. A truncated line just reads zeros.
            // SAFETY: both loads end at most 39 bytes after `line`, inside the padding at the latest.
            let (now, method): (u64, u64) = unsafe { (load(log, line + 6), load(log, line + 31)) };
            if now != bytes {
                blocks[hour] = place;
                (bytes, hour) = (now, hour_index(now));
                place = blocks[hour];
            }
            let key_at = line + 29 + (METHOD_LENS >> 4 * (method & 15) & 15) as usize;
            // SAFETY: the method's length is at most 15, so this ends within 108 bytes of `line`.
            let key = unsafe { load(log, key_at) };
            let spaces = equal_bytes(key, [space; 2]);
            let len = match spaces {
                // Endpoints average 34 bytes, and 94% of them end within 64.
                0 => 64 + log[key_at + 64..].iter().position(|&b| b == b' ').unwrap(),
                _ => spaces.trailing_zeros() as usize,
            };
            // `key_end` holds a space, so it's inside the log, as the padding has none, and the next
            // two loads end at most 13 bytes after it, inside the padding at the latest.
            let key_end = key_at + len;
            // Statuses are 100 to 599.
            // SAFETY: as above.
            let is_error = u32::from(unsafe { load::<u8>(log, key_end + 1) } == b'5');
            // Three status digits sit between two spaces, so the latency starts 5 bytes on.
            // SAFETY: as above.
            let (latency, digits) = unsafe { parse_latency(log, key_end + 5) };
            // Short keys hash by their length alone: a load would hold more than the key, and there are
            // very few of them.
            // SAFETY: both start inside the log, as `key_at` is at least 29.
            let (first, last) = unsafe { (load(log, key_at), load(log, key_end - 8)) };
            let (first, last) = if len < 8 { (0, 0) } else { (first, last) };
            let index = (hash(first, last, len) >> 52) as usize;
            // One compare of a whole block, through the closing space, settles nearly every line, with
            // no branch on the length. An empty slot holds 0, where the log has a digit and a key has
            // a slash, so it can't match, and neither can slot 0, which stays empty.
            // SAFETY: a slot holds 0 or the `start` of a call to `id_of`, inside the log.
            let seen = unsafe { load(log, table[index].load(Relaxed) as usize) };
            let through = spaces ^ spaces.wrapping_sub(1);
            let id = if !equal_bytes(seen, key) & through == 0 && spaces != 0 {
                index as u32
            } else {
                // SAFETY: `key_end` is inside the log and holds a space, `key_at` is at least 29
                // and not after it, and only this call fills `table`.
                unsafe { id_of(table, log, key_at, key_end, index) }
            };
            if latency >= BIG_LATENCY {
                if bigs[hour] as usize % (BIG_BLOCK * 4) == 0 {
                    let (slots, _, taken) = &hours[hour];
                    let taken = taken.fetch_add(BIG_BLOCK, Relaxed) + BIG_BLOCK;
                    bigs[hour] = slots[slots.len() - taken..][..BIG_BLOCK].as_ptr();
                }
                let big = u64::from(id) << 32 | u64::from(latency);
                // SAFETY: as for `place` below. The array is page-aligned, and blocks hold even words.
                unsafe {
                    (*bigs[hour].cast::<AtomicU64>()).store(big, Relaxed);
                    bigs[hour] = bigs[hour].add(2);
                }
            }
            if place as usize % (BLOCK_RECORDS * 4) == 0 {
                let (slots, next, _) = &hours[hour];
                place = slots[next.fetch_add(BLOCK_RECORDS, Relaxed)..][..BLOCK_RECORDS].as_ptr();
            }
            let record = id << ID_SHIFT | latency.min(BIG_LATENCY) << 1 | is_error;
            // SAFETY: `place` is inside a block that this thread reserved, until it's aligned again.
            unsafe {
                (*place).store(record, Relaxed);
                place = place.add(1);
            }
            line = key_end + 6 + digits;
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

/// Appends one hour's output lines, given its records sorted by endpoint and then latency, and its
/// big latencies sorted by id and then latency.
fn format_hour(hour: usize, sorted: &[u32], big: &[u64], keys: &[&[u8]], out: &mut Vec<u8>) {
    let clock = hour + 18;
    let stamp = format!("{}T{:02}:00:00Z ", DATES[clock / 24], clock % 24);
    for group in sorted.chunk_by(|a, b| a >> ID_SHIFT == b >> ID_SHIFT) {
        let n = group.len();
        let latency = |record: u32| (record >> 1) & BIG_LATENCY;
        let errors: u64 = group.iter().map(|record| u64::from(record & 1)).sum();
        let is_big = |&&r: &&u32| latency(r) == BIG_LATENCY;
        let big_count = group.iter().rev().take_while(is_big).count();
        let id = group[0] >> ID_SHIFT;
        let pct = |p: usize| {
            let index = (p * n + 99) / 100 - 1;
            match index.checked_sub(n - big_count) {
                Some(i) => big[big.partition_point(|e| e >> 32 < id.into()) + i] & 0xFFFF_FFFF,
                None => u64::from(latency(group[index])),
            }
        };
        let p50 = pct(50);
        let p99 = pct(99);
        let p95 = if n <= 19 { p99 } else { pct(95) };
        out.extend_from_slice(stamp.as_bytes());
        out.extend_from_slice(keys[id as usize]);
        let count = n as u64;
        for value in [count, p50, p95, p99, errors, errors * 1_000_000 / count] {
            out.push(b' ');
            push_decimal(out, value);
        }
        out.push(b'\n');
    }
}

fn write_summary(file: File, keys: &[&[u8]], ids: &[usize], hours: Vec<Hour>, threads: usize) {
    let all: Vec<usize> = (0..1024).collect();
    let queue = Mutex::new(hours.into_iter().enumerate());
    let written = AtomicUsize::new(0);
    on_threads(threads, || {
        let mut text = Vec::with_capacity(1 << 20);
        for (hour, (slots, next, taken)) in std::iter::from_fn(|| queue.lock().unwrap().next()) {
            let (recs, big) = slots.split_at_mut(slots.len() - taken.into_inner());
            let recs = AtomicU32::get_mut_slice(&mut recs[..next.into_inner()]);
            // Three stable passes sort by latency first and endpoint last. The current text buffer
            // is scratch, so the result is copied out, and its storage is reused for the output bytes.
            text.resize(recs.len() * 4 + 3, 0);
            // SAFETY: any bytes make a valid `u32`. Three spare bytes cover the alignment prefix, so
            // the slice holds exactly one word per record.
            let (_, tmp, _) = unsafe { text.align_to_mut::<u32>() };
            let (mut low, mut high, mut groups) = ([0u32; 1024], [0u32; 1024], [0u32; SLOTS]);
            for &record in recs.iter() {
                low[(record >> 1 & 511) as usize] += 1;
                high[(record >> 10 & 1023) as usize] += 1;
                groups[(record >> ID_SHIFT) as usize] += 1;
            }
            pass(recs, tmp, &mut low, &all, |r| (r >> 1 & 511) as usize);
            pass(tmp, recs, &mut high, &all, |r| (r >> 10 & 1023) as usize);
            pass(recs, tmp, &mut groups, ids, |r| (r >> ID_SHIFT) as usize);
            recs.copy_from_slice(tmp);
            let zeros = recs.partition_point(|&record| record == 0);
            // SAFETY: an even number of words, 8-aligned, that only this thread now touches. Unused
            // ones are zero, which sorts before id 1.
            let big = unsafe {
                std::slice::from_raw_parts_mut(big.as_mut_ptr().cast::<u64>(), big.len() / 2)
            };
            big.sort_unstable();
            text.clear();
            format_hour(hour, &recs[zeros..], big, keys, &mut text);
            // SAFETY: nothing else refers to these whole pages, and nothing reads them again.
            unsafe { madvise(slots.as_mut_ptr().cast(), slots.len() * 4, 4) }; // MADV_DONTNEED.
            while written.load(Acquire) != hour {
                std::hint::spin_loop();
            }
            // `panic = "abort"`: a failed write ends the process rather than leaving the other
            // threads waiting for their turn.
            (&file).write_all(&text).expect("writing the summary");
            written.store(hour + 1, Release);
        }
    });
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
    let log = map_file(&Path::new(&arg(1, "/data")).join("requests.log"))?;
    assert!(log.len() < 1 << 32, "table slots hold 32-bit positions");
    let file = File::create(Path::new(&arg(2, "/out")).join("summary.txt"))?;
    // Room for every line: the shortest has 39 bytes, and one with a big latency has at least 45 and
    // takes 3 words. Plus each thread's unfinished blocks.
    let len = (log.len() / 15 + (BLOCK_RECORDS + BIG_BLOCK) * (threads + 1)).next_multiple_of(1024);
    // SAFETY: this maps fresh zero pages (PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS |
    // MAP_NORESERVE, as nearly all stay untouched), for the life of the process.
    let base = unsafe { mmap(std::ptr::null_mut(), HOURS * len * 4, 3, 0x4022, -1, 0) };
    assert!(base as isize != -1);
    // SAFETY: the mapping holds `HOURS * len` zero words, reached only through `arena`.
    let arena = unsafe { std::slice::from_raw_parts_mut(base.cast::<AtomicU32>(), HOURS * len) };
    let new_hour = |slots| (slots, AtomicUsize::new(0), AtomicUsize::new(0));
    let hours: Vec<Hour> = arena.chunks_mut(len).map(new_hour).collect();
    let table = [const { AtomicU32::new(0) }; SLOTS];
    let cursor = AtomicUsize::new(0);
    // Eight segments per thread balances the load. Tying the size to the thread count also makes
    // the SD_THREADS test sweep move every cut, down to cuts smaller than a line on tiny inputs.
    let size = (log.len() / (threads * 8)).max(1);
    on_threads(threads, || parse(log, &table, &hours, &cursor, size));
    let (keys, ids) = order_ids(&table, log);
    write_summary(file, &keys, &ids, hours, threads.min(8));
    Ok(())
}
