# Track B: performance log

**Result:** the reference takes **56.35 s / 2,200 MB** on the board, and our best run is **0.0983 s / 41.6 MB** (`run-f39141d33527`). That is K = **36.97** and **78.4 speed points**. The same code's T ranges from 0.097 to 0.101 between runs.

**Check it yourself:** the board run named there, or locally `python3 tools/verify.py --tracks b`. Output SHA-256 on seed 1: `f86973c210228141bcb6f1f1b10188ad2a9b65f71e7cd7cf4590b38de4e80927`.

## Where it loses

- S is 1,015 code lines against the reference's 91, which costs (91/1015)^0.15, K x 0.70, about 7.9 speed points. It's the most code of any of our tracks.
- The scan scales 4.6x on six local cores, and 6.55x on 12 threads, and we haven't found what's shared that limits it.
- Reading a posting list walks a chain, which is slower than reading an array.
- Printing keeps an order: threads claim batches, and each waits for the batch before it to reach the file. On an idle box that's flat against printing at known offsets, but a busy box stalls it (D26).

**What each phase costs on the box.** We can't run a profiler there, so a labelled `exp:` push ran one phase twice, on throwaway state, and T minus the usual T is that phase's wall time, against T of about 0.099:

| Run twice | Run | T (s) | Phase's wall time |
|---|---|---|---|
| The scan | `run-804bf7c7f1a1` | 0.1761 | about 77 ms |
| Parsing the queries and planning | `run-526859a8c142` | 0.1206 | about 22 ms, 7 to 13 locally |
| Answering and printing | `run-7baf964cdb45` | 0.1136 | about 15 ms, 6.7 locally |

These runs timed the build before one round of threads, with T of about 0.099 as the baseline, so the rows are 77, 22 and 15 ms. They sum to 114 ms, more than T, so they overstate: a phase run a second time finds the caches full of other data.

## Where the reference spends its time

About 62% is building the index, with a set for every one of 609,833 terms, and 37% is the queries, where every operation builds a new set.

## The journey

Every step cites the live-board run that measured it. Numbers marked *local* come from our scorer, on a faster PC, and only explain decisions.

| Step | Change | Why we expected it to help | Run ID | T (s) | M (MB) | Points |
|---|---|---|---|---|---|---|
| 0 | The reference, unchanged | — | — | 56.35 | 2,200 | 0 |
| 1 | v1: query terms only, bitmaps for common terms, phrases anchored on the rarest term, 12 threads | Most of the corpus's terms aren't asked about, and most tokens trigger no phrase check | run-35592356e93c | 0.360 | 137.6 | 60.3 |
| 2 | Block scan, a term table with keys inline, and postings as chains of 2-byte offsets | The profile showed the scan at 75% of the time, and postings were held twice | run-2d498d33b5d9 (with `1bc82e4`) | 0.3003 | 69.4 | 64.6 |
| 3 | AND and NOT runs reordered, rarest first, and a skip while R is empty | Most queries have small answers, so most list reads were wasted | run-1408d1e22436 | 0.297 | 70.2 | 64.7 |
| 4 | Bitmaps for more terms, at df >= N/48 instead of N/18 | Setting a bit is cheaper than extending a chain, and memory barely moved | run-5908f2d87a23 | 0.273 | 72.9 | 65.5 |
| 5 | Answer BOOL queries during the scan, from term stamps, and store postings only for queries anchored on a common term | Chains held most of the memory, and nine queries in ten have a rare term in every match | run-d962e8c4f494 | 0.332 | 39.6 | 65.7 |
| 6 | A lookup with one exit branch, and plain bitmap stores in ranges of whole bitmap words | The scan took 2.2 mispredicts per token, and `find` was 60% of the instructions | run-7b57f5310a53 | 0.308 | 40.5 | 66.4 |
| 7 | Scan a document in stages: token edges, a small hot table with prefetched misses, a bit per term held, truth-table BOOL checks | Local phases showed BOOL evaluation at 42% and lookup at 28% of the scan, with stamps, packed queries and hits all missing cache | run-90115f4bef37 | 0.2107 | 41.0 | 70.0 |
| 8 | Plan without sorts, shared term counts, 5-slot checks, a 4-way hot table of short keys, ranges claimed in order | Single-thread plan 30 -> 20 ms; the commonest OR term of a query needs no check; threads finished unevenly; hot table compares took 40 instructions per token | run-b1ba6e3dc099 | 0.1935 | 42.8 | 70.8 |
| 9 | A set-based main table, one heap, a parallel sample, postings queries answered before any printing | The main table's probes mispredicted on every collision, page faults cost 15% of the serial phases, the sample ran on one thread, and bitmap queries made batches wait on each other | run-49487d623c21 (with `34ac4a4`) | 0.1795 | 42.7 | 71.1 |
| 10 | Chain blocks in chunks that never move, short checks for short queries, the common-term line moved to N/24 | One heap put M at 52 MB, 8 over the cap: growing Vecs of blocks held twice their size, and bitmaps took 11 MB; three slots cover nine checks in ten | run-49487d623c21 | 0.1795 | 42.7 | 71.1 |
| 11 | One prefetched term table instead of two levels, no coin-toss branches in lookups or checks, BOOL checks listed and prefetched before they run | Hardware counters showed lookup at 35% of cycles and 43% of mispredicts: half the tokens went through the second level, and the compiler had turned a select into a branch | run-45be71f36a92 | 0.1496 | 41.7 | 72.9 |
| 12 | Query terms prefetched before they're added, a word per term written to the table with no branch, shorter numbering | Serial phases are now the largest cheap target: 12 of 95 ms before the scan | run-29a7940da073 | 0.1455 | 40.7 | 73.3 |
| 13 | Vectors in lookup, tokens' edges from a byte table, three-way term sets, a byte per term, leaner BOOL checks and bitmap answers | Counters showed 200 instructions per token and per-core IPC of 2.4, so instruction count was the limit, not memory | run-f0caefefff87 (with `c6d9ab9`) | 0.1124 | 42.9 | 75.8 |
| 14 | The same design in 960 lines: a plan that owns everything, one counting sort for all lists | S 1325 cost 5% of K against the leader's 782, and most of it was structure and formatting, not features | run-23608e2a9830 | 0.1045 | 42.3 | 77.6 |
| 15 | exp: read the corpus in 128 KiB pieces, linked statically | On C, reading in pieces beat mmap on the box, though local timing couldn't tell | run-11a9f3af506f (with `4aa4535`) | 0.1047 | 42.0 | 77.5 |
| 16 | exp: 32 KiB pieces in place of 128 KiB | 128 KiB gave T 0.1047, 0.1037 and 0.1033 against 0.1045, 0.1041 and 0.1045 with mmap, flat for 23 more lines | run-5c55e7c459ea | 0.1051 | 42.1 | 77.5 |
| 17 | Back to mapping the corpus, linked statically | Reading in pieces gained nothing on B: T 0.1033 to 0.1051 against 0.1041 to 0.1045 mapped, for 18 to 23 more lines | run-d559f7c2e83c (with `c1fdf29`) | 0.1038 | 42.7 | 77.7 |
| 18 | Bitmap words listed on their own, rows transposed per 64 documents, a threaded query parse, cheaper second looks | Serial phases were 13% of T, and per-document bitmap writes and second looks still cost 5% of the instructions each | run-5dcbf70e187b | 0.0994 | 40.9 | 77.8 |
| 19 | The same design in 130 fewer lines | S 1060 cost 3.8% of K against Cause Of Death's 818, whose T of 0.108 is close behind ours | run-fbc2411d398a | 0.1007 | 42.7 | 78.1 |
| 20 | exp: the scan twice | Where B's time goes on the box | run-804bf7c7f1a1 | 0.1761 | 55.3 | 71.0 |
| 21 | exp: plain faults, no populate | Where B's time goes on the box | run-fa98f771cce1 | 0.1008 | 41.6 | 78.1 |
| 22 | exp: parse and plan twice | Where B's time goes on the box | run-526859a8c142 | 0.1206 | 55.4 | 75.1 |
| 23 | exp: the answer phase twice | Where B's time goes on the box | run-7baf964cdb45 | 0.1136 | 44.1 | 76.8 |
| 24 | Back to the build before the measurements | The measurements of B's phases are done | run-b1e785532ecd | 0.1036 | 41.2 | 77.8 |
| 25 | Answers printed at known offsets with no ordered handoffs, chained OR and NOT applied directly, ranges that shrink toward the end | Parse, plan and answers are likely 25 to 30 ms of T on the box, about twice their local time, and the print phase's 196 ordered write-and-yield steps stall on anything else running | run-3e06bf691391 | 0.1028 | 41.5 | 77.8 |
| 26 | Back to ordered printing, without the page mapping ahead | Known offsets, direct OR and NOT, and shrinking ranges read T 0.1028 and 0.1008 at S 954, against a median of 0.1006 at S 928 | run-f39141d33527 | 0.0983 | 41.6 | 78.4 |
| 27 | Terms found without their set's lock, one round of threads for the scan and the answers, larger print batches | Serial costs on the box are about twice local, and another ello reached T 0.077 at S 949, so there's more to find than instructions per token | run-b5173908b9b3 | 0.0988 | 42.5 | 78.3 |
| 28 | exp: each thread gives back a range's pages when it has scanned it | The kernel's unmap at exit is serial, about 62K pages' mapping counts and references, cold by then, and T counts up to exit; on A, mapping lost to reading by 3.3 ms though reading copies every byte | run-cddb558fe995 | 0.1030 | 40.8 | 77.8 |
| 29 | Fifty-seven fewer lines outside the scan, `main` as the C entry point, and each range's pages left alone | Cause Of Death passed us on B at T 0.100 and S 823, so S is the cheap gain here: 100 lines is K +1.5% | run-d2958120f77f | 0.1100 | 41.9 | 77.3 |
| 30 | The same machine code with named structs instead of tuples, named constants, and every comment checked against the code | A reader had to remember what `.0` and `.1` held across a dozen record types, and some comments no longer told the truth | pending | — | — | — |
| 31 | 48 ranges per thread instead of 16, and tuning meant for older CPUs lifted | With 16 ranges a thread's last one takes about 5 ms of the box's time, so threads finish that far apart and wait; C gained 2 to 4% from the same change. For the generic target LLVM also clears the output of TZCNT and POPCNT first, against a false dependency the box's cores don't have | pending | — | — | — |
| 32 | Queries with common anchors answered in the scan, 256 documents at a time, in vectors, with no bitmaps of the corpus and no chains | Bitmaps and chains held 18 of 42 MB, and were only read after the scan. A pass over a query's terms costs the same in vectors over a block's columns, which a thread reuses, and M is K^0.20 | pending | — | — | — |
| 33 | exp: block queries at anchors holding N/48 documents, not N/96 | A thread's columns take 216 KB of L2 at N/96, read and cleared each block, and the box's L2 is 1.25 MB per core for two threads. Local cycles rank instruction cuts, not this | pending | — | — | — |
| 34 | exp: block queries at anchors holding N/24 documents, not N/48 | N/48 read T 0.0909 against 0.0996 at N/96, though local cycles rose 1.5%, so what a block's queries cost on the box isn't what local cycles say | pending | — | — | — |
| 35 | Block queries at anchors holding N/48 documents | Board: N/96 T 0.0996, N/48 T 0.0909, N/24 T 0.0942, one sample each, and local cycles ranked them the other way round: 2.10G, 2.13G, 2.16G | pending | — | — | — |
| 36 | Marks as stamps with no clearing per document, a one-multiply hash, ids from a counter, per-thread sample counts, a fourth way for short terms, phrases compared in one vector, the second pass 8 words at a time, and the corpus prefetched 16 blocks ahead | On the box T is thread cycles over 12 threads at 2.5 GHz, and about 35 mispredicts per document came mostly from each stage's loop exit, so whole stages could go | pending | — | — | — |
| 37 | Parked threads map the corpus ahead with MADV_POPULATE_READ, a common sure term's tally runs by blocks, columns numbered by first use and aligned, short checks packed in 8 bytes, a phrase anchor's neighbour stored for a first test, answers on their own cache lines, no barrier in the answer round, and 78 fewer lines | The board showed B more sensitive to L2 than local cycles say, so this round goes after bytes a thread drags through L2 per document and per block: block queries 2767 to 1913, columns 5067 to 3013, columns read per block 158 to 94 KB | pending | — | — | — |

## Decision log

**D1. Start with threads.** Context: unlike A, C and D, the single-threaded version of this design has a different shape, since the postings need a merge. Options: one thread first, or threads straight away. Chose: threads, with equal byte ranges, because the merge then needs no sort. Cost: a slow thread slows the whole scan. Evidence: the sweep passes at 1, 2, 3, 7, 12 and 64 threads. Outcome: kept.

**D2. Anchor each phrase on its rarest term, not Aho-Corasick.** Context: phrases need matching against 36M tokens. Options: an automaton stepping on every token, or a check around each occurrence of one rare term. Chose: the anchor, which is simpler, and any term gives correct answers. A wrong guess from the sample only costs more checks. Cost: each document's tokens are buffered, which is small. Outcome: kept for now.

**D3. Chains of 2-byte offsets instead of a list and a final copy.** Context: v1 held each posting twice for a moment, 4 bytes in a thread's list and 4 in the final array. Options: compress the lists, or write postings straight to their final place. Chose: a chain of 32-byte blocks per term per thread, each with 14 offsets from a part's first document. Threads cover document order, so no merge or sort is needed. A part holds at most 65,535 documents, and running with few threads exercises several parts. Cost: reading a list now walks a chain, and S grew. Evidence: M 127.4 -> 99.3 MB, T 0.203 -> 0.194 s (local). Outcome: kept.

**D4. Reorder AND and NOT runs.** Context: answering took 55 of 195 ms, with every step reading its whole list, even with R already empty. AND and NOT both intersect, so a run of them between two ORs can go in any order, and the query's first term counts as an AND. Chose: the rarest ANDs first, by the sample's counts, then NOTs, skipping every step while R is empty. A wrong guess only costs time. Cost: 26 lines. Evidence: T 0.194 -> 0.175 s, M 99.3 -> 71.2 MB, K 17.73 -> 19.88 (local). Outcome: kept.

**D5. Move the bitmap threshold by measuring.** Context: we set the threshold from memory alone, at N/18. We then compared N/18, N/24, N/32, N/48 and N/64 on our scorer, and N/48 looked best. `ab.sh` over 9 runs gave -7.1% with 3.7% noise, just inside its twice-the-noise bar, so the board decided: T 0.297 -> 0.273 s, M 70.2 -> 72.9 MB, K 19.70 -> 20.40. Cost: one number. Outcome: kept.

**D6. Answer most BOOL queries in the scan.** Context: M was 73 MB, with the cap at 44 MB, and chains took most of it. Options: compress the chains, or not store them. Chose the second. Every document matching a BOOL query contains an anchor: the rarest AND of its last run, or else each OR term. So at an anchor's first occurrence in a document, each term's stamp (the last document holding it) gives the whole formula in six compares with no branch. Counts and sums accumulate per part of the corpus, and the ten smallest sit behind a lock per query. Evidence: on seed 1, 28,346 of 31,000 BOOL queries are answered this way, in 13.6M evaluations, and 71K terms store nothing. Local M 73 -> 41 MB, T flat, and S 832 -> 790, because the reordering and list modes went too. What would change our minds: a board T above 0.30 s, since this adds to the scan.

**D7. Take the branches out of the lookup.** Context: T rose to 0.33 s with step 5, because every anchor now evaluates formulas during the scan, and counts showed 318 mispredicts per document. Chose: `find` always returns a slot, where a miss lands on an empty slot whose term is one no query uses, and its compares are joined into one exit test. A one-multiply hash takes the top bits. Thread ranges are rounded to 64 documents, so no two threads share a bitmap word, and every token that needs no chain does one plain `load | bit | store`, with stamp-only terms hitting a spare word. Evidence: mispredicts 318 -> 198 per document, instructions +8%, and local T -5.9% (noise 7.1%). What would change our minds: no gain on the board, since this clock favours fewer instructions.

**D8.** Replace the token-at-a-time scan. Context: one thread took 1120 ms to scan: BOOL 470, lookup 320, phrases 110. Chose stages per document with one branch each: edges from 64-byte masks, a 1024-slot table for hot terms whose misses are prefetched and resolved in a batch, a bit per term instead of a stamp, and a 64-bit truth table per query anchor so a check has no branch. Cost: S 799 -> about 950. Evidence: 12-thread local T 194 -> 140 ms, 43/43, 228/228, const-hash 38/38. What would change our minds: the board's T falling by less than S costs.

**D9.** Keep cutting the serial plan and BOOL checks. Context: the first rewrite left BOOL at a third of the scan and the plan at 20% of T. Chose: query parsing through the token edges; lists placed by counting instead of sorted; the commonest trailing OR term of a query counted once per term with other anchors skipping its documents; checks with five slots since the anchor's own bit is known; 4-way hot sets of 8-byte keys; ranges claimed in order by each thread, with sums kept as ids instead of offsets, so no parts. Evidence: local T 139 -> 120 ms, grid and thread sweep pass. What would change our minds: board T not falling.

**D10.** Remove what stays serial and what mispredicts. Context: at 12 threads, the plan and answer phases were 26 of 120 ms, and lookup misses 18% of the scan. Chose: a main table of 64-byte sets of four keys compared with one vector instruction, so a lookup has no branch; one heap with a 64 MB top pad and one arena, so huge pages replace thousands of small faults; the sample split between threads, shared counters; bitmap queries first, then lines in order. Evidence: local T 120 -> 105 ms under THP always, mispredicts 86 -> 59 per document, all grids pass. What would change our minds: M above the 44 MB cap, since one arena changes how memory is laid out.

**D11.** Bring M back under the cap at the same T. Context: the one-heap build cost M 44 -> 52 MB. Measured in a fresh cgroup under THP always: block Vecs doubled to 13.7 MB for 7.3 MB of blocks, and the sample's temporary arrays left 7 MB of holes. Chose: chunks of 4096 blocks; a byte per term in the sample; bitmaps only at df above N/24, where local T was flat from N/12 to N/48 within noise and M fell 44-46 -> 42-43 MB; checks of three terms in 16 bytes for queries of up to four terms. What would change our minds: T rising on the board with the threshold.

**D12.** Follow the counters. perf stat works in WSL now, and thread cycles repeat to about 1%, where wall time is noisy here. Chose: one table of four-way sets, every token's set prefetched, then keys compared; tokens over 8 bytes, or reaching a full set, get a second look. A coin-toss select the compiler kept as a branch is arithmetic now. BOOL checks: prefetch lists, list short checks, then run them in one loop. Evidence on seed 1, 12 threads: cycles 4.63G -> 3.91G, instructions 7.82G -> 7.50G, mispredicts 26.5M -> 12.4M; 43/43, 228/228. Removing any of short checks, shared counts, the limit, or commonest-first numbering costs 2.5 to 9% of cycles, so they stay despite S.

**D13.** Keep cutting what runs on one thread. Chose: terms of the queries wait in a short list with their sets prefetched (parse 5.3 -> 3.7 ms); the table's words change in one pass with no branch on which ways are in use (1.45 -> 0.48 ms); term numbers come from one counting pass over classes. Evidence: local serial phases 12 -> 9 ms, hash unchanged, grids pass.

**D14.** Cut instructions everywhere in the scan. Keys and hashes of four tokens at a time in AVX2, and the compare in vectors with table-driven lists (-10% of cycles); edges from a per-byte table instead of 24 takes (-6%); sets of three ways whose fourth word holds what the key tests used to (-7%); checks with the add inlined and no branch on their result (-5%); AVX2 nibble popcounts for bitmap answers (-3%); a byte per term instead of a bit for what a document holds, the one cache trade here, for the board to judge (-1%). On seed 1, 12 threads: cycles 3.97G -> 2.66G, instructions 7.58G -> 4.07G, mispredicts 12.4M -> 11.8M, M flat at 42 MB, S 1213 -> 1318. Grid 43/43, threads 228/228, const-hash 38/38, stress 0 wrong in 300. What would change our minds: the board's T falling by less than the cycles suggest.

**D15.** Cut lines with no loss of speed. Plan now owns the queries, answers and bitmaps, which removes Index, Queries, Answers, Evaluator and AnchorLists and their struct literals; make_plan is one loop plus one `group` counting sort, also used for phrase hits, with number_terms and list_phrases split out. Chain state holds its first document (-2.5% of cycles), total_hits visits each answer once (-0.4 ms), and one round of threads evaluates and prints (-0.5 ms). Blocks stay in chunks, as a reserved Vec put M at 47 MB. On seed 1, 12 threads: cycles 2.62G -> 2.56G, instructions 4.07G -> 3.96G, S 1325 -> 964, M max 43.75 -> 42.62 MB. Grid 43/43, threads 228/228, const-hash 38/38, stress 0 wrong in 300.

**D16.** Experiment. Locally: page faults 4091 -> 428, thread cycles 2.55G -> 2.44G, sys CPU 38 -> 60 ms, wall flat, S 964 -> 987. Hyper-V inflates both faults and syscalls here, so the board decides, against T 0.1045 at S 964. Static linking saves 0.25 ms from exec to main.

**D17.** Experiment: the other piece size, before deciding. On A and C, 32 KiB is what paid. If this isn't clearly better either, B goes back to mmap, which has fewer lines.

**D18.** Decided by the board. B's time is its user code, unlike A and C, where faults mattered. Mapped, S 964. The static link stays, as it saves 0.25 ms from exec to main.

**D19.** The next 11%. A list for bitmap-term words with a fill to clear them (-177M instructions); a cheaper second look (-186M); rows per document, transposed per 64 documents, instead of a store per bit (-86M, -1.9M mispredicts); chains walked in turn with prefetch (cycles -2.3%, a cache change); threaded query parse and totals (about -1 ms); MADV_POPULATE_READ per range (user cycles -3%, wall flat, for the board to judge). On seed 1, 12 threads: cycles 2.53G -> 2.24G, instructions 3.96G -> 3.57G, mispredicts 11.3M -> 10.3M, local wall -12%, M max 41.8 MB, S 964 -> 1060. Grid 43/43, threads 228/228, const-hash 38/38, stress 0 wrong in 300.

**D20.** Cut lines with no loss of speed. Each of the recent gains was measured again and still pays: threaded parse 2.5 ms against 6.05 serial, chains walked in turn 2.2% of cycles, the bitmap word list about 2%, transposed rows about 1.4%, MADV_POPULATE_READ faults 400 against 4215. Phrases now count under their answer's lock, and the sample counts occurrences, not documents; neither changes an answer. Over 31 interleaved runs: cycles +0.6%, instructions -0.1%, mispredicts -5.7%, M max 43.1 MB against 41.9, S 1060 -> 928. Grid 43/43, threads 228/228, const-hash 38/38, stress 0 wrong in 300.

**D21.** Measurement only: the scan run twice. T 0.1761, so the scan takes about 77 ms on the box.

**D22.** Measurement only: plain page faults, no MADV_POPULATE_READ. T 0.1008, flat, for 3 fewer lines, so it goes (D27).

**D23.** Measurement only: parsing the queries and planning twice. T 0.1206, so they take about 22 ms on the box, against 7 to 13 locally.

**D24.** Measurement only: answering and printing twice. T 0.1136, so they take about 15 ms on the box, against 6.7 locally.

**D25.** Revert the experiments of D21 to D24: the measurements are done.

**D26.** Cut the serial side. Printing takes two passes: batch lengths first, then each thread pwrites its batches at known offsets, so no thread waits for another: under load the answer phase stayed at 5 to 6 ms against 31 to 546. Chained OR and NOT steps go straight to the set, which skips two 31 KB passes for 3,283 of 5,528 posting steps. Scan ranges shrink toward the end, so the last thread finishes sooner. MADV_POPULATE_READ goes, as the board showed it flat. Local: flat within noise, as this PC hides these costs; expected on the box. M 42.2 to 42.9 MB, S 928 -> 947. Grid 43/43, threads 228/228, const-hash 38/38, stress 0 wrong in 300.

**D27.** Decided by the board: they don't pay for 26 lines on an idle box, though they did hold up better under load. MADV_POPULATE_READ stays out, as plain faults read T 0.1008, flat, for 3 fewer lines.

**D28.** For the board, as these are latency gains, not instruction cuts. Most query tokens are terms already in the table, and `intern` now finds those with no lock, since a way's key goes in last, with Release. `pack` makes its masks without a division. Threads stay on after the scan instead of a new round being started, and 1024 queries a batch means 49 ordered writes instead of 196. Locally, 12 threads, medians of 7: parse 3.3 -> 2.8 ms, plan 3.6 -> 3.3, answers 6.0 -> 4.5; cycles 2.34G -> 2.33G, instructions +0.3%, M 40.9 to 41.5 MB, S 925 -> 927. Tried and dropped: per-thread counts in the sample, an inlined compare of long keys (+1% instructions). Grid 43/43, threads 228/228, const-hash 38/38, stress 0 wrong in 300.

**D29.** Experiment, against T 0.098 to 0.102 at S 934. Each of 192 calls stops the other threads briefly for its TLB flush, which costs more here in a VM than on the box, so local timing can't judge this.

**D30.** Cut lines with no loss of speed, in the plan and answer code; the scan isn't touched. Over alternating runs: cycles 2.359G flat, instructions +0.06%, M max 42.74 MB. `#![no_main]` skips std's setup before main, as on A. Giving back each range's pages after its scan read T 0.1030 against a median of 0.1013, so the serial unmap at exit isn't what limits B, and that goes. Also measured on B and dropped, locally: lookups in windows of tokens (flat), PGO (flat), an L1 table of hot terms (cycles +10 to 14%, as hot lines were already cached), new layouts for check_bools (cycles +2 to 3.5%), marks as bits (cycles +3%). Grid 43/43, threads 228/228, const-hash 38/38, stress 0 wrong in 300.

**D31.** A trade: S 880 -> 1015, K x 0.979, for code someone else could change. Answer, Query, AnchorList, Tally, Check, Block, Chain, DocLists, Progress and Shared have named fields, with `repr(C)` where rustc would move a field. Layouts have the same size and offsets, by `offset_of!`, but for Query, which the scan doesn't read. Over 8 alternating rounds: instructions +0.03%, cycles +0.3%, mark and check_bools have identical machine code. M 40.9 to 43.07 MB, under the cap. Grid 43/43, threads 228/228, const-hash 38/38 and 114/114, stress 0 wrong in 300. `make_room` stays out of line, as inlining it into look_up costs +0.9% instructions. The README and this log were read against the code too, and 24 claims were corrected.

**D32.** No lines. Local paired wall -1.0 ms of 57, and wall's spread fell from about 7% to 2%, with cycles flat. Instructions: 3.572G -> 3.539G from the tuning, then 3.563G with more ranges. Hits stay ascending within a thread, as ranges are claimed in order.

**D33.** Local, seed 1, 12 threads: memory peak 42 to 27 MB, cycles 2.35G to 2.11G, instructions 3.56G to 3.34G, S 1015 to 1004. Each block query is now cheap beside checks at its anchors' documents, so a query goes by blocks at anchors holding N/96 documents in all, where N/24 of any one was the line, and rows start at N/48. Ranges need no rounding, since threads share no bitmap words, which also ends tokenizing up to 63 documents again at each range's start. Grid, threads and const-hash by the gate, stress 0 wrong in 300.

**D34.** Measurement of what columns cost on the box. N/48 has 28% fewer columns, for +1.5% local cycles. Against T 0.0996.

**D35.** Measurement. N/24 has fewer queries and columns again. Against T 0.0909.

**D36.** Decided by the board, and a second sample of N/48. Fewer block queries cost less on the box than local cycles say, as a block's columns and tallies go through a cache shared by two threads at 2.5 GHz, and N/24 leaves more checks. No new lines.

**D37.** Local, seed 1, 12 threads, on the N/96 build: cycles 2.109G to 1.763G (seed 2 2.084G to 1.759G), instructions 3.337G to 2.885G, mispredicts 8.63M to 4.85M, wall 47.4 to 41.0 ms, S +45. Grid 43/43, threads 228/228, const-hash 38/38, stress 0 wrong in 300. Dropped, no gain: batched tallies, SWAR print, LF block apart, block-sized ranges, phrase pre-prefetch, 1 and 4 MiB samples. Worse: table of 1<<17 sets +4%, 1<<15 +6%, NTA on sets +2%, sets prefetched a document ahead +3%. The corpus prefetch is the one cache change here, for the board to judge.

**D38.** Local, seed 1, 12 threads: cycles 1.84G to 1.69G, instructions 2.917G to 2.869G, cache misses 44.5M to 35.8M, wall -2.4 to -5%, S 1056 to 1032, M about 27 MB. Grid 43/43, threads 228/228, const-hash 38/38, stress 0 wrong in 300. The "too many ids" assert goes; the README's limits give the corpus size that the 40-bit sums hold. Tried and dropped: one thread round (+1 ms), the plan on threads, heap prefault, mapped output, skipping empty sets (12% are empty, +1.5%), selective clears.

## What didn't work

This PC's wall time is noisy, so thread cycles from `perf stat`, which repeat to about 1%, ranked instruction cuts. Anything to do with the kernel or with cache layout went to the board.

| Experiment | What we expected | What we measured | Where | Kept? |
|---|---|---|---|---|
| Chain blocks growing from 6 to 192 postings, to chase fewer pointers and waste less on rare terms | Lower M and T | M unchanged, T about 1% better, S +16. Chains are full enough already | local | reverted |
| Tokens in batches of 48, with prefetching of table slots, list states and bitmaps | Up to 30% off the scan, which the profile showed was mostly cache misses | T 0.175 -> 0.169 s, S 832 -> 878, K +0.7%, on the design of that time. The later stages kept the idea, per document | local | reverted |
| Disabling transparent huge pages, to stop per-thread arenas rounding up to 2 MB | M down by about 39 MB, the part we couldn't account for | M 105.8 -> 97.2 MB, T 0.224 -> 0.190 s on B, but A flat and C and D about 15% slower. One heap solved this instead | local | not kept |
| Hot terms in a small table of their own, and the rest in a second | Most tokens never reach the larger table | Half the tokens went through the second level, for 35% of cycles and 43% of mispredicts. One prefetched table replaced it | local counters, then `run-45be71f36a92` | replaced |
| Reading the corpus in pieces with `pread`, at 128 and 32 KiB | On A and C this beat mapping on the box | T 0.1047, 0.1037, 0.1033 and 0.1051 against 0.1045, 0.1041, 0.1045 mapped, for 23 more lines. B's time is its user code, where A's and C's was faults | `run-11a9f3af506f`, `run-5c55e7c459ea` | reverted |
| `MADV_POPULATE_READ` for each range | Faults 4,215 -> 400, user cycles -3% | Flat: T 0.1008 without it, for 3 fewer lines | `run-fa98f771cce1` | removed |
| Printing at known offsets, chained OR and NOT applied directly, ranges shrinking toward the end | Parse, plan and answers are about twice their local time on the box | T 0.1028 and 0.1008 at S 954, against a median of 0.1006 at S 928. They held up better under load, but the box is idle | `run-3e06bf691391`, `run-918ece6b0a51` | reverted |
| Hot table sizes, sets at 2 MB, `present.fill`, skipping zero words, op-specific bitmap loops, 1 or 4 MB samples, compiler flags | Each a few percent of cycles | Nothing above 1% | local | not kept |

## The final design

- **Data layout.** A table of 64-byte sets of three terms, with a short key, a long key and a word for each; the unused fourth way's word is what a lookup that finds none gets, so no branch depends on it. Terms are numbered by what they need, those with bitmaps, then those with chains, then the rest, the commonest first, so what the scan touches most sits together. A document's marks are a byte per term. BOOL checks are a truth table with three or five term numbers. Queries anchored on a common term read postings: a bitmap of N bits for each of their common terms, and for their other terms a chain per thread of 32-byte blocks with 12 offsets of 2 bytes each. All heap comes from one region grown 64 MB at a time, so huge pages replace thousands of small faults.
- **I/O.** Both files are mapped read-only, with zero padding so that block loads can run past the end. Output is written in batches of 1,024 queries, in order.
- **Parallelism.** Threads claim ranges of the corpus in order, each 1/16 of a thread's share (1/192 at 12 threads), rounded to 64 documents, so no two threads write the same bitmap word and each thread's postings ascend. Each thread keeps a count and sum per check; summing them after the scan is deterministic. A BOOL answer's ten smallest ids sit behind a lock that only a thread's first ten hits per check can reach, and a phrase's match takes it every time, as matches are so few.
- **The hot loop.** Per document: `token_edges` lists where tokens start and end, from 64-byte masks and a table per byte. `look_up` takes four tokens at a time: their keys and hashes in AVX2, a prefetch of each set, then in a second pass one vector compare per set and lists of what to do, kept with no branch by a table per mask. A third pass gives the unsure words a second look, and lists where phrase anchors are. `mark` writes the byte of each term with flags or no bitmap, and lists the first occurrence of those with BOOL checks or chains; a loop in `scan_ranges` writes the rest. `set_row` takes 64 marks to a bit each. `store_rows` transposes 64 documents' rows into bitmap words. `check_bools` runs the short checks in one loop, as a loop per anchor mispredicts its exit. About 98 instructions per token, at 1.5 instructions per cycle per thread.

## How we know it's correct

- Every build is checked by SHA-256 against the naive reference on 15 generator seeds at tiny and sample, 4 answer-preserving rewrites of the input and 4 limit cases, at 1, 2, 3, 7, 12 and 64 threads, and on 5 seeds at full scale at the default thread count.
- A constant-hash test build, where every key collides, gives the same answers.
- Repeated runs at 2 to 14 threads, since the term table is filled by all threads at once, and races don't show on one pass: 300 runs with none wrong, before any change there goes out.
- An adversarial review of the build before D28 found no wrong answers: 960 race runs and hostile inputs against the reference. Its only limits are outside the spec, and are listed in the [README](README.md#limits).
- **Reproduce it yourself:** `python3 tools/verify.py --full`, at the default thread count (Docker, git and Python only).

## With another 24 hours

To be written at freeze.
