# Track D: performance log

**Result:** the reference takes **72.69 s / 893 MB** on the board. Our best run is `run-f10dee4d919e`: **0.166 s / 67.9 MB**, K **24.81** and **69.7** speed points.

**Check it yourself:** the board run named there, or locally `python3 tools/verify.py --tracks d`. Output SHA-256 on seed 1: `a23dbdf0a3e49cc8901310a9ee1dc6a9a569a85297b6da75c2a7002cb2ed277e`.

## Where it loses

- M was 67.9 MB on the u16-directory board run, down from 70.1 MB on the preceding k=7 run; the two graphs account for about 54 MB.
- S is 428 code lines on the current candidate against the reference's 43, which costs about 7.9 speed points.

## Where the reference spends its time

About 89% of the time is Dijkstra: 28.7M heap pops, each searching forward from the source only.

## The journey

Every step cites the live-board run that measured it. Numbers marked *local* come from our scorer, on a faster PC, and only explain decisions.

| Step | Change | Why we expected it to help | Run ID | T (s) | M (MB) | Points |
|---|---|---|---|---|---|---|
| 0 | The reference, unchanged | — | — | 72.69 | 893 | 0 |
| 1 | v1: two-pass parse into two compressed arrays, two-ended Dijkstra, one thread | Scans far fewer nodes per query, and no Python objects | run-… | 1.886 *local* | 105.4 *local* | 40.8 *local* |
| 2 | 12 threads: parallel parsing with atomic counters, and a query pool with search state per thread | Queries are independent, and parsing can be cut at line boundaries | run-0e6c1ebf489a | 0.454 | 201.5 | 55.8 |
| 3 | Per-query hash table for labels, and pruned pushes | The largest search labels 78,554 of 1.74M nodes, and the median 196 | run-9000879b77a6 | 0.380 | 130 | 59.1 |
| 4 | Nodes renumbered by depth-first walks, SWAR parsing, and a batched, prefetched build | Random ids make every hop a cache miss, and the build was mostly cache misses on shared counters | run-… | 0.199 *local* | 164.4 *local* | 60.9 *local* |
| 5 | Arcs packed into 4 bytes, with a side table for heavy weights | 8 bytes per arc cost 80 MB, and fewer arcs fit in a cache line | run-b0774bc3ff3f | 0.389 | 123.9 | 57.6 |
| 6 | A 32-bit search with a 64-bit fallback | Nearly every distance fits in 32 bits, which halves labels and queue entries | run-… | 0.180 *local* | 93.2 *local* | 63.9 *local* |
| 7 | Numbering seeds claimed in blocks, and a read before each locked swap | On the board, step 5 was far slower than our scorer predicted, and all threads were queueing on one counter | run-9453b8f82b3d | 0.272 | 91.8 | 62.5 |
| 8 | Labels in pages found through a directory, no hash table, new numbers given at the first visit, and 104 fewer lines | A directory read and a page read replace probing and key compares, and each side fills only its own pages | run-d47336fddad4 | 0.238 | 78.0 | 65.4 |
| 9 | A node's out-count and links on one cache line, and prefetching into L2 | Each edge of the count pass touched three lines, and on a shared core the L1 prefetch queue is small | run-6c4a3ec99255 | 0.230 | 80.4 | 65.7 |
| 10 | The reverse graph derived from the forward one, by sweeping it | After renumbering, neighbours are close, so a sweep has few misses, where the file's order made every edge a random touch of six lines | run-3cffbc842db0 | 0.208 | 79.4 | 66.7 |
| 11 | A radix queue instead of binary heaps | Keys never go below the last one popped, so a key moves a few times in all, with no comparisons against other keys | run-20366d30099c | 0.184 | 82.1 | 67.8 |
| 12 | Nodes numbered in clusters the size of a label page, breadth first inside each | Two links per node instead of three cost 20%, so the numbering's quality matters far more than we'd assumed | run-03f74b0ab497 | 0.180 | 78.2 | 68.2 |
| 13 | Queries parsed and `numbers` dropped before the reverse graph is built | The 7 MB of old-to-new numbers were still live at the memory peak, and nothing in the reverse build reads them | run-… | 0.128 *local* | 69.5 *local* | 69.4 *local* |
| 14 | `INF` as `u64::MAX`, so an answer prints as a plain cast | The cast already gives -1 for "no path", which removes the output loop's branch and 5 lines | run-… | 0.128 *local* | 69.5 *local* | 69.4 *local* |
| 15 | Prefetching a node's offsets when it's pushed | Its arcs are read from those offsets soon after, on a dependent miss | run-… | 0.128 *local* | 69.5 *local* | 69.4 *local* |
| 16 | Coarse radix queue, k=7 | Skip sorting keys within a block; occasionally re-expand improved labels | run-a94ddb39f2f8 / run-6722fbed4b6d | 0.166 / 0.166 | 70.7 / 70.1 | 69.6 / 69.6 |
| 17 | u16 directory entries with a page-count guard | Halves the directory footprint while rejecting out-of-range page ids | run-f10dee4d919e | 0.166 | 67.9 | 69.7 |

## Decision log

**D17. Coarsen the radix queue into 128-distance blocks.** Context: sorting keys inside a small distance range costs more than occasional re-expansion. Chose: `COARSE_BITS = 7`; bucket 0 is LIFO inside the block, while `top()` remains a conservative lower bound for termination and pruning. The stale-label check skips an older, longer path. Evidence (`counts.sh`, seed 1 full): instructions 1,202.4 -> 1,154.0 per edge, branches 156.8 -> 142.6, mispredicts 7.32 -> 5.88. Quick 45/45 and thread sweep 240/240 pass. Local `ab.sh` over 31 pairs leaned -4.5% with 18.3% spread; query-phase measurements in the scan were 13–17% faster. The one-run local `score.sh` was T 0.1328 s, M 69.8 MB, S 424, K 23.98; its comparison was unpaired and does not resolve the A/B. Board runs `run-a94ddb39f2f8` and `run-6722fbed4b6d` scored K 24.64 and 24.72, both at T 0.166 s and S 424; M was 70.7 and 70.1 MB. Outcome: keep k=7; two board runs agree on T and improve the previous latest K 23.78 at T 0.181 s. A 31-round instrumented local sweep of widths 5–9 found query medians of 62.94, 61.04, 60.21, 61.23 and 74.81 ms. Paired medians vs k=7 were +4.92% (k=5), +3.48% (k=6), +2.92% (k=8) and +24.62% (k=9); k=6/8 quartiles cross zero, but neither beats k=7. All four alternate widths passed 45/45 grid and 240/240 thread cases.
**D18. Pack the per-thread label directory into 16-bit page ids.** Context: each side keeps one 32-bit directory entry per 64-node page, four directories per worker, though the full graph needs only about 27,200 pages. Chose: `u16` for both directory and used-page entries, guarded by an assertion on the renumbered page count before queries; larger numbered graphs fail before any narrowing can wrap. Evidence: full-scale `counts.sh` changed 1,154.4 -> 1,153.5 instructions/edge, branches 142.6 -> 142.2 and mispredicts 5.89 -> 5.88. Over 61 paired local runs, query medians were 63.33 -> 63.10 ms (paired -0.82%, Q1/Q3 -3.37%/+2.25%); whole-run medians were 145.28 -> 145.52 ms (paired -0.33%, -3.00%/+1.88%), so timing is unresolved. Five cgroup runs reduced median peak 65.94 -> 64.74 MiB (ranges 65.71–66.29 and 64.68–65.06). Quick 45/45, thread sweep 240/240, a 4.1M-node synthetic graph and the explicit larger-graph guard pass. Board S was 424; local `score.sh` reports S 428 for the candidate. One unpaired local run gave T 0.0836 s, M 70.1 MB, K 30.17 and is not comparable to the older local score. Board run `run-f10dee4d919e` scored K 24.81 at T 0.166 s, M 67.9 MB, S 428, versus the preceding k=7 run `run-6722fbed4b6d` at K 24.72, T 0.166 s, M 70.1 MB, S 424. Outcome: keep; the board confirms lower memory and the same time, with four extra code lines.

**D1. Alternate the two sides by work.** Context: the spec's island queries start on 14 nodes that can't reach the main body. Options: expand the side with the smaller top, or the side with less work. Chose: less work, because with the smaller top the backward side keeps winning and settles about 1.74M nodes per island query before the forward side runs out. Cost: none. Evidence: both rules are exact, and the full-scale grid passes. Outcome: kept.

**D2. Parse the file twice instead of holding an edge list.** Context: a list of 5M edges costs 60 MB at the peak. Chose: a counting pass and a placing pass over the mapped file. Cost: parsing runs twice. Outcome: kept.

**D3. Atomics for the shared build.** Context: threads count and place arcs in shared arrays. Options: per-thread count arrays (7 MB per thread per direction, 167 MB), or shared atomic counters. Chose: shared atomics, with `fetch_sub` reserving each arc's slot, which also means no `unsafe`. Relaxed atomic loads and stores compile to plain moves on x86, so the search doesn't pay for them. Cost: the order of arcs within a node depends on scheduling, which can't change a shortest distance. Evidence: T 1.886 -> 0.342 s, K 6.56 -> 13.05 (local), and the thread sweep passes at 1, 2, 3, 7, 12 and 64 threads. Outcome: kept.

**D4. Replace sparse-set slots with a small hash table per query.** Context: we measured seed 1's searches: 8.1M labels over 1,994 searches, median 196, largest 78,554, while the sparse set's slot array cost 7 MB per thread, 84 MB in all, and random ids soon touch every page. Options: keep it, or find labels through a table that starts at 1,024 slots each query and grows by 4x. Chose: the table. Cost: S rose by 34 lines. Evidence: M 201.8 -> 130.7 MB, T 0.342 -> 0.299 s, K 13.05 -> 14.95 (local). Outcome: kept.

**D5. Prune pushes.** Context: a node whose distance plus the other queue's top already reaches the best path can't be on a better one, unless the other side has settled it, in which case the best path already counted it. Chose: skip the push in that case, after updating the label and the best path. Evidence: the same run as D4, and the answers still match. Outcome: kept.

**D6. Renumber nodes, and batch the build.** Context: we tried renumbering alone first, and it nearly halved query time (190 to 105 ms), but the extra build time kept the total at 265 ms against 299 ms. We compared BFS, DFS and BFS limited to cells of 64 to 16,384 nodes, and DFS was best, by a small margin. A profile showed the build was mostly cache misses on shared counters and arcs. So: parse 32 edges at a time, prefetch all their cache lines, then touch them, reserving arc slots before storing, because an atomic update waits for earlier stores. Numbers parse from one 8-byte load. Cost: S 288 -> 427, and M 130.7 -> 164.4 MB, for the numbering's arrays and because threads now find more cache lines worth touching. Evidence: T 0.299 -> 0.199 s, K 14.95 -> 16.50 (local). Outcome: kept.

**D7. Pack arcs into 4 bytes.** Context: arcs took 80 MB. A node id needs 21 bits at this size, which leaves 11 bits for a weight, and most weights fit. Chose: the bit split follows the node count, the highest weight value means "look in the side table", and that table is sorted by arc number once the graph is built. Cost: S 427 -> 465. Evidence: M 164.4 -> 128.0 MB, T 0.199 -> 0.190 s, K 16.50 -> 17.50 (local). Outcome: kept.

**D8. Search in 32 bits first.** Context: labels took 24 bytes and queue entries 16, yet only the spec's huge-weight queries pass 2^32. Options: a narrower type with escapes, or generic code run twice. Chose: the same search, generic over the distance type. The 32-bit run gives up as soon as a distance doesn't fit, and that query runs again in 64 bits. Cost: S 465 -> 500. The first attempt compared before checking the fit, so a too-large candidate was silently dropped, and the bank caught it at once. Evidence: M 128.0 -> 93.2 MB, T 0.190 -> 0.180 s, K 17.50 -> 18.99 (local). Outcome: kept.

**D9. Trust the board over our scorer.** Context: step 5 took 0.389 s on the board, where our scorer predicted 0.253 s, so K fell from 15.30 to 14.19 despite the local gains. Fitting earlier runs put the board's build at about 124 ms before step 4 and about 260 ms after it. The only new shared write was in numbering: near the end, every thread fetched seeds from one counter, about 1.74M locked increments on one cache line. That costs little on our single-CCD PC, and much more with the box's slower cross-core traffic. Chose: seeds in blocks of 4,096, and a plain read before each locked swap. Cost: S +8. Evidence: T 0.180 -> 0.157 s, K 18.99 -> 20.32 (local). The board confirmed it: T 0.389 -> 0.272 s, K 14.19 -> 17.80. Outcome: kept.

**D10. Pages instead of a hash table.** Context: a review measured the search at 54% of the run, with the label table as its largest cost. Options: a bigger table, a node-sized array per thread (7 MB each), or pages. Chose: 64-node pages of 4-byte distances, through a directory of `nodes / 64` entries per side. Neighbours have nearby numbers, so the largest search fills about 585 KB, where the table and labels filled about 2 MB. Page 0 is all `UNREACHED` and empty directory entries point at it, so a read has no branch. With no key hash left, `sd_const_hash` has nothing to guard here. Evidence: local T 0.178 -> 0.138 s, M 87 -> 76 MB, and S 508 -> 404. What would change our minds: a lower K on the board.

**D11. Derive the reverse graph.** Context: Cause Of Death passed us with T 0.165 s against 0.230, at M 126 MB. Our stage timings were count 20 ms, numbering 12, moving counts 5, placing 43 and queries 64, so the build was over half the time, and a reader of the file can only visit edges in random order. Options: hold an edge list (costs M), or partition (a review found it slower). Chose: place only the forward arcs from the file, then fill the reverse array by sweeping them in node order. By then neighbours have nearby numbers, so in-degrees and reverse arcs land in memory that's nearly in cache, with no parsing and no number lookups, and the first pass no longer counts in-degrees. Evidence: local T -12.9% on `ab.sh`. What would change our minds: no gain on the board.

**D12. A radix queue.** Context: a review measured -6 to -8% with a radix queue, and our heaps took a mispredict on most sift steps. Chose: 65 buckets by the highest bit differing from the last key popped. It relies on Dijkstra's keys never going below that key. Evidence: local T -11.8% (noise 5.5%) on `ab.sh`, for 26 more lines. What would change our minds: no gain on the board.

**D13. Number in clusters.** Context: dropping from three links per node to two made the whole run 19.6% slower, so better numbering might pay as much. Options measured on `ab.sh` against depth first: pure breadth first +6.8%, clusters of 16 -0.2%, 64 -5.3% and -4.6% on a second run, 256 -2.4%, 1,024 +1.4%, and four or more links per node +3.5% and up, from the larger entries. Chose: clusters of 64, the size of a label page, growing breadth first, with the newest unexpanded node starting the next one. What would change our minds: no gain on the board.

**D14. Free the numbers before the reverse build.** Context: a review found the anonymous peak at 60.8 MB when the reverse graph finished, with `numbers` (7 MB) still live, while the queries reach only 54.8 MB. Chose: `build_graphs` splits into `build_graph` and `reverse`, and `main` parses the queries and drops `numbers` between them. Cost: S +5. Evidence: the peak by phase fell from 60.75 to 54.76 MB, now in the query phase. Scorer M is noisier: HEAD gave 71.8 and 69.6 MB (74.0 earlier), the new build 69.5, 69.5 and 69.6, so the gain shows as a steadier and lower ceiling, not a clean 6 MB. `counts.sh`: instructions 1,268.0 -> 1,186.4 per record, mispredicts 7.23 -> 7.26, which we didn't expect from a move and can't yet explain. What would change our minds: no lower M on the board.

**D15. A larger `INF`.** Context: the output loop only tested for `INF` to print -1, and `u64::MAX as i64` is -1. Nothing adds to `INF`, and the spec puts every real distance below 2^63, so two of them add without wrapping. Cost: S -5. Evidence: `counts.sh` instructions 1,186.4 -> 1,186.3 per record, mispredicts 7.26 -> 7.27, so this is a line saving only.

**D16. Prefetch offsets at push.** Context: a pop's first read is the node's offsets, a dependent miss. Chose: one prefetch at push, kept because it costs one line, though it's unproven: `ab.sh` over 31 runs gave -2.9% at 20.6% noise, the review's own run -3.5% at 20%, and query-phase medians over two rounds of 15 were 67.8 and 68.9 ms without it, 68.3 and 68.1 ms with it. `counts.sh`: instructions 1,186.4 -> 1,202.4 per record, mispredicts 7.27 -> 7.32. What would change our minds: a lower K on the board, or a need for this one line.

## What didn't work

| Experiment | What we expected | What we measured | Where | Kept? |
|---|---|---|---|---|
| PREFETCHW instead of a read prefetch for counters, offsets and arcs, to avoid stalling on another core's copy | Faster build on the box, whose cross-core traffic is slow | T 0.272 -> 0.276 s, K 17.80 -> 17.60: no gain. Its time now tracks plain clock speed | run-98438f4073de | reverted |
| Node id as its own label-table hash, since renumbered neighbours are close | Fewer cache misses in the label table | About 5x slower, because linear probing clusters on consecutive ids: T 0.21 -> 1.03 s by `ab.sh` | local | reverted |
| Also prefetching arcs during bucket redistribution (from a review) | A further gain on top of the offsets prefetch | No further gain | local | not applied |
| A radix queue as an array of buckets with an occupancy mask (from a review) | A cheaper scan for the next bucket | Wrong answers, as the bit for keys equal to `last` must stay clear, and the scan isn't hot anyway | local | not applied |
| A thread pool in place of 7 x 12 spawns (from a review) | Cheaper starts | Spawns cost 2.6 ms in all, a `Barrier` pool 2.9 ms | local | not applied |
| Two-tier exact Dial queue (4,096 near-distance ring buckets plus a binary-heap overflow) | Cheaper queue operations and fewer dependent heap steps | Correct: quick 45/45, threads 240/240. `counts.sh` vs guarded u16 radix k=7 was flat: 1,153.5 -> 1,153.6 instructions/edge, branches 142.2/142.2, mispredicts 5.88/5.88. Across 31 alternating full-scale pairs, query median 41.484 -> 41.588 ms (+0.25% paired median; mean +0.17 ms, 95% CI includes zero); whole-process median 102.174 -> 102.691 ms (+0.32% paired median); RSS 151.191 -> 151.121 MB. No material timing or memory benefit. | local scratch | rejected |

## The final design

Not final yet: see [README](README.md#approach) for the current one.

## How we know it's correct

- Every build is checked by SHA-256 against the naive reference on 15 generator seeds (tiny and sample) and 5 seeds at full scale, plus 8 answer-preserving rewrites of the input and 6 limit cases.
- **Reproduce it yourself:** `python3 tools/verify.py` (Docker, git and Python only).

## With another 24 hours

To be written at freeze.
