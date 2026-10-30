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
| 3 | AND and NOT runs reordered, rarest first, and a skip while R is empty | Most queries have small answers, so most list reads were wasted | run-… | 0.175 *local* | 71.2 *local* | 64.9 *local* |

## Decision log

**D1. Start with threads.** Context: unlike A, C and D, the single-threaded version of this design has a different shape, since the postings need a merge. Options: one thread first, or threads straight away. Chose: threads, with equal byte ranges, because the merge then needs no sort. Cost: a slow thread slows the whole scan. Evidence: the sweep passes at 1, 2, 3, 7, 12 and 64 threads. Outcome: kept.

**D2. Anchor each phrase on its rarest term, not Aho-Corasick.** Context: phrases need matching against 36M tokens. Options: an automaton stepping on every token, or a check around each occurrence of one rare term. Chose: the anchor, which is simpler, and any term gives correct answers. A wrong guess from the sample only costs more checks. Cost: each document's tokens are buffered, which is small. Outcome: kept for now.

**D3. Chains of 2-byte offsets instead of a list and a final copy.** Context: v1 held each posting twice for a moment, 4 bytes in a thread's list and 4 in the final array. Options: compress the lists, or write postings straight to their final place. Chose: a chain of 32-byte blocks per term per thread, each with 14 offsets from a part's first document. Threads cover document order, so no merge or sort is needed. A part holds at most 65,535 documents, and running with few threads exercises several parts. Cost: reading a list now walks a chain, and S grew. Evidence: M 127.4 -> 99.3 MB, T 0.203 -> 0.194 s (local). Outcome: kept.

**D4. Reorder AND and NOT runs.** Context: answering took 55 of 195 ms, with every step reading its whole list, even with R already empty. AND and NOT both intersect, so a run of them between two ORs can go in any order, and the query's first term counts as an AND. Chose: the rarest ANDs first, by the sample's counts, then NOTs, skipping every step while R is empty. A wrong guess only costs time. Cost: 26 lines. Evidence: T 0.194 -> 0.175 s, M 99.3 -> 71.2 MB, K 17.73 -> 19.88 (local). Outcome: kept.

## What didn't work

| Experiment | What we expected | What we measured | Where | Kept? |
|---|---|---|---|---|
| Disabling transparent huge pages, to stop per-thread arenas rounding up to 2 MB | M down by about 39 MB, the part we couldn't account for | M 105.8 -> 97.2 MB, T 0.224 -> 0.190 s on B, but A flat and C and D about 15% slower | local | not kept |

## The final design

Not final yet: see [README](README.md#approach) for the current one.

## How we know it's correct

- Every build is checked by SHA-256 against the naive reference on 15 generator seeds (tiny and sample) and 5 seeds at full scale, plus 8 answer-preserving rewrites of the input and 4 limit cases.
- A constant-hash test build, where every key collides, gives the same answers on tiny.
- **Reproduce it yourself:** `python3 tools/verify.py` (Docker, git and Python only).

## With another 24 hours

To be written at freeze.
