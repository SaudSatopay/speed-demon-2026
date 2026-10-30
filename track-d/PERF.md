# Track D: performance log

**Result:** the reference takes **72.69 s / 893 MB** on the board. Ours is in the last row of [the journey](#the-journey).

**Check it yourself:** the board run named there, or locally `python3 tools/verify.py --tracks d`. Output SHA-256 on seed 1: `a23dbdf0a3e49cc8901310a9ee1dc6a9a569a85297b6da75c2a7002cb2ed277e`.

## Where it loses

- M is about 164 MB locally, of which 94 MB is the graph.
- S is 427 code lines against the reference's 43, which costs about 7.5 speed points.
- Each arc takes 8 bytes in each direction, and most weights would fit in far fewer bits.

## Where the reference spends its time

About 89% of the time is Dijkstra: 28.7M heap pops, each searching forward from the source only.

## The journey

Every step cites the live-board run that measured it. Numbers marked *local* come from our scorer, on a faster PC, and only explain decisions.

| Step | Change | Why we expected it to help | Run ID | T (s) | M (MB) | Points |
|---|---|---|---|---|---|---|
| 0 | The reference, unchanged | — | — | 72.69 | 893 | 0 |
| 1 | v1: two-pass parse into two compressed arrays, two-ended Dijkstra, one thread | Scans far fewer nodes per query, and no Python objects | run-… | 1.886 *local* | 105.4 *local* | 40.8 *local* |
| 2 | 12 threads: parallel parsing with atomic counters, and a query pool with search state per thread | Queries are independent, and parsing can be cut at line boundaries | run-… | 0.342 *local* | 201.8 *local* | 55.8 *local* |
| 3 | Per-query hash table for labels, and pruned pushes | The largest search labels 78,554 of 1.74M nodes, and the median 196 | run-… | 0.299 *local* | 130.7 *local* | 58.7 *local* |
| 4 | Nodes renumbered by depth-first walks, SWAR parsing, and a batched, prefetched build | Random ids make every hop a cache miss, and the build was mostly cache misses on shared counters | run-… | 0.199 *local* | 164.4 *local* | 60.9 *local* |

## Decision log

**D1. Alternate the two sides by work.** Context: the spec's island queries start on 14 nodes that can't reach the main body. Options: expand the side with the smaller top, or the side with less work. Chose: less work, because with the smaller top the backward side keeps winning and settles about 1.74M nodes per island query before the forward side runs out. Cost: none. Evidence: both rules are exact, and the full-scale grid passes. Outcome: kept.

**D2. Parse the file twice instead of holding an edge list.** Context: a list of 5M edges costs 60 MB at the peak. Chose: a counting pass and a placing pass over the mapped file. Cost: parsing runs twice. Outcome: kept.

**D3. Atomics for the shared build.** Context: threads count and place arcs in shared arrays. Options: per-thread count arrays (7 MB per thread per direction, 167 MB), or shared atomic counters. Chose: shared atomics, with `fetch_sub` reserving each arc's slot, which also means no `unsafe`. Relaxed atomic loads and stores compile to plain moves on x86, so the search doesn't pay for them. Cost: the order of arcs within a node depends on scheduling, which can't change a shortest distance. Evidence: T 1.886 -> 0.342 s, K 6.56 -> 13.05 (local), and the thread sweep passes at 1, 2, 3, 7, 12 and 64 threads. Outcome: kept.

**D4. Replace sparse-set slots with a small hash table per query.** Context: we measured seed 1's searches: 8.1M labels over 1,994 searches, median 196, largest 78,554, while the sparse set's slot array cost 7 MB per thread, 84 MB in all, and random ids soon touch every page. Options: keep it, or find labels through a table that starts at 1,024 slots each query and grows by 4x. Chose: the table. Cost: S rose by 34 lines. Evidence: M 201.8 -> 130.7 MB, T 0.342 -> 0.299 s, K 13.05 -> 14.95 (local). Outcome: kept.

**D5. Prune pushes.** Context: a node whose distance plus the other queue's top already reaches the best path can't be on a better one, unless the other side has settled it, in which case the best path already counted it. Chose: skip the push in that case, after updating the label and the best path. Evidence: the same run as D4, and the answers still match. Outcome: kept.

**D6. Renumber nodes, and batch the build.** Context: we tried renumbering alone first, and it nearly halved query time (190 to 105 ms), but the extra build time kept the total at 265 ms against 299 ms. We compared BFS, DFS and BFS limited to cells of 64 to 16,384 nodes, and DFS was best, by a small margin. A profile showed the build was mostly cache misses on shared counters and arcs. So: parse 32 edges at a time, prefetch all their cache lines, then touch them, reserving arc slots before storing, because an atomic update waits for earlier stores. Numbers parse from one 8-byte load. Cost: S 288 -> 427, and M 130.7 -> 164.4 MB, for the numbering's arrays and because threads now find more cache lines worth touching. Evidence: T 0.299 -> 0.199 s, K 14.95 -> 16.50 (local). Outcome: kept.

## What didn't work

Nothing tried yet.

## The final design

Not final yet: see [README](README.md#approach) for the current one.

## How we know it's correct

- Every build is checked by SHA-256 against the naive reference on 15 generator seeds (tiny and sample) and 5 seeds at full scale, plus 8 answer-preserving rewrites of the input and 6 limit cases.
- **Reproduce it yourself:** `python3 tools/verify.py` (Docker, git and Python only).

## With another 24 hours

To be written at freeze.
