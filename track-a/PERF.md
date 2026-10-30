# Track A: performance log

**Result:** the reference takes **32.05 s / 228 MB** on the board. Ours is in the last row of [the journey](#the-journey).

**Check it yourself:** the board run named there, or locally `python3 tools/verify.py --tracks a`. Output SHA-256 on seed 1: `495ca09682b9673ef836a506496177680bb4a3af54255174befddb1f83481cd3`.

## Where it loses

- M is 55 MB locally, far above what the output needs (10 MB). Each thread keeps its own table and its own record lists, which grow by doubling, and sorting needs a second copy of each hour.
- S is 519 code lines against the reference's 56, which costs about 7.2 speed points. The reference is short because Python does the work.

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
| 5 | Output through a shared mapping, numbers printed by arithmetic, and no restart paths | Serial `write`s to tmpfs queue on the file's lock, and formatting cost 220 ns a line | run-… | 0.021 *local* | 27.1 *local* | 74.9 *local* |

## Decision log

**D1. Start with one thread.** Context: a correct v1 is worth more than half the reachable points, and a wrong answer scores zero. Options: start threaded, or start simple. Chose: one thread, so the first run proves the parsing and the output format alone. Cost: an idle box, for one push. Evidence: 44/44 on the local grid. Outcome: kept as step 1.

**D2. Records per hour instead of a list per group.** Context: with 12 threads, a list header per (thread, endpoint, hour) would cost about 50 MB before storing anything. Options: a shared table with locks, per-thread group tables, or per-thread lists per hour. Chose: per-thread lists per hour, holding one integer per request, because the sort that percentiles need then also does the grouping. Cost: M rose from 36 to 55 MB (local), since records are 8 bytes and sorting copies them. Evidence: T 0.230 -> 0.037 s, K 10.27 -> 22.65 (local), and the thread sweep passes at 1, 2, 3, 7, 12 and 64 threads. Outcome: kept.

**D3. Tie the segment size to the thread count.** Context: fixed 2 MB segments would keep every cut in the same place, so a thread-count sweep would test nothing about chunk edges. Chose: log size / (threads x 8), so each thread count moves every cut, down to cuts shorter than a line on tiny inputs. Cost: none measurable. Outcome: kept.

**D4. Block loads, and one shared table.** Context: on rung 2, `user` time was about 85% of the total, about 350 cycles per line, with a byte loop for the endpoint, a byte-at-a-time hash, and a byte loop for the latency. Options: keep the loops and tune, or load blocks. Chose: two 32-byte AVX2 loads to find the space, a hash of three words, a SWAR parse of up to 8 digits, and one lock-free table with keys inline in a cache line, which also removed the per-thread tables and their merge. The padded mapping means no tail code is needed for loads past the end of the file. Cost: S rose from 275 to 394 lines, and the program now uses `unsafe` for the loads, each behind a bounds assert. Evidence: T 0.037 -> 0.025 s, K 22.65 -> 26.17 (local). Outcome: kept.

**D5. One shared array per hour.** Context: M was 54 MB: 8-byte records with doubling slack, a copy to sort each hour, and the 10 MB output on top. Options: per-thread lists of 32-bit records freed hour by hour, or one array per hour that threads reserve blocks of. We tried the first and measured it (see below): M 31 MB but T 33 ms, because 864 small mappings cost about 13 ms. Chose: the second, with 72 mappings, no merge and an in-place sort. Latencies that don't fit in a record go to a side list. Cost: S 394 -> 514, and one `unsafe` cast from atomics to integers once threads have joined. Evidence: T 0.025 -> 0.022 s, M 54.4 -> 26.8 MB, K 26.17 -> 30.48 (local). Outcome: kept.

**D6. Write the output through a shared mapping.** Context: SpeedDEMONS passed us on the board with T 0.032 s. A profile showed the output stage at about half of T, with `write` calls serial, because tmpfs takes the file's lock for each. Chose: stretch the file to an upper bound, map it, let each thread claim its stretch in hour order, copy without waiting, and cut the file to length at the end. Numbers now print with no digit loop. We also dropped both restart paths, which cost lines for inputs the spec rules out: each hour's array now has room for every line, in pages that stay untouched, and more than 3,584 endpoints aborts. Cost: one `unsafe impl Sync`. Evidence: T 0.022 -> 0.021 s, K 30.48 -> 31.44 (local). Outcome: see the journey table.

## What didn't work

| Experiment | What we expected | What we measured | Where | Kept? |
|---|---|---|---|---|
| 32-bit records in 864 per-thread lists, freed hour by hour | M about 20 MB at the same T | M 31.4 MB, but T 25 -> 33 ms, K 26.17 -> 24.48. Each list being its own mapping cost about 13 ms | local | replaced by D5 |
| The first SWAR digit parse, with its multipliers written the wrong way round | Faster latency parsing | Wrong answers on 43 of 44 grid cells, caught at once | local | fixed before any push |

## The final design

Not final yet: see [README](README.md#approach) for the current one.

## How we know it's correct

- Every build is checked by SHA-256 against the naive reference on 15 generator seeds (tiny and sample) and 5 seeds at full scale, plus 8 answer-preserving rewrites of the input and 5 limit cases.
- A constant-hash test build, where every key collides, gives the same answers on tiny.
- **Reproduce it yourself:** `python3 tools/verify.py` (Docker, git and Python only).

## With another 24 hours

To be written at freeze.
