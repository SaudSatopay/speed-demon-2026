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

- M was 70.1 to 70.7 MB over the latest two board runs; the graph takes about 54 MB.
- The label-page index is 16 bits; a guard rejects graphs if renumbering needs a larger page id.
- Label-page indexes use 16 bits; a guard rejects a graph if its renumbered pages exceed that range.
- A query whose distances pass 2^32 runs twice, first in 32 bits and then in 64. There are only a few of these.
- Node ids are random, so nearly every hop is a cache miss.

## Approach

The mapped graph file is parsed twice by 12 threads, in batches of 32 edges with prefetching. The first pass counts each node's edges and keeps up to 3 neighbours. Threads then walk those neighbours depth first, claiming nodes as they go, and that order gives every node a new number, so neighbours sit close in memory. The second pass places each edge into a compressed adjacency array, in new numbers, and sweeping that array fills the reverse one, with few cache misses because neighbours are close by then. Each arc packs its neighbour and weight into 32 bits, with a small side table for the rare heavy weight. So no temporary edge list is ever held. Threads then claim queries from an atomic counter. Each query runs a two-ended Dijkstra, with one radix queue per side and lazy deletion, in 32 bits, falling back to 64 bits for the few queries that need it. The side with less work so far goes next, and the search stops when the two queue tops add up to at least the best path seen. Each thread keeps labels in 64-node pages, reached through a u16 directory; a guard rejects graphs whose renumbered page ids exceed that range. Each answer goes into its own slot, so the output order doesn't depend on scheduling.

## Why it's fast

See [PERF.md](PERF.md) for run IDs.

1. **Two-ended search.** The reference searches from the source only, so it settles everything closer than the target.
2. **Sides alternate by work, not by the smaller top.** Queries from a small island, and queries to a target nothing reaches, finish after a few settles rather than crossing the whole main body.
3. **Neighbours close in memory.** The input's ids are random, so renumbering takes the queries from about 190 ms to about 105 ms.
4. **Batched build.** A batch of edges prefetches its cache lines and lets them arrive together.
5. **Search state sized to the search.** Labels live in 64-node pages, found through a u16 directory. A median search stays in the fastest caches, and the largest fills about 585 KB. A node-sized array would cost 7 MB per thread.

## Ideas and where they came from

- The stopping rule: Goldberg and Harrelson.
- Alternating by work: Haeupler et al. show this is instance-optimal, and alternating by smallest key isn't.
- Pruning pushes against the other queue's top: a standard bidirectional search refinement.
- Node order for road networks: PHAST (Delling et al.) measured DFS order at 2.2 to 2.8 times random order.
- The 8-digit SWAR parse: Lemire (2022).

## Correctness

The spec's edge cases need no special code: parallel edges and duplicates are all relaxed, so the lightest wins, a self-loop can't improve a label, weight 0 is an ordinary weight, and distances are 64-bit. `s = t` answers 0 before any search, even on a node with no edges. If either queue runs out, that side has settled everything it can reach, so the best path seen is exact, or there is none. Every build is checked by SHA-256 against the naive reference on 15 generator seeds, 8 answer-preserving rewrites and 6 limit cases. Anyone can reproduce this with one command: `python3 tools/verify.py` (see [tools/README.md](../tools/README.md)).

## Dependencies

None. `mmap` is declared directly against glibc, which `std` already links, and the prefetches come from `std::arch`.

## Build and run

- **Container (as scored):** `docker build -t track-d .`, then run it with `/data` mounted read-only. It writes exactly one file, `/out/distances.txt`.
- **Native (for profiling):** `cargo build --release`, then `./target/release/traverse <data dir> <out dir>`.

## AI use

Written with AI assistance (Claude Code and Codex (GPT-6 Luna)). Every decision and number in PERF.md was checked, and can be explained, by the author.
