# Track D: Traverse

Exact shortest paths for 2,000 queries on a directed road network of 1.74M nodes and 5M edges. Rust, standard library only.

## Where everything is

| What | Where |
|---|---|
| Headline numbers and their board run IDs | [PERF.md](PERF.md), first lines |
| Why each decision was made, reverted ones included | [PERF.md](PERF.md#decision-log) |
| The hot loop | `src/main.rs`, function `Search::distance` |
| How correctness was checked | [Correctness](#correctness), below |

## Limits

- M is about 43 MB on the board; the peak is in the search phase, when the two graphs and each thread's search state are all live.
- Label pages are found through 16-bit directory entries, so a graph that keeps more than 4.19M nodes after simplifying stops at an assertion before any query runs. The full-scale graph has 1.74M before simplifying.
- A query whose distances pass 2^32 runs twice, first in 32 bits and then in 64. There are only a few of these.
- Two threads writing one count record at the same instant can lose a numbering link. That costs locality and nothing else, since the count is exact and a node the walks miss becomes a seed.

## Approach

The mapped graph file is parsed twice by up to 12 threads, in batches of 32 edges with prefetching, so no temporary edge list is ever held. The first pass counts each node's edges and keeps up to 3 neighbours, packed into a 12-byte record. Threads then number the nodes from those neighbours, in round patches made of 64-node clusters grown breadth first, so neighbours sit close in memory. The second pass places each edge into a compressed adjacency array, in new numbers. Each arc packs its neighbour and weight into 32 bits, with a small side table for the rare heavy weight.

The queries come next. With them known, low-degree nodes that none of them starts or ends at are removed, joining their neighbours by shortcuts: each thread works alone on a region of 8,192 nodes, and a node with an arc to or from another region stays. The remaining nodes get dense numbers and the arrays close up. Sweeping the smaller forward array then fills the reverse one, with few cache misses because neighbours are close by then.

Threads then claim queries from an atomic counter. Each query runs a two-ended Dijkstra in 32 bits, falling back to 64 bits for the few queries that need it, with lazy deletion and one coarse radix queue per side: bucket 0 holds one block of 128 distances, unsorted, and pops the newest first. The side with less work so far goes next, and the search stops when the two queue tops add up to at least the best path seen. Each thread keeps labels in 64-node pages, reached through a `u16` directory. Each answer goes into its own slot, so the output order doesn't depend on scheduling. The main thread touches one word per 2 MiB of every shared array first, because threads racing to touch a huge page each clear their own, and the memory peak counts them all.

## Why it's fast

See [PERF.md](PERF.md) for run IDs.

1. **Two-ended search.** The reference searches from the source only, so it settles everything closer than the target.
2. **Sides alternate by work, not by the smaller top.** Queries from a small island, and queries to a target nothing reaches, finish after a few settles rather than crossing the whole main body.
3. **Neighbours close in memory.** The input's ids are random, so renumbering takes the queries from about 190 ms to about 105 ms.
4. **Batched build.** A batch of edges prefetches its cache lines and lets them arrive together.
5. **Fewer nodes to search.** Removing low-degree nodes with shortcuts took T from 0.166 to 0.152 s and M from 67.9 to 46.0 MB on the board (steps 17 to 19), and removing it now costs 9% of K (step 27).
6. **A coarse radix queue.** Keys never go below the last block popped, so a key moves a few times in all, with no comparisons against other keys, and nothing sorts inside a block. Local counts fell from 1,202 to 1,154 instructions and from 7.32 to 5.88 mispredicts per edge (D17).
7. **Search state sized to the search.** Labels live in 64-node pages, found through a u16 directory. A median search stays in the fastest caches, and the largest fills about 585 KB. A node-sized array would cost 7 MB per thread.

## Ideas and where they came from

- The stopping rule: Goldberg and Harrelson.
- Alternating by work: Haeupler et al. show this is instance-optimal, and alternating by smallest key isn't.
- Pruning pushes against the other queue's top: a standard bidirectional search refinement.
- Removing nodes with shortcuts: node contraction, from Contraction Hierarchies (Geisberger et al., 2008).
- Node order for road networks: PHAST (Delling et al.) measured DFS order at 2.2 to 2.8 times random order.
- The 8-digit SWAR parse: Lemire (2022).

## Correctness

The spec's edge cases need no special code: parallel edges and duplicates are all relaxed, so the lightest wins, a self-loop can't improve a label, weight 0 is an ordinary weight, and distances are 64-bit. `s = t` answers 0 before any search, even on a node with no edges. If either queue runs out, that side has settled everything it can reach, so the best path seen is exact, or there is none. Every build is checked by SHA-256 against the naive reference on 15 generator seeds, 4 answer-preserving rewrites and 6 limit cases. Anyone can reproduce this with one command: `python3 tools/verify.py` (see [tools/README.md](../tools/README.md)).

## Dependencies

None. `mmap` is declared directly against glibc, which `std` already links, and the prefetches come from `std::arch`.

## Build and run

- **Container (as scored):** `docker build -t track-d .`, then run it with `/data` mounted read-only. It writes exactly one file, `/out/distances.txt`.
- **Native (for profiling):** `cargo build --release`, then `./target/release/traverse <data dir> <out dir>`.

## AI use

Written with AI assistance (Claude Code and Codex (GPT-6 Luna)). Every decision and number in PERF.md was checked, and can be explained, by the author.
