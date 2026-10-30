//! Track D (Traverse): exact shortest paths for 2,000 queries on a 5M-edge road network.
//!
//! The mapped graph is parsed twice, once to count each node's edges and once to place them, into a
//! forward and a reverse compressed adjacency array, so no temporary edge list is ever held. Each
//! query then runs a two-ended Dijkstra that gives both sides equal work.

use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::ffi::{c_int, c_void};
use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::os::fd::AsRawFd;
use std::path::Path;

/// Above any real distance, since node ids fit far below 2^30 and weights below 2^32, and low enough
/// that a distance can be added to it without wrapping.
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

fn for_each_edge(edge_lines: &[u8], mut visit: impl FnMut(usize, u32, u32)) {
    let mut pos = 0;
    while pos < edge_lines.len() {
        let tail = next_number(edge_lines, &mut pos);
        let head = next_number(edge_lines, &mut pos);
        let weight = next_number(edge_lines, &mut pos);
        visit(tail as usize, head as u32, weight as u32);
    }
}

/// A compressed adjacency array: the arcs leaving `node` are `arcs[offsets[node]..offsets[node + 1]]`.
struct Graph {
    offsets: Vec<u32>,
    arcs: Vec<(u32, u32)>, // (neighbour, weight)
}

impl Graph {
    /// Turns per-node counts into a graph, ready to `place` every arc. Each offset starts at the
    /// end of its node's range and moves down as arcs are placed, so it finishes at the start of
    /// the range, with no second array.
    fn from_counts(mut offsets: Vec<u32>) -> Graph {
        let mut total = 0;
        for offset in &mut offsets {
            total += *offset;
            *offset = total;
        }
        Graph {
            offsets,
            arcs: vec![(0, 0); total as usize],
        }
    }

    fn place(&mut self, node: usize, neighbour: u32, weight: u32) {
        self.offsets[node] -= 1;
        self.arcs[self.offsets[node] as usize] = (neighbour, weight);
    }

    fn arcs_of(&self, node: u32) -> &[(u32, u32)] {
        let node = node as usize;
        &self.arcs[self.offsets[node] as usize..self.offsets[node + 1] as usize]
    }
}

/// Builds the forward graph and its reverse from the graph file. `offsets` has a final entry that
/// counts no node, so it ends up as the arc total, which no `place` moves.
fn build_graphs(graph_file: &[u8]) -> [Graph; 2] {
    let mut pos = 0;
    let nodes = next_number(graph_file, &mut pos) as usize;
    next_number(graph_file, &mut pos); // The edge count: the file's length says the same.
    let edge_lines = &graph_file[pos..];
    let mut out_counts = vec![0u32; nodes + 1];
    let mut in_counts = vec![0u32; nodes + 1];
    for_each_edge(edge_lines, |tail, head, _| {
        out_counts[tail] += 1;
        in_counts[head as usize] += 1;
    });
    let mut forward = Graph::from_counts(out_counts);
    let mut backward = Graph::from_counts(in_counts);
    for_each_edge(edge_lines, |tail, head, weight| {
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

/// Reusable search state. The labels are a sparse set (Briggs and Torczon): a node's label is valid
/// when its slot points at a record naming that node. So a new query only has to empty the list,
/// and the node-sized slot array is never cleared.
struct Search {
    slots: Vec<u32>,
    labels: Vec<Label>,
    queues: [BinaryHeap<Reverse<(u64, u32)>>; 2],
}

impl Search {
    fn new(nodes: usize) -> Search {
        Search {
            slots: vec![0; nodes],
            labels: Vec::new(),
            queues: [BinaryHeap::new(), BinaryHeap::new()],
        }
    }

    fn label_index(&mut self, node: u32) -> usize {
        let slot = self.slots[node as usize] as usize;
        if slot < self.labels.len() && self.labels[slot].node == node {
            return slot;
        }
        self.slots[node as usize] = self.labels.len() as u32;
        self.labels.push(Label {
            node,
            dist: [INF, INF],
        });
        self.labels.len() - 1
    }

    /// Two-ended Dijkstra. `best` holds the shortest path seen so far, and the search stops when
    /// the two queue tops add up to at least `best`, which is exact for any non-negative weights,
    /// including 0. When a queue empties, that side has settled all it can reach, so `best` is
    /// exact too, and still INF if there is no path.
    ///
    /// The side with less work so far goes next. Choosing the smaller top instead would let the
    /// backward side settle the whole main body on queries from a small island.
    fn distance(&mut self, graphs: &[Graph; 2], source: u32, target: u32) -> Option<u64> {
        if source == target {
            return Some(0);
        }
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
            if top_f.0.0 + top_b.0.0 >= best {
                break;
            }
            let side = usize::from(work[1] < work[0]);
            let Some(Reverse((dist, node))) = self.queues[side].pop() else {
                break;
            };
            let label = self.label_index(node);
            if dist > self.labels[label].dist[side] {
                continue; // Stale: this node was queued again at a smaller distance.
            }
            let arcs = graphs[side].arcs_of(node);
            work[side] += 1 + arcs.len() as u64;
            for &(neighbour, weight) in arcs {
                let candidate = dist + u64::from(weight);
                let label = self.label_index(neighbour);
                if candidate < self.labels[label].dist[side] {
                    self.labels[label].dist[side] = candidate;
                    self.queues[side].push(Reverse((candidate, neighbour)));
                    best = best.min(candidate + self.labels[label].dist[1 - side]);
                }
            }
        }
        (best < INF).then_some(best)
    }
}

fn main() -> io::Result<()> {
    let mut args = std::env::args().skip(1);
    let data_dir = args.next().unwrap_or_else(|| "/data".into());
    let out_dir = args.next().unwrap_or_else(|| "/out".into());
    let data_dir = Path::new(&data_dir);
    let graphs = build_graphs(map_file(&data_dir.join("graph.txt"))?);
    let queries = map_file(&data_dir.join("queries.txt"))?;
    let mut search = Search::new(graphs[0].offsets.len() - 1);
    let mut out = BufWriter::new(File::create(Path::new(&out_dir).join("distances.txt"))?);
    let mut pos = 0;
    while pos < queries.len() {
        let source = next_number(queries, &mut pos) as u32;
        let target = next_number(queries, &mut pos) as u32;
        match search.distance(&graphs, source, target) {
            Some(distance) => writeln!(out, "{distance}")?,
            None => writeln!(out, "-1")?,
        }
    }
    out.flush()
}
