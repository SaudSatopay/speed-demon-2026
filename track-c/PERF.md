# Track C: performance log

**Result:** the reference takes **14.44 s / 367 MB** on the board. Ours is in the last row of [the journey](#the-journey).

**Check it yourself:** the board run named there, or locally `python3 tools/verify.py --tracks c`. Output SHA-256 on seed 1: `d8088b1cd42ba1338b1345b107b55db7b96ef1f5066f3ff64fd26a8912a0f536`.

## Where it loses

- M is 22 MB locally, against the 7.3 MB where more savings stop counting. The name table takes 8.4 MB of that.
- S is 549 code lines against the reference's 108, which costs about 5.3 speed points.

## Where the reference spends its time

About 28% is the number regular expressions, 27% the aggregation and 19% reading the CSV, all in Python objects.

## The journey

Every step cites the live-board run that measured it. Numbers marked *local* come from our scorer, on a faster PC, and only explain decisions.

| Step | Change | Why we expected it to help | Run ID | T (s) | M (MB) | Points |
|---|---|---|---|---|---|---|
| 0 | The reference, unchanged | — | — | 14.44 | 367 | 0 |
| 1 | v1: scalar parser from the spec, no strings, interned names, one thread | Removes Python objects and all decoding | run-7908768d3243 | 0.770 | 10.6 | 44.2 |
| 2 | 12 threads: cuts at CRLF, a lock-free shared name table with one cache line per name | Records are independent, and name lookups dominate the time | run-e9b10f4d8fed | 0.086 | 14 | 65.4 |
| 3 | Block loads for every field, and lookups batched with prefetching | About 470 cycles per record, with a mispredicted loop exit per field and a stall per name lookup | run-… | 0.028 *local* | 22.2 *local* | 68.5 *local* |

## Decision log

**D1. Never unescape.** Context: `names_distinct` compares unescaped values, and `name_bytes` counts them. Options: unescape into a buffer, or work from the raw bytes. Chose: raw bytes. Inside a quoted field, every `"` belongs to a `""` pair, so the raw bytes encode the value one to one, and the value's length is the raw length minus the number of pairs. Cost: none. Evidence: the grid's rewrites and limit cases. Outcome: kept.

**D2. Cut segments at CRLF, not by counting quotes.** Context: a thread must start on a record boundary, and quoted notes contain LFs. Options: count quotes in a first pass to find the parity at each cut, or use section 2.2's promise that a CR is only ever the start of a CRLF. Chose: the CRLF, which needs no extra pass, since about half the records end in one. Cost: a file with no CRLF isn't split, though it's still right. The `lf-except-last` variant in our bank tests exactly that. Outcome: kept.

**D3. One shared name table instead of one per thread.** Context: 12 tables and bitsets would cost about 58 MB, and 2x of memory is worth 3 speed points. Options: per-thread tables merged at the end, or one lock-free table. Chose: one table, where each 64-byte slot holds a header, the first 24 bytes of the name, and one bit per country. On seed 1, 99.99% of names fit inline, so a lookup usually costs one cache miss, without touching the file. Cost: 8.4 MB for the table. If a file ever holds more names than the table, the pass restarts with a bigger table. A test build starts with 16 slots, to exercise that path. Evidence: T 0.404 -> 0.041 s, K 7.11 -> 20.11 (local). Sweep passes at 1, 2, 3, 7, 12 and 64 threads, and at full scale on seeds 1 and 7. Outcome: kept.

**D4. Block loads, and batched lookups.** Context: a record cost about 470 cycles. Each field's byte loop mispredicted its exit, and each name lookup stalled on a cache miss. Chose: 64-byte quote masks for text fields, one 8-byte load per number, and an amount squeezed into 8 digits by moving the digits before the point up a byte. Records then wait in batches of 16 while their table slots and region entries are prefetched. The padded mapping means no tail code. Cost: S 410 -> 549. Evidence: T 0.041 -> 0.028 s, K 20.11 -> 23.44 (local), and the sweep, including `lf-except-last`, still passes. Outcome: kept.

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
