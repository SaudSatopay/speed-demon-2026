# Track C: performance log

**Result:** the reference takes **14.44 s / 367 MB** on the board. Ours is in the last row of [the journey](#the-journey).

**Check it yourself:** the board run named there, or locally `python3 tools/verify.py --tracks c`. Output SHA-256 on seed 1: `d8088b1cd42ba1338b1345b107b55db7b96ef1f5066f3ff64fd26a8912a0f536`.

## Where it loses

- It's single-threaded, so 11 of the box's 12 threads sit idle.
- It reads one byte at a time, where block-at-a-time scanning is known to be far faster.

## Where the reference spends its time

About 28% is the number regular expressions, 27% the aggregation and 19% reading the CSV, all in Python objects.

## The journey

Every step cites the live-board run that measured it. Numbers marked *local* come from our scorer, on a faster PC, and only explain decisions.

| Step | Change | Why we expected it to help | Run ID | T (s) | M (MB) | Points |
|---|---|---|---|---|---|---|
| 0 | The reference, unchanged | — | — | 14.44 | 367 | 0 |
| 1 | v1: scalar parser from the spec, no strings, interned names, one thread | Removes Python objects and all decoding | run-… | 0.404 *local* | 17.8 *local* | 42.6 *local* |

## Decision log

**D1. Never unescape.** Context: `names_distinct` compares unescaped values, and `name_bytes` counts them. Options: unescape into a buffer, or work from the raw bytes. Chose: raw bytes. Inside a quoted field, every `"` belongs to a `""` pair, so the raw bytes encode the value one to one, and the value's length is the raw length minus the number of pairs. Cost: none. Evidence: the grid's rewrites and limit cases. Outcome: kept.

## What didn't work

Nothing tried yet.

## The final design

Not final yet: see [README](README.md#approach) for the current one.

## How we know it's correct

- Every build is checked by SHA-256 against the naive reference on 15 generator seeds (tiny and sample) and 5 seeds at full scale, plus 10 answer-preserving rewrites of the input and 4 limit cases.
- A constant-hash test build, where every key collides, gives the same answers on tiny.
- **Reproduce it yourself:** `python3 tools/verify.py` (Docker, git and Python only).

## With another 24 hours

To be written at freeze.
