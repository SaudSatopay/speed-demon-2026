# Track A: performance log

**Result:** the reference takes **32.05 s / 228 MB** on the board. Ours is in the last row of [the journey](#the-journey).

**Check it yourself:** the board run named there, or locally `python3 tools/verify.py --tracks a`. Output SHA-256 on seed 1: `495ca09682b9673ef836a506496177680bb4a3af54255174befddb1f83481cd3`.

## Where it loses

- It's single-threaded, so 11 of the box's 12 threads sit idle.
- Each latency list grows by doubling, which wastes memory and time on allocation.
- S is 208 code lines against the reference's 56, which costs about 8.5 speed points. The reference is short because Python does the work.

## Where the reference spends its time

About 77% of the time is the per-line `strptime`, and the latency lists hold the 228 MB of Python ints.

## The journey

Every step cites the live-board run that measured it. Numbers marked *local* come from our scorer, on a faster PC, and only explain decisions.

| Step | Change | Why we expected it to help | Run ID | T (s) | M (MB) | Points |
|---|---|---|---|---|---|---|
| 0 | The reference, unchanged | — | — | 32.05 | 228 | 0 |
| 1 | v1: mmap, fixed-offset parsing, full-key table, one thread | Removes `strptime` and per-line objects | run-… | 0.230 *local* | 36.1 *local* | 50.6 *local* |

## Decision log

**D1. Start with one thread.** Context: a correct v1 is worth more than half the reachable points, and a wrong answer scores zero. Options: start threaded, or start simple. Chose: one thread, so the first run proves the parsing and the output format alone. Cost: an idle box, for one push. Evidence: 44/44 on the local grid. Outcome: kept as step 1.

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
