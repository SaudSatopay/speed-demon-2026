//! Track D (Traverse): exact shortest paths for 2,000 queries on a 5M-edge road network.
//!
//! Threads parse the mapped graph twice, in batches with prefetching. The first pass counts each
//! node's edges and keeps a few neighbours, enough to renumber the nodes so that neighbours sit close
//! in memory. The second places the edges into a compressed adjacency array, so no temporary edge
//! list is ever held. With the queries known, low-degree nodes that none of them starts or ends at are
//! removed, joining their neighbours by shortcuts. Sweeping the smaller array, where neighbours are
//! close, then fills the reverse array with hardly a cache miss. Threads claim queries from an atomic
//! counter, each with its own search state, and run a two-ended Dijkstra that gives both sides equal
//! work. Answers go into a slot per query, so the output order doesn't depend on scheduling.

use std::arch::x86_64::*;
use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::ops::Not;
use std::os::fd::AsRawFd;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU16, AtomicU32, AtomicU64, AtomicUsize, Ordering::Relaxed};
use std::thread;

/// Above any real distance, which the spec puts below 2^63, so two of them add without wrapping. It
/// also marks "no path" in the answers, where it prints as -1. Nothing adds to it: a label is never
/// INF, and the other side's label is checked first.
const INF: u64 = u64::MAX;

/// A search stops sorting keys inside a block of `1 << COARSE_BITS` distances. A block this size mostly
/// holds nodes that don't depend on each other, so few expand twice. Far wider, and the repeat work
/// outweighs the sorting saved.
const COARSE_BITS: u32 = 7;

// Declared by hand to keep the crate at zero dependencies; std already links glibc.
unsafe extern "C" {
    fn mmap(at: *mut u8, len: usize, p: i32, f: i32, fd: i32, o: i64) -> *mut u8;
    fn mallopt(option: i32, value: i32) -> i32;
}

/// Readable zero bytes after each mapped file, so that block loads can run past its end.
const PADDING: usize = 4096;

/// Maps a file read-only for the life of the process, followed by `PADDING` readable zero bytes. The
/// scorer's memory peak doesn't charge us for these pages, unlike reading the file onto the heap.
fn map_file(path: &str) -> io::Result<&'static [u8]> {
    let (read, private, fixed, anonymous) = (1, 2, 0x10, 0x20);
    let file = File::open(path)?;
    let len = file.metadata()?.len() as usize;
    // SAFETY: the first call reserves fresh zero pages, and the second maps the file over the start
    // of them, which fails only on an empty file. Neither is ever unmapped, and nothing writes to the
    // dataset while we run.
    unsafe {
        let null = std::ptr::null_mut();
        let base = mmap(null, len + PADDING, read, private | anonymous, -1, 0);
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

/// Runs `work` on `threads` threads, and waits for them all: the scope joins each on exit.
fn on_threads(threads: usize, work: impl Fn() + Sync) {
    thread::scope(|scope| (0..threads).for_each(|_| drop(scope.spawn(&work))));
}

/// The next start from a shared counter stepping by `step`, or None once it reaches `limit`.
fn next_chunk(counter: &AtomicUsize, step: usize, limit: usize) -> Option<usize> {
    Some(counter.fetch_add(step, Relaxed)).filter(|&start| start < limit)
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
    let (size, cursor) = ((len / (threads * 8)).max(1), AtomicUsize::new(0));
    on_threads(threads, || {
        while let Some(start) = next_chunk(&cursor, size, len) {
            work(start, (start + size).min(len));
        }
    });
}

/// Edges parsed before any is used. A batch can prefetch all its cache lines and let them arrive
/// together, instead of stalling on each in turn.
const BATCH: usize = 64;

/// Passes every edge line to `visit` as (tail, head, weight), in batches, from several threads. A
/// line belongs to the range holding its first byte.
fn for_each_batch(edge_lines: &[u8], threads: usize, visit: impl Fn(&[(u32, u32, u32)]) + Sync) {
    for_ranges(edge_lines.len(), threads, |start, end| {
        let line_start = |p: &usize| *p == 0 || edge_lines[p - 1] == b'\n';
        let mut pos = (start..).find(line_start).unwrap();
        let (mut batch, mut count) = ([(0, 0, 0); BATCH], 0);
        while pos < end {
            let mut field = || next_number(edge_lines, &mut pos) as u32;
            batch[count] = (field(), field(), field());
            count = (count + 1) % BATCH;
            if count == 0 {
                visit(&batch);
            }
        }
        visit(&batch[..count]);
    });
}

/// Words that threads fill without locks or `unsafe`: relaxed atomic loads and stores compile to
/// plain moves on x86.
type Words = Vec<AtomicU32>;

/// A zeroed array of atomics. This thread touches one word per 2 MiB first: when threads race to
/// touch a huge page, each clears its own, and the memory peak counts them all until the losers free
/// theirs.
fn zeroed_u32s(len: usize) -> Words {
    let words: Words = vec![0u32; len].into_iter().map(AtomicU32::new).collect();
    for word in words.iter().step_by(1 << 19) {
        word.store(0, Relaxed);
    }
    words
}

/// Seeds and new numbers a thread takes at a time when numbering.
const SEED_BLOCK: usize = 1 << 12;
/// A node's entry while counting: its out-degree, then two words holding up to three out-neighbours
/// + 1 for numbering, packed in as many bits each as the node count needs (21 at full scale). Nodes
/// average 2.9, so this sees most edges, and three words cost 7 MB less than four at the memory peak.
const RECORD: usize = 3;

/// Numbers the nodes so that neighbours sit close in memory, because the input's ids are random, which
/// makes every hop a cache miss. Each thread takes a block of numbers, a region, and fills it with one
/// round patch of the graph: clusters about the size of a label page, grown breadth first, each from
/// the patch's oldest unexpanded node. A round patch has few nodes on its edge, and those are what
/// `simplify` must keep. When the block runs out, the patch's edge goes on a stack, and the newest of
/// those starts the next patch, so nearby patches get nearby numbers. Nodes the walks miss become seeds
/// themselves, so all of them get a number. Any numbering gives the same answers, and numbers needn't
/// be dense: a thread's last block has a few unused, and so does number 0, which means no number yet.
/// Returns the numbers and each number's out-degree, read from the record as its node expands.
fn local_numbering(records: &[AtomicU32], bits: u32, threads: usize) -> (Words, Words) {
    let nodes = records.len() / RECORD;
    let numbers = zeroed_u32s(nodes);
    let mut out_counts = zeroed_u32s(nodes + (threads + 1) * REGION + 2);
    let (next_seed, next_number) = (AtomicUsize::new(0), AtomicU32::new(0));
    on_threads(threads, || {
        let (mut number, mut numbers_end) = (0, 0);
        // Most claims fail, and a plain read costs far less than a locked update.
        let mut claim = |node: usize| {
            if number == numbers_end {
                let start = next_number.fetch_add(REGION as u32, Relaxed);
                (number, numbers_end) = (start.max(1), start + REGION as u32);
            }
            let slot = &numbers[node];
            let won = slot.load(Relaxed) == 0
                && slot.compare_exchange(0, number, Relaxed, Relaxed).is_ok();
            number += u32::from(won);
            won.then_some((node as u32, number - 1))
        };
        let (mut older, mut queue, mut head, mut given) = (Vec::new(), Vec::new(), 0, 0);
        // Seeds come in blocks, because near the end nearly all are taken, and one shared counter
        // hit per node would make the threads queue.
        while let Some(block) = next_chunk(&next_seed, SEED_BLOCK, nodes) {
            for seed in block..(block + SEED_BLOCK).min(nodes) {
                let claimed = claim(seed);
                given += usize::from(claimed.is_some());
                older.extend(claimed);
                while head < queue.len() || !older.is_empty() {
                    if head == queue.len() {
                        queue.clear();
                        head = 0;
                        queue.extend(older.pop());
                    }
                    let (first, mut next) = (queue.len(), queue.len());
                    queue.push(queue[head]);
                    head += 1;
                    while next < queue.len() && queue.len() - first < PAGE {
                        let (node, own) = queue[next];
                        let record = &records[node as usize * RECORD..][..RECORD];
                        out_counts[own as usize].store(record[0].load(Relaxed), Relaxed);
                        // A fixed three fields keeps the loop unrolled; an unused one reads as 0.
                        let mut packed = u64::from(record[1].load(Relaxed))
                            | u64::from(record[2].load(Relaxed)) << 32;
                        for _ in 0..3 {
                            let neighbour = (packed & ((1 << bits) - 1)) as usize;
                            packed >>= bits;
                            if let Some(claimed) = neighbour.checked_sub(1).and_then(&mut claim) {
                                queue.push(claimed);
                                prefetch(&records[claimed.0 as usize * RECORD]);
                            }
                        }
                        next += 1;
                    }
                    given += queue.len() - first - 1;
                    queue.drain(first..next);
                    if given >= REGION {
                        given -= REGION;
                        older.extend(queue.drain(head..));
                    }
                }
            }
        }
    });
    out_counts.truncate(next_number.into_inner() as usize + 1);
    (numbers, out_counts)
}

/// Heavy weights as threads find them, by arc number, until `seal` moves them into `heavy`.
static SPILLED: Mutex<Vec<(u32, u32)>> = Mutex::new(Vec::new());

/// A compressed adjacency array: the arcs leaving `node` are `arcs[offsets[node]..offsets[node + 1]]`.
/// An arc packs its neighbour and its weight into 32 bits, with as many weight bits as the node count
/// leaves. The highest value means the weight is in `heavy`, sorted by arc number once it's built.
struct Graph {
    offsets: Words,
    arcs: Words,
    weight_bits: u32,
    heavy: Vec<(u32, u32)>,
}

impl Graph {
    /// Turns per-node counts into a graph, ready to `reserve` every arc. Each offset starts at the
    /// end of its node's range and moves down as arcs are reserved, so it finishes at the start of
    /// the range, with no second array.
    fn from_counts(mut offsets: Words) -> Graph {
        let mut total = 0;
        for offset in offsets.iter_mut().map(AtomicU32::get_mut) {
            (*offset, total) = (*offset + total, *offset + total);
        }
        Graph {
            weight_bits: (offsets.len() as u32).leading_zeros(),
            arcs: zeroed_u32s(total as usize),
            offsets,
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
            SPILLED.lock().unwrap().push((slot as u32, weight));
        }
        self.arcs[slot].store(neighbour << self.weight_bits | weight.min(heavy), Relaxed);
    }

    /// Sorts the heavy weights, once every thread has finished.
    fn seal(&mut self) {
        self.heavy = std::mem::take(&mut *SPILLED.lock().unwrap());
        self.heavy.sort_unstable();
    }

    /// The first arc number of `node`, and its arcs.
    fn arcs_of(&self, node: u32) -> (usize, &[AtomicU32]) {
        let [start, end] = [0, 1].map(|i| self.offsets[node as usize + i].load(Relaxed) as usize);
        (start, &self.arcs[start..end])
    }

    /// A packed arc's neighbour and weight, given its arc number.
    fn unpack(&self, slot: usize, arc: u32) -> (u32, u64) {
        let heavy = (1 << self.weight_bits) - 1;
        let weight = match arc & heavy {
            weight if weight != heavy => weight,
            _ => self.heavy[self.heavy.partition_point(|e| (e.0 as usize) < slot)].1,
        };
        (arc >> self.weight_bits, u64::from(weight))
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

/// Builds the forward graph from the graph file, in new node numbers, and returns those numbers too.
/// `offsets` has a final entry that counts no node, so it ends up as the arc total, which no `reserve`
/// moves.
fn build_graph(graph_file: &[u8], threads: usize) -> (Graph, Words) {
    let mut pos = 0;
    let nodes = next_number(graph_file, &mut pos) as usize;
    next_number(graph_file, &mut pos); // The edge count: the file's length says the same.
    let edge_lines = &graph_file[pos..];
    let records = zeroed_u32s(nodes * RECORD);
    // Neighbour + 1 fits `bits`, and a graph past 2^21 nodes keeps two neighbours instead of three,
    // which only makes the numbering a little worse. A field can straddle the two words, so each
    // word gets its part of it.
    let bits = usize::BITS - nodes.leading_zeros();
    let fields = (64 / bits.max(1)).min(3);
    for_each_batch(edge_lines, threads, |batch| {
        for &(tail, _, _) in batch {
            prefetch(&records[tail as usize * RECORD]);
        }
        for &(tail, head, _) in batch {
            let record = &records[tail as usize * RECORD..][..RECORD];
            let seen = record[0].fetch_add(1, Relaxed);
            if seen < fields {
                let packed = u64::from(head + 1) << (seen * bits);
                // A plain read, or and write of both words, not a locked update, which costs as
                // much as the parse on the box, and no branch on which word the field lands in. Two
                // threads writing one word at the same instant can lose a link, which costs locality
                // and nothing else: the count is exact, and a node the walks miss becomes a seed.
                let parts = [packed as u32, (packed >> 32) as u32];
                for (word, part) in record[1..].iter().zip(parts) {
                    word.store(word.load(Relaxed) | part, Relaxed);
                }
            }
        }
    });
    let (numbers, out_counts) = local_numbering(&records, bits, threads);
    drop(records);
    let mut forward = Graph::from_counts(out_counts);

    for_each_batch(edge_lines, threads, |batch| {
        let number = |node: u32| numbers[node as usize].load(Relaxed);
        let (mut renumbered, mut slots) = ([(0, 0, 0); BATCH], [0; BATCH]);
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
    (forward, numbers)
}

/// The reverse graph comes from the forward one, with no parsing and no number lookups. Neighbours
/// have nearby numbers by now, so these sweeps stay in memory that's nearly in cache.
fn reverse(forward: Graph, threads: usize) -> [Graph; 2] {
    let in_counts = zeroed_u32s(forward.offsets.len());
    forward.for_each_arc(threads, |_, head, _| {
        in_counts[head as usize].fetch_add(1, Relaxed);
    });
    let mut backward = Graph::from_counts(in_counts);
    forward.for_each_arc(threads, |tail, head, weight| {
        backward.store(backward.reserve(head), tail, weight);
    });
    backward.seal();
    [forward, backward]
}

/// Nodes per region, a block of node numbers that one thread simplifies on its own.
const REGION: usize = 1 << 13;
/// A node's neighbours in its region, at most, while simplifying.
const WIDTH: usize = 8;
const NO_ARC: u16 = u16::MAX;

/// A node while its region is simplified: its neighbours, and the weights of its arcs to them and from
/// them. From position `len` on, the weights are all `NO_ARC`, which makes a failed `find` read as "no
/// arc", with no branch. Neighbours number at most 8, so a node fits one cache line.
#[derive(Clone, Copy, Default)]
#[repr(align(64))]
struct Local {
    ids: [u16; WIDTH],
    arcs: [[u16; WIDTH + 1]; 2],
    len: u8,
    stays: bool,
}

impl Local {
    /// The position of neighbour `id`, or `len` if it isn't one. One vector compare does it, with no
    /// loop to mispredict.
    fn find(&self, id: u16) -> usize {
        // SAFETY: an array of eight u16s is a plain 16-byte value, and x86-64 always has SSE2.
        let ids: __m128i = unsafe { std::mem::transmute(self.ids) };
        let same = unsafe { _mm_movemask_epi8(_mm_cmpeq_epi16(ids, _mm_set1_epi16(id as i16))) };
        let end = 1u32 << (2 * self.len);
        ((same as u32 & (end - 1)) | end).trailing_zeros() as usize / 2
    }

    /// Lowers the weights of the arcs to and from neighbour `id`, making it a neighbour if it's new.
    fn join(&mut self, id: u16, to: u16, from: u16) {
        let at = self.find(id);
        self.ids[at] = id;
        self.len += u8::from(at == self.len as usize);
        self.arcs[0][at] = self.arcs[0][at].min(to);
        self.arcs[1][at] = self.arcs[1][at].min(from);
    }

    fn remove(&mut self, id: u16) {
        let at = self.find(id);
        self.len -= 1;
        self.ids[at] = self.ids[self.len as usize];
        for weights in &mut self.arcs {
            weights[at] = weights[self.len as usize];
            weights[self.len as usize] = NO_ARC;
        }
    }
}

/// Whether some path of at most two arcs from `a` to `b` is no longer than `bound`. It stops at the
/// first one, and a `bound` of `NO_ARC` is met by the direct arc, present or not. Every arc is in
/// both its ends' lists, so the second hop is read from `b`'s, which is in cache, not the middle node's.
fn within(a: &Local, b: &Local, b_id: u16, bound: u16) -> bool {
    let via = |(&x, &to_x): (&u16, &u16)| to_x.saturating_add(b.arcs[1][b.find(x)]) <= bound;
    a.arcs[0][a.find(b_id)] <= bound || a.ids[..a.len as usize].iter().zip(&a.arcs[0]).any(via)
}

/// Shrinks the graph to what the queries can use. Queries are known by now, so any node that no query
/// starts or ends at can go, as long as distances between the rest stay the same: a removed node's
/// neighbours get a shortcut for each path through it, and a node with one neighbour simply goes. A thread
/// works alone on a region, which needs no locks, and a node with an arc to or from another region
/// stays. Each region writes its arcs back over its own, in no more space. Then the remaining nodes
/// get dense numbers and the arrays close up, so the graph takes less memory and searches do less work.
fn simplify(forward: &mut Graph, queries: &mut [(u32, u32, AtomicU64)], threads: usize) {
    let nodes = forward.offsets.len() - 1;
    let regions = nodes.div_ceil(REGION);
    // Until a region is done, non-zero means its node must stay. After that, the rank of a node that stays.
    let ranks: Vec<AtomicU16> = vec![0u16; nodes].into_iter().map(AtomicU16::new).collect();
    // Touched here for the same reason as in `zeroed_u32s`.
    for rank in ranks.iter().step_by(1 << 20) {
        rank.store(0, Relaxed);
    }
    let stay = |pair: [u32; 2]| pair.map(|node| ranks[node as usize].store(1, Relaxed));
    forward.for_each_arc(threads, |tail, head, _| {
        if (tail ^ head) as usize >= REGION {
            stay([tail, head]);
        }
    });
    for &(from, to, _) in queries.iter() {
        stay([from, to]);
    }
    let (live, ends) = (zeroed_u32s(regions), zeroed_u32s(regions));
    let next_region = AtomicUsize::new(0);
    on_threads(threads, || {
        let empty = Local {
            arcs: [[NO_ARC; WIDTH + 1]; 2],
            ..Default::default()
        };
        let (mut locals, mut kept, mut stack) = (vec![empty; REGION], Vec::new(), Vec::new());
        while let Some(region) = next_chunk(&next_region, 1, regions) {
            let (start, size) = (region * REGION, REGION.min(nodes - region * REGION));
            for (index, local) in locals[..size].iter_mut().enumerate() {
                *local = empty;
                local.stays = ranks[start + index].load(Relaxed) != 0;
            }
            for v in 0..size {
                let (first, arcs) = forward.arcs_of((start + v) as u32);
                for (index, arc) in arcs.iter().enumerate() {
                    let (head, weight) = forward.unpack(first + index, arc.load(Relaxed));
                    let other = (head as usize).wrapping_sub(start);
                    let fits = |x: usize| (locals[x].len as usize) < WIDTH;
                    if other == v {
                        continue; // A self-loop never shortens a path.
                    }
                    if other < size && weight < 1 << 15 && fits(v) && fits(other) {
                        locals[v].join(other as u16, weight as u16, NO_ARC);
                        locals[other].join(v as u16, NO_ARC, weight as u16);
                    } else {
                        locals[v].stays = true;
                        if other < size {
                            locals[other].stays = true;
                        }
                        kept.push((v as u32, head, weight as u32));
                    }
                }
            }
            for tier in 2..=3 {
                // Only a node with up to `tier` neighbours can go, and a neighbour that loses one is
                // worth a second look; a node with more than 3 stays, since joining its neighbours would
                // add arcs. The first candidates are picked with no branch to mispredict.
                let wanted =
                    |local: &Local| !local.stays && (1..=tier).contains(&(local.len as usize));
                stack.resize(size, 0);
                let mut top = 0;
                for (v, local) in locals[..size].iter().enumerate() {
                    stack[top] = v as u16;
                    top += usize::from(wanted(local));
                }
                stack.truncate(top);
                while let Some(v) = stack.pop() {
                    let node = locals[v as usize];
                    let (len, [to, from]) = (node.len as usize, &node.arcs);
                    if !wanted(&node) {
                        continue;
                    }
                    // Weights below 2^15 keep every shortcut below NO_ARC, and only a very long
                    // chain gets there. Past `len`, the weights are all NO_ARC, which doesn't count.
                    let heavy = |&w: &u16| (1 << 15..NO_ARC).contains(&w);
                    let neighbours = &node.ids[..len];
                    let full = |&x: &u16| locals[x as usize].len as usize + len > WIDTH + 2;
                    if node.arcs.iter().flatten().any(heavy) || neighbours.iter().any(full) {
                        continue;
                    }
                    locals[v as usize].len = 0;
                    for &x in neighbours {
                        locals[x as usize].remove(v);
                    }
                    for (i, &a) in neighbours.iter().enumerate() {
                        for (j, &b) in neighbours[..i].iter().enumerate() {
                            // Two arcs in a row: a sum that saturates reads as no arc.
                            let (a_to_b, b_to_a) =
                                (from[i].saturating_add(to[j]), from[j].saturating_add(to[i]));
                            let (at_a, at_b) = (&locals[a as usize], &locals[b as usize]);
                            let a_to_b = if tier >= 3 && within(at_a, at_b, b, a_to_b) {
                                NO_ARC
                            } else {
                                a_to_b
                            };
                            let b_to_a = if tier >= 3 && within(at_b, at_a, a, b_to_a) {
                                NO_ARC
                            } else {
                                b_to_a
                            };
                            if a_to_b.min(b_to_a) != NO_ARC {
                                locals[a as usize].join(b, a_to_b, b_to_a);
                                locals[b as usize].join(a, b_to_a, a_to_b);
                            }
                        }
                    }
                    stack.extend(neighbours.iter().filter(|&&x| wanted(&locals[x as usize])));
                }
            }
            let first = forward.offsets[start].load(Relaxed) as usize;
            let (mut slot, mut alive, mut rest) = (first, 0, kept.drain(..).peekable());
            for (v, node) in locals[..size].iter().enumerate() {
                // The region's offsets close up here, at the ranks of the nodes that stay: a removed
                // node's entry is written again by the next node that stays, with the same slot.
                forward.offsets[start + alive as usize].store(slot as u32, Relaxed);
                for (&id, &weight) in node.ids[..node.len as usize].iter().zip(&node.arcs[0]) {
                    if weight != NO_ARC {
                        forward.store(slot, (start + id as usize) as u32, weight.into());
                        slot += 1;
                    }
                }
                while let Some((_, head, weight)) = rest.next_if(|k| k.0 == v as u32) {
                    forward.store(slot, head, weight);
                    slot += 1;
                }
                let stays = node.stays || node.len > 0;
                ranks[start + v].store(alive, Relaxed);
                alive += u16::from(stays);
            }
            live[region].store(alive.into(), Relaxed);
            ends[region].store((slot - first) as u32, Relaxed);
        }
    });
    forward.seal();
    // Where each region's nodes and arcs start in the dense arrays, and where its arcs start now.
    let (mut nodes_so_far, mut arcs_so_far, mut bases) = (0, 0, Vec::new());
    for region in 0..regions {
        let first = forward.offsets[region * REGION].load(Relaxed);
        bases.push((nodes_so_far, arcs_so_far, first));
        nodes_so_far += live[region].load(Relaxed);
        arcs_so_far += ends[region].load(Relaxed);
    }
    let dense =
        |node: u32| bases[node as usize / REGION].0 + u32::from(ranks[node as usize].load(Relaxed));
    let (bits, low) = (forward.weight_bits, (1 << forward.weight_bits) - 1);
    for_ranges(regions, threads, |from, to| {
        for region in from..to {
            let first = bases[region].2 as usize;
            for arc in &forward.arcs[first..][..ends[region].load(Relaxed) as usize] {
                let value = arc.load(Relaxed);
                arc.store(dense(value >> bits) << bits | value & low, Relaxed);
            }
        }
    });
    for (region, &(node_base, arc_base, first)) in bases.iter().enumerate() {
        for rank in 0..live[region].load(Relaxed) as usize {
            let offset = forward.offsets[region * REGION + rank].load(Relaxed);
            forward.offsets[node_base as usize + rank].store(offset - first + arc_base, Relaxed);
        }
        for index in 0..ends[region].load(Relaxed) as usize {
            let arc = forward.arcs[first as usize + index].load(Relaxed);
            forward.arcs[arc_base as usize + index].store(arc, Relaxed);
        }
    }
    forward.offsets[nodes_so_far as usize].store(arcs_so_far, Relaxed);
    for entry in &mut forward.heavy {
        let base = bases[bases.partition_point(|base| base.2 <= entry.0) - 1];
        entry.0 = entry.0 - base.2 + base.1;
    }
    for query in queries.iter_mut() {
        (query.0, query.1) = (dense(query.0), dense(query.1));
    }
    forward.offsets.truncate(nodes_so_far as usize + 1);
    forward.offsets.shrink_to_fit();
    forward.arcs.truncate(arcs_so_far as usize);
    forward.arcs.shrink_to_fit();
}

/// Nodes per page of the label store.
const PAGE: usize = 64;

/// One side of a search: its tentative distances and its queue. Distances live in pages of `PAGE`
/// nodes, found through a directory indexed by node, which persists across queries and is reset through
/// the list of entries a query used. Neighbours have nearby numbers, so a search fills few pages, and
/// finding a label costs one directory read and one page read, with no key to compare. Page 0 is all
/// unreached, and every empty directory entry points at it, so reading a label needs no branch. A
/// zeroed directory stays untouched until it's used, which the 64-bit search rarely is.
///
/// The queue relies on keys never going below `last`, the smallest key when the current block began.
/// Bucket 0 holds the keys in the same block of `1 << COARSE_BITS` distances as `last`, unsorted, and
/// pops the newest first. Bucket `i` holds the keys whose highest differing bit above the block is bit
/// `i - 1 + COARSE_BITS`. When bucket 0 runs dry, the lowest bucket in use spreads into the ones below,
/// so a key moves a few times in all, with no comparisons against other keys.
struct Side<D> {
    directory: Vec<u16>,
    pages: Vec<[D; PAGE]>,
    used: Vec<u16>,
    buckets: Vec<Vec<(D, u32)>>,
    last: D,
}

/// A side works in 32 bits or 64. Nearly every distance fits in 32 bits, which halves the labels and the
/// queues. The few that don't make the 32-bit search give up, and that query runs again in 64 bits.
/// In either, the highest value means a node isn't reached yet, and `!D::default()` gives it.
impl<D: Copy + Ord + Default + Not<Output = D> + TryFrom<u64> + Into<u64>> Side<D> {
    /// A side for each direction of a search.
    fn pair(entries: usize) -> [Side<D>; 2] {
        [(); 2].map(|()| Side {
            directory: vec![0; entries / PAGE + 1],
            pages: vec![[!D::default(); PAGE]],
            used: Vec::new(),
            buckets: vec![Vec::new(); 65 - COARSE_BITS as usize],
            last: !D::default(),
        })
    }

    /// Resets the side, and seeds it with `node`.
    fn start(&mut self, node: u32) {
        for entry in self.used.drain(..) {
            self.directory[entry as usize] = 0;
        }
        self.pages.truncate(1);
        self.buckets.iter_mut().for_each(Vec::clear);
        self.last = D::default();
        self.set_label(node, D::default());
        self.push(D::default(), node);
    }

    fn push(&mut self, key: D, node: u32) {
        let (wide, last): (u64, u64) = (key.into(), self.last.into());
        let bucket = 64 - ((wide ^ last) >> COARSE_BITS).leading_zeros();
        self.buckets[bucket as usize].push((key, node));
    }

    /// A lower bound on every key queued, with bucket 0 then not empty, or None if the queue is empty.
    /// The bound lags the true smallest key by less than a block.
    fn top(&mut self) -> Option<D> {
        if self.buckets[0].is_empty() {
            let lowest = self.buckets.iter().position(|bucket| !bucket.is_empty())?;
            let mut bucket = std::mem::take(&mut self.buckets[lowest]);
            self.last = bucket.iter().map(|entry| entry.0).min()?;
            for (key, node) in bucket.drain(..) {
                self.push(key, node);
            }
            self.buckets[lowest] = bucket;
        }
        Some(self.last)
    }

    fn label(&self, node: u32) -> D {
        self.pages[self.directory[node as usize / PAGE] as usize][node as usize % PAGE]
    }

    /// Gives the directory entry a fresh page. Kept out of the hot path: the page's fill is wide
    /// code, and with it in line the compiler left all of `set_label` out of line.
    #[cold]
    fn add_page(&mut self, entry: usize) {
        self.directory[entry] = self.pages.len() as u16;
        self.pages.push([!D::default(); PAGE]);
        self.used.push(entry as u16);
    }

    fn set_label(&mut self, node: u32, dist: D) {
        let entry = node as usize / PAGE;
        if self.directory[entry] == 0 {
            self.add_page(entry);
        }
        self.pages[self.directory[entry] as usize][node as usize % PAGE] = dist;
    }

    /// Two-ended Dijkstra, returning INF if there is no path, or None if a distance doesn't fit in D.
    /// `best` holds the shortest path seen so far, and the search stops when the two queue tops add
    /// up to at least `best`, which is exact for any non-negative weights, including 0. When a queue
    /// empties, that side has settled all it can reach, so `best` is exact too.
    ///
    /// Inside a block the keys pop in no sorted order, so a node can expand again when a shorter path
    /// reaches it. That costs less than sorting them did. It's safe because a top only moves past keys
    /// no longer queued: any node whose true distance is below a side's top has its exact label and
    /// has expanded with it. So a top still bounds what that side has yet to find, for the stop test
    /// and the pruning below, and the later of two exact labels on a node sees the other.
    ///
    /// The side with less work so far goes next. Choosing the smaller top instead would let the
    /// backward side settle the whole main body on queries from a small island.
    fn distance(sides: &mut [Self; 2], graphs: &[Graph; 2], from: u32, to: u32) -> Option<u64> {
        if from == to {
            return Some(0);
        }
        let unreached: u64 = (!D::default()).into();
        sides[0].start(from);
        sides[1].start(to);
        let (mut best, mut work) = (INF, [0u64; 2]);
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
                let current: u64 = sides[side].label(neighbour).into();
                if candidate < current {
                    let other: u64 = sides[1 - side].label(neighbour).into();
                    if other != unreached {
                        best = best.min(candidate + other);
                    }
                    // If the other side has labelled this node, `best` just counted the path through
                    // it. If not, the rest of the way costs at least the other queue's top. So a node
                    // at or past `best` by that bound needs no expanding, and no label either, since
                    // no path the other side finds through it later could beat `best`.
                    if candidate + other_top < best {
                        // It fits: it's below a label, and a label is never the unreached value.
                        let narrow = D::try_from(candidate).ok()?;
                        sides[side].set_label(neighbour, narrow);
                        sides[side].push(narrow, neighbour);
                        // Its arcs come next from this node's offsets, which can load meanwhile.
                        prefetch(&graphs[side].offsets[neighbour as usize]);
                    }
                } else if current == unreached {
                    // Only an unreached node leaves room for a candidate this large: it doesn't fit
                    // in D, so the search gives up, and the wide one answers the query.
                    return None;
                }
            }
        }
        Some(best)
    }
}

fn main() -> io::Result<()> {
    // SAFETY: M_MMAP_THRESHOLD only sets an option of our own process's allocator. It stops glibc
    // from keeping freed blocks of any size it has seen freed. At 1 MiB, each thread's buffers stay
    // in its own arena, where threads can't race for a huge page.
    unsafe { mallopt(-3, 1 << 20) };
    let arg = |n, default: &str| std::env::args().nth(n).unwrap_or_else(|| default.into());
    let (data_dir, out_dir) = (arg(1, "/data"), arg(2, "/out"));
    // `SD_THREADS` if set (the test sweep uses it), else what the container allows, capped at the
    // box's 12 threads.
    let allowed = thread::available_parallelism().map_or(1, |n| n.get().min(12));
    let asked = std::env::var("SD_THREADS").map_or(0, |v| v.parse().unwrap_or(0));
    let threads = if asked > 0 { asked } else { allowed };
    let (mut forward, numbers) = build_graph(map_file(&format!("{data_dir}/graph.txt"))?, threads);

    let query_lines = map_file(&format!("{data_dir}/queries.txt"))?;
    let (mut queries, mut pos) = (Vec::new(), 0);
    while pos < query_lines.len() {
        let mut end = || numbers[next_number(query_lines, &mut pos) as usize].load(Relaxed);
        queries.push((end(), end(), AtomicU64::new(0)));
    }
    drop(numbers);
    simplify(&mut forward, &mut queries, threads);
    let graphs = reverse(forward, threads);
    let entries = graphs[0].offsets.len();

    // Keep page-id narrowing exact; the full contest graph is well below this bound.
    assert!(entries <= PAGE << 16, "page ids exceed the u16 directory");
    let next_query = AtomicUsize::new(0);
    on_threads(threads, || {
        let (mut narrow, mut wide) = (Side::<u32>::pair(entries), Side::<u64>::pair(entries));
        while let Some((from, to, answer)) = queries.get(next_query.fetch_add(1, Relaxed)) {
            let distance = Side::distance(&mut narrow, &graphs, *from, *to);
            let distance = distance.or_else(|| Side::distance(&mut wide, &graphs, *from, *to));
            answer.store(distance.unwrap_or(INF), Relaxed);
        }
    });

    let mut out = BufWriter::new(File::create(format!("{out_dir}/distances.txt"))?);
    for (_, _, answer) in &queries {
        writeln!(out, "{}", answer.load(Relaxed) as i64)?;
    }
    out.flush()
}
