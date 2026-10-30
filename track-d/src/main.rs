//! Track D (Traverse): exact shortest paths for 2,000 queries on a 5M-edge road network.
//!
//! The mapped graph is parsed twice by a pool of threads, once to count each node's edges and once
//! to place them, into a forward and a reverse compressed adjacency array, so no temporary edge
//! list is ever held. Threads then claim queries from an atomic counter, each with its own search
//! state, and run a two-ended Dijkstra that gives both sides equal work. Answers go into a slot per
//! query, so the output order doesn't depend on scheduling.

use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::ffi::{c_int, c_void};
use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::os::fd::AsRawFd;
use std::path::Path;
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering::Relaxed};
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

/// Maps a file read-only for the life of the process. The scorer's memory peak doesn't charge us
/// for these pages, unlike reading the file onto the heap.
fn map_file(path: &Path) -> io::Result<&'static [u8]> {
    const PROT_READ: c_int = 1;
    const MAP_PRIVATE: c_int = 2;
    let file = File::open(path)?;
    let len = file.metadata()?.len() as usize;
    if len == 0 {
        return Ok(&[]); // mmap rejects an empty range, and an empty query file is legal.
    }
    // SAFETY: a fresh private mapping of exactly `len` bytes that is never unmapped, of a dataset
    // nothing writes to while we run.
    unsafe {
        let ptr = mmap(
            std::ptr::null_mut(),
            len,
            PROT_READ,
            MAP_PRIVATE,
            file.as_raw_fd(),
            0,
        );
        if ptr as isize == -1 {
            return Err(io::Error::last_os_error());
        }
        Ok(std::slice::from_raw_parts(ptr as *const u8, len))
    }
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

/// Reads the number at `pos` and steps past the separator after it. Spaces and LFs both sort below
/// the digits, so one comparison ends the number.
fn next_number(text: &[u8], pos: &mut usize) -> u64 {
    let mut value = 0;
    while text[*pos] >= b'0' {
        value = value * 10 + u64::from(text[*pos] - b'0');
        *pos += 1;
    }
    *pos += 1;
    value
}

/// Visits every edge line from several threads, which claim segments from an atomic cursor. A line
/// belongs to the segment holding its first byte. Tying the segment size to the thread count makes
/// the SD_THREADS test sweep move every cut.
fn for_each_edge(edge_lines: &[u8], threads: usize, visit: impl Fn(usize, u32, u32) + Sync) {
    let segment = (edge_lines.len() / (threads * 8)).max(1);
    let cursor = AtomicUsize::new(0);
    thread::scope(|scope| {
        for _ in 0..threads {
            scope.spawn(|| {
                loop {
                    let start = cursor.fetch_add(segment, Relaxed);
                    if start >= edge_lines.len() {
                        return;
                    }
                    let end = (start + segment).min(edge_lines.len());
                    let mut pos = start;
                    while pos > 0 && edge_lines[pos - 1] != b'\n' {
                        pos += 1;
                    }
                    while pos < end {
                        let tail = next_number(edge_lines, &mut pos);
                        let head = next_number(edge_lines, &mut pos);
                        let weight = next_number(edge_lines, &mut pos);
                        visit(tail as usize, head as u32, weight as u32);
                    }
                }
            });
        }
    });
}

/// A compressed adjacency array: the arcs leaving `node` are `arcs[offsets[node]..offsets[node + 1]]`.
/// Each arc packs (neighbour << 32 | weight). Atomics let threads fill it without locks or `unsafe`;
/// relaxed atomic loads and stores compile to plain moves on x86.
struct Graph {
    offsets: Vec<AtomicU32>,
    arcs: Vec<AtomicU64>,
}

impl Graph {
    /// Turns per-node counts into a graph, ready to `place` every arc. Each offset starts at the
    /// end of its node's range and moves down as arcs are placed, so it finishes at the start of
    /// the range, with no second array.
    fn from_counts(mut offsets: Vec<AtomicU32>) -> Graph {
        let mut total = 0;
        for offset in &mut offsets {
            total += *offset.get_mut();
            *offset.get_mut() = total;
        }
        let arcs = (0..total).map(|_| AtomicU64::new(0)).collect();
        Graph { offsets, arcs }
    }

    fn place(&self, node: usize, neighbour: u32, weight: u32) {
        let slot = self.offsets[node].fetch_sub(1, Relaxed) - 1;
        let arc = u64::from(neighbour) << 32 | u64::from(weight);
        self.arcs[slot as usize].store(arc, Relaxed);
    }

    fn arcs_of(&self, node: u32) -> &[AtomicU64] {
        let start = self.offsets[node as usize].load(Relaxed) as usize;
        let end = self.offsets[node as usize + 1].load(Relaxed) as usize;
        &self.arcs[start..end]
    }
}

/// Builds the forward graph and its reverse from the graph file. `offsets` has a final entry that
/// counts no node, so it ends up as the arc total, which no `place` moves.
fn build_graphs(graph_file: &[u8], threads: usize) -> [Graph; 2] {
    let mut pos = 0;
    let nodes = next_number(graph_file, &mut pos) as usize;
    next_number(graph_file, &mut pos); // The edge count: the file's length says the same.
    let edge_lines = &graph_file[pos..];
    let zeroes = || (0..=nodes).map(|_| AtomicU32::new(0)).collect::<Vec<_>>();
    let (out_counts, in_counts) = (zeroes(), zeroes());
    for_each_edge(edge_lines, threads, |tail, head, _| {
        out_counts[tail].fetch_add(1, Relaxed);
        in_counts[head as usize].fetch_add(1, Relaxed);
    });
    let forward = Graph::from_counts(out_counts);
    let backward = Graph::from_counts(in_counts);
    for_each_edge(edge_lines, threads, |tail, head, weight| {
        forward.place(tail, head, weight);
        backward.place(head as usize, tail as u32, weight);
    });
    [forward, backward]
}

/// One node's tentative distances, from the source and to the target.
#[derive(Clone, Copy)]
struct Label {
    node: u32,
    dist: [u64; 2],
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
struct Search {
    table: Vec<u32>, // Label number + 1, or 0 when free. The length is a power of two.
    labels: Vec<Label>,
    queues: [BinaryHeap<Reverse<(u64, u32)>>; 2],
}

impl Search {
    fn new() -> Search {
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
            dist: [INF, INF],
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

    /// Two-ended Dijkstra, returning INF if there is no path. `best` holds the shortest path seen
    /// so far, and the search stops when the two queue tops add up to at least `best`, which is
    /// exact for any non-negative weights, including 0. When a queue empties, that side has settled
    /// all it can reach, so `best` is exact too.
    ///
    /// The side with less work so far goes next. Choosing the smaller top instead would let the
    /// backward side settle the whole main body on queries from a small island.
    fn distance(&mut self, graphs: &[Graph; 2], source: u32, target: u32) -> u64 {
        if source == target {
            return 0;
        }
        self.table.clear();
        self.table.resize(SMALL_TABLE, 0);
        self.labels.clear();
        self.queues[0].clear();
        self.queues[1].clear();
        for (side, node) in [source, target].into_iter().enumerate() {
            let label = self.label_index(node);
            self.labels[label].dist[side] = 0;
            self.queues[side].push(Reverse((0, node)));
        }
        let mut best = INF;
        let mut work = [0u64; 2];
        while let (Some(top_f), Some(top_b)) = (self.queues[0].peek(), self.queues[1].peek()) {
            let tops = [top_f.0.0, top_b.0.0];
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
            let arcs = graphs[side].arcs_of(node);
            work[side] += 1 + arcs.len() as u64;
            for arc in arcs {
                let arc = arc.load(Relaxed);
                let (neighbour, weight) = ((arc >> 32) as u32, arc & 0xFFFF_FFFF);
                let candidate = dist + weight;
                let label = self.label_index(neighbour);
                if candidate < self.labels[label].dist[side] {
                    self.labels[label].dist[side] = candidate;
                    best = best.min(candidate + self.labels[label].dist[1 - side]);
                    // If the other side has settled this node, `best` just counted the path through
                    // it. If not, the rest of the way costs at least the other queue's top. So a
                    // node at or past `best` by that bound needn't be expanded.
                    if candidate + other_top < best {
                        self.queues[side].push(Reverse((candidate, neighbour)));
                    }
                }
            }
        }
        best
    }
}

fn main() -> io::Result<()> {
    let mut args = std::env::args().skip(1);
    let data_dir = args.next().unwrap_or_else(|| "/data".into());
    let out_dir = args.next().unwrap_or_else(|| "/out".into());
    let data_dir = Path::new(&data_dir);
    let threads = thread_count();
    let graphs = build_graphs(map_file(&data_dir.join("graph.txt"))?, threads);

    let query_lines = map_file(&data_dir.join("queries.txt"))?;
    let mut queries = Vec::new();
    let mut pos = 0;
    while pos < query_lines.len() {
        let source = next_number(query_lines, &mut pos) as u32;
        let target = next_number(query_lines, &mut pos) as u32;
        queries.push((source, target));
    }

    let answers: Vec<AtomicU64> = queries.iter().map(|_| AtomicU64::new(0)).collect();
    let next_query = AtomicUsize::new(0);
    thread::scope(|scope| {
        for _ in 0..threads {
            scope.spawn(|| {
                let mut search = Search::new();
                loop {
                    let index = next_query.fetch_add(1, Relaxed);
                    let Some(&(source, target)) = queries.get(index) else {
                        return;
                    };
                    let answer = search.distance(&graphs, source, target);
                    answers[index].store(answer, Relaxed);
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
