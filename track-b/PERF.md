# Track B: performance log

**Result:** the reference takes **56.35 s / 2,200 MB** on the board. Ours is in the last row of [the journey](#the-journey).

**Check it yourself:** the board run named there, or locally `python3 tools/verify.py --tracks b`. Output SHA-256 on seed 1: `f86973c210228141bcb6f1f1b10188ad2a9b65f71e7cd7cf4590b38de4e80927`.

## Where it loses

- M is 127 MB locally, about 3× the 44 MB where more savings stop counting. Each thread's list is held alongside the final lists while they're built, and postings are 4 bytes each.
- S is 700 code lines against the reference's 91, which costs about 6.6 speed points.
- Every token costs a byte-at-a-time hash and at least one table probe.

## Where the reference spends its time

About 62% is building the index, with a set for every one of 609,833 terms, and 37% is the queries, where every operation builds a new set.

## The journey

Every step cites the live-board run that measured it. Numbers marked *local* come from our scorer, on a faster PC, and only explain decisions.

| Step | Change | Why we expected it to help | Run ID | T (s) | M (MB) | Points |
|---|---|---|---|---|---|---|
| 0 | The reference, unchanged | — | — | 56.35 | 2,200 | 0 |
| 1 | v1: query terms only, bitmaps for common terms, phrases anchored on the rarest term, 12 threads | Most of the corpus's terms aren't asked about, and most tokens trigger no phrase check | run-… | 0.203 *local* | 127.4 *local* | 61.4 *local* |

## Decision log

**D1. Start with threads.** Context: unlike A, C and D, the single-threaded version of this design has a different shape, since the postings need a merge. Options: one thread first, or threads straight away. Chose: threads, with equal byte ranges, because the merge then needs no sort. Cost: a slow thread slows the whole scan. Evidence: the sweep passes at 1, 2, 3, 7, 12 and 64 threads. Outcome: kept.

**D2. Anchor each phrase on its rarest term, not Aho-Corasick.** Context: phrases need matching against 36M tokens. Options: an automaton stepping on every token, or a check around each occurrence of one rare term. Chose: the anchor, which is simpler, and any term gives correct answers. A wrong guess from the sample only costs more checks. Cost: each document's tokens are buffered, which is small. Outcome: kept for now.

## What didn't work

Nothing tried yet.

## The final design

Not final yet: see [README](README.md#approach) for the current one.

## How we know it's correct

- Every build is checked by SHA-256 against the naive reference on 15 generator seeds (tiny and sample) and 5 seeds at full scale, plus 8 answer-preserving rewrites of the input and 4 limit cases.
- A constant-hash test build, where every key collides, gives the same answers on tiny.
- **Reproduce it yourself:** `python3 tools/verify.py` (Docker, git and Python only).

## With another 24 hours

To be written at freeze.
