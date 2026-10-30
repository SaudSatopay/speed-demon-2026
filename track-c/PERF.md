# Track C: performance log

**Result:** the reference takes **14.44 s / 367 MB** on the board. Ours is in the last row of [the journey](#the-journey).

**Check it yourself:** the board run named there, or locally `python3 tools/verify.py --tracks c`. Output SHA-256 on seed 1: `d8088b1cd42ba1338b1345b107b55db7b96ef1f5066f3ff64fd26a8912a0f536`.

## Where it loses

- M is 13 MB on the board, against the 7.3 MB where more savings stop counting. The name table takes 8.4 MB of that.
- S is 339 code lines against the reference's 108, which costs about 3.7 speed points.

## Where the reference spends its time

About 28% is the number regular expressions, 27% the aggregation and 19% reading the CSV, all in Python objects.

## The journey

Every step cites the live-board run that measured it. Numbers marked *local* come from our scorer, on a faster PC, and only explain decisions.

| Step | Change | Why we expected it to help | Run ID | T (s) | M (MB) | Points |
|---|---|---|---|---|---|---|
| 0 | The reference, unchanged | — | — | 14.44 | 367 | 0 |
| 1 | v1: scalar parser from the spec, no strings, interned names, one thread | Removes Python objects and all decoding | run-7908768d3243 | 0.770 | 10.6 | 44.2 |
| 2 | 12 threads: cuts at CRLF, a lock-free shared name table with one cache line per name | Records are independent, and name lookups dominate the time | run-e9b10f4d8fed | 0.086 | 14 | 65.4 |
| 3 | Block loads for every field, and lookups batched with prefetching | About 470 cycles per record, with a mispredicted loop exit per field and a stall per name lookup | run-239daeb7287a | 0.055 | 14.2 | 69.3 |
| 4 | Slow paths the spec rules out removed, 1-byte region entries | Sanjay Sah passed us with half our lines and less memory, and S and M were costing more than T | run-c96eba9a4379 | 0.050 | 12.9 | 71.2 |
| 5 | Same design in 369 lines: one exit from the table's loop, a row that is an array, and regions parsed once | Two passes over `regions.csv` were serial time, and rivals have half our lines | run-126b276f6fae | 0.061 | 12.8 | 69.9 |
| 6 | Regions parsed in one pass, and the output sorted once, when it's written | The temporary list, the `seen` table and the output loop's destructuring were all lines with no job at run time | run-… | 0.0304 *local* | 23.5 *local* | 68.8 *local* |
| 7 | BZHI masks the key words, in place of a `u128` shift | Three 128-bit shifts per record were about 5% of all instructions, and the box is limited by its clock | run-… | 0.0314 *local* | 21.1 *local* | 69.0 *local* |

## Decision log

**D1. Never unescape.** Context: `names_distinct` compares unescaped values, and `name_bytes` counts them. Options: unescape into a buffer, or work from the raw bytes. Chose: raw bytes. Inside a quoted field, every `"` belongs to a `""` pair, so the raw bytes encode the value one to one, and the value's length is the raw length minus the number of pairs. Cost: none. Evidence: the grid's rewrites and limit cases. Outcome: kept.

**D2. Cut segments at CRLF, not by counting quotes.** Context: a thread must start on a record boundary, and quoted notes contain LFs. Options: count quotes in a first pass to find the parity at each cut, or use section 2.2's promise that a CR is only ever the start of a CRLF. Chose: the CRLF, which needs no extra pass, since about half the records end in one. Cost: a file with no CRLF isn't split, though it's still right. The `lf-except-last` variant in our bank tests exactly that. Outcome: kept.

**D3. One shared name table instead of one per thread.** Context: 12 tables and bitsets would cost about 58 MB, and 2x of memory is worth 3 speed points. Options: per-thread tables merged at the end, or one lock-free table. Chose: one table, where each 64-byte slot holds a header, the first 24 bytes of the name, and one bit per country. On seed 1, 99.99% of names fit inline, so a lookup usually costs one cache miss, without touching the file. Cost: 8.4 MB for the table. If a file ever holds more names than the table, the pass restarts with a bigger table. A test build starts with 16 slots, to exercise that path. Evidence: T 0.404 -> 0.041 s, K 7.11 -> 20.11 (local). Sweep passes at 1, 2, 3, 7, 12 and 64 threads, and at full scale on seeds 1 and 7. Outcome: kept.

**D4. Block loads, and batched lookups.** Context: a record cost about 470 cycles. Each field's byte loop mispredicted its exit, and each name lookup stalled on a cache miss. Chose: 64-byte quote masks for text fields, one 8-byte load per number, and an amount squeezed into 8 digits by moving the digits before the point up a byte. Records then wait in batches of 16 while their table slots and region entries are prefetched. The padded mapping means no tail code. Cost: S 410 -> 549. Evidence: T 0.041 -> 0.028 s, K 20.11 -> 23.44 (local), and the sweep, including `lf-except-last`, still passes. Outcome: kept.

**D5. Remove what the spec rules out.** Context: Sanjay Sah took 1st with T 0.066 s against our 0.051, but S 275 against our 549. Section 2.2 caps ids, region ids and amounts at 8 bytes, so their byte-loop fallbacks could go, and 240 countries fit in a byte, which frees 1 MB. More than 255 countries now aborts rather than risking a wrong answer. Cost: none in time. Evidence: S 549 -> 485, M 22.2 -> 20.8 MB, K 23.46 -> 24.09 (local). Outcome: see the journey table.

**D6. Cut the code, not the design.** Context: on Track A, cutting lines without touching the hot paths took first place. Options: the same here. Chose: a row as an array of eight numbers, where a single record is a row too, so one `merge` adds a record to its country and adds up the threads' rows. Regions are parsed once, into a temporary list, instead of twice, because that stage is serial. Ids past the highest region share one last entry of the table, so finding a country has no branch. Evidence: S 485 -> 369, with local T -2.5% (noise 7.4%), and all checks passing. What would change our minds: a higher T on the board, over a few runs, since T swings between 0.050 and 0.063 s on the board with no change in the code.

**D7. Give out country indices as codes appear, and sort at the end.** Context: byte-order indices needed every code before the first index could be given out, so regions took a temporary list and a second pass. Options: keep that, or give out indices on first sight and sort the 241 output lines once. Chose: the latter, with index 0 as `_UNMATCHED`, and `country_of` grown in doubling steps, so its last entry still lies past every id. Cost: none; the sort is 241 short strings. Evidence: S 356 -> 339, counts.sh 415.0 -> 414.5 instructions and 1.53 -> 1.54 mispredicts per record, K 23.27 -> 23.81 (local, with T and M inside their noise). Outcome: kept.

**D8. BZHI for the key mask.** Context: masking a name's three key words used a `u128` shift each, to cope with a full 8-byte word. Options: keep it, a branch on the length, or BZHI, which x86-64-v3 has. Chose: BZHI. It reads only the low 8 bits of its count, so a name of 32 bytes or more can lose a word from its hash, which is harmless because those names are compared in full. Cost: none in lines. Evidence: counts.sh 414.5 -> 393.1 instructions per record (-5.2%), though the simulated mispredicts move 1.54 -> 1.70, an estimated 3 cycles against about 12 saved. ab.sh 15 runs: -1.9%, inside 16% noise, so T is unproven locally, and the case rests on the box being bound by its clock. Outcome: kept; the board decides.

## What didn't work

| Experiment | What we expected | What we measured | Where | Kept? |
|---|---|---|---|---|
| Counting distinct names in a second phase, one partition per thread, with no shared writes per record | Nearly every record sets a new country bit in a shared cache line, and the box's cross-core traffic is slow, so T should fall a lot | T 0.055 -> 0.059 s, M 14.2 -> 37.4 MB, K 24.35 -> 19.08. Shared bits weren't the bottleneck | run-be17f3a6a2e0 | reverted |

## The final design

Not final yet: see [README](README.md#approach) for the current one.

## How we know it's correct

- Every build is checked by SHA-256 against the naive reference on 15 generator seeds (tiny and sample) and 5 seeds at full scale, plus 10 answer-preserving rewrites of the input and 4 limit cases.
- A constant-hash test build, where every key collides, gives the same answers on tiny.
- **Reproduce it yourself:** `python3 tools/verify.py` (Docker, git and Python only).

## With another 24 hours

To be written at freeze.
