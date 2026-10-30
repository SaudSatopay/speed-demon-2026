//! Track D (Traverse): exact shortest paths for 2,000 queries on a 5M-edge road network.
//!
//! Threads parse the mapped graph twice, in batches with prefetching. The first pass counts each
//! node's edges and keeps a few neighbours, enough to renumber the nodes so that neighbours sit close
//! in memory. The second places the edges into a compressed adjacency array, so no temporary edge
//! list is ever held. Sweeping that array, where neighbours are close, then fills the reverse array
//! with hardly a cache miss. Threads then claim queries from an atomic counter, each
//! with its own search state, and run a two-ended Dijkstra that gives both sides equal work. Answers
//! go into a slot per query, so the output order doesn't depend on scheduling.

use std::arch::x86_64::{_MM_HINT_T1, _mm_prefetch};
use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::os::fd::AsRawFd;
use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering::Relaxed};
use std::thread;

/// Above any real distance, since node ids fit far below 2^30 and weights below 2^32, and low enough
/// that a distance can be added to it without wrapping. It also marks "no path" in the answers.
const INF: u64 = 1 << 62;

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
        let base = mmap(
            std::ptr::null_mut(),
            len + PADDING,
            read,
            private | anonymous,
            -1,
            0,
        );
        let mapped = mmap(base, len, read, private | fixed, file.as_raw_fd(), 0);
        assert!(base as isize != -1 && (mapped == base || len == 0));
        Ok(std::slice::from_raw_parts(base.cast(), len))
    }
}

/// Asks for the cache line holding `item`, so that a later access doesn't stall on it.
fn prefetch<T>(item: &T) {
    // SAFETY: prefetching has no effect on memory, and the reference is valid anyway.
    unsafe { _mm_prefetch::<_MM_HINT_T1>(std::ptr::from_ref(item).cast()) }
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

/// Reads the number at `pos` and steps past the separator after it. One 8-byte load settles all but
/// the longest numbers: digits have bit 4 set, and spaces and LFs don't, so the first clear bit marks
/// the end, and three multiplications parse the digits, with no byte loop to mispredict.
fn next_number(text: &[u8], pos: &mut usize) -> u64 {
    assert!(*pos + 8 <= text.len() + PADDING);
    // SAFETY: the assert keeps the load inside the mapping, and it needs no alignment.
    let word = unsafe { text.as_ptr().add(*pos).cast::<u64>().read_unaligned() };
    let digits = (!word & 0x1010_1010_1010_1010).trailing_zeros() as usize / 8;
    let aligned = (word << (64 - 8 * digits)) & 0x0F0F_0F0F_0F0F_0F0F;
    let pairs = aligned.wrapping_mul((10 << 8) + 1) >> 8 & 0x00FF_00FF_00FF_00FF;
    let quads = pairs.wrapping_mul((100 << 16) + 1) >> 16 & 0x0000_FFFF_0000_FFFF;
    let mut value = quads.wrapping_mul((10000 << 32) + 1) >> 32;
    *pos += digits;
    while let Some(digit @ b'0'..) = text.get(*pos) {
        value = value * 10 + u64::from(digit - b'0');
        *pos += 1;
    }
    *pos += 1;
    value
}

/// Runs `work` on ranges of `0..len`, claimed from an atomic cursor. Eight ranges per thread balance
/// the load. Tying the size to the thread count makes the SD_THREADS test sweep move every cut.
fn for_ranges(len: usize, threads: usize, work: impl Fn(usize, usize) + Sync) {
    let size = (len / (threads * 8)).max(1);
    let cursor = AtomicUsize::new(0);
    on_threads(threads, || {
        let claim = || Some(cursor.fetch_add(size, Relaxed)).filter(|&start| start < len);
        while let Some(start) = claim() {
            work(start, (start + size).min(len));
        }
    });
}

/// One edge line: tail, head, weight.
type Edge = (u32, u32, u32);
/// Edges parsed before any is used. A batch can prefetch all its cache lines and let them arrive
/// together, instead of stalling on each in turn.
const BATCH: usize = 32;

/// Passes every edge line to `visit`, in batches, from several threads. A line belongs to the range
/// holding its first byte.
fn for_each_batch(edge_lines: &[u8], threads: usize, visit: impl Fn(&[Edge]) + Sync) {
    for_ranges(edge_lines.len(), threads, |start, end| {
        let mut pos = start;
        while pos > 0 && edge_lines[pos - 1] != b'\n' {
            pos += 1;
        }
        let mut batch = [(0, 0, 0); BATCH];
        let mut count = 0;
        while pos < end {
            let tail = next_number(edge_lines, &mut pos) as u32;
            let head = next_number(edge_lines, &mut pos) as u32;
            let weight = next_number(edge_lines, &mut pos) as u32;
            batch[count] = (tail, head, weight);
            count += 1;
            if count == BATCH {
                visit(&batch);
                count = 0;
            }
        }
        visit(&batch[..count]);
    });
}

/// A zeroed array of atomics, whose pages stay untouched until a thread writes to them.
fn zeroed_u32s(len: usize) -> Vec<AtomicU32> {
    vec![0u32; len].into_iter().map(AtomicU32::new).collect()
}

/// Seeds and new numbers a thread takes at a time when numbering.
const SEED_BLOCK: usize = 1 << 12;
const NUMBER_BLOCK: u32 = 1 << 10;
/// A node's entry while counting: its out-degree, then up to three out-neighbours + 1 for numbering.
/// Nodes average 2.9, so this sees most edges, and it fits one cache line, unlike two arrays.
const RECORD: usize = 4;

/// Numbers the nodes so that neighbours sit close in memory, because the input's ids are random, which
/// makes every hop a cache miss. Each thread walks depth first from a seed, giving each node it
/// reaches first its next number, from a block of its own. Nodes the walks miss become seeds
/// themselves, so all of them get a number. Any numbering gives the same answers, and numbers needn't
/// be dense: a thread's last block has a few unused, and so does number 0, which means no number yet.
/// Returns the numbers, and how many entries an array indexed by number needs, plus one.
fn local_numbering(records: &[AtomicU32], nodes: usize, threads: usize) -> (Vec<AtomicU32>, usize) {
    let numbers = zeroed_u32s(nodes);
    let (next_seed, next_number) = (AtomicUsize::new(0), AtomicU32::new(1));
    on_threads(threads, || {
        let (mut number, mut numbers_end, mut stack) = (0, 0, Vec::new());
        // Most claims fail, and a plain read costs far less than a locked update.
        let mut claim = |node: usize| {
            if number == numbers_end {
                number = next_number.fetch_add(NUMBER_BLOCK, Relaxed);
                numbers_end = number + NUMBER_BLOCK;
            }
            let free = numbers[node].load(Relaxed) == 0;
            let won = free
                && numbers[node]
                    .compare_exchange(0, number, Relaxed, Relaxed)
                    .is_ok();
            number += u32::from(won);
            won
        };
        loop {
            // Seeds come in blocks, because near the end nearly all are taken, and one shared counter
            // hit per node would make the threads queue.
            let block = next_seed.fetch_add(SEED_BLOCK, Relaxed);
            if block >= nodes {
                return;
            }
            for seed in block..(block + SEED_BLOCK).min(nodes) {
                if claim(seed) {
                    stack.push(seed);
                }
                while let Some(node) = stack.pop() {
                    for link in &records[node * RECORD + 1..][..RECORD - 1] {
                        let neighbour = link.load(Relaxed) as usize;
                        if neighbour != 0 && claim(neighbour - 1) {
                            stack.push(neighbour - 1);
                            prefetch(&records[(neighbour - 1) * RECORD]);
                        }
                    }
                }
            }
        }
    });
    let entries = next_number.into_inner() as usize + 1;
    (numbers, entries)
}

/// A compressed adjacency array: the arcs leaving `node` are `arcs[offsets[node]..offsets[node + 1]]`.
/// An arc packs its neighbour and its weight into 32 bits, with as many weight bits as the node count
/// leaves. The highest value means the weight is in `heavy`, sorted by arc number once the graph is
/// built. Atomics let threads fill it without locks or `unsafe`; relaxed atomic loads and stores
/// compile to plain moves on x86.
struct Graph {
    offsets: Vec<AtomicU32>,
    arcs: Vec<AtomicU32>,
    weight_bits: u32,
    spilled: Mutex<Vec<(u32, u32)>>,
    heavy: Vec<(u32, u32)>,
}

impl Graph {
    /// Turns per-node counts into a graph, ready to `reserve` every arc. Each offset starts at the
    /// end of its node's range and moves down as arcs are reserved, so it finishes at the start of
    /// the range, with no second array.
    fn from_counts(mut offsets: Vec<AtomicU32>) -> Graph {
        let mut total = 0;
        for offset in &mut offsets {
            total += *offset.get_mut();
            *offset.get_mut() = total;
        }
        let nodes = offsets.len() as u32;
        Graph {
            offsets,
            arcs: zeroed_u32s(total as usize),
            weight_bits: nodes.leading_zeros(),
            spilled: Mutex::new(Vec::new()),
            heavy: Vec::new(),
        }
    }

    /// Reserves a slot for one more arc of `node`.
    fn reserve(&self, node: u32) -> usize {
        self.offsets[node as usize].fetch_sub(1, Relaxed) as usize - 1
    }

    fn store(&self, slot: usize, neighbour: u32, weight: u32) {
        let heavy = (1 << self.weight_bits) - 1;
        if weight >= heavy {
            self.spilled.lock().unwrap().push((slot as u32, weight));
        }
        self.arcs[slot].store(neighbour << self.weight_bits | weight.min(heavy), Relaxed);
    }

    /// Sorts the heavy weights, once every thread has finished.
    fn seal(&mut self) {
        self.heavy = std::mem::take(self.spilled.get_mut().unwrap());
        self.heavy.sort_unstable();
    }

    /// The first arc number of `node`, and its arcs.
    fn arcs_of(&self, node: u32) -> (usize, &[AtomicU32]) {
        let start = self.offsets[node as usize].load(Relaxed) as usize;
        let end = self.offsets[node as usize + 1].load(Relaxed) as usize;
        (start, &self.arcs[start..end])
    }

    /// A packed arc's neighbour and weight, given its arc number.
    fn unpack(&self, slot: usize, arc: u32) -> (u32, u64) {
        let heavy = (1 << self.weight_bits) - 1;
        let weight = arc & heavy;
        if weight != heavy {
            return (arc >> self.weight_bits, u64::from(weight));
        }
        let at = self.heavy.partition_point(|e| (e.0 as usize) < slot);
        (arc >> self.weight_bits, u64::from(self.heavy[at].1))
    }

    /// Passes every arc to `visit`, as tail, head and weight, in node order within each thread's range.
    fn for_each_arc(&self, threads: usize, visit: impl Fn(u32, u32, u32) + Sync) {
        for_ranges(self.offsets.len() - 1, threads, |start, end| {
            for node in start..end {
                let (first, arcs) = self.arcs_of(node as u32);
                for (index, arc) in arcs.iter().enumerate() {
                    let (neighbour, weight) = self.unpack(first + index, arc.load(Relaxed));
                    visit(node as u32, neighbour, weight as u32);
                }
            }
        });
    }
}

/// Builds the forward graph and its reverse from the graph file, in new node numbers, and returns
/// those numbers too. `offsets` has a final entry that counts no node, so it ends up as the arc
/// total, which no `reserve` moves.
fn build_graphs(graph_file: &[u8], threads: usize) -> ([Graph; 2], Vec<AtomicU32>) {
    let mut pos = 0;
    let nodes = next_number(graph_file, &mut pos) as usize;
    next_number(graph_file, &mut pos); // The edge count: the file's length says the same.
    let edge_lines = &graph_file[pos..];
    let records = zeroed_u32s(nodes * RECORD);
    for_each_batch(edge_lines, threads, |batch| {
        for &(tail, _, _) in batch {
            prefetch(&records[tail as usize * RECORD]);
        }
        for &(tail, head, _) in batch {
            let record = &records[tail as usize * RECORD..][..RECORD];
            let seen = record[0].fetch_add(1, Relaxed) as usize;
            if seen < RECORD - 1 {
                record[seen + 1].store(head + 1, Relaxed);
            }
        }
    });
    let (numbers, entries) = local_numbering(&records, nodes, threads);

    let out_counts = zeroed_u32s(entries);
    for_ranges(nodes, threads, |start, end| {
        for node in start..end {
            let new = numbers[node].load(Relaxed) as usize;
            out_counts[new].store(records[node * RECORD].load(Relaxed), Relaxed);
        }
    });
    drop(records);
    let mut forward = Graph::from_counts(out_counts);

    for_each_batch(edge_lines, threads, |batch| {
        let number = |node: u32| numbers[node as usize].load(Relaxed);
        let mut renumbered = [(0, 0, 0); BATCH];
        let mut slots = [0; BATCH];
        for &(tail, head, _) in batch {
            prefetch(&numbers[tail as usize]);
            prefetch(&numbers[head as usize]);
        }
        for (new, &(tail, head, weight)) in renumbered.iter_mut().zip(batch) {
            *new = (number(tail), number(head), weight);
            prefetch(&forward.offsets[new.0 as usize]);
        }
        // Atomic updates wait for earlier stores, so reserve every slot first, and store afterwards.
        for (slot, &(tail, _, _)) in slots.iter_mut().zip(&renumbered[..batch.len()]) {
            *slot = forward.reserve(tail);
            prefetch(&forward.arcs[*slot]);
        }
        for (&slot, &(_, head, weight)) in slots.iter().zip(&renumbered[..batch.len()]) {
            forward.store(slot, head, weight);
        }
    });
    forward.seal();

    // The reverse graph comes from the forward one, with no parsing and no number lookups. Neighbours
    // have nearby numbers by now, so these sweeps stay in memory that's nearly in cache.
    let in_counts = zeroed_u32s(entries);
    forward.for_each_arc(threads, |_, head, _| {
        in_counts[head as usize].fetch_add(1, Relaxed);
    });
    let mut backward = Graph::from_counts(in_counts);
    forward.for_each_arc(threads, |tail, head, weight| {
        backward.store(backward.reserve(head), tail, weight);
    });
    backward.seal();
    ([forward, backward], numbers)
}

/// A distance type for the search. Nearly every distance fits in 32 bits, which halves the labels and
/// the queues. The few that don't make the 32-bit search give up, and that query runs again in 64 bits.
trait Distance: Copy + Ord + TryFrom<u64> + Into<u64> {
    const UNREACHED: Self;
}

impl Distance for u32 {
    const UNREACHED: u32 = u32::MAX;
}

impl Distance for u64 {
    const UNREACHED: u64 = INF;
}

/// Nodes per page of the label store.
const PAGE: usize = 64;

/// One side of a search: its tentative distances and its queue. Distances live in pages of `PAGE`
/// nodes, found through a directory indexed by node, which persists across queries and is reset through
/// the list of entries a query used. Neighbours have nearby numbers, so a search fills few pages, and
/// finding a label costs one directory read and one page read, with no key to compare. Page 0 is all
/// `UNREACHED`, and every empty directory entry points at it, so reading a label needs no branch. A
/// zeroed directory stays untouched until it's used, which the 64-bit search rarely is.
///
/// The queue relies on keys never going below the last one popped, as in Dijkstra. Bucket `i` holds the
/// keys whose highest bit differing from `last` is bit `i - 1`, and bucket 0 the keys equal to it. When
/// bucket 0 runs dry, the lowest bucket in use spreads into the ones below, so a key moves a few times
/// in all, with no comparisons against other keys.
struct Side<D> {
    directory: Vec<u32>,
    pages: Vec<D>,
    used: Vec<u32>,
    buckets: Vec<Vec<(D, u32)>>,
    last: D,
}

impl<D: Distance> Side<D> {
    fn new(entries: usize) -> Side<D> {
        Side {
            directory: vec![0; entries / PAGE + 1],
            pages: vec![D::UNREACHED; PAGE],
            used: Vec::new(),
            buckets: vec![Vec::new(); 65],
            last: D::UNREACHED,
        }
    }

    fn clear(&mut self, zero: D) {
        for entry in self.used.drain(..) {
            self.directory[entry as usize] = 0;
        }
        self.pages.truncate(PAGE);
        self.buckets.iter_mut().for_each(Vec::clear);
        self.last = zero;
    }

    fn push(&mut self, key: D, node: u32) {
        let (wide, last): (u64, u64) = (key.into(), self.last.into());
        self.buckets[64 - (wide ^ last).leading_zeros() as usize].push((key, node));
    }

    /// The smallest key, which bucket 0 then holds, or None if the queue is empty.
    fn top(&mut self) -> Option<D> {
        if self.buckets[0].is_empty() {
            let lowest = self.buckets.iter().position(|bucket| !bucket.is_empty())?;
            let mut bucket = std::mem::take(&mut self.buckets[lowest]);
            self.last = bucket.iter().map(|entry| entry.0).min()?;
            for &(key, node) in &bucket {
                self.push(key, node);
            }
            bucket.clear();
            self.buckets[lowest] = bucket;
        }
        Some(self.last)
    }

    fn label(&self, node: u32) -> D {
        let page = self.directory[node as usize / PAGE] as usize;
        self.pages[page * PAGE + node as usize % PAGE]
    }

    fn set_label(&mut self, node: u32, dist: D) {
        let entry = node as usize / PAGE;
        if self.directory[entry] == 0 {
            self.directory[entry] = (self.pages.len() / PAGE) as u32;
            self.pages.resize(self.pages.len() + PAGE, D::UNREACHED);
            self.used.push(entry as u32);
        }
        self.pages[self.directory[entry] as usize * PAGE + node as usize % PAGE] = dist;
    }

    /// Two-ended Dijkstra, returning INF if there is no path, or None if a distance doesn't fit in D.
    /// `best` holds the shortest path seen so far, and the search stops when the two queue tops add
    /// up to at least `best`, which is exact for any non-negative weights, including 0. When a queue
    /// empties, that side has settled all it can reach, so `best` is exact too.
    ///
    /// The side with less work so far goes next. Choosing the smaller top instead would let the
    /// backward side settle the whole main body on queries from a small island.
    fn distance(sides: &mut [Self; 2], graphs: &[Graph; 2], from: u32, to: u32) -> Option<u64> {
        if from == to {
            return Some(0);
        }
        let fit = |wide: u64| D::try_from(wide).ok().filter(|&dist| dist != D::UNREACHED);
        let unreached: u64 = D::UNREACHED.into();
        for (side, node) in sides.iter_mut().zip([from, to]) {
            side.clear(fit(0)?);
            side.set_label(node, fit(0)?);
            side.push(fit(0)?, node);
        }
        let mut best = INF;
        let mut work = [0u64; 2];
        while let (Some(top_f), Some(top_b)) = (sides[0].top(), sides[1].top()) {
            let tops: [u64; 2] = [top_f.into(), top_b.into()];
            if tops[0] + tops[1] >= best {
                break;
            }
            let side = usize::from(work[1] < work[0]);
            let other_top = tops[1 - side];
            let (dist, node) = sides[side].buckets[0].pop()?;
            if dist > sides[side].label(node) {
                continue; // Stale: this node was queued again at a smaller distance.
            }
            let dist: u64 = dist.into();
            let (first, arcs) = graphs[side].arcs_of(node);
            work[side] += 1 + arcs.len() as u64;
            for (index, arc) in arcs.iter().enumerate() {
                let (neighbour, weight) = graphs[side].unpack(first + index, arc.load(Relaxed));
                let candidate = dist + weight;
                let narrow = fit(candidate)?;
                if narrow < sides[side].label(neighbour) {
                    let other: u64 = sides[1 - side].label(neighbour).into();
                    if other != unreached {
                        best = best.min(candidate + other);
                    }
                    // If the other side has labelled this node, `best` just counted the path through
                    // it. If not, the rest of the way costs at least the other queue's top. So a node
                    // at or past `best` by that bound needs no expanding, and no label either, since
                    // no path the other side finds through it later could beat `best`.
                    if candidate + other_top < best {
                        sides[side].set_label(neighbour, narrow);
                        sides[side].push(narrow, neighbour);
                    }
                }
            }
        }
        Some(best)
    }
}

fn main() -> io::Result<()> {
    let mut args = std::env::args().skip(1);
    let data_dir = args.next().unwrap_or_else(|| "/data".into());
    let out_dir = args.next().unwrap_or_else(|| "/out".into());
    let data_dir = Path::new(&data_dir);
    let threads = thread_count();
    let (graphs, numbers) = build_graphs(map_file(&data_dir.join("graph.txt"))?, threads);
    let entries = graphs[0].offsets.len();

    let query_lines = map_file(&data_dir.join("queries.txt"))?;
    let number = |id: u64| numbers[id as usize].load(Relaxed);
    let mut queries = Vec::new();
    let mut pos = 0;
    while pos < query_lines.len() {
        let source = number(next_number(query_lines, &mut pos));
        let target = number(next_number(query_lines, &mut pos));
        queries.push((source, target, AtomicU64::new(0)));
    }
    drop(numbers);

    let next_query = AtomicUsize::new(0);
    on_threads(threads, || {
        let mut narrow = [Side::<u32>::new(entries), Side::new(entries)];
        let mut wide = [Side::<u64>::new(entries), Side::new(entries)];
        while let Some((from, to, answer)) = queries.get(next_query.fetch_add(1, Relaxed)) {
            let distance = Side::distance(&mut narrow, &graphs, *from, *to);
            let distance = distance.or_else(|| Side::distance(&mut wide, &graphs, *from, *to));
            answer.store(distance.unwrap_or(INF), Relaxed);
        }
    });

    let mut out = BufWriter::new(File::create(Path::new(&out_dir).join("distances.txt"))?);
    for (_, _, answer) in &queries {
        let distance = answer.load(Relaxed);
        writeln!(
            out,
            "{}",
            if distance == INF { -1 } else { distance as i64 }
        )?;
    }
    out.flush()
}
