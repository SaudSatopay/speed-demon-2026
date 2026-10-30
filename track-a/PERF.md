# Track A: performance log

**Result:** the reference takes **32.05 s / 228 MB** on the board, and our best run is **0.030 s / 20.7 MB** (`run-9b077878b013`). That's K = **41.40** and **80.9** speed points.

**Check it yourself:** the board run named there, or locally `python3 tools/verify.py --tracks a`. Output SHA-256 on seed 1: `495ca09682b9673ef836a506496177680bb4a3af54255174befddb1f83481cd3`.

## Where it loses

- M is 55 MB locally, far above what the output needs (10 MB). Each thread keeps its own table and its own record lists, which grow by doubling, and sorting needs a second copy of each hour.
- S is far above the reference's 56 code lines, which costs about 7 speed points. The reference is short because Python does the work.

## Where the reference spends its time

About 77% of the time is the per-line `strptime`, and the latency lists hold the 228 MB of Python ints.

## The journey

Every step cites the live-board run that measured it. Numbers marked *local* come from our scorer, on a faster PC, and only explain decisions.

| Step | Change | Why we expected it to help | Run ID | T (s) | M (MB) | Points |
|---|---|---|---|---|---|---|
| 0 | The reference, unchanged | — | — | 32.05 | 228 | 0 |
| 1 | v1: mmap, fixed-offset parsing, full-key table, one thread | Removes `strptime` and per-line objects | run-45e4054a9bf6 | 0.443 | 35.3 | 50.3 |
| 2 | 12 threads: segments from an atomic cursor, one sortable record per request, ordered appends | Parsing and sorting are independent per segment and per hour | run-4817f2d2cc47 | 0.062 | 48.3 | 69.4 |
| 3 | Block loads on the hot path: AVX2 space search, word hashing, SWAR latency, one shared lock-free table | We measured about 350 cycles per line, and only 15% of the time in the kernel | run-d35f7df5c506 | 0.039 | 48.0 | 73.4 |
| 4 | 32-bit records in one shared array per hour, sorted in place and freed as the text is written | The output counts toward M, and records were 8 bytes, with a copy to sort | run-f1c9f21494d2 | 0.036 | 20.3 | 77.1 |
| 5 | Output through a shared mapping, numbers printed by arithmetic, and no restart paths | Serial `write`s to tmpfs queue on the file's lock, and formatting cost 220 ns a line | run-caafa693c324 | 0.036 | 20.8 | 76.9 |
| 6 | A smaller endpoint table, comparing keys against the log | Inline keys cost 262 KB of table and 20 lines, for just 1,842 endpoints | run-e1d40bd48c9a | 0.034 | 19.9 | 77.8 |
| 7 | Group each hour by counting, in place, and select percentiles | A full sort was about half the output stage, for three order statistics per group | run-c17dc23201a0 | 0.033 | 20.3 | 77.9 |
| 8 | Back to ordered appends for the output | The mapped output never gained anything on the board, and cost about 35 lines | run-5289b7130599 | 0.033 | 20.0 | 78.5 |
| 9 | Same design in 307 lines: one word per table slot, published by one compare-and-swap, and no separate rank pass | Rivals were at our T with 210 lines, and 2x S is worth 2.26 points. Cutting needed no speed | run-43987ef4ad03 | 0.032 | 20.9 | 79.7 |
| 10 | Keys compared by one block compare, not memcmp | A call to memcmp branches on the length, which varies from line to line | run-aaaf2f99c450 | 0.031 | 20.7 | 80.2 |
| 11 | Shorter slicing and prefix sums, and hours grouped in their own arrays with no copy | Nothing here runs per record, and 5 lines are worth 0.1 points | run-… | 0.036 *local* | 26.9 *local* | 70.9 *local* |
| 12 | One bounds check per line for the fixed-distance loads, not one per load | The load asserts cost 16 instructions per record, and the padding already covers these loads | run-… | 0.041 *local* | 27.0 *local* | 69.4 *local* |
| 13 | Percentiles selected upward, p50 then p95 then p99 | After p50 splits a large group, the later selections only search its top half | run-… | 0.037 *local* | 26.5 *local* | 70.7 *local* |
| 14 | Table slots store a u32 position and compare through the closing space | Halves the table; the full-key comparison still handles collisions | run-5c3a4b720daa / run-80bd6120701c / run-2f9fbbf914cf | 0.030 / 0.030 / 0.031 | 20.6 / 21.2 / 20.4 | 80.7 / 80.7 / 80.4 |
| 15 | Generic unaligned load and shorter rank assignment | Eight fewer lines; no measurable hot-path change | run-d7167a1f13e5 / run-9b077878b013 | 0.030 / 0.030 | 21.3 / 20.7 | 80.8 / 80.9 |

## Decision log

**D13. Keep only a 32-bit first position in each endpoint slot.** Context: the prior slot packed a hash fragment, length and log position into 64 bits. Chose: a u32 position, with zero still meaning empty, and compare through the endpoint's closing space so keys with shared prefixes remain distinct. The spec's 3.1M-line log is below the added 2^32 byte bound. Evidence (`counts.sh`, seed 1 full): 288.6 -> 277.0 instructions per request, 32-bit slots; mispredicts 1.27 -> 1.26. S 295 -> 292. Constant-hash passed 39/39, and the current tree's quick grid and thread sweep passed 44/44 and 234/234. Board runs `run-5c3a4b720daa` and `run-80bd6120701c` gave K 41.06 and 41.17 at T 0.030 s; `run-2f9fbbf914cf` gave K 40.51 at T 0.031 s. The earlier best remains K 41.24. Outcome: keep; the three board times are within run noise, while the smaller slot and lower instruction count are repeatable.

**D14. Replace two load helpers with one generic unaligned load, and shorten rank assignment.** Context: `load_u64` and `load_block` both do checked-at-call-site unaligned reads from the padded mapping. Chose: one monomorphized helper for both shapes, and a `zip`/`for_each` rank pass. Cost: the method-length expression is written without redundant parentheses to keep the formatted line short. Evidence: S 292 -> 284; counts stay at 277.0 instructions and 1.26 simulated mispredicts per request. `ab.sh` (31 pairs) was +0.4% with 105.6% spread, which separates neither build. Quick 44/44, thread sweep 234/234, constant-hash tiny 20/20, and pre-push 44/44 pass. Board runs `run-d7167a1f13e5` and `run-9b077878b013` gave K 41.24 and 41.40, both at T 0.030 s and S 284; M was 21.3 and 20.7 MB. Outcome: keep the eight-line cut; its second timing also supports no regression, and the latest run sets a new best.
**D1. Start with one thread.** Context: a correct v1 is worth more than half the reachable points, and a wrong answer scores zero. Options: start threaded, or start simple. Chose: one thread, so the first run proves the parsing and the output format alone. Cost: an idle box, for one push. Evidence: 44/44 on the local grid. Outcome: kept as step 1.

**D2. Records per hour instead of a list per group.** Context: with 12 threads, a list header per (thread, endpoint, hour) would cost about 50 MB before storing anything. Options: a shared table with locks, per-thread group tables, or per-thread lists per hour. Chose: per-thread lists per hour, holding one integer per request, because the sort that percentiles need then also does the grouping. Cost: M rose from 36 to 55 MB (local), since records are 8 bytes and sorting copies them. Evidence: T 0.230 -> 0.037 s, K 10.27 -> 22.65 (local), and the thread sweep passes at 1, 2, 3, 7, 12 and 64 threads. Outcome: kept.

**D3. Tie the segment size to the thread count.** Context: fixed 2 MB segments would keep every cut in the same place, so a thread-count sweep would test nothing about chunk edges. Chose: log size / (threads x 8), so each thread count moves every cut, down to cuts shorter than a line on tiny inputs. Cost: none measurable. Outcome: kept.

**D4. Block loads, and one shared table.** Context: on rung 2, `user` time was about 85% of the total, about 350 cycles per line, with a byte loop for the endpoint, a byte-at-a-time hash, and a byte loop for the latency. Options: keep the loops and tune, or load blocks. Chose: two 32-byte AVX2 loads to find the space, a hash of three words, a SWAR parse of up to 8 digits, and one lock-free table with keys inline in a cache line, which also removed the per-thread tables and their merge. The padded mapping means no tail code is needed for loads past the end of the file. Cost: S rose from 275 to 394 lines, and the program now uses `unsafe` for the loads, at first each behind a bounds assert (D11 cut those). Evidence: T 0.037 -> 0.025 s, K 22.65 -> 26.17 (local). Outcome: kept.

**D5. One shared array per hour.** Context: M was 54 MB: 8-byte records with doubling slack, a copy to sort each hour, and the 10 MB output on top. Options: per-thread lists of 32-bit records freed hour by hour, or one array per hour that threads reserve blocks of. We tried the first and measured it (see below): M 31 MB but T 33 ms, because 864 small mappings cost about 13 ms. Chose: the second, with 72 mappings, no merge and an in-place sort. Latencies that don't fit in a record go to a side list. Cost: S 394 -> 514, and one `unsafe` cast from atomics to integers once threads have joined. Evidence: T 0.025 -> 0.022 s, M 54.4 -> 26.8 MB, K 26.17 -> 30.48 (local). Outcome: kept.

**D6. Write the output through a shared mapping.** Context: SpeedDEMONS passed us on the board with T 0.032 s. A profile showed the output stage at about half of T, with `write` calls serial, because tmpfs takes the file's lock for each. Chose: stretch the file to an upper bound, map it, let each thread claim its stretch in hour order, copy without waiting, and cut the file to length at the end. Numbers now print with no digit loop. We also dropped both restart paths, which cost lines for inputs the spec rules out: each hour's array now has room for every line, in pages that stay untouched, and more than 3,584 endpoints aborts. Cost: one `unsafe impl Sync`. Evidence: T 0.022 -> 0.021 s, K 30.48 -> 31.44 (local), but the board stayed at T 0.036 s, so `write` wasn't the bottleneck there. Kept for now. Outcome: removed again in step 8, when it cost lines and still hadn't gained time on the board. T held at 0.033 s and K rose from 36.19 to 37.15.

**D7. Remove the inline keys.** Context: with only 1,842 endpoints, the key texts in the log fit in the caches anyway, so a 64-byte slot per endpoint might not earn its 20 lines. Chose: a 16-byte slot, and a compare against the key's first occurrence in the log. It came out both simpler and faster, because the table now fits the L1 cache. Evidence: T 0.021 -> 0.018 s, S 519 -> 492, K 31.44 -> 34.06 (local). Outcome: kept.

**D8. Cut the code, not the design.** Context: ttimesai took first place with our T and 210 lines to our 468, so code size was what cost us. Options: drop the fast paths, or find what the fast paths don't need. Chose: the second. A table slot became one word (16 bits of hash, the length, the key's first position), so one compare-and-swap publishes it, with no busy state to spin on, and the table halved to 32 KB. Grouping looks up each record's rank itself, so the rank pass went. The method table became a 40-bit constant. Safe code replaced the cast from atomics, and arrays of counts moved onto the stack. Evidence: S 468 -> 307, with local T unchanged at 19.2 ms, and all checks passing. The first version moved big latencies into a list per hour behind a lock, next to the block counter and the array pointer that every record read. Locally it looked free, and on the board T rose from 0.033 to 0.037 s (run-d7cbabc9f01a). So each thread now keeps its own big latencies, and its unfilled blocks as slices, and the hot path reads no line another thread writes.

**D9. Compare keys without memcmp.** Context: on Track D, cutting mispredicts paid more than the instruction count suggested. Every line here compares its endpoint against the endpoint's first occurrence. Chose: load 64 bytes from both, and check the run of equal bytes covers the key, which has no branch on the length for the 94% of keys that fit. Evidence: at sample scale, instructions 492.7 -> 460.4 per record and mispredicts 3.26 -> 2.63 per record, on `counts.sh`. Sorting each group instead of selecting three percentiles was 4 lines shorter, but cost +5.2% instructions and +9% mispredicts at full scale, so we kept selecting.

**D10. Cut five lines nobody runs per record.** Context: a review found slicing, prefix sums and a collect that could be shorter. `AtomicU32::get_mut_slice` also removes the copy from atomics to integers. Evidence: S 301 -> 296, with instructions 319.2 -> 319.3 per record and mispredicts 1.19 -> 1.19 per record at full scale, on `counts.sh`.

**D11. One bounds check per line.** Context: the asserts inside `load_u64` and `load_block` ran several times per record, but each load is a short fixed distance from a position already known to be inside the log, and `PADDING` is 4,096 bytes. Options: keep them, or make the load helpers `unsafe` and check only where a position can actually run away. Chose: the checked `log[key_end + 1]` proves the space is inside the log, before `id_of` and `parse_latency` rely on it, and one assert in the rare path of `offset_of_space` covers its later blocks. Memory safety holds on any input: 1,701 malformed inputs at 1 and 3 threads gave no SIGSEGV, SIGBUS or SIGILL, and matched the old build. Evidence (`counts.sh`, seed 1 full): instructions 319.6 -> 300.3 per record (-6.0%), branches 37.7 -> 33.5, mispredicts 1.19 unchanged; S 296 -> 295. Parse phase about -1 to -2.5% in paired stamped builds (local). Whole-run T is inside the noise: patched K was 21.0, 22.0, 26.7 and 24.4 across four runs, against 23.7 for one base run. The estimate is about +0.3% K, derived and too small for the board to confirm. Outcome: kept, on the deterministic counts and the parse timings.

**D13. Sort each hour by radix.** Context: the current grouping plus three order-statistic selections can mispredict heavily; a stable LSD sort can order every record by `(endpoint rank, latency)` in four passes. Scratch v3 uses the output `Vec<u8>` as aligned u32 scratch, then reuses it for text. Ported to current source, it passes quick 44/44, thread sweep 234/234 and constant-hash tiny 20/20. `counts.sh`, S, peak memory and paired timings remain pending; this is not integrated.

**D12. Select percentiles upward.** Context: selecting p99 first and then searching below it repartitions nearly the whole group each time, about 5.9n of work on a large group. Options: keep it, or select p50 first and search above each result, about 3.1n. Chose: upward. Big latencies still sort last, and a repeated rank in a group of 1 or 2 reuses the value already selected. Evidence (`counts.sh`, seed 1 full): instructions 300.3 -> 288.6 per record (-3.9%), branches 33.5 -> 33.1, mispredicts 1.19 -> 1.27 per record (+7%); S 295 unchanged, since the extra comment line doesn't count. Stamped `format_hour` at 1 thread: 18.61 -> 17.37 ms (-6.6%, quartiles apart), and total -2.2% by median, -1.3% by min (local). At 12 threads the timing was inconclusive, and `ab.sh` could not resolve it either. `score.sh`: K 25.89 for one run, inside the earlier 21.0 to 26.7 spread. The estimate is +0.5 to +0.65% K, too small for the board to confirm, and the box was not measured; its higher mispredict penalty may take back more. Outcome: kept, on the instruction counts and the stage timings.

## What didn't work

| Experiment | What we expected | What we measured | Where | Kept? |
|---|---|---|---|---|
| 32-bit records in 864 per-thread lists, freed hour by hour | M about 20 MB at the same T | M 31.4 MB, but T 25 -> 33 ms, K 26.17 -> 24.48. Each list being its own mapping cost about 13 ms | local | replaced by D5 |
| A software prefetch of the log, 256 or 1,024 bytes ahead of the parse | Less stalling on the data | -3.8% and +0.1%, with noise of 17% and 19% | local | not kept |
| The first SWAR digit parse, with its multipliers written the wrong way round | Faster latency parsing | Wrong answers on 43 of 44 grid cells, caught at once | local | fixed before any push |
| Bounded ready-output handoff, variants v1-v4 | Remove ordered-turn waits while bounding waiting text | v1 (Condvar broadcasts) +7.08% wall at 12 threads. v2 (`notify_one`) -1.85%, RSS flat, but S 284 -> 334 makes estimated relative K -1.4%. v3 (per-hour slots) +5.49%, S 284 -> 306, RSS +0.22 MB. v4 (split claim gate/writer) -1.15% paired median at 12 threads, 95% CI for mean time includes zero, S 284 -> 323, RSS +0.30 MB; estimated relative K -1.5%. All passed quick 44/44 and thread 234/234; first-hour-heavy differential passed for all, constant-hash tiny passed for v1/v2/v4. No candidate improves line-cost-adjusted K. | local, seed 1 full; 41 alternating pairs at 1/12 threads | rejected |

## The final design

Not final yet: see [README](README.md#approach) for the current one.

## How we know it's correct

- Every build is checked by SHA-256 against the naive reference on 15 generator seeds (tiny and sample) and 5 seeds at full scale, plus 8 answer-preserving rewrites of the input and 5 limit cases.
- A constant-hash test build, where every key collides, gives the same answers on tiny.
- **Reproduce it yourself:** `python3 tools/verify.py` (Docker, git and Python only).

## With another 24 hours

To be written at freeze.
