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
| 7 | Scan a document in stages: token edges, a small hot table with prefetched misses, a bit per term held, truth-table BOOL checks | Local phases showed BOOL evaluation at 42% and lookup at 28% of the scan, with stamps, packed queries and hits all missing cache | pending | — | — | — |
| 8 | Plan without sorts, shared term counts, 5-slot checks, a 4-way hot table of short keys, ranges claimed in order | Single-thread plan 30 -> 20 ms; the commonest OR term of a query needs no check; threads finished unevenly; hot table compares took 40 instructions per token | pending | — | — | — |
| 9 | A set-based main table, one heap, a parallel sample, postings queries answered before any printing | The main table's probes mispredicted on every collision, page faults cost 15% of the serial phases, the sample ran on one thread, and bitmap queries made batches wait on each other | pending | — | — | — |
| 10 | Chain blocks in chunks that never move, short checks for short queries, the common-term line moved to N/24 | One heap put M at 52 MB, 8 over the cap: growing Vecs of blocks held twice their size, and bitmaps took 11 MB; three slots cover nine checks in ten | pending | — | — | — |
| 11 | One prefetched term table instead of two levels, no coin-toss branches in lookups or checks, BOOL checks listed and prefetched before they run | Hardware counters showed lookup at 35% of cycles and 43% of mispredicts: half the tokens went through the second level, and the compiler had turned a select into a branch | pending | — | — | — |
| 12 | Query terms prefetched before they're added, a word per term written to the table with no branch, shorter numbering | Serial phases are now the largest cheap target: 12 of 95 ms before the scan | pending | — | — | — |
| 13 | Vectors in lookup, tokens' edges from a byte table, three-way term sets, a byte per term, leaner BOOL checks and bitmap answers | Counters showed 200 instructions per token and per-core IPC of 2.4, so instruction count was the limit, not memory | pending | — | — | — |
| 14 | The same design in 960 lines: a plan that owns everything, one counting sort for all lists | S 1325 cost 5% of K against the leader's 782, and most of it was structure and formatting, not features | pending | — | — | — |
| 15 | exp: read the corpus in 128 KiB pieces, linked statically | On C, reading in pieces beat mmap on the box, though local timing couldn't tell | pending | — | — | — |
| 16 | exp: 32 KiB pieces in place of 128 KiB | 128 KiB gave T 0.1047, 0.1037 and 0.1033 against 0.1045, 0.1041 and 0.1045 with mmap, flat for 23 more lines | pending | — | — | — |
| 17 | Back to mapping the corpus, linked statically | Reading in pieces gained nothing on B: T 0.1033 to 0.1051 against 0.1041 to 0.1045 mapped, for 18 to 23 more lines | pending | — | — | — |
| 18 | Bitmap words listed on their own, rows transposed per 64 documents, a threaded query parse, cheaper second looks | Serial phases were 13% of T, and per-document bitmap writes and second looks still cost 5% of the instructions each | pending | — | — | — |

## Decision log

**D1. Start with threads.** Context: unlike A, C and D, the single-threaded version of this design has a different shape, since the postings need a merge. Options: one thread first, or threads straight away. Chose: threads, with equal byte ranges, because the merge then needs no sort. Cost: a slow thread slows the whole scan. Evidence: the sweep passes at 1, 2, 3, 7, 12 and 64 threads. Outcome: kept.

**D2. Anchor each phrase on its rarest term, not Aho-Corasick.** Context: phrases need matching against 36M tokens. Options: an automaton stepping on every token, or a check around each occurrence of one rare term. Chose: the anchor, which is simpler, and any term gives correct answers. A wrong guess from the sample only costs more checks. Cost: each document's tokens are buffered, which is small. Outcome: kept for now.

**D3. Chains of 2-byte offsets instead of a list and a final copy.** Context: v1 held each posting twice for a moment, 4 bytes in a thread's list and 4 in the final array. Options: compress the lists, or write postings straight to their final place. Chose: a chain of 32-byte blocks per term per thread, each with 14 offsets from a part's first document. Threads cover document order, so no merge or sort is needed. A part holds at most 65,535 documents, and running with few threads exercises several parts. Cost: reading a list now walks a chain, and S grew. Evidence: M 127.4 -> 99.3 MB, T 0.203 -> 0.194 s (local). Outcome: kept.

**D4. Reorder AND and NOT runs.** Context: answering took 55 of 195 ms, with every step reading its whole list, even with R already empty. AND and NOT both intersect, so a run of them between two ORs can go in any order, and the query's first term counts as an AND. Chose: the rarest ANDs first, by the sample's counts, then NOTs, skipping every step while R is empty. A wrong guess only costs time. Cost: 26 lines. Evidence: T 0.194 -> 0.175 s, M 99.3 -> 71.2 MB, K 17.73 -> 19.88 (local). Outcome: kept.

**D5. Move the bitmap threshold by measuring.** Context: we set the threshold from memory alone, at N/18. We then compared N/18, N/24, N/32, N/48 and N/64 on our scorer, and N/48 looked best. `ab.sh` over 9 runs gave -7.1% with 3.7% noise, just inside its twice-the-noise bar, so the board decided: T 0.297 -> 0.273 s, M 70.2 -> 72.9 MB, K 19.70 -> 20.40. Cost: one number. Outcome: kept.

**D6. Answer most BOOL queries in the scan.** Context: M was 73 MB, with the cap at 44 MB, and chains took most of it. Options: compress the chains, or not store them. Chose the second. Every document matching a BOOL query contains an anchor: the rarest AND of its last run, or else each OR term. So at an anchor's first occurrence in a document, each term's stamp (the last document holding it) gives the whole formula in six compares with no branch. Counts and sums accumulate per part of the corpus, and the ten smallest sit behind a lock per query. Evidence: on seed 1, 28,346 of 31,000 BOOL queries are answered this way, in 13.6M evaluations, and 71K terms store nothing. Local M 73 -> 41 MB, T flat, and S 832 -> 790, because the reordering and list modes went too. What would change our minds: a board T above 0.30 s, since this adds to the scan.

**D7. Take the branches out of the lookup.** Context: T rose to 0.33 s with step 5, because every anchor now evaluates formulas during the scan, and counts showed 318 mispredicts per document. Chose: `find` always returns a slot, where a miss lands on an empty slot whose term is one no query uses, and its compares are joined into one exit test. A one-multiply hash takes the top bits. Thread ranges are rounded to 64 documents, so no two threads share a bitmap word, and every token that needs no chain does one plain `load | bit | store`, with stamp-only terms hitting a spare word. Evidence: mispredicts 318 -> 198 per document, instructions +8%, and local T -5.9% (noise 7.1%). What would change our minds: no gain on the board, since this clock favours fewer instructions.

**D8. Replace the token-at-a-time scan. Context: one thread took 1120 ms to scan: BOOL 470, lookup 320, phrases 110. Chose stages per document with one branch each: edges from 64-byte masks, a 1024-slot table for hot terms whose misses are prefetched and resolved in a batch, a bit per term instead of a stamp, and a 64-bit truth table per query anchor so a check has no branch. Cost: S 799 -> about 950. Evidence: 12-thread local T 194 -> 140 ms, 43/43, 228/228, const-hash 38/38. What would change our minds: the board's T falling by less than S costs.

**D9. Keep cutting the serial plan and BOOL checks. Context: the first rewrite left BOOL at a third of the scan and the plan at 20% of T. Chose: query parsing through the token edges; lists placed by counting instead of sorted; the commonest trailing OR term of a query counted once per term with other anchors skipping its documents; checks with five slots since the anchor's own bit is known; 4-way hot sets of 8-byte keys; ranges claimed in order by each thread, with sums kept as ids instead of offsets, so no parts. Evidence: local T 139 -> 120 ms, grid and thread sweep pass. What would change our minds: board T not falling.

**D10. Remove what stays serial and what mispredicts. Context: at 12 threads, the plan and answer phases were 26 of 120 ms, and lookup misses 18% of the scan. Chose: a main table of 64-byte sets of four keys compared with one vector instruction, so a lookup has no branch; one heap with a 64 MB top pad and one arena, so huge pages replace thousands of small faults; the sample split between threads, shared counters; bitmap queries first, then lines in order. Evidence: local T 120 -> 105 ms under THP always, mispredicts 86 -> 59 per document, all grids pass. What would change our minds: M above the 44 MB cap, since one arena changes how memory is laid out.

**D11. Bring M back under the cap at the same T. Context: the one-heap build cost M 44 -> 52 MB. Measured in a fresh cgroup under THP always: block Vecs doubled to 13.7 MB for 7.3 MB of blocks, and the sample's temporary arrays left 7 MB of holes. Chose: chunks of 4096 blocks; a byte per term in the sample; bitmaps only at df above N/24, where local T was flat from N/12 to N/48 within noise and M fell 44-46 -> 42-43 MB; checks of three terms in 16 bytes for queries of up to four terms. What would change our minds: T rising on the board with the threshold.

**D12. Follow the counters. perf stat works in WSL now, and thread cycles repeat to about 1%, where wall time is noisy here. Chose: one table of four-way sets, every token's set prefetched, then keys compared; tokens over 8 bytes, or reaching a full set, get a second look. A coin-toss select the compiler kept as a branch is arithmetic now. BOOL checks: prefetch lists, list short checks, then run them in one loop. Evidence on seed 1, 12 threads: cycles 4.63G -> 3.91G, instructions 7.82G -> 7.50G, mispredicts 26.5M -> 12.4M; 43/43, 228/228. Removing any of short checks, shared counts, the limit, or commonest-first numbering costs 2.5 to 9% of cycles, so they stay despite S.

**D13. Keep cutting what runs on one thread. Chose: terms of the queries wait in a short list with their sets prefetched (parse 5.3 -> 3.7 ms); the table's words change in one pass with no branch on which ways are in use (1.45 -> 0.48 ms); term numbers come from one counting pass over classes. Evidence: local serial phases 12 -> 9 ms, hash unchanged, grids pass.

**D14. Cut instructions everywhere in the scan. Keys and hashes of four tokens at a time in AVX2, and the compare in vectors with table-driven lists (-10% of cycles); edges from a per-byte table instead of 24 takes (-6%); sets of three ways whose fourth word holds what the key tests used to (-7%); checks with the add inlined and no branch on their result (-5%); AVX2 nibble popcounts for bitmap answers (-3%); a byte per term instead of a bit for what a document holds, the one cache trade here, for the board to judge (-1%). On seed 1, 12 threads: cycles 3.97G -> 2.66G, instructions 7.58G -> 4.07G, mispredicts 12.4M -> 11.8M, M flat at 42 MB, S 1213 -> 1318. Grid 43/43, threads 228/228, const-hash 38/38, stress 0 wrong in 300. What would change our minds: the board's T falling by less than the cycles suggest.

**D15. Cut lines with no loss of speed. Plan now owns the queries, answers and bitmaps, which removes Index, Queries, Answers, Evaluator and AnchorLists and their struct literals; make_plan is one loop plus one `group` counting sort, also used for phrase hits, with number_terms and list_phrases split out. Chain state holds its first document (-2.5% of cycles), total_hits visits each answer once (-0.4 ms), and one round of threads evaluates and prints (-0.5 ms). Blocks stay in chunks, as a reserved Vec put M at 47 MB. On seed 1, 12 threads: cycles 2.62G -> 2.56G, instructions 4.07G -> 3.96G, S 1325 -> 964, M max 43.75 -> 42.62 MB. Grid 43/43, threads 228/228, const-hash 38/38, stress 0 wrong in 300.

**D16. Experiment. Locally: page faults 4091 -> 428, thread cycles 2.55G -> 2.44G, sys CPU 38 -> 60 ms, wall flat, S 964 -> 987. Hyper-V inflates both faults and syscalls here, so the board decides, against T 0.1045 at S 964. Static linking saves 0.25 ms from exec to main.

**D17. Experiment: the other piece size, before deciding. On A and C, 32 KiB is what paid. If this isn't clearly better either, B goes back to mmap, which has fewer lines.

**D18. Decided by the board. B's time is its user code, unlike A and C, where faults mattered. Mapped, S 964. The static link stays, as it saves 0.25 ms from exec to main.

**D19. The next 11%. A list for bitmap-term words with a fill to clear them (-177M instructions); a cheaper second look (-186M); rows per document, transposed per 64 documents, instead of a store per bit (-86M, -1.9M mispredicts); chains walked in turn with prefetch (cycles -2.3%, a cache change); threaded query parse and totals (about -1 ms); MADV_POPULATE_READ per range (user cycles -3%, wall flat, for the board to judge). On seed 1, 12 threads: cycles 2.53G -> 2.24G, instructions 3.96G -> 3.57G, mispredicts 11.3M -> 10.3M, local wall -12%, M max 41.8 MB, S 964 -> 1060. Grid 43/43, threads 228/228, const-hash 38/38, stress 0 wrong in 300.

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
