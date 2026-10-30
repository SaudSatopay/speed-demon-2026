# Track B: performance log

**Result:** the reference takes **56.35 s / 2,200 MB** on the board. Ours is in the last row of [the journey](#the-journey).

**Check it yourself:** the board run named there, or locally `python3 tools/verify.py --tracks b`. Output SHA-256 on seed 1: `f86973c210228141bcb6f1f1b10188ad2a9b65f71e7cd7cf4590b38de4e80927`.

## Where it loses

- M is about 71 MB locally, above the 44 MB where more savings stop counting.
- S is 832 code lines against the reference's 91, which costs about 7.2 speed points.
- Reading a posting list walks a chain, which is slower than reading an array.

## Where the reference spends its time

About 62% is building the index, with a set for every one of 609,833 terms, and 37% is the queries, where every operation builds a new set.

## The journey

Every step cites the live-board run that measured it. Numbers marked *local* come from our scorer, on a faster PC, and only explain decisions.

| Step | Change | Why we expected it to help | Run ID | T (s) | M (MB) | Points |
|---|---|---|---|---|---|---|
| 0 | The reference, unchanged | — | — | 56.35 | 2,200 | 0 |
| 1 | v1: query terms only, bitmaps for common terms, phrases anchored on the rarest term, 12 threads | Most of the corpus's terms aren't asked about, and most tokens trigger no phrase check | run-35592356e93c | 0.360 | 137.6 | 60.3 |
| 2 | Block scan, a term table with keys inline, and postings as chains of 2-byte offsets | The profile showed the scan at 75% of the time, and postings were held twice | run-… | 0.194 *local* | 99.3 *local* | 62.4 *local* |
| 3 | AND and NOT runs reordered, rarest first, and a skip while R is empty | Most queries have small answers, so most list reads were wasted | run-1408d1e22436 | 0.297 | 70.2 | 64.7 |
| 4 | Bitmaps for more terms, at df >= N/48 instead of N/18 | Setting a bit is cheaper than extending a chain, and memory barely moved | run-5908f2d87a23 | 0.273 | 72.9 | 65.5 |
| 5 | Answer BOOL queries during the scan, from term stamps, and store postings only for queries anchored on a common term | Chains held most of the memory, and nine queries in ten have a rare term in every match | run-d962e8c4f494 | 0.332 | 39.6 | 65.7 |
| 6 | A lookup with one exit branch, and plain bitmap stores in ranges of whole bitmap words | The scan took 2.2 mispredicts per token, and `find` was 60% of the instructions | run-7b57f5310a53 | 0.308 | 40.5 | 66.4 |

## Decision log

**D1. Start with threads.** Context: unlike A, C and D, the single-threaded version of this design has a different shape, since the postings need a merge. Options: one thread first, or threads straight away. Chose: threads, with equal byte ranges, because the merge then needs no sort. Cost: a slow thread slows the whole scan. Evidence: the sweep passes at 1, 2, 3, 7, 12 and 64 threads. Outcome: kept.

**D2. Anchor each phrase on its rarest term, not Aho-Corasick.** Context: phrases need matching against 36M tokens. Options: an automaton stepping on every token, or a check around each occurrence of one rare term. Chose: the anchor, which is simpler, and any term gives correct answers. A wrong guess from the sample only costs more checks. Cost: each document's tokens are buffered, which is small. Outcome: kept for now.

**D3. Chains of 2-byte offsets instead of a list and a final copy.** Context: v1 held each posting twice for a moment, 4 bytes in a thread's list and 4 in the final array. Options: compress the lists, or write postings straight to their final place. Chose: a chain of 32-byte blocks per term per thread, each with 14 offsets from a part's first document. Threads cover document order, so no merge or sort is needed. A part holds at most 65,535 documents, and running with few threads exercises several parts. Cost: reading a list now walks a chain, and S grew. Evidence: M 127.4 -> 99.3 MB, T 0.203 -> 0.194 s (local). Outcome: kept.

**D4. Reorder AND and NOT runs.** Context: answering took 55 of 195 ms, with every step reading its whole list, even with R already empty. AND and NOT both intersect, so a run of them between two ORs can go in any order, and the query's first term counts as an AND. Chose: the rarest ANDs first, by the sample's counts, then NOTs, skipping every step while R is empty. A wrong guess only costs time. Cost: 26 lines. Evidence: T 0.194 -> 0.175 s, M 99.3 -> 71.2 MB, K 17.73 -> 19.88 (local). Outcome: kept.

**D5. Move the bitmap threshold by measuring.** Context: we set the threshold from memory alone, at N/18. We then compared N/18, N/24, N/32, N/48 and N/64 on our scorer, and N/48 looked best. `ab.sh` over 9 runs gave -7.1% with 3.7% noise, just inside its twice-the-noise bar, so the board decided: T 0.297 -> 0.273 s, M 70.2 -> 72.9 MB, K 19.70 -> 20.40. Cost: one number. Outcome: kept.

**D6. Answer most BOOL queries in the scan.** Context: M was 73 MB, with the cap at 44 MB, and chains took most of it. Options: compress the chains, or not store them. Chose the second. Every document matching a BOOL query contains an anchor: the rarest AND of its last run, or else each OR term. So at an anchor's first occurrence in a document, each term's stamp (the last document holding it) gives the whole formula in six compares with no branch. Counts and sums accumulate per part of the corpus, and the ten smallest sit behind a lock per query. Evidence: on seed 1, 28,346 of 31,000 BOOL queries are answered this way, in 13.6M evaluations, and 71K terms store nothing. Local M 73 -> 41 MB, T flat, and S 832 -> 790, because the reordering and list modes went too. What would change our minds: a board T above 0.30 s, since this adds to the scan.

**D7. Take the branches out of the lookup.** Context: T rose to 0.33 s with step 5, because every anchor now evaluates formulas during the scan, and counts showed 318 mispredicts per document. Chose: `find` always returns a slot, where a miss lands on an empty slot whose term is one no query uses, and its compares are joined into one exit test. A one-multiply hash takes the top bits. Thread ranges are rounded to 64 documents, so no two threads share a bitmap word, and every token that needs no chain does one plain `load | bit | store`, with stamp-only terms hitting a spare word. Evidence: mispredicts 318 -> 198 per document, instructions +8%, and local T -5.9% (noise 7.1%). What would change our minds: no gain on the board, since this clock favours fewer instructions.

## What didn't work

| Experiment | What we expected | What we measured | Where | Kept? |
|---|---|---|---|---|
| Chain blocks growing from 6 to 192 postings, to chase fewer pointers and waste less on rare terms | Lower M and T | M unchanged, T about 1% better, S +16. Chains are full enough already | local | reverted |
| Tokens in batches of 48, with prefetching of table slots, list states and bitmaps | Up to 30% off the scan, which the profile showed was mostly cache misses | T 0.175 -> 0.169 s, S 832 -> 878, K +0.7%. Not worth the code | local | reverted |
| Disabling transparent huge pages, to stop per-thread arenas rounding up to 2 MB | M down by about 39 MB, the part we couldn't account for | M 105.8 -> 97.2 MB, T 0.224 -> 0.190 s on B, but A flat and C and D about 15% slower | local | not kept |

## The final design

Not final yet: see [README](README.md#approach) for the current one.

## How we know it's correct

- Every build is checked by SHA-256 against the naive reference on 15 generator seeds (tiny and sample) and 5 seeds at full scale, plus 8 answer-preserving rewrites of the input and 4 limit cases.
- A constant-hash test build, where every key collides, gives the same answers on tiny.
- **Reproduce it yourself:** `python3 tools/verify.py` (Docker, git and Python only).

## With another 24 hours

To be written at freeze.
