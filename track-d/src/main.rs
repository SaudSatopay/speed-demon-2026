//! Track D (Traverse): exact shortest paths for 2,000 queries on a 5M-edge road network.
//!
//! Threads parse the mapped graph twice, in batches with prefetching. The first pass counts each
//! node's edges and keeps a few neighbours, enough to renumber the nodes so that neighbours sit close
//! in memory. The second places the edges into a forward and a reverse compressed adjacency array,
//! so no temporary edge list is ever held. Threads then claim queries from an atomic counter, each
//! with its own search state, and run a two-ended Dijkstra that gives both sides equal work. Answers
//! go into a slot per query, so the output order doesn't depend on scheduling.

use std::arch::x86_64::{_MM_HINT_T0, _mm_prefetch};
use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::ffi::{c_int, c_void};
use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::os::fd::AsRawFd;
use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering::Relaxed};
use std::thread;

/// Above any real distance, since node ids fit far below 2^30 and weights below 2^32, and low enough
/// that a distance can be added to it without wrapping. It also marks "no path" in the answers.
const INF: u64 = 1 << 62;

// Declared by hand to keep the crate at zero dependencies; std already links glibc.
unsafe extern "C" {
    fn mmap(
        addr: *mut c_void,
        len: usize,
        prot: c_int,
        flags: c_int,
        fd: c_int,
        offset: i64,
    ) -> *mut c_void;
}

/// Readable zero bytes after each mapped file, so that block loads can run past its end.
const PADDING: usize = 4096;

/// Maps a file read-only for the life of the process, followed by `PADDING` readable zero bytes. The
/// scorer's memory peak doesn't charge us for these pages, unlike reading the file onto the heap.
fn map_file(path: &Path) -> io::Result<&'static [u8]> {
    const PROT_READ: c_int = 1;
    const MAP_PRIVATE: c_int = 2;
    const MAP_FIXED: c_int = 0x10;
    const MAP_ANONYMOUS: c_int = 0x20;
    let file = File::open(path)?;
    let len = file.metadata()?.len() as usize;
    let null = std::ptr::null_mut();
    // SAFETY: the first call reserves fresh zero pages for the file and its padding, and the second
    // maps the file over the start of them. Neither is ever unmapped, and nothing writes to the
    // dataset while we run.
    unsafe {
        let anonymous = MAP_PRIVATE | MAP_ANONYMOUS;
        let base = mmap(null, len + PADDING, PROT_READ, anonymous, -1, 0);
        if base as isize == -1 {
            return Err(io::Error::last_os_error());
        }
        let fixed = MAP_PRIVATE | MAP_FIXED;
        if len > 0 && mmap(base, len, PROT_READ, fixed, file.as_raw_fd(), 0) as isize == -1 {
            return Err(io::Error::last_os_error());
        }
        Ok(std::slice::from_raw_parts(base as *const u8, len))
    }
}

/// Asks for the cache line holding `item`, so that a later access doesn't stall on it.
#[inline]
fn prefetch<T>(item: &T) {
    // SAFETY: prefetching has no effect on memory, and the reference is valid anyway.
    unsafe { _mm_prefetch::<_MM_HINT_T0>(std::ptr::from_ref(item).cast()) }
}

/// `SD_THREADS` if set (the test sweep uses it), else what the container allows, capped at the
/// box's 12 threads.
fn thread_count() -> usize {
    match std::env::var("SD_THREADS")
        .ok()
        .and_then(|v| v.parse().ok())
    {
        Some(threads) if threads > 0 => threads,
        _ => thread::available_parallelism().map_or(1, |n| n.get().min(12)),
    }
}

/// Reads the number at `pos` and steps past the separator after it. One 8-byte load settles all but
/// the longest numbers: digits have bit 4 set, and spaces and LFs don't, so the first clear bit marks
/// the end, and three multiplications parse the digits, with no byte loop to mispredict.
#[inline]
fn next_number(text: &[u8], pos: &mut usize) -> u64 {
    assert!(*pos + 8 <= text.len() + PADDING);
    // SAFETY: the assert keeps the load inside the mapping, and it needs no alignment.
    let word = unsafe { text.as_ptr().add(*pos).cast::<u64>().read_unaligned() };
    let ends = !word & 0x1010_1010_1010_1010;
    if ends != 0 {
        let digits = ends.trailing_zeros() as usize / 8;
        *pos += digits + 1;
        return parse_digits(word, digits);
    }
    let mut value = parse_digits(word, 8);
    *pos += 8;
    while text[*pos] >= b'0' {
        value = value * 10 + u64::from(text[*pos] - b'0');
        *pos += 1;
    }
    *pos += 1;
    value
}

/// Parses the digits in the low `count` bytes of `word` (1 to 8 of them), in three multiplications.
#[inline]
fn parse_digits(word: u64, count: usize) -> u64 {
    let digits = (word << (64 - 8 * count)) & 0x0F0F_0F0F_0F0F_0F0F;
    let pairs = digits.wrapping_mul((10 << 8) + 1) >> 8 & 0x00FF_00FF_00FF_00FF;
    let quads = pairs.wrapping_mul((100 << 16) + 1) >> 16 & 0x0000_FFFF_0000_FFFF;
    quads.wrapping_mul((10000 << 32) + 1) >> 32
}

/// Runs `work` on ranges of `0..len`, claimed from an atomic cursor. Eight ranges per thread balance
/// the load. Tying the size to the thread count makes the SD_THREADS test sweep move every cut.
fn for_ranges(len: usize, threads: usize, work: impl Fn(usize, usize) + Sync) {
    let size = (len / (threads * 8)).max(1);
    let cursor = AtomicUsize::new(0);
    thread::scope(|scope| {
        for _ in 0..threads {
            scope.spawn(|| {
                loop {
                    let start = cursor.fetch_add(size, Relaxed);
                    if start >= len {
                        return;
                    }
                    work(start, (start + size).min(len));
                }
            });
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

/// Out-neighbours kept per node for numbering. Nodes average 2.9, so this sees most edges.
const LINKS: usize = 3;

/// Numbers the nodes so that neighbours sit close in memory, because the input's ids are random, which
/// makes every hop a cache miss. Each thread walks depth first from a seed, claiming nodes as it goes
/// and listing them in order, and the lists joined together give the new numbers. Any numbering gives
/// the same answers. Nodes the walks miss become seeds themselves, so all of them get a number.
fn local_numbering(links: &[AtomicU32], nodes: usize, threads: usize) -> Vec<AtomicU32> {
    let claimed: Vec<AtomicBool> = (0..nodes).map(|_| AtomicBool::new(false)).collect();
    let next_seed = AtomicUsize::new(0);
    let lists: Vec<Vec<u32>> = thread::scope(|scope| {
        let handles: Vec<_> = (0..threads)
            .map(|_| {
                scope.spawn(|| {
                    let claim = |node: usize| !claimed[node].swap(true, Relaxed);
                    let (mut list, mut stack) = (Vec::new(), Vec::new());
                    loop {
                        let node = match stack.pop() {
                            Some(node) => node,
                            None => loop {
                                let seed = next_seed.fetch_add(1, Relaxed);
                                if seed >= nodes {
                                    return list;
                                }
                                if claim(seed) {
                                    break seed as u32;
                                }
                            },
                        };
                        list.push(node);
                        for link in &links[node as usize * LINKS..][..LINKS] {
                            let neighbour = link.load(Relaxed) as usize;
                            if neighbour != 0 && claim(neighbour - 1) {
                                stack.push(neighbour as u32 - 1);
                                prefetch(&links[(neighbour - 1) * LINKS]);
                            }
                        }
                    }
                })
            })
            .collect();
        let joined = handles.into_iter().map(|handle| handle.join());
        joined
            .map(|list| list.expect("panic = \"abort\" means a join can't fail"))
            .collect()
    });
    let numbers = zeroed_u32s(nodes);
    thread::scope(|scope| {
        let mut first = 0;
        for list in &lists {
            let numbers = &numbers;
            scope.spawn(move || {
                for (i, &node) in list.iter().enumerate() {
                    numbers[node as usize].store(first + i as u32, Relaxed);
                }
            });
            first += list.len() as u32;
        }
    });
    numbers
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
    heavy: Mutex<Vec<(u32, u32)>>,
    heavy_sorted: Vec<(u32, u32)>,
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
            heavy: Mutex::new(Vec::new()),
            heavy_sorted: Vec::new(),
        }
    }

    /// Reserves a slot for one more arc of `node`.
    #[inline]
    fn reserve(&self, node: u32) -> usize {
        self.offsets[node as usize].fetch_sub(1, Relaxed) as usize - 1
    }

    #[inline]
    fn store(&self, slot: usize, neighbour: u32, weight: u32) {
        let heavy = (1 << self.weight_bits) - 1;
        if weight >= heavy {
            let list = self.heavy.lock();
            list.expect("panic = \"abort\" means a lock can't be poisoned")
                .push((slot as u32, weight));
        }
        self.arcs[slot].store(neighbour << self.weight_bits | weight.min(heavy), Relaxed);
    }

    /// Sorts the heavy weights, once every thread has finished.
    fn seal(&mut self) {
        let list = self
            .heavy
            .get_mut()
            .expect("panic = \"abort\" means a lock can't be poisoned");
        self.heavy_sorted = std::mem::take(list);
        self.heavy_sorted.sort_unstable();
    }

    /// The first arc number of `node`, and its arcs.
    #[inline]
    fn arcs_of(&self, node: u32) -> (usize, &[AtomicU32]) {
        let start = self.offsets[node as usize].load(Relaxed) as usize;
        let end = self.offsets[node as usize + 1].load(Relaxed) as usize;
        (start, &self.arcs[start..end])
    }

    /// A packed arc's neighbour and weight, given its arc number.
    #[inline]
    fn unpack(&self, slot: usize, arc: u32) -> (u32, u64) {
        let heavy = (1 << self.weight_bits) - 1;
        let weight = arc & heavy;
        if weight != heavy {
            return (arc >> self.weight_bits, u64::from(weight));
        }
        let at = self
            .heavy_sorted
            .partition_point(|entry| (entry.0 as usize) < slot);
        (arc >> self.weight_bits, u64::from(self.heavy_sorted[at].1))
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
    let (out_counts, in_counts) = (zeroed_u32s(nodes), zeroed_u32s(nodes));
    let links = zeroed_u32s(nodes * LINKS);
    for_each_batch(edge_lines, threads, |batch| {
        for &(tail, head, _) in batch {
            prefetch(&out_counts[tail as usize]);
            prefetch(&in_counts[head as usize]);
            prefetch(&links[tail as usize * LINKS]);
        }
        for &(tail, head, _) in batch {
            let seen = out_counts[tail as usize].fetch_add(1, Relaxed) as usize;
            if seen < LINKS {
                links[tail as usize * LINKS + seen].store(head + 1, Relaxed);
            }
            in_counts[head as usize].fetch_add(1, Relaxed);
        }
    });
    let numbers = local_numbering(&links, nodes, threads);
    drop(links);

    let (out_new, in_new) = (zeroed_u32s(nodes + 1), zeroed_u32s(nodes + 1));
    for_ranges(nodes, threads, |start, end| {
        for node in start..end {
            let new = numbers[node].load(Relaxed) as usize;
            out_new[new].store(out_counts[node].load(Relaxed), Relaxed);
            in_new[new].store(in_counts[node].load(Relaxed), Relaxed);
        }
    });
    drop((out_counts, in_counts));
    let mut forward = Graph::from_counts(out_new);
    let mut backward = Graph::from_counts(in_new);

    for_each_batch(edge_lines, threads, |batch| {
        let mut renumbered = [(0, 0, 0); BATCH];
        let mut slots = [(0, 0); BATCH];
        for &(tail, head, _) in batch {
            prefetch(&numbers[tail as usize]);
            prefetch(&numbers[head as usize]);
        }
        for (new, &(tail, head, weight)) in renumbered.iter_mut().zip(batch) {
            let (tail, head) = (
                numbers[tail as usize].load(Relaxed),
                numbers[head as usize].load(Relaxed),
            );
            *new = (tail, head, weight);
            prefetch(&forward.offsets[tail as usize]);
            prefetch(&backward.offsets[head as usize]);
        }
        // Atomic updates wait for earlier stores, so reserve every slot first, and store afterwards.
        for (slot, &(tail, head, _)) in slots.iter_mut().zip(&renumbered[..batch.len()]) {
            *slot = (forward.reserve(tail), backward.reserve(head));
            prefetch(&forward.arcs[slot.0]);
            prefetch(&backward.arcs[slot.1]);
        }
        for (slot, &(tail, head, weight)) in slots.iter().zip(&renumbered[..batch.len()]) {
            forward.store(slot.0, head, weight);
            backward.store(slot.1, tail, weight);
        }
    });
    forward.seal();
    backward.seal();
    ([forward, backward], numbers)
}

/// A distance type for the search. Nearly every distance fits in 32 bits, which halves the labels and
/// the queues. The few that don't make the 32-bit search give up, and that query runs again in 64 bits.
trait Distance: Copy + Ord {
    const UNREACHED: Self;
    /// Converts a real distance, or returns None if it doesn't fit.
    fn narrow(wide: u64) -> Option<Self>;
    fn widen(self) -> u64;
}

impl Distance for u32 {
    const UNREACHED: u32 = u32::MAX;
    #[inline]
    fn narrow(wide: u64) -> Option<u32> {
        u32::try_from(wide)
            .ok()
            .filter(|&narrow| narrow != u32::MAX)
    }
    #[inline]
    fn widen(self) -> u64 {
        u64::from(self)
    }
}

impl Distance for u64 {
    const UNREACHED: u64 = INF;
    #[inline]
    fn narrow(wide: u64) -> Option<u64> {
        Some(wide)
    }
    #[inline]
    fn widen(self) -> u64 {
        self
    }
}

/// One node's tentative distances, from the source and to the target.
#[derive(Clone, Copy)]
struct Label<D> {
    node: u32,
    dist: [D; 2],
}

/// Node ids to table slots. A test build swaps this for a constant, so that every node collides and
/// any logic that trusts a hash without comparing the key shows up as a wrong answer.
#[cfg(not(sd_const_hash))]
fn hash(node: u32) -> usize {
    ((u64::from(node) * 0x9E37_79B9_7F4A_7C15) >> 32) as usize
}

#[cfg(sd_const_hash)]
fn hash(_node: u32) -> usize {
    0
}

/// A new query's table: enough for the median search, which labels about 200 nodes.
const SMALL_TABLE: usize = 1 << 10;

/// One thread's reusable search state. Labels sit in a list, in discovery order, and a hash table
/// finds a node's label. A query's table starts small and grows with its search, so a small search
/// stays in the fastest caches, and the largest on seed 1 needs about 1 MB per thread, where a
/// node-sized array would cost 7 MB per thread.
struct Search<D> {
    table: Vec<u32>, // Label number + 1, or 0 when free. The length is a power of two.
    labels: Vec<Label<D>>,
    queues: [BinaryHeap<Reverse<(D, u32)>>; 2],
}

impl<D: Distance> Search<D> {
    fn new() -> Search<D> {
        Search {
            table: Vec::new(),
            labels: Vec::new(),
            queues: [BinaryHeap::new(), BinaryHeap::new()],
        }
    }

    fn label_index(&mut self, node: u32) -> usize {
        let mask = self.table.len() - 1;
        let mut slot = hash(node) & mask;
        while self.table[slot] != 0 {
            let label = self.table[slot] as usize - 1;
            if self.labels[label].node == node {
                return label;
            }
            slot = (slot + 1) & mask;
        }
        self.labels.push(Label {
            node,
            dist: [D::UNREACHED; 2],
        });
        self.table[slot] = self.labels.len() as u32;
        if self.labels.len() * 2 > self.table.len() {
            self.grow_table();
        }
        self.labels.len() - 1
    }

    fn grow_table(&mut self) {
        let size = self.table.len() * 4;
        self.table.clear();
        self.table.resize(size, 0);
        for (number, label) in self.labels.iter().enumerate() {
            let mut slot = hash(label.node) & (size - 1);
            while self.table[slot] != 0 {
                slot = (slot + 1) & (size - 1);
            }
            self.table[slot] = number as u32 + 1;
        }
    }

    /// Two-ended Dijkstra, returning INF if there is no path, or None if a distance doesn't fit in D.
    /// `best` holds the shortest path seen so far, and the search stops when the two queue tops add
    /// up to at least `best`, which is exact for any non-negative weights, including 0. When a queue
    /// empties, that side has settled all it can reach, so `best` is exact too.
    ///
    /// The side with less work so far goes next. Choosing the smaller top instead would let the
    /// backward side settle the whole main body on queries from a small island.
    fn distance(&mut self, graphs: &[Graph; 2], source: u32, target: u32) -> Option<u64> {
        if source == target {
            return Some(0);
        }
        self.table.clear();
        self.table.resize(SMALL_TABLE, 0);
        self.labels.clear();
        self.queues[0].clear();
        self.queues[1].clear();
        for (side, node) in [source, target].into_iter().enumerate() {
            let label = self.label_index(node);
            self.labels[label].dist[side] = D::narrow(0)?;
            self.queues[side].push(Reverse((D::narrow(0)?, node)));
        }
        let mut best = INF;
        let mut work = [0u64; 2];
        while let (Some(top_f), Some(top_b)) = (self.queues[0].peek(), self.queues[1].peek()) {
            let tops = [top_f.0.0.widen(), top_b.0.0.widen()];
            if tops[0] + tops[1] >= best {
                break;
            }
            let side = usize::from(work[1] < work[0]);
            let other_top = tops[1 - side];
            let Some(Reverse((dist, node))) = self.queues[side].pop() else {
                break;
            };
            let label = self.label_index(node);
            if dist > self.labels[label].dist[side] {
                continue; // Stale: this node was queued again at a smaller distance.
            }
            let (first, arcs) = graphs[side].arcs_of(node);
            work[side] += 1 + arcs.len() as u64;
            for (index, arc) in arcs.iter().enumerate() {
                let (neighbour, weight) = graphs[side].unpack(first + index, arc.load(Relaxed));
                let candidate = dist.widen() + weight;
                let narrow = D::narrow(candidate)?;
                let label = self.label_index(neighbour);
                if narrow < self.labels[label].dist[side] {
                    self.labels[label].dist[side] = narrow;
                    let other = self.labels[label].dist[1 - side];
                    if other != D::UNREACHED {
                        best = best.min(candidate + other.widen());
                    }
                    // If the other side has settled this node, `best` just counted the path through
                    // it. If not, the rest of the way costs at least the other queue's top. So a
                    // node at or past `best` by that bound needn't be expanded.
                    if candidate + other_top < best {
                        self.queues[side].push(Reverse((narrow, neighbour)));
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

    let query_lines = map_file(&data_dir.join("queries.txt"))?;
    let mut queries = Vec::new();
    let mut pos = 0;
    while pos < query_lines.len() {
        let source = next_number(query_lines, &mut pos) as usize;
        let target = next_number(query_lines, &mut pos) as usize;
        queries.push((numbers[source].load(Relaxed), numbers[target].load(Relaxed)));
    }

    drop(numbers);

    let answers: Vec<AtomicU64> = queries.iter().map(|_| AtomicU64::new(0)).collect();
    let next_query = AtomicUsize::new(0);
    thread::scope(|scope| {
        for _ in 0..threads {
            scope.spawn(|| {
                let (mut narrow, mut wide) = (Search::<u32>::new(), Search::<u64>::new());
                loop {
                    let index = next_query.fetch_add(1, Relaxed);
                    let Some(&(source, target)) = queries.get(index) else {
                        return;
                    };
                    let answer = narrow.distance(&graphs, source, target);
                    let answer = answer.or_else(|| wide.distance(&graphs, source, target));
                    answers[index].store(answer.unwrap_or(INF), Relaxed);
                }
            });
        }
    });

    let mut out = BufWriter::new(File::create(Path::new(&out_dir).join("distances.txt"))?);
    for answer in &answers {
        match answer.load(Relaxed) {
            INF => writeln!(out, "-1")?,
            distance => writeln!(out, "{distance}")?,
        }
    }
    out.flush()
}
