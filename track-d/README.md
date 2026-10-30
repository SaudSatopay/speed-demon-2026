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

- It's single-threaded for now.
- Node ids are random, so nearly every hop is a cache miss.

## Approach

The mapped graph file is parsed twice: once to count each node's edges, once to place them into forward and reverse compressed adjacency arrays. That way, no temporary edge list is ever held. Each query runs a two-ended Dijkstra, with one binary heap per side and lazy deletion. The side with less work so far goes next, and the search stops when the two queue tops add up to at least the best path seen. Labels live in a sparse set, so a new query clears nothing.

## Why it's fast

See [PERF.md](PERF.md) for run IDs.

1. **Two-ended search.** The reference searches from the source only, so it settles everything closer than the target.
2. **Sides alternate by work, not by the smaller top.** Queries from a small island, and queries to a target nothing reaches, finish after a few settles rather than crossing the whole main body.
3. **No per-node objects.** The graph is two flat arrays, and the search state is reused.

## Ideas and where they came from

- The stopping rule: Goldberg and Harrelson.
- Alternating by work: Haeupler et al. show this is instance-optimal, and alternating by smallest key isn't.
- Sparse-set labels: Briggs and Torczon.

## Correctness

The spec's edge cases need no special code: parallel edges and duplicates are all relaxed, so the lightest wins, a self-loop can't improve a label, weight 0 is an ordinary weight, and distances are 64-bit. `s = t` answers 0 before any search, even on a node with no edges. If either queue runs out, that side has settled everything it can reach, so the best path seen is exact, or there is none. Every build is checked by SHA-256 against the naive reference on 15 generator seeds, 8 answer-preserving rewrites and 6 limit cases. Anyone can reproduce this with one command: `python3 tools/verify.py` (see [tools/README.md](../tools/README.md)).

## Dependencies

None. `mmap` is declared directly against glibc, which `std` already links.

## Build and run

- **Container (as scored):** `docker build -t track-d .`, then run it with `/data` mounted read-only. It writes exactly one file, `/out/distances.txt`.
- **Native (for profiling):** `cargo build --release`, then `./target/release/traverse <data dir> <out dir>`.

## AI use

Written with AI assistance (Claude Code). Every decision and number in PERF.md was checked, and can be explained, by the author.
