# Track A: performance log

**Result:** the reference takes **32.05 s / 228 MB** on the board. Ours is in the last row of [the journey](#the-journey).

**Check it yourself:** the board run named there, or locally `python3 tools/verify.py --tracks a`. Output SHA-256 on seed 1: `495ca09682b9673ef836a506496177680bb4a3af54255174befddb1f83481cd3`.

## Where it loses

- M is 55 MB locally, far above what the output needs (10 MB). Each thread keeps its own table and its own record lists, which grow by doubling, and sorting needs a second copy of each hour.
- S is 275 code lines against the reference's 56, which costs about 5.2 speed points. The reference is short because Python does the work.

## Where the reference spends its time

About 77% of the time is the per-line `strptime`, and the latency lists hold the 228 MB of Python ints.

## The journey

Every step cites the live-board run that measured it. Numbers marked *local* come from our scorer, on a faster PC, and only explain decisions.

| Step | Change | Why we expected it to help | Run ID | T (s) | M (MB) | Points |
|---|---|---|---|---|---|---|
| 0 | The reference, unchanged | — | — | 32.05 | 228 | 0 |
| 1 | v1: mmap, fixed-offset parsing, full-key table, one thread | Removes `strptime` and per-line objects | run-… | 0.230 *local* | 36.1 *local* | 50.6 *local* |
| 2 | 12 threads: segments from an atomic cursor, one sortable record per request, ordered appends | Parsing and sorting are independent per segment and per hour | run-… | 0.037 *local* | 55.0 *local* | 67.8 *local* |

## Decision log

**D1. Start with one thread.** Context: a correct v1 is worth more than half the reachable points, and a wrong answer scores zero. Options: start threaded, or start simple. Chose: one thread, so the first run proves the parsing and the output format alone. Cost: an idle box, for one push. Evidence: 44/44 on the local grid. Outcome: kept as step 1.

**D2. Records per hour instead of a list per group.** Context: with 12 threads, a list header per (thread, endpoint, hour) would cost about 50 MB before storing anything. Options: a shared table with locks, per-thread group tables, or per-thread lists per hour. Chose: per-thread lists per hour, holding one integer per request, because the sort that percentiles need then also does the grouping. Cost: M rose from 36 to 55 MB (local), since records are 8 bytes and sorting copies them. Evidence: T 0.230 -> 0.037 s, K 10.27 -> 22.65 (local), and the thread sweep passes at 1, 2, 3, 7, 12 and 64 threads. Outcome: kept.

**D3. Tie the segment size to the thread count.** Context: fixed 2 MB segments would keep every cut in the same place, so a thread-count sweep would test nothing about chunk edges. Chose: log size / (threads x 8), so each thread count moves every cut, down to cuts shorter than a line on tiny inputs. Cost: none measurable. Outcome: kept.

## What didn't work

Nothing tried yet.

## The final design

Not final yet: see [README](README.md#approach) for the current one.

## How we know it's correct

- Every build is checked by SHA-256 against the naive reference on 15 generator seeds (tiny and sample) and 5 seeds at full scale, plus 8 answer-preserving rewrites of the input and 5 limit cases.
- A constant-hash test build, where every key collides, gives the same answers on tiny.
- **Reproduce it yourself:** `python3 tools/verify.py` (Docker, git and Python only).

## With another 24 hours

To be written at freeze.
