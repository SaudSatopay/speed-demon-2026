# Track C: performance log

**Result:** the reference takes **14.44 s / 367 MB** on the board. Our best run is `run-700275291d62`: **0.0195 s / 10.4 MB**, K **45.56** and **82.9** speed points. That's one of 3 runs of the final source, which gave T 0.0195 to 0.0198 s, M 10.4 to 10.4 MB and a median K of 45.42 (D94).

**Check it yourself:** the board run named there, or locally `python3 tools/verify.py --tracks c`. Output SHA-256 on seed 1: `d8088b1cd42ba1338b1345b107b55db7b96ef1f5066f3ff64fd26a8912a0f536`.

## Where it loses

- S is 399 code lines on the board's last run, against the reference's 108, which costs about 4 speed points. Each block of it was measured against what it buys: the two-stage parser, the position lists built with PEXT and GFNI, parallel setup and raw threads pay in T and M. Line cuts have mostly been free: one pass took out 56 cold lines, and S fell from 483 to 446 since `7d61093` (the locked set, no pin, line trims) with T inside the noise below. The exception is the record loop: two trims there saved 7 lines and looked like 3% of T (`e7232af`). The last pass took out 47 more with T inside the noise (D92 to D94).
- T moved between 0.0200 and 0.0207 over today's board runs from `7d61093` on, except `e7232af`, `c036bab` and `a17ec2a`, which we rejected at 0.0212, 0.0211 and 0.0215. On one source, `7d61093` and its second sample `7ca3b51`, T was 0.0200 and 0.0204 and M 10.44 and 11.04 MB, so gains under about 3% in T need several samples.
- On Sunday Nov 1, NickTrapp leads this track with K 47.50 at T 0.018, M 12.8 and S 303. Against us, their lines are worth about 4% of K and their time about 5%, and we get about 4% back on M. We don't know how they reach that T with that S: the board shows only the numbers.

## What the box taught us

The box is an Intel i5-13500: Golden Cove P-cores, a 48 KB L1 that two threads share, 1.25 MB of L2 and 24 MB of L3. Our local PC is a Zen 4 7600X, so local cycles only rank instruction cuts; anything that turns on caches, locks or system calls needed a board run. Each finding names the commit and the run in the board's log. C over D is C's T divided by D's T from the same run.

- **A second cache-line fill per record costs about 22%, and the lock isn't the limit.** `4d52c96` wrote marks in batches under one lock, with keys and bits in separate tables (`run-fc66302c3d21`): T 0.0244, against 0.0199 before it (`15d35df`, `run-b2919f2e99ea`) and 0.0200 once it was reverted (`7d61093`, `run-ef9c22ce540f`). It had fewer locked operations, each record touched a second line, and it lost.
- **A separate tag-prefetch loop bought nothing.** `7d61093` merged it into the loop after it: T 0.0200 against 0.0199, S 483 against 495 (`run-ef9c22ce540f`, `run-b2919f2e99ea`). The tag lines are L2 hits there, which the core's reorder window evidently covers.
- **Fewer instructions help less on the box than locally.** `2bfc56f`, with `ef4eba8` before it, took the instructions per record from 258 to 227 (local counts, D69 and D70), and cycles fell 8% locally (D69). On the box T fell about 2.5%: 0.0206 and 0.0205 on `01bd03f` and `49541ad` (`run-4b19cac796d7`, `run-2f38c5a3b9e0`), then 0.0200 and 0.0199 on `34d00e9` and `15d35df` (`run-2e148f178165`, `run-b2919f2e99ea`), which ran its code because the board skipped `2bfc56f` itself. C over D fell 2.3%.
- **Claiming pieces instead of segments loses about 3%.** `5cf2c6d` and `9364e41` had each thread claim one 31 KiB piece at a time (`run-eac7456d8237`, `run-7614b1a9fa91`): T 0.0206 and 0.0207 against 0.0200, C over D 0.1420 and 0.1410 against 0.1364, and K 43.26 and 43.39 against 43.74, though they were 10 and 23 lines shorter. Rejected.
- **The BOM by arithmetic loses too.** `e7232af` dropped the byte order mark branch and counted quotes in a closure: T 0.0212 and C over D 0.1464 in one run (`run-a1af68b512ae`), for 7 lines fewer than `28c4157` (T 0.0206, `run-8bd4c7d3275e`). Rejected.
- **One source moves about 2% in T and up to 6% in M.** `bfba195` (the locked set for names with no slot, no pin, line trims) gave T 0.0204, M 10.46, S 446 and K 43.88, the best K of any scored run of this track (`run-13115bed748f`). `7ca3b51` ran `7d61093`'s source again and gave T 0.0204, M 11.04 and K 42.89 (`run-6cee893dbf6c`), against T 0.0200, M 10.44 and K 43.74 the first time.
- **The region prefetch pays, though the tag prefetch didn't.** `a17ec2a` dropped it (`run-34c6a4bf2815`): T 0.0215 and C over D 0.148, for one line less. The join table is nine times the size of the tags, which may be why, but we didn't measure that. Rejected.
- **More segments don't pay.** `c036bab` used 48 segments per thread instead of 24 (`run-4b4240212d91`): T 0.0211 and C over D 0.1435. First to last finish is only about 600 us, so halving it could gain little, and each segment costs two small reads and a short piece. One sample, so it may be noise, but nothing says it helps. Rejected.

## Where the reference spends its time

About 28% is the number regular expressions, 27% the aggregation and 19% reading the CSV, all in Python objects.

## The journey

Every step cites the live-board run that measured it. Numbers marked *local* come from our scorer on a Zen 4 7600X, not on the box's Golden Cove, and only explain decisions.

| Step | Change | Why we expected it to help | Run ID | T (s) | M (MB) | Points |
|---|---|---|---|---|---|---|
| 0 | The reference, unchanged | — | — | 14.44 | 367 | 0 |
| 1 | v1: scalar parser from the spec, no strings, interned names, one thread | Removes Python objects and all decoding | run-7908768d3243 | 0.770 | 10.6 | 44.2 |
| 2 | 12 threads: cuts at CRLF, a lock-free shared name table with one cache line per name | Records are independent, and name lookups dominate the time | run-e9b10f4d8fed | 0.086 | 14 | 65.4 |
| 3 | Block loads for every field, and lookups batched with prefetching | About 470 cycles per record, with a mispredicted loop exit per field and a stall per name lookup | run-239daeb7287a | 0.055 | 14.2 | 69.3 |
| 4 | Slow paths the spec rules out removed, 1-byte region entries | Sanjay Sah passed us with half our lines and less memory, and S and M were costing more than T | run-c96eba9a4379 | 0.050 | 12.9 | 71.2 |
| 5 | Same design in 369 lines: one exit from the table's loop, a row that is an array, and regions parsed once | Two passes over `regions.csv` were serial time, and rivals have half our lines | run-126b276f6fae | 0.061 | 12.8 | 69.9 |
| 6 | Regions parsed in one pass, and the output sorted once, when it's written | The temporary list, the `seen` table and the output loop's destructuring were all lines with no job at run time | superseded before it ran | 0.0304 *local* | 23.5 *local* | 68.8 *local* |
| 7 | BZHI masks the key words, in place of a `u128` shift | Three 128-bit shifts per record were about 5% of all instructions, and the box is limited by its clock | run-77d40f3a0a13 | 0.0490 | 12.9 | 72.6 |
| 8 | Parser and thread-helper line cuts | Twenty-four fewer counted lines; parser semantics rely only on spec 2.2 guarantees | run-30c0ac031f43 / run-424808ccefdc | 0.050 / 0.051 | 12.9 / 13.6 | 72.5 / 72.1 |
| 9 | One tag byte per slot picks the slot, no shared insert counter, 110,000 slots to start | Local paired T -10.2% median, 40 of 41 wins on seed 1 and -9.7%, 21 of 21 on seed 7; lines per lookup 2.53 -> about 1; M 13.37 -> 11.79 MB; 393.2 -> 427.6 instructions and 1.53 -> 1.32 mispredicts per record; S +28 | run-ce5c6d99e125 | 0.043 | 16.5 | 72.9 |
| 10 | Thirty-two input segments per thread | Smaller chunks keep worker finishes closer and reduce the tail | run-39305ec9b1ed | 0.044 | 23.5 | 71.1 |
| 11 | Touch one word per 2 MiB table extent before workers | Avoid racing to first-fault shared THP extents | run-a906ff073a9f | 0.043 | 11.8 | 74.4 |
| 12 | Line cuts after the tag table landed: helpers merged, region table fixed at the spec's 1 MiB, no early exit from a full table | S 348 -> 294 for K about +2.6%, with 461.8 -> 455.9 instructions and 1.45 mispredicts per record flat, and a 21-pair local A/B at -0.1% | run-fbb083bab1d0 | 0.0422 | 11.7 | 75.1 |
| 13 | exp: the slot line is prefetched with intent to write (`+prfchw`, `_MM_HINT_ET0`), and `mark` does one locked OR instead of a load and an OR | Nine records in ten set a new bit on a line another core wrote last. A read prefetch leaves the line shared, so the locked OR paid a second cross-core trip to own it, and the plain load before it rarely saved anything. counts.sh sample 455.9 -> 453.4 instructions and 1.45 -> 1.43 mispredicts per record; quick 59/59, threads 324/324, constant-hash 29/29; S unchanged; local ab.sh -2.5 to -3.2%, inside its noise, so the board decides | run-7eded1297288 (on a339892, same code) | 0.0405 | 12.1 | 75.4 |
| 14 | exp: glibc linked statically (`+crt-static` in the config's target features), no code change | Same reasoning as Track A: the loader's work is inside T. 150 alternating pairs on the tiny dataset, one thread, local: 1550 -> 1308 us median, paired -235 us, so about 0.4 ms of the box's 42 ms. Quick 59/59, threads 324/324; the official offline Docker build and grid pass 59/59, image 30 MB | run-20fb099cf745 (on 3f8a0d3, same code) | 0.0420 | 11.9 | 75.0 |
| 15 | Fewer mispredicts: a quoted field's closing quote found from the quote parity (one carry-less multiply per 64-byte window) instead of a branch per `""` pair, and the tag group chosen without a branch in the lookup | counts.sh sample: 452.8 -> 453.1 instructions, 1.42 -> 0.56 mispredicts per record (parity alone 0.90, lookup alone 1.12); one mispredict costs 15-20 instructions on the box. S 294 -> 306; quick 59/59, threads 324/324, constant-hash 29/29; a 240-file grammar fuzz at 1, 2, 3 and 7 threads matches the old binary | run-fca909ccd556 | 0.0402 | 12.1 | 75.3 |
| 16 | Step 15 reverted: the quote scan and the lookup are as in step 14 again | Two board runs with step 15, run-f2dc362cb1f6 at T 0.0400 and run-fca909ccd556 at 0.0402, sit inside the 0.0399 to 0.0404 band of the three runs before it, so cutting 0.86 mispredicts per record moved nothing on the box, and its 12 lines cost K 0.6% | run-1bf401f347ee | 0.0407 | 12.1 | 75.3 |
| 17 | A tight per-record path: one quote mask and carry-less multiply per record, slot guess kept from the prefetch, vector key compare, 32-byte batch entries, batch of 8 | Stubbing showed parse 55% and name table 40% of the run, with locked ORs near nothing (racy store: -2%); at 12 threads memory waits on the region table and the tags stood out, and instructions were 467 per record against a hand budget near 200 | run-2c1664173483 | 0.0342 | 12.4 | 76.3 |
| 18 | Threads start before the join table, tags as one vector per two groups (no spill branch), three numbers in one vector, name hash from its first 8 bytes, no bounds checks in the apply step | Traces show 88% of the run is parallel with every thread busy, so instructions still rule; a sample of the disassembly showed the spill branch, three digit parsers and repeated checks | run-3e3a6211acd0 | 0.0316 | 12.3 | 77.0 |
| 19 | MADV_POPULATE_READ on each claimed segment | A sample of the 12-thread run showed about 14% in the kernel, mostly mapping file pages a fault at a time | run-fc3cd980cb14 | 0.0296 | 11.6 | 77.9 |
| 20 | Slots, tags and the join table in one 2 MiB aligned mapping | The four huge pages under the slots were charged in full with 1.3 MB spare, while the join table and tags sat elsewhere on small pages | run-875784a1cc5b | 0.0298 | 12.0 | 77.6 |
| 21 | Back to separate slots, tags and join table, and threads share the setup: first touches, then region chunks, through one job counter | The shared mapping lost on the box (T flat, M 11.6 -> 12.0: the old layout's last huge page was never whole, so nothing was being wasted). Setup is the only serial part left, about 2 of 16 ms locally | run-d97756a91b99 (on 34ac4a4, same code) | 0.0302 | 11.5 | 77.5 |
| 22 | Segment sizes round up, so that no tail is lost at many threads on a tiny file | aede9f8 lost the end of a tiny file at 64 threads (the 12-thread full run was never affected); a chained script of mine pushed it past the failed sweep | run-d7a1887aa3a1 | 0.0276 | 11.3 | 78.5 |
| 23 | exp: two-stage parse: fixed blocks list the separators, then records read their fields from the list | Parse IPC stuck at 2.1 matches the 75-cycle chain through pos: each field's position depends on loading the one before. One-thread time held with far more instructions, so the chain was the limit | run-cdd73a08538b | 0.0283 | 11.8 | 78.0 |
| 24 | exp: read the file in 32 KiB pieces with pread, instead of mapping it | Mapping costs about 15% of the CPU in the kernel, plus TLB misses on small pages and the page tables in M; a small buffer also stays in the thread's caches | run-05a581a880bf (on 3ad1469, same code) | 0.0252 | 12.9 | 78.5 |
| 25 | No page faults or heap on the worker threads: one mapping holds the table, tags, join table and thread stacks, touched by the main thread while pinned to one CPU; raw pthreads on those stacks; country keys and long-name compares without the heap | A kernel trace showed memory.peak includes each CPU's unused charge batch, up to 252 KB per CPU that ever charges anything; workers still charged through stack faults and malloc arenas, and main wandered between CPUs | run-5a7af9740ca5 | 0.0242 | 10.6 | 79.4 |
| 26 | A shorter build of the same design: 32 bytes of key in the slot, with a header in the bitmap's spare bits; no file offset or spare buffer | S 576 cost 6% of K against the leader. With 32 inline bytes no generated name goes to the slow path, so the table no longer needs the file | run-16aa7850a49c | 0.0241 | 12.0 | 79.3 |
| 27 | No huge page past the last whole one, and smaller stacks | The mapping's tail sometimes covered a whole fifth 2 MiB extent, depending on the address, which added 1.4 MB: the difference between the two last board runs, M 10.55 and 11.98 | run-b6e30ba52d59 | 0.0241 | 10.5 | 79.8 |
| 28 | LOCK BTS instead of a compare-exchange loop for a country's bit | The bit's word and mask took 10 instructions per record, and the atomic left a load and a loop; BTS with a register offset finds the word itself | run-b2a076472b89 | 0.0239 | 10.6 | 79.9 |
| 29 | A key padded with ones and compared whole, with no header; shorter setup and record code | A header needed its own load, shift and compare, and a length check on top; a padded key can't equal a free or half-written slot, so one vector compare answers everything | run-ec53b3392f0d | 0.0236 | 10.2 | 80.2 |
| 30 | exp: 16 KiB pieces instead of 32 | A smaller buffer fits L1 even with two hyper-threads sharing it, at twice the reads; local timing can't weigh that for the box | run-74097560210f | 0.0290 | 9.9 | 78.1 |
| 31 | Read 128 KiB pieces, 12 region chunks fewer per thread, the file length once | 16 KiB pieces cost T 0.0236 -> 0.0290 on the box: about 10 us per read there, so 32 KiB pieces still spent about 5 ms of T in syscalls | run-4123f6d73011 | 0.0256 | 11.5 | 78.8 |
| 32 | exp: 64 KiB pieces | 128 KiB gave T 0.0256 M 11.5, worse than 32 KiB's T 0.0236 M 10.2, so L1 fit matters as well as the number of reads | run-ec4c117ee076 | 0.0235 | 12.2 | 79.5 |
| 33 | Back to 32 KiB pieces, keeping fewer region chunks and one length query per file | Box, T and M: 16 KiB 0.0290 and 9.9, 32 KiB 0.0236 and 10.2, 64 KiB 0.0235 and 12.2, 128 KiB 0.0256 and 11.5. A piece that fits L1 matters as much as the number of reads | run-ff03b26be635 | 0.0235 | 10.3 | 80.3 |
| 34 | exp: 48 KiB pieces | A fit of T against piece size: about 18.2 ms plus 173/P ms for reads (10 us each here), plus a penalty only above 32 KiB, where the piece leaves L1. 48 KiB should halve the difference | run-ef7020a9cfbc | 0.0233 | 10.7 | 80.2 |
| 35 | exp: prefetch the tag window a stage before reading it, with 32 KiB pieces again | The tag load was the top stall in the samples (8%); 48 KiB pieces gave T 0.0233 M 10.7, no better than 32 KiB | run-c1390be655e6 | 0.0241 | 10.3 | 79.9 |
| 36 | exp: chunks of 8 blocks instead of 16, without the tag prefetch | Tag prefetch gave T 0.0241, no gain. Fewer records between a prefetch and its use could keep more of them in L1 | run-36825a973028 | 0.0238 | 10.6 | 80.0 |
| 37 | One open file per thread, and chunks of 16 blocks again | Every read takes and drops a reference on the file, so a shared descriptor bounces that cache line between all the threads; 8-block chunks gave T 0.0238, no gain | run-5e7678ecad91 | 0.0236 | 12.1 | 79.4 |
| 38 | One shared file again | One file per thread gave T 0.0236, no gain over 0.0235, for 12 more lines | run-34ca4bf56387 | 0.0237 | 10.4 | 80.1 |
| 39 | Each row on its own cache line | Hardware counters (perf stat works in WSL now): 165 thread cycles per record, 108 not retiring, 45 of those waiting on loads, 2.9 first-level misses per record. A row straddling two lines is one of them | run-db39be141d11 | 0.0237 | 10.3 | 80.1 |
| 40 | exp: map the file, populate a segment at a time, and copy pieces in user space | Piece sizes on the box suggest about 5 ms of T goes to the reads themselves, and that the copy into an L1-sized buffer is what helps. This keeps the copy with about 100 system calls instead of 6,100 | run-eae13dca1386 (on 1de6339, same code) | 0.0287 | 11.4 | 77.5 |
| 41 | Reads again | Mapping, populating and copying gave T 0.0287 on the box against 0.0237, though it was 1.5% faster locally: mapping pages costs far more there | run-aed60fb1e1b8 (on 016e0af, same code) | 0.0237 | 10.3 | 80.1 |
| 42 | Sixteen fewer lines, same code | S costs K too | superseded before it ran | — | — | — |
| 43 | Count with repeats in one word, name bytes with note bytes in another | The compiler vectorised pairs of adjacent adds into four instructions each, and turning a set bit into a new-name flag took four more | run-03dd6f13e5ea | 0.0240 | 10.3 | 80.1 |
| 44 | MADV_POPULATE_WRITE on the mapping, in one call | Touching a page at a time took 150 page faults more, and the piece-size runs suggest small-page faults cost a lot on the box: each larger piece needed more stack pages | run-c4842a2cf4c2 | 0.0238 | 10.7 | 80.0 |
| 45 | exp: 64 KiB pieces again, now that stack pages take no faults | 64 KiB pieces halve the reads but gained nothing before; if faults on the extra stack pages were the reason, this should show it | run-9969baf99d50 | 0.0249 | 11.1 | 79.3 |
| 46 | 32 KiB pieces, to stay | 64 KiB gave T 0.0249 and M 11.1 even with no faults on the stack pages, so faults weren't why it gained nothing. Also tried and dropped, by hardware counters: listing only commas (cycles flat, S +9), counting long notes' quotes with masks (cycles +1%), prefetching rows (+1%) | run-861d7f77375d (on ad4fd74, same code) | 0.0252 | 10.4 | 79.5 |
| 47 | List writes by bit-plane transpose: PEXT and PDEP turn a block's separators into 8 position bytes at once; the amount's end from one bit test; a fixed copy for the list's tail | The first stage's list writes were the largest block of instructions, and a sibling stall is not the whole story: fewer ops still buy cycles | run-66ae7ca08130 | 0.0232 | 10.4 | 80.2 |
| 48 | exp: a first short loop per chunk hashes each name and prefetches its tag window | The load-stall profile puts 36% of load stalls on the tag-window chain: a thread's half of the reorder buffer can't run far enough ahead to cover even an L2 hit | run-79723ba010ba | 0.0227 | 10.7 | 80.2 |
| 49 | Fifty-six fewer lines, the same hot code: no job struct, keys built in main, one-line helper signatures, simpler cold paths | S 541 cost about 8% of K against the leader. A Fable agent cut only cold code, checking counters after each group | run-9c9b12a38866 | 0.0235 | 10.4 | 80.3 |
| 50 | Read a long name's pointer only after its slot says it's complete | A Fable review found it: the pointer and length were loaded before the Acquire, so a reader could compare against a null pointer while a writer was part way through. 2 crashes in 1,900 runs on a spec-legal stress input; the generator almost never writes such a name (over 32 bytes: about 1 seed in 100,000) | run-40b0e94343ad | 0.0227 | 10.3 | 80.7 |
| 51 | C's entry point instead of std's | Std reads /proc/self/maps, installs handlers and allocates a signal stack before main: about 15 system calls the timer counts, and system calls cost far more on the box than locally. Finding from Track A | run-55bdb13c33fe | 0.0228 | 10.4 | 80.6 |
| 52 | exp: one pass per record, with no list: one mask from the name's start, table work a record behind | A Fable agent's test of a leaner parser. Local: cycles level, instructions 295 -> 264, S 493 -> 512, and its stall analysis says the box's larger reorder buffer should favour it | run-2775b3f75d4a | 0.0251 | 10.5 | 79.4 |
| 53 | Back to the two-stage parser | One pass per record gave T 0.0251 on the box against a steady 0.0228, though it matched locally with 10% fewer instructions | run-f3f3e8a17e16 | 0.0247 | 10.3 | 79.8 |
| 54 | A piece's bytes line up with the file's within a cache line | The kernel's copy runs fastest when source and destination agree modulo 64; a finding from Track A | run-3caf48b61129 | 0.0228 | 10.4 | 80.6 |
| 55 | 24 segments per thread instead of 8, regions stay at 8 parts; the join table loses its unused 48 KB | With 8 per thread, threads that ran out of segments sat idle while the last ones finished: first to last finish was 1.05 ms of 13.9 locally | run-52609ee14f2a | 0.0232 | 10.5 | 80.3 |
| 56 | exp: four extra 1-byte reads per piece | To time the fixed part of a read on the box: pieces of 16 KiB ran T 0.0290 against 0.0236 at 32 KiB, which suggests about 10 us per call, against 3.3 us in all locally | run-bf4f339f917a | 0.0240 | 10.2 | 80.1 |
| 57 | exp: 64 bytes again from each page of a piece | To tell the per-page cost of a read from the per-byte cost on the box: four small reads per piece cost only about 0.5 us each, while reading each piece twice costs about 11 us | run-7c8ac310d8c8 | 0.0263 | 10.3 | 79.1 |
| 58 | A home for every slot instead of every 16, so the 32-slot windows overlap | 124,000 lookups a run were of names outside their home window, and each costs a mispredict and extra misses. A table with more slots cut that, but costs memory | run-59eef47377d7 | 0.0221 | 10.3 | 80.9 |
| 59 | exp: name slots of 52 bytes, 20 of key and 32 of bits, instead of a cache line; names over 20 bytes kept apart | M -13% locally (9.85 to 8.53). Names are short: over 20 raw bytes is 1 name in 80. Locally T +3.6%, so the box must decide | run-8e400f022e03 | 0.0234 | 9.0 | 80.8 |
| 60 | exp: four more huge pages mapped, touched and unmapped at the start | To time what zeroing and mapping huge pages costs on the box: locally 65 us per page. Memory reads high in this run | run-ab6c9a703129 | 0.0232 | 18.3 | 77.9 |
| 61 | Rows unpacked to 8 words, one line each; min and max as branches; repeats added from BTS's carry; the locked write last in a record | The third loop had 103 instructions, with 5 register spills, 5 instructions turning the carry into a count and 8 for min and max, which hardly ever change | run-9fb79229bd83 | 0.0216 | 10.5 | 81.1 |
| 62 | Position bytes from one GFNI matrix multiply instead of six PDEPs, with +gfni in the build's target features | Six PDEPs a block take the one port that runs them, and the box's Raptor Lake cores have GFNI | run-9c8ac175663f (on e23ff69, same code) | 0.0224 | 10.3 | 80.8 |
| 63 | Row columns reordered so sums aren't adjacent; tuning for old CPUs lifted (false dependencies of bit counts, slow three-part LEA) | The compiler paired adjacent additions into vector instructions that took more than the two would, and added zeroing instructions and split LEAs meant for old CPUs | run-7f5e16b8c475 | 0.0208 | 10.7 | 81.4 |
| 64 | exp: 52-byte name slots on the current base, with the join table first in the mapping | Local M 9.85 to 8.53, wall +3%, S +17; its one earlier sample came in a run where A and D also ran slow | run-8bae16321801 (on 901c373, same code) | 0.0218 | 9.2 | 81.5 |
| 65 | Amounts parsed with saturating subtraction and their own weights, with no PEXT; back to 64-byte name slots | A saturating subtract turns the sign and the point into zero digits, and per-lane weights skip the point. 52-byte slots: T 0.0218 M 9.22 S 513, K 42.62, level with 64-byte at 17 more lines | run-7388e3ba58ca | 0.0231 | 10.6 | 80.3 |
| 66 | The padding table on one cache line; 8 lines saved outside the hot path | PADS straddled a line, so most key loads split across two. mark's signature fits one line now, and find always gives a slot, since a full table throws the pass away anyway | run-b3beb9066dee | 0.0205 | 10.5 | 81.7 |
| 67 | exp: a second locked operation per record, on the thread's own row | To time one uncontended locked operation on the box: Golden Cove's are about twice Zen 4's in cycles, and locally dropping the lock saves only 2% | run-55160d6e0d76 | 0.0230 | 10.4 | 80.5 |
| 68 | Back on the build without the extra locked operation | The experiment has run | run-4b19cac796d7 | 0.0206 | 10.4 | 81.7 |
| 69 | Per-record path: 27 fewer instructions (home and tag kept from the first loop, byte offsets for slots, branch for the BOM, CMP/SBB sign, shuffle classifier, PEXT directly on the mask, ADC-style empty-field fix, locked write's table pointers held in registers) | The box is a 2.5 GHz Golden Cove near its rename limit, so instructions set T: last time -7% instructions gave -5% T | run-35483c1c080c | 0.0208 | 10.6 | 81.5 |
| 70 | Fewer instructions again: saturating byte counts as before, a name's length in 32 bits, a real branch for the byte order mark | ef4eba8's first sample showed no gain, in a run where A was 15% slow: this one also gives a second sample | run-2e148f178165 (on 34d00e9, same code) | 0.0200 | 10.5 | 82.0 |
| 71 | No lock per record: marks wait in batches per part of the bits, and a batch is written under one lock. Keys and bits in separate tables | An extra lock per record cost T +9% on the box, while removing 10% of the instructions gave -2% | run-fc66302c3d21 | 0.0244 | 10.4 | 79.4 |
| 72 | Back to c-v88 (batched locks: T 0.0244), with 16 KiB read pieces and a smaller stack | Two threads share a 48 KiB L1 on the box, so a 32 KiB piece per thread doesn't fit; a second line fill per record cost +22% T, so fills look like the limit | run-21a7d786c717 | 0.0207 | 13.3 | 80.6 |
| 73 | Back to 32 KiB pieces (16 KiB: T 0.0207, M 13.28). The first loop merged into the second: tags read without a prefetch | Instructions -8 and S about -13; the tag lines are L2 hits on the box, which its reorder window should cover | run-ef9c22ce540f | 0.0200 | 10.4 | 82.0 |
| 74 | exp: threads claim one 31 KiB piece at a time, each finding its own cuts in the text it read | First to last finish is about 600 us with 24 segments per thread, and each segment costs two small reads. Claiming pieces should close the gap and drop those reads; also 10 fewer lines | run-eac7456d8237 | 0.0206 | 10.4 | 81.8 |
| 75 | exp: names with no slot (long, or every window to the end of the table full) go to one locked set, so no restart and no pointers in slots; on top of the piece claims | Thirteen fewer lines with only cold paths changed, and a second sample of the piece claims | run-7614b1a9fa91 | 0.0207 | 10.3 | 81.9 |
| 76 | Back to segments, with the locked set for names with no slot, and line trims: BOM by arithmetic, quotes counted in a closure | Piece claims cost about 3% of T in two samples, C over D 0.141 against 0.136, so they go; the locked set changes only cold paths and is 13 lines shorter, and the trims 8 more | run-a1af68b512ae | 0.0212 | 10.7 | 81.5 |
| 77 | exp: only the locked set for names with no slot, with the BOM branch and the old quote count back | The trims gave T 0.0212 and C over D 0.146, so this takes them out to see whether the locked set alone keeps T 0.0200 | run-8bd4c7d3275e | 0.0206 | 10.4 | 81.9 |
| 78 | exp: the main thread is no longer pinned while it touches the mapping | Eight lines for a guard against a move in the first millisecond; if the main thread rarely moves, M stays and S falls | run-3a52bd37d7cc | 0.0207 | 10.4 | 81.9 |
| 79 | Best source again, c-v91, as a second sample | Every build since has T near 0.0206 against its one sample of 0.0200, so this shows whether that was luck or the box drifting | run-6cee893dbf6c | 0.0204 | 11.0 | 81.6 |
| 80 | exp: the locked set, no pin, and two line trims with the BOM branch kept | c-v91 re-sampled at T 0.0204 M 11.04, so T moves 2% and M 6% on one source, and the removals look neutral; this is a second sample of them | run-98bd53a6de8f | 0.0206 | 10.4 | 82.0 |
| 81 | The slot guess written in the record loop, and three statements on fewer lines | Seven fewer lines locally (S 452 -> 446 on the board) with the same instructions per record, and a third sample of the locked set with no pin | run-13115bed748f | 0.0204 | 10.5 | 82.1 |
| 82 | exp: no prefetch of the region byte | The tag prefetch loop did nothing on the box, so this one may not either; one line and a few instructions per record | run-34c6a4bf2815 | 0.0215 | 10.4 | 81.6 |
| 83 | exp: 48 segments per thread instead of 24, region prefetch back | No region prefetch gave T 0.0215, so it stays; first to last finish is about 600 us, and twice the segments should halve that for a little more reading | run-4b4240212d91 | 0.0211 | 10.3 | 81.8 |
| 84 | Best source again, c-v102 | 48 segments per thread gave T 0.0211, C over D 0.1435, and no region prefetch T 0.0215, C over D 0.148, so both go | run-918ea96eea72 | 0.0200 | 10.7 | 82.2 |
| 85 | T0 for the slot prefetch | ask to read the line first, it might queue less on a contended line | run-4262cd2eb150 | 0.0227 | 10.3 | 81.0 |
| 86 | Tag windows on multiples of 32 | homes aligned so a window never straddles a line, at the cost of more names outside their first window | run-029d59804052 | 0.0210 | 10.4 | 81.8 |
| 87 | T0 for the slot prefetch, second sample | first sample ran while the box was slow, A slowed 10 percent in the same run with no code change | run-37447595dbd9 | 0.0203 | 10.4 | 82.2 |
| 88 | Tag windows on multiples of 32, second sample | first sample T 0.0210 K 43.18, inside the range the chosen source has shown | run-e6a8ded823e8, on docs commit 1e21980 with this source | 0.0211 | 10.3 | 81.8 |
| 89 | T0 for the slot prefetch, third sample | samples so far K 41.66 on a slow box and K 43.96 | run-2bf3009dbcc3 | 0.0207 | 10.2 | 82.0 |
| 90 | Tag windows on multiples of 32, third sample | samples so far K 43.18 and K 43.28, the second on docs commit 1e21980 | run-ee0a2bfc62e2 | 0.0230 | 10.2 | 80.9 |
| 91 | Chosen source restored | neither idea beat it over three samples | run-a80a4965a4fa | 0.0203 | 10.4 | 82.1 |
| 92 | Twenty line cuts on cold paths: the input files as plain functions, main does its own work, one job claim call, one miss chain, one BOM line | S 446 to 426 is worth about 0.7 percent of K, and instructions fall 302.5 to 300.7 per record, so nothing hot pays for it | pending | — | — | — |
| 93 | All memory is one zeroed 2 MiB aligned static in the image: no mmap, no NameTable struct, table functions are free functions, and the run ends in _exit | S 426 to 407 is worth about 0.7 percent of K. Local: instructions 300.7 to 289.7 per record in the counting build, paired T -2.5 percent, M flat at 9.70 | pending | — | — | — |
| 94 | Helper merges: one function gives a name's home and tag, one closure for arguments and one for claiming a job, a full block uses up its own mask | S 407 to 399 is worth about 0.3 percent of K. Instructions per record unchanged at 289.7 in the counting build | pending | — | — | — |
| 95 | exp: records file mapped, pages released per segment, page tables touched first, one prefetch per block | Mapped lost in D19 to D24 when exit tore pages down serially on the clock; A found that worth 2.7 ms. No copy, smaller stacks | pending | — | — | — |
| 96 | exp: mapped file, part and boundary merged, thread start without generics | Same design as d4f32c1, whose T and M tied the read design; these cold cuts cost nothing at run time | pending | — | — | — |
| 97 | exp: positions by a fixed count of eight TZCNT steps instead of PEXT and GFNI | 14 lines. Local says T up 9 percent but local has misranked this track repeatedly and this was never run on the box | pending | — | — | — |
| 98 | exp: back to PEXT and GFNI positions on the mapped file | TZCNT gave T 0.0208 and C over D 0.143 against 0.134, for 14 lines, so the box agrees with local here | pending | — | — | — |
| 99 | exp: mapped file without the prefetch per block | Does a software prefetch of the mapped text pay on the box, or does the core stream it anyway. Two lines | pending | — | — | — |
| 100 | exp: text prefetch back on the mapped file | Without it four samples gave median T 0.0201 and K 45.04 | pending | — | — | — |
| 101 | exp: 12 segments per thread on the mapped file | Each segment now costs one release with a TLB shootdown to every thread, not two reads, so fewer may pay | pending | — | — | — |
| 102 | exp: 6 segments per thread on the mapped file | 12 gave K 45.58 on one sample; this shows the trend of fewer releases against worse balance | pending | — | — | — |
| 103 | exp: 12 segments per thread, sample 2 | One sample cannot decide; C moves 5 percent on one source | pending | — | — | — |
| 104 | exp: 6 segments per thread, sample 2 | One sample cannot decide; C moves 5 percent on one source | pending | — | — | — |
| 105 | exp: 12 segments per thread, sample 3 | One sample cannot decide; C moves 5 percent on one source | pending | — | — | — |
| 106 | exp: 6 segments per thread, sample 3 | 6 has K 45.46 and 46.24 against 12 at median 45.56; fewer segments mean fewer TLB shootdowns | pending | — | — | — |
| 107 | exp: 3 segments per thread, sample 1 | 6 has K 45.46 and 46.24 against 12 at median 45.56; fewer segments mean fewer TLB shootdowns | pending | — | — | — |
| 108 | exp: 6 segments per thread, sample 4 | 6 has K 45.46 and 46.24 against 12 at median 45.56; fewer segments mean fewer TLB shootdowns | pending | — | — | — |
| 109 | exp: 3 segments per thread, sample 2 | 6 has K 45.46 and 46.24 against 12 at median 45.56; fewer segments mean fewer TLB shootdowns | pending | — | — | — |
| 110 | exp: 6 segments per thread, sample 5 | 6 has K 45.46 and 46.24 against 12 at median 45.56; fewer segments mean fewer TLB shootdowns | pending | — | — | — |
| 111 | exp: 3 segments per thread, sample 3 | 6 has K 45.46 and 46.24 against 12 at median 45.56; fewer segments mean fewer TLB shootdowns | pending | — | — | — |
| 112 | exp: 1seg, sample 1 | 3 segments per thread median K 45.90; 1 segment could also drop the claim. 105000 homes is 0.32 MB less | pending | — | — | — |
| 113 | exp: h105k, sample 1 | 3 segments per thread median K 45.90; 1 segment could also drop the claim. 105000 homes is 0.32 MB less | pending | — | — | — |
| 114 | exp: 1seg, sample 2 | 3 segments per thread median K 45.90; 1 segment could also drop the claim. 105000 homes is 0.32 MB less | pending | — | — | — |
| 115 | exp: h105k, sample 2 | 3 segments per thread median K 45.90; 1 segment could also drop the claim. 105000 homes is 0.32 MB less | pending | — | — | — |
| 116 | exp: 1seg, sample 3 | 3 segments per thread median K 45.90; 1 segment could also drop the claim. 105000 homes is 0.32 MB less | pending | — | — | — |
| 117 | exp: h105k, sample 3 | 3 segments per thread median K 45.90; 1 segment could also drop the claim. 105000 homes is 0.32 MB less | pending | — | — | — |
| 118 | exp: 3seg, sample 7 | 1 segment had an outlier T 0.0274 and 105000 homes median K 44.84, both rejected. 105000 cost 5 percent T, so test the other direction | pending | — | — | — |
| 119 | exp: h115k, sample 1 | 1 segment had an outlier T 0.0274 and 105000 homes median K 44.84, both rejected. 105000 cost 5 percent T, so test the other direction | pending | — | — | — |
| 120 | exp: 3seg, sample 8 | 1 segment had an outlier T 0.0274 and 105000 homes median K 44.84, both rejected. 105000 cost 5 percent T, so test the other direction | pending | — | — | — |
| 121 | exp: h115k, sample 2 | 1 segment had an outlier T 0.0274 and 105000 homes median K 44.84, both rejected. 105000 cost 5 percent T, so test the other direction | pending | — | — | — |
| 122 | exp: 3seg, sample 9 | 1 segment had an outlier T 0.0274 and 105000 homes median K 44.84, both rejected. 105000 cost 5 percent T, so test the other direction | pending | — | — | — |
| 123 | exp: h115k, sample 3 | 1 segment had an outlier T 0.0274 and 105000 homes median K 44.84, both rejected. 105000 cost 5 percent T, so test the other direction | pending | — | — | — |
| 124 | exp: 3seg, sample 10 | 1 segment had an outlier T 0.0274 and 105000 homes median K 44.84, both rejected. 105000 cost 5 percent T, so test the other direction | pending | — | — | — |
| 125 | exp: h125k, sample 1 | 115000 homes gave median K 46.13 over 5, T 0.0190, against 45.94; test further | pending | — | — | — |
| 126 | exp: h140k, sample 1 | 115000 homes gave median K 46.13 over 5, T 0.0190, against 45.94; test further | pending | — | — | — |
| 127 | exp: h125k, sample 2 | 115000 homes gave median K 46.13 over 5, T 0.0190, against 45.94; test further | pending | — | — | — |
| 128 | exp: h140k, sample 2 | 115000 homes gave median K 46.13 over 5, T 0.0190, against 45.94; test further | pending | — | — | — |
| 129 | exp: h125k, sample 3 | 115000 homes gave median K 46.13 over 5, T 0.0190, against 45.94; test further | pending | — | — | — |
| 130 | exp: h140k, sample 3 | 115000 homes gave median K 46.13 over 5, T 0.0190, against 45.94; test further | pending | — | — | — |
| 131 | exp: h115k, sample 4 | 115000 homes gave median K 46.13 over 5, T 0.0190, against 45.94; test further | pending | — | — | — |
| 132 | exp: cuts, sample 1 | Cuts: 19 cold lines, hot code the same. Tags16: wrong guesses from 8 bit tag collisions send about 4 percent of records to the slow path, 140000 homes showed T falls with them | pending | — | — | — |
| 133 | exp: tags16, sample 1 | Cuts: 19 cold lines, hot code the same. Tags16: wrong guesses from 8 bit tag collisions send about 4 percent of records to the slow path, 140000 homes showed T falls with them | pending | — | — | — |
| 134 | exp: cuts, sample 2 | Cuts: 19 cold lines, hot code the same. Tags16: wrong guesses from 8 bit tag collisions send about 4 percent of records to the slow path, 140000 homes showed T falls with them | pending | — | — | — |
| 135 | exp: tags16, sample 2 | Cuts: 19 cold lines, hot code the same. Tags16: wrong guesses from 8 bit tag collisions send about 4 percent of records to the slow path, 140000 homes showed T falls with them | pending | — | — | — |
| 136 | exp: cuts, sample 3 | Cuts: 19 cold lines, hot code the same. Tags16: wrong guesses from 8 bit tag collisions send about 4 percent of records to the slow path, 140000 homes showed T falls with them | pending | — | — | — |
| 137 | exp: tags16, sample 3 | Cuts: 19 cold lines, hot code the same. Tags16: wrong guesses from 8 bit tag collisions send about 4 percent of records to the slow path, 140000 homes showed T falls with them | pending | — | — | — |
| 138 | exp: tags16-h105k, sample 1 | 16 bit tags median K 46.25 with T 0.0189 and M 10.95. They remove wrong guesses, so a fuller table may cost less T than before, and memory is worth K | pending | — | — | — |
| 139 | exp: tags16-h110k, sample 1 | 16 bit tags median K 46.25 with T 0.0189 and M 10.95. They remove wrong guesses, so a fuller table may cost less T than before, and memory is worth K | pending | — | — | — |
| 140 | exp: tags16-h105k, sample 2 | 16 bit tags median K 46.25 with T 0.0189 and M 10.95. They remove wrong guesses, so a fuller table may cost less T than before, and memory is worth K | pending | — | — | — |
| 141 | exp: tags16-h110k, sample 2 | 16 bit tags median K 46.25 with T 0.0189 and M 10.95. They remove wrong guesses, so a fuller table may cost less T than before, and memory is worth K | pending | — | — | — |
| 142 | exp: 16 bit tags at 115000 homes, the best measured source, for handover | Median K 46.25 against 45.95 at 110000 and 45.81 at 105000 | pending | — | — | — |
| 143 | c-c3: 28 lines cut, empty-name hash fix | S 383 to about 355 | pending | — | — | — |
| 144 | c-c4 parts shrink geometrically, 8 per thread, sample 1 | less idle at the end | pending | — | — | — |
| 145 | c-c3 sample 2 | second sample | pending | — | — | — |
| 146 | c-c4 sample 2 | second sample | pending | — | — | — |
| 147 | c-c3 sample 3 | third sample | pending | — | — | — |
| 148 | c-c4 sample 3 | third sample | pending | — | — | — |
| 149 | c-p1 populate each segment before parsing, sample 1 | faults per 16 pages cost kernel time, and text prefetch stops at unmapped pages | pending | — | — | — |
| 150 | c-c3 restored | populate sample T 0.0196 K 45.92, no gain in one run, not proven worse | pending | — | — | — |
| 151 | c-pack2 48-byte short slots packed across lines, 64-byte long ones, sample 1 | memory: local peak 10.22 to 8.65 MB | pending | — | — | — |
| 152 | c64f 64-byte slots at 104500 homes, sample 1 | memory: local peak 10.26 to 9.47 MB, S -3 | pending | — | — | — |
| 153 | c-pack2 sample 2 | second sample | pending | — | — | — |
| 154 | c64f sample 2 | second sample | pending | — | — | — |
| 155 | c64f sample 3 | third sample | pending | — | — | — |
| 156 | c-f40 table freed before output, sample 1 | local peak steady at 9.44 MB, worst of 5 sets M | pending | — | — | — |
| 157 | c-f40 sample 2 | second sample | pending | — | — | — |
| 158 | c-f40 sample 3 | third sample | pending | — | — | — |
| 159 | c-p4 packed slots at three huge pages plus small ones, sample 1 | memory: local peak 8.12 MB | pending | — | — | — |
| 160 | c64f restored | c64f median K 46.64 over 4, c-f40 46.66 over 3 for one more line, so level | pending | — | — | — |
| 161 | c-k1 find walks hits and free slots in one loop, plain spin, 11 lines cut, sample 1 | S 352 to 341 with hot code unchanged | pending | — | — | — |
| 162 | c-k1 sample 3 | third sample | pending | — | — | — |
| 163 | c-ph1 12 segments per thread, pages released after parsing, spills hop by tag, sample 1 | end idle was 8.6 percent of T and windows walked fall from 780k to 251k; local -5.6 percent | pending | — | — | — |
| 164 | c-ph1 sample 2 | second sample | pending | — | — | — |
| 165 | c-ph1 sample 3 | third sample | pending | — | — | — |
| 166 | c-ph2 same with no release at all, sample 1 | exit cost on the box is unknown, S 8 lower | pending | — | — | — |
| 167 | c-ph2 sample 2 | second sample | pending | — | — | — |
| 168 | c-kp line cuts with end release and tag hops, sample 1 | c-k1 S 341 and c-ph1 T 0.0192 combined; c-ph2 with no release lost, T 0.0228 | pending | — | — | — |
| 169 | c-kp sample 2 | second sample | pending | — | — | — |
| 170 | c-kp sample 3 | third sample | pending | — | — | — |
| 171 | c-kp2 yield restored in the wait, sample 1 | c-kp T 0.0196 against c-ph1 0.0192, and the plain spin moved hot registers | pending | — | — | — |
| 172 | c-kp2 sample 2 | second sample | pending | — | — | — |
| 173 | c-kp2 sample 3 | third sample | pending | — | — | — |
| 174 | c-kp3 24 segments per thread, sample 1 | a shorter end idle, now that a segment costs no release | pending | — | — | — |
| 175 | c-kp2 sample 4 | alternate, so box hours even out | pending | — | — | — |
| 176 | c-kp3 sample 2 | second sample | pending | — | — | — |
| 177 | c-kp2 sample 5 | alternate | pending | — | — | — |
| 178 | c-kp3 sample 3 | third sample | pending | — | — | — |
| 179 | c-kp2 restored, sample 6 | chosen source at HEAD | pending | — | — | — |
| 180 | c-map files mapped on a 2 MiB boundary, sample 1 | local faults 3094 to 651 and T -8 percent, if the box caches huge folios | pending | — | — | — |
| 181 | c-map sample 2 | second sample | pending | — | — | — |
| 182 | c-map sample 3 | third sample | pending | — | — | — |
| 183 | c-scout cold scout guesses spilled and new names, sample 1 | local find calls 282k to 125k, T -2.3 percent | pending | — | — | — |
| 184 | c-scout sample 2 | second sample | pending | — | — | — |
| 185 | c-scout sample 3 | third sample | pending | — | — | — |
| 186 | c-kp2 restored | chosen source at HEAD until these are judged | pending | — | — | — |
| 187 | c-ms scout and aligned mapping together, sample 1 | c-scout T 0.0190, c-map M about 10.0 | pending | — | — | — |
| 188 | c-ms sample 2 | second sample | pending | — | — | — |
| 189 | c-ms sample 3 | third sample | pending | — | — | — |

## Decision log

**D12. First-touch the table's 2 MiB extents before worker inserts.** Context: C board memory was 16.5 MB on `cf10d8a`, then fluctuated to 24.5 MB on the reverted eight-segment build; THP is `always` in the scorer. Chose: store zero once per 2 MiB of the shared name table before spawning workers. Evidence: eight alternating, prewarmed, 12-thread cgroup-memory runs under THP `always` lowered median memory peak 12.15 -> 10.92 MiB on `cf10d8a` and 12.63 -> 10.88 MiB on the 32-segment source. Sample counts stayed flat at 461.8 instructions and 1.45 mispredicts per record. Full 59/59, thread 324/324, constant-hash tiny 29/29, and Docker/pre-push 59/59 pass. Official board `run-27f5cc082b16` measured T 0.043 s, M 12.0 MB, K 30.67; latest `run-a906ff073a9f` confirms T 0.043 s, M 11.8 MB and K 30.74. Kept. The observed memory reduction is confirmed; attributing the exact THP/page-fault mechanism remains an inference because `NameTable::new` collects the atomic vector on the main thread.
**D11. Thirty-two parser segments per thread.** Context: eight chunks per worker leave a longer tail when segment costs vary. Chose: 32 chunks per worker, retaining CRLF cuts and the same atomic cursor. Evidence: full 59/59, thread 324/324, constant-hash 54/54, Docker/pre-push 59/59; `counts.sh` 461.8 -> 462.7 instructions/record and 1.45 -> 1.45 simulated mispredicts. Local whole-run timing leaned about 2% faster but remained inside noise. Outcome: board `run-39305ec9b1ed` returned T 0.044 s, M 23.5 MB, K 26.46 (71.1 points), below the prior best K 28.72. Reverted the chunk-size change; first-touch is tested separately.
**D10. Isolate the shared insertion counter on its own cache line.** Context: the Rust layout put NameTable.mask at byte 48 and used at byte 56, so every observed 16-byte stack alignment still shared their cache line; alignment alone did not explain the board's two timing modes. Chose: wrap used in a 64-byte-aligned counter, which gives it a full cache line apart from the fields read on every record. Evidence: a 50-run diagnostic confirmed four base alignments with overlapping timing ranges, then a 50-pair instrumented comparison measured -3.276 ms mean (95% CI -3.805 to -2.746), with all outputs correct. The clean uninstrumented 61-pair full comparison measured medians 27.046 -> 24.460 ms, paired mean -2.298 ms (95% CI -2.778 to -1.819), paired median -2.729 ms (-10.09%). Full-scale counts stayed flat: 393.2 instructions, 43.9 branches and 1.53 misses per record before and after. The five-run local scorer moved K 26.45 -> 28.85 (+9.1%), T 0.0251 -> 0.0223 s, M 23.7 -> 20.6 MB, and S 315 -> 317. Quick 59/59, thread sweep 324/324 and constant-hash 29/29 pass. Outcome: send through the pre-push gate; the board will show whether this removes the slow mode.

**D9. Cut parser and helper lines that the spec no longer needs.** Context: several serial helpers and temporary region structures existed only for broader input forms. Chose: inline the single-use thread helper, treat unquoted text fields as empty per spec 2.2, derive header/record starts from the fixed header and CRLF rules, return the two needed region arrays directly, and shorten the hash fold. Evidence: S 339 -> 315. Full-scale counts: 393.1 -> 393.2 instructions per record (+123,588 total), branches 43.8 -> 43.9 (163,286,683 -> 163,418,711), simulated mispredicts 1.70 -> 1.53 (6,333,591 -> 5,712,170). Quick 59/59, thread sweep 324/324, constant-hash tiny 29/29 and pre-push 59/59 pass. Local `ab.sh` gave -3.9% with 89% spread, inconclusive. Board runs `run-30c0ac031f43`, `run-424808ccefdc`, `run-5de497312867` and `run-d99643ef6e94` gave K 28.22 (T 0.050 s, M 12.9 MB), K 27.67 (T 0.051 s, M 13.6 MB), K 25.31 (T 0.062 s, M 13.3 MB) and K 25.48 (T 0.061 s, M 13.1 MB); the earlier best remains K 28.28. Controlled A/B used scratch builds of pre-cut `fd374dc` and the current source; both passed quick 59/59 and thread sweep 324/324. At SD_THREADS=1, 41 alternating full-scale pairs put current at +1.57% median wall time (paired-mean CI +2.105 to +3.388 ms); RSS was unchanged. At SD_THREADS=12, the same 41-pair comparison was flat: paired median -0.11%, mean -0.035 ms, 95% CI -0.871 to +0.830 ms; RSS 205.18 vs 205.22 MB. Outcome: keep the line cuts for the 24-line S reduction; scored 12-thread timing is unresolved/flat locally, and the board slowdown is not reproduced by this A/B. Monitor the single-thread cost. The existing local A/B did not separate the builds.
**D1. Never unescape.** Context: `names_distinct` compares unescaped values, and `name_bytes` counts them. Options: unescape into a buffer, or work from the raw bytes. Chose: raw bytes. Inside a quoted field, every `"` belongs to a `""` pair, so the raw bytes encode the value one to one, and the value's length is the raw length minus the number of pairs. Cost: none. Evidence: the grid's rewrites and limit cases. Outcome: kept.

**D2. Cut segments at CRLF, not by counting quotes.** Context: a thread must start on a record boundary, and quoted notes contain LFs. Options: count quotes in a first pass to find the parity at each cut, or use section 2.2's promise that a CR is only ever the start of a CRLF. Chose: the CRLF, which needs no extra pass, since about half the records end in one. Cost: a file with no CRLF isn't split, though it's still right. The `lf-except-last` variant in our bank tests exactly that. Outcome: kept.

**D3. One shared name table instead of one per thread.** Context: 12 tables and bitsets would cost about 58 MB, and 2x of memory is worth 3 speed points. Options: per-thread tables merged at the end, or one lock-free table. Chose: one table, where each 64-byte slot holds a header, the first 24 bytes of the name, and one bit per country. On seed 1, 99.99% of names fit inline, so a lookup usually costs one cache miss, without touching the file. Cost: 8.4 MB for the table. If a file ever holds more names than the table, the pass restarts with a bigger table. A test build starts with 16 slots, to exercise that path. Evidence: T 0.404 -> 0.041 s, K 7.11 -> 20.11 (local). Sweep passes at 1, 2, 3, 7, 12 and 64 threads, and at full scale on seeds 1 and 7. Outcome: kept.

**D4. Block loads, and batched lookups.** Context: a record cost about 470 cycles. Each field's byte loop mispredicted its exit, and each name lookup stalled on a cache miss. Chose: 64-byte quote masks for text fields, one 8-byte load per number, and an amount squeezed into 8 digits by moving the digits before the point up a byte. Records then wait in batches of 16 while their table slots and region entries are prefetched. The padded mapping means no tail code. Cost: S 410 -> 549. Evidence: T 0.041 -> 0.028 s, K 20.11 -> 23.44 (local), and the sweep, including `lf-except-last`, still passes. Outcome: kept.

**D5. Remove what the spec rules out.** Context: Sanjay Sah took 1st with T 0.066 s against our 0.051, but S 275 against our 549. Section 2.2 caps ids, region ids and amounts at 8 bytes, so their byte-loop fallbacks could go, and 240 countries fit in a byte, which frees 1 MB. More than 255 countries now aborts rather than risking a wrong answer. Cost: none in time. Evidence: S 549 -> 485, M 22.2 -> 20.8 MB, K 23.46 -> 24.09 (local). Outcome: see the journey table.

**D6. Cut the code, not the design.** Context: on Track A, cutting lines without touching the hot paths took first place. Options: the same here. Chose: a row as an array of eight numbers, where a single record is a row too, so one `merge` adds a record to its country and adds up the threads' rows. Regions are parsed once, into a temporary list, instead of twice, because that stage is serial. Ids past the highest region share one last entry of the table, so finding a country has no branch. Evidence: S 485 -> 369, with local T -2.5% (noise 7.4%), and all checks passing. What would change our minds: a higher T on the board, over a few runs, since T swings between 0.050 and 0.063 s on the board with no change in the code.

**D7. Give out country indices as codes appear, and sort at the end.** Context: byte-order indices needed every code before the first index could be given out, so regions took a temporary list and a second pass. Options: keep that, or give out indices on first sight and sort the 241 output lines once. Chose: the latter, with index 0 as `_UNMATCHED`, and `country_of` grown in doubling steps, so its last entry still lies past every id. Cost: none; the sort is 241 short strings. Evidence: S 356 -> 339, counts.sh 415.0 -> 414.5 instructions and 1.53 -> 1.54 mispredicts per record, K 23.27 -> 23.81 (local, with T and M inside their noise). Outcome: kept.

**D8. BZHI for the key mask.** Context: masking a name's three key words used a `u128` shift each, to cope with a full 8-byte word. Options: keep it, a branch on the length, or BZHI, which x86-64-v3 has. Chose: BZHI. It reads only the low 8 bits of its count, so a name of 32 bytes or more can lose a word from its hash, which is harmless because those names are compared in full. Cost: none in lines. Evidence: counts.sh 414.5 -> 393.1 instructions per record (-5.2%), though the simulated mispredicts move 1.54 -> 1.70, an estimated 3 cycles against about 12 saved. ab.sh 15 runs: -1.9%, inside 16% noise, so T is unproven locally, and the case rests on the box being bound by its clock. Outcome: kept; the board decides.

**D9. Tags instead of probing. Context: linear probing at this load touched 2.53 slot lines per lookup, and every insert updated a shared counter. Chose a word of 8 tag bytes per group, so a lookup goes straight to the slot's line and a full group spills into the next. No counter: a pass gives up only when a search has seen every group full. The table starts at 110,000 slots for the spec's pool of 100,000 names, and more names than slots still restart with a bigger table.

**D10. Fewer lines, same hot path. Context: the tag table and first-touch took S from 317 to 348. Chose to merge helpers and drop what the spec rules out. A file with more names than slots now finishes its pass before restarting, which only costs time in that case.

**D13. Prefetch the slot line for writing. Context: PREFETCHW was tried once with inline assembly (-3.6% locally, inside 16% noise, for 5 lines) and dropped. The `+prfchw` target feature, which the box's i5-13500 has, makes it one hint constant and no lines, and the locked OR then needs no plain load first. Evidence: `prefetchw` in the binary; counts.sh 455.9 -> 453.4 instructions and 1.45 -> 1.43 mispredicts per record; all grids pass. What would change our minds: no gain on the board over two runs, since local timing can't see a cross-core effect on this one-CCD PC.

**D14. Link statically, as on Track A. Cost: binary 0.5 -> 1.5 MB, S unchanged. Evidence: 150 paired tiny-scale runs, 1550 -> 1308 us median, paired -235 us; all grids and the offline gate pass. What would change our minds: a higher T on the board over two runs.

**D15. Cut mispredicts rather than instructions. Context: a full 64-byte block classifier was built and lost (559 instructions per 60-byte record against 456), because the per-field scan already costs one quote mask per text field; its one asset, no branch per pair, survives here. The parity of the quotes up to each byte is odd at a pair's first quote, at the closing quote and after it, so the closing quote is the first odd bit followed by another; a window carry handles longer fields. In the lookup, the second tag group is chosen with arithmetic. Cost: S +12, about +6 instructions per record. What would change our minds: no gain over two board runs.

**D16. Give the 12 lines back. Context: the box's C time did not move for a 60% cut in mispredicts, so C there is not limited by branch prediction; what limits it is elsewhere (the parse's instruction count, or the lookup's cache traffic). Chose: the step 14 source. What would change our minds: nothing for this patch; the lesson is that a mispredict cut needs a board run before it earns lines.

**D17. Rewrite the per-record path rather than the design. Context: rivals are at half our T with the same M. Options: per-thread rings for the shared bits (racy-store test says at most 2%), or fewer instructions and fewer waits. Chose the second: local 12-thread median 26.5 -> 18.9 ms with huge pages always, one-thread instructions 467 -> 331 per record, S 294 -> 379 for now. What would change our minds: no T gain on the box.

**D18. Same design, fewer instructions. Options: per-thread rings (no gain measured), or more of the same cuts. Chose the cuts: one-thread instructions 331 -> about 289 per record, local 12-thread 18.9 -> about 17.5 ms. The byte-wise tag exchange first allowed two slots for one name (3 wrong in 600 stress runs, caught by the grid); one load now answers both the hit and the free question (0 wrong in 600). What would change our minds: a wrong answer in any stress run, or no T gain on the box.

**D19. Map each segment's pages in one call. Context: page faults cost about a tenth of the CPU. Options: read() into buffers, which costs memory and a copy, or this. Chose this: local scorer T up 434x -> 502x, paired -3.6%, 3 lines. What would change our minds: no T gain on the box, where faults may cost less.

**D20. One mapping for all three. Options: leave them apart, or grow the slots to fill the pages. Chose this: paired local -9%, 2 more names' worth of spare slots, S +15. What would change our minds: M or T worse on the box.

**D21. Parallel setup, no arena. Options: keep the arena, or parse regions on one thread. Chose this: paired local -6.5% against the populate build, S 400 -> 436. What would change our minds: K below 36.22 on the box.

**D22. Fix the rounding. The push script now refuses without a passed check on the exact tree.

**D23. Test on the box. Options: tune the single-pass parser, or break the chain. Local: 1 thread -8%, 12 threads +8%, 307 instructions per record against 265, no fallback parser. The local 12-thread run is dominated by Hyper-V's lock waits, so only the box can decide.

**D24. Test on the box, where syscalls and page faults cost differently from WSL. Local 12 threads: +3%, with system time up. Removes the mapping, padding and populate code. What would change our minds: T or M worse than the mapped two-stage build over three samples.

**D25. Main does all the touching. Options: leave the charges, or one extra thread touching. Local cgroup peak 12.1 -> 9.9 MB (median of 25), worst 11.5; local 12-thread T -10%. S 501 -> 576 for now. What would change our minds: K below the pread build's 37.5.

**D26. Compact rewrite. S 576 -> 503, paired local -3.8%, M the same, all grids pass. What would change our minds: K below 38.69 over three samples.

**D27. Mark the tail no-huge. Local peak over 40 runs, worst 11.32 -> 10.19, median 9.85 unchanged. Stacks 80 -> 68 KB (61.6 KB seen in use) and none for the main thread. What would change our minds: M still above 11 on the box.

**D28. One instruction. Instructions 309 -> 299 per record, paired local -2.5%. Also tried and dropped: reusing the first stage's quote masks (more instructions), PGO (flat, code is nearly straight-line), a third loop for the locked writes (+1.7%), spawning during the first touches (+3.5%), row alignment, piece and chunk sizes (all flat locally). What would change our minds: T up on the box, where BTS m,r is microcoded.

**D29. Padded keys. Instructions 299 -> 292 per record, S 511 -> 499, local flat. Also: Option::then_some in mark cost 15 instructions per record, so it stays an if. What would change our minds: a wrong answer in any stress run (0 of 300 on seeds 2 and 3).

**D30. Test: local +3%, M -0.2 MB. Keep only if K beats 40.23 over samples.

**D31. Weigh T^0.5 against M^0.2: 64 KiB +5.2% K, 128 KiB +7%, 256 KiB +6% by the model. Start at 128 KiB. Each thread's stack grows by 96 KB. What would change our minds: K below 40.23.

**D32. Find the optimum between them on the box.

**D33. 32 KiB: the best K of the four, with no T left to gain above it.

**D34. Test the middle.

**D35. Test on the box, where L2 hits cost more cycles than locally: local -1.8%, S +5.

**D36. Test on the box; local is flat.

**D37. Local -1%, S +6. What would change our minds: K below 40.3 over samples.

**D38. Simpler. M 12.1 on that run looks like a rare outlier that also hit the 64 KiB run: watching for more.

**D39. Align rows. Cycles 164.7 -> 160.6 per record, misses 2.93 -> 2.21, both steady over repeats. A 40-byte row gave the same, for more code. 8, 4 and 32-block chunks, 24 KiB pieces and a tag prefetch all land at 160 to 164. What would change our minds: K below 40.3 over samples.

**D40. Test on the box: local -1.5%, M +0.5 MB, S +18.

**D41. Reads in 32 KiB pieces stay. Local timing can't rank kernel paths: only the box can.

**D42. Shorter names where rustfmt was breaking calls over several lines. Counters unchanged.

**D43. Pack them: all fit 32 bits per spec section 3. Cycles 159.7 -> 158.0, instructions 300 -> 297, S +3.

**D44. One call. Faults 210 -> 65, kernel cycles flat locally, M the same, S -2. What would change our minds: T or M worse on the box over samples.

**D45. Test: compare with T 0.0238.

**D46. 32 KiB is settled.

**D47. A Fable agent's pipeline work. Local: instructions 296.7 -> 279.3, cycles about -3.5%, S 491 -> 530. Grid, sweep, constant-hash and 300 stress runs pass. What would change our minds: K below 40 over samples.

**D48. A Fable agent's work on the name table. Local: L1 misses from L2 2.24 -> 1.24, from L3 0.30 -> 0.13, load waits -9 cycles, instructions +14, cycles about -1%, S +11. Keep only if T clearly beats 0.0232.

**D49. Keep the design. S 541 -> 485, instructions +0.4 per record, cycles flat, stack use unchanged. A barrier between the phases cost 2% wall locally, so threads wait on a count of finished region jobs instead. What would change our minds: T up on the box.

**D50. Acquire first. 0 crashes in 2,100 runs on the same input, hot path untouched, S +2. The same review found no wrong output in about 940 comparisons against the reference and 7,300 repeat runs.

**D51.** Take it, S +6: about 0.15 ms, too small to show in one sample.

**D52.** Test: keep only if K clearly beats 41 over samples.

**D53.** Two-stage stays: without a list, one record's latencies serialize.

**D54.** Take it: kernel cycles about -2% locally, S +2. Too small to see in one board run.

**D55.** Paired local wall -2 to -4% over five runs at 16 to 48 segments, identical binaries -0.4%. 24 balances the idle time against two small reads and a short piece per segment

**D56.** An experiment, reverted right after its run

**D57.** An experiment, reverted right after its run

**D58.** Cycles per record -3%, mispredicts 0.38 to 0.33, S -1, M the same. One name, one slot still holds: 1,800 repeat runs on seed 2, 144 hostile runs. Tag groups of 8, 4 and 2 measure the same or between

**D59.** One sample or more, then keep or revert

**D60.** An experiment, reverted right after its run

**D61.** Instructions per record 290 to 272, cycles -3.4% over 15 runs each, S no higher. SLP off is flat

**D62.** Cycles per record -3.3%, instructions the same, wall -3 to -5% locally, S +4

**D63.** Instructions per record 271 to 261, cycles -1.5 to -2%, S the same

**D64.** Keep or revert after several samples

**D65.** Cycles -2 to -3%, S +1

**D66.** S -8, cycles flat to -1%

**D67.** An experiment, reverted right after its run

**D68.** See PROGRESS for the cost of a locked operation on the box

**D69.** Instructions 258 -> 229 per record, cycles -8% locally, S 492. Keep if the board's T falls

**D70.** Instructions 229 -> 227. Judge both builds over several samples

**D71.** exp: local wall +10%, which hides the lock's cost. The box decides; revert unless T falls

**D72.** exp: keep if T holds near 0.020 with M lower

**D73.** exp: keep if K beats 43.70 over samples

**D74.** exp: Threads claim one 31 KiB piece at a time and find their own cuts in the text they read, which drops the segments, their small reads and some lines. Board `run-eac7456d8237` (`5cf2c6d`): T 0.0206, M 10.44, S 473 (-10), K 43.26, C over D 0.1420. The segment build before it (`7d61093`) had T 0.0200 and C over D 0.1364, so this is 3 to 4% slower. One sample; D75 adds a second.

**D75.** exp: Names with no slot (long, or every window to the end of the table full) go to one locked set, so there is no restart and no pointer in a slot; on top of the piece claims. Board `run-7614b1a9fa91` (`9364e41`): T 0.0207, M 10.35, S 460 (-13), K 43.39, C over D 0.1410. That is a second sample of the piece claims, and they are again 3 to 4% slower than 0.0200 and 0.1364. Outcome: piece claims rejected (D76); the locked set stays, since it changes only cold paths and takes 13 lines out.

**D76.** Back to segments, with the locked set and two line trims (the BOM by arithmetic, quotes counted in a closure). Board `run-a1af68b512ae` (`e7232af`): T 0.0212, M 10.69, S 459, K 42.63, C over D 0.1464, the slowest of the segment builds. Outcome: the trims were 7 lines for about 3% of T (D77's build has S 466), so they are rejected; the piece claims stay out.

**D77.** exp: Only the locked set, with the BOM branch and the old quote count back. Board `run-8bd4c7d3275e` (`28c4157`): T 0.0206, M 10.40, S 466, K 43.40, C over D 0.1385. T is back inside the 0.0200 to 0.0207 band of the other segment builds, so the locked set is not what cost T in D76. Outcome: the locked set is kept; the BOM arithmetic and the quote closure are rejected.

**D78.** exp: The main thread is not pinned while it touches the mapping. Board `run-3a52bd37d7cc` (`cfa0c46`): T 0.0207, M 10.42, S 457 (-9), K 43.43, C over D 0.1419. M stayed where it was (10.40 on the run before), so a move of the main thread in that first millisecond does not show. Outcome: the pin is gone; D80 and D81 ran without it at M 10.40 and 10.46.

**D79.** Best source again (`7d61093`'s, with no difference in `git diff`), as a second sample. Board `run-6cee893dbf6c` (`7ca3b51`): T 0.0204, M 11.04, S 483, K 42.89, against T 0.0200, M 10.44 and K 43.74 the first time. One source moved 2% in T, 6% in M and 2% in K. Outcome: the first sample, 0.0200, was the good end of the noise; builds within 0.0200 to 0.0207 can't be told apart on one sample.

**D80.** exp: The locked set, no pin and two line trims, with the BOM branch kept. Board `run-98bd53a6de8f` (`8f4bd27`): T 0.0206, M 10.40, S 452, K 43.62, C over D 0.1351. Against the two samples of `7d61093`'s source (T 0.0200 and 0.0204) that is level in T, for 31 lines fewer (S 483). Outcome: kept.

**D81.** The slot guess is written in the record loop, and three statements are on fewer lines. Board `run-13115bed748f` (`bfba195`): T 0.0204, M 10.46, S 446 (-6), K 43.88, C over D 0.1348. Locally the instructions per record are the same. It is the third sample of the locked set with no pin, and the three agree at T 0.0204 to 0.0207. K 43.88 is the best of any scored run of this track. Outcome: kept; it is the last row of the journey.

**D82.** exp: No prefetch of the region byte, because the tag prefetch loop had done nothing on the box. Board `run-34c6a4bf2815` (`a17ec2a`): T 0.0215, M 10.42, S 445 (-1), K 42.78, C over D 0.1482, against T 0.0200 to 0.0204 and C over D 0.126 to 0.138 for the source with it. Outcome: reverted, since one line doesn't pay for T up 5%.

**D83.** exp: 48 segments per thread instead of 24, with the region prefetch back, to halve the 600 us between the first and last thread's finish. Board `run-4b4240212d91` (`c036bab`): T 0.0211, M 10.28, S 446, K 43.22, C over D 0.1435. Outcome: reverted, since T rose where it should have fallen, on one sample, and the most it could gain is small.

**D84.** Best source again, `~/sd-scan/c/c-v102` in our notes: 24 segments and the region prefetch. Board `run-918ea96eea72` (`b8d9d47`): T 0.0200, M 10.70, S 446, K 44.04. This source first ran at `bfba195` (`run-13115bed748f`, K 43.88), and its best is at `8e7eee2` (`run-73a8aecd2907`, K 44.29, the best of any scored run of this track). Over its 5 runs up to Nov 1: T 0.0200 to 0.0215, M 10.31 to 10.70, median K 44.00. Outcome: the final source.

**D85.** Test T0 in place of ET0 on the board, three samples.

**D86.** T0 (7c28e9d) rejected on one sample, T 0.0227 K 41.66, far past the noise. Test aligned homes next, three samples.

**D87.** Correction to the last entry: one T0 sample at T 0.0227 does not reject it, since A slowed in that run too. Three samples each, taken in turn.

**D88.** Second sample of aligned homes, taken in turn with T0.

**D89.** Third sample of T0, taken in turn with aligned homes.

**D90.** T0 rejected: K 41.66, 43.96, 43.73, median 43.73 against the chosen source at 44.02. Third sample of aligned homes.

**D91.** Rejected: T0 median K 43.73 (41.66, 43.96, 43.73). Rejected: homes on multiples of 32, median K 43.18 (43.18, 43.28, 41.48). The chosen source stands at median K 44.02.

**D92.** Twenty line cuts on cold paths. Instructions fell from 302.5 to 300.7 per record in the counting build. Board: K 44.41, 44.10, 44.28 at S 426, median 44.28 against 44.00 for the source before it. Outcome: kept.

**D93.** All memory is one zeroed static aligned to 2 MiB, so the mapping's setup, the `NameTable` struct and `self` go. `.bss` starts on a 2 MiB boundary too, and `exit` writes its first byte, which cost one whole huge page (local peak 9.70 to 12.06 MB), so the run ends in `_exit`, and the locked state is a static that isn't all zeros, which keeps it out of that extent. The box's kernel, 6.12, honours the alignment for a static PIE. Local: instructions 300.7 to 289.7 per record, paired T -2.5%, peak 9.70 MB. Board `run-0766e5fe11fd`: T 0.0203, M 10.25, S 407, K 44.68. Outcome: kept, with D94 on top.

**D94.** Helpers merged: one function gives a name's home and tag, closures for the arguments and for claiming a job. Instructions per record unchanged. Board: K 45.42, 45.29, 45.56 at S 399, median 45.42 against 44.00. Outcome: kept.

**D95.** Keep only if median K over 3 runs beats the current source

**D96.** Keep only if median K over 3 runs beats the current source

**D97.** Keep only if median K over 3 runs beats the current source

**D98.** TZCNT rejected. Mapped file sampled again

**D99.** Compare C over D and T with the mapped samples

**D100.** No-prefetch rejected, two lines do not pay

**D101.** Follow up only if clearly above the mapped samples

**D102.** Compare with 12 and 24

**D103.** Median of three decides

**D104.** Median of three decides

**D105.** Median of three decides

**D106.** Median of three decides

**D107.** Median of three decides

**D108.** Median of three decides

**D109.** Median of three decides

**D110.** Median of three decides

**D111.** Median of three decides

**D112.** Median of three decides against 45.90

**D113.** Median of three decides against 45.90

**D114.** Median of three decides against 45.90

**D115.** Median of three decides against 45.90

**D116.** Median of three decides against 45.90

**D117.** Median of three decides against 45.90

**D118.** Median of three decides against 45.90

**D119.** Median of three decides against 45.90

**D120.** Median of three decides against 45.90

**D121.** Median of three decides against 45.90

**D122.** Median of three decides against 45.90

**D123.** Median of three decides against 45.90

**D124.** Median of three decides against 45.90

**D125.** Median of three decides against 46.13

**D126.** Median of three decides against 46.13

**D127.** Median of three decides against 46.13

**D128.** Median of three decides against 46.13

**D129.** Median of three decides against 46.13

**D130.** Median of three decides against 46.13

**D131.** Median of three decides against 46.13

**D132.** Median of three decides

**D133.** Median of three decides

**D134.** Median of three decides

**D135.** Median of three decides

**D136.** Median of three decides

**D137.** Median of three decides

**D138.** Median of three decides against 46.25

**D139.** Median of three decides against 46.25

**D140.** Median of three decides against 46.25

**D141.** Median of three decides against 46.25

**D142.** Fuller tables rejected. S about 355 builds still to sample

**D143.** exp: judged over 3 samples against c-t16 46.25

**D144.** exp: judged over 3 samples each, c-c3 against c-c4

**D145.** exp: judged over 3 samples each, c-c3 against c-c4

**D146.** exp: judged over 3 samples each, c-c3 against c-c4

**D147.** exp: judged over 3 samples each, c-c3 against c-c4

**D148.** exp: judged over 3 samples each, c-c3 against c-c4

**D149.** exp: judged over 3 samples against c-c3 46.40

**D150.** c-c3 is the chosen source, median K 46.40 over 4

**D151.** exp: judged over 3 samples against c-c3

**D152.** exp: judged over 3 samples against c-c3

**D153.** exp: judged over 3 samples against c-c3

**D154.** exp: judged over 3 samples against c-c3

**D155.** exp: judged over 3 samples against c64f 46.87

**D156.** exp: judged over 3 samples against c64f 46.87

**D157.** exp: judged over 3 samples against c64f 46.87

**D158.** exp: judged over 3 samples against c64f 46.87

**D159.** exp: local T +5 to 10 percent, so the board decides

**D160.** c64f is the chosen source

**D161.** exp: judged over 3 samples against c64f 46.64

**D162.** exp: judged over 3 samples against c64f 46.64

**D163.** exp: judged over 3 samples against c64f 46.64

**D164.** exp: judged over 3 samples against c64f 46.64

**D165.** exp: judged over 3 samples against c64f 46.64

**D166.** exp: judged over 3 samples against c64f 46.64

**D167.** exp: judged over 3 samples against c64f 46.64

**D168.** exp: judged over 3 samples against c-ph1 46.91

**D169.** exp: judged over 3 samples against c-ph1 46.91

**D170.** exp: judged over 3 samples against c-ph1 46.91

**D171.** exp: judged over 3 samples against c-ph1 46.91

**D172.** exp: judged over 3 samples against c-ph1 46.91

**D173.** exp: judged over 3 samples against c-ph1 46.91

**D174.** exp: judged over 3 samples against c-kp2 46.85

**D175.** exp: judged over 3 samples against c-kp2 46.85

**D176.** exp: judged over 3 samples against c-kp2 46.85

**D177.** exp: judged over 3 samples against c-kp2 46.85

**D178.** exp: judged over 3 samples against c-kp2 46.85

**D179.** exp: judged over 3 samples against c-kp2 46.85

**D180.** exp: judged over 3 samples against c-kp2 46.84

**D181.** exp: judged over 3 samples against c-kp2 46.84

**D182.** exp: judged over 3 samples against c-kp2 46.84

**D183.** exp: judged over 3 samples against c-kp2 46.84

**D184.** exp: judged over 3 samples against c-kp2 46.84

**D185.** exp: judged over 3 samples against c-kp2 46.84

**D186.** exp: judged over 3 samples against c-kp2 46.84

**D187.** exp: judged over 3 samples against c-kp2 46.84, c-map 47.01, c-scout 47.02

**D188.** exp: judged over 3 samples against c-kp2 46.84, c-map 47.01, c-scout 47.02

**D189.** exp: judged over 3 samples against c-kp2 46.84, c-map 47.01, c-scout 47.02

## What didn't work

| Experiment | What we expected | What we measured | Where | Kept? |
|---|---|---|---|---|
| Counting distinct names in a second phase, one partition per thread, with no shared writes per record | Nearly every record sets a new country bit in a shared cache line, and the box's cross-core traffic is slow, so T should fall a lot | T 0.055 -> 0.059 s, M 14.2 -> 37.4 MB, K 24.35 -> 19.08. Shared bits weren't the bottleneck | run-be17f3a6a2e0 | reverted |
| PREFETCHW for the name table's slot line | Most records write a line another core owns | ab.sh -3.6%, inside 16% noise, for S +5; the same idea did nothing on Track D | local | not applied |
| Parsing `regions.csv` on several threads, with atomic table entries, a lock for new codes, and the table sized from the last line | The serial parse is about 4% of the run, about +2% K for about 12 lines | Regions phase, 41 paired runs: 1,193 -> 949 us at 12 threads, 1,194 -> 609 us at 4, 1,215 -> 598 us at 6; thread spawns eat the rest. Whole run unchanged (31,770 -> 31,946 us at 4 threads, 32,298 -> 32,086 at 6). S 339 -> 356 costs 0.74% K, and the phase's best case gives back +0.9%. Scorer, 3 runs each, median K 24.02 -> 24.24, with a range of 3% per side | local | reverted |
| Parallel regions parser on current code: atomic byte table, code lock and region blocks | Cut the serial stage by parsing disjoint sorted-id ranges in parallel | Quick 59/59, thread sweep 324/324, constant-hash 29/29; counts 393.2 -> 393.0 instructions/record, 43.9 -> 43.5 branches, 1.53 -> 1.85 misses. 41-pair full A/B was +3.5% (20.7% spread); three-run scorer K 26.63 -> 26.63 while S rose 315 -> 369 | local scratch | reverted |
| Serial region-table preallocation from the sorted last id | Avoid repeated vector growth while parsing regions | Correctness gates pass; counts 393.2 -> 392.6 instructions/record, 43.9 -> 43.5 branches, 1.53 -> 1.60 misses. 41-pair A/B +0.3% inside 27.9% spread. Matched scorer pairs disagreed: K 26.41 -> 27.34, then 26.99 -> 26.62; peak memory was not consistently lower and S rose 315 -> 326 | local scratch | reverted |
| A plain TZCNT loop in place of PEXT and GFNI for the position lists | 13 fewer lines, worth about 0.5% of K, if the box ignored the extra instructions as it has others | 61 paired local runs: T +8.7%, about 4% of K. Every block runs it, so we didn't spend box runs on it | local | not applied |

## The final design

The file is cut into 24 segments per thread, which threads claim from a shared counter and read in 32 KiB pieces. Each piece is parsed in two stages: fixed 64-byte blocks list the separators (quote parity by carry-less multiply, positions by PEXT and one GFNI matrix multiply), then each record reads its fields from the list, in two passes over a chunk of 16 blocks. One shared name table keeps a cache line per name (32 bytes of key, one bit per country) behind vector tags over 32-slot windows, and names with no slot go to a locked set. All memory is one zeroed static, aligned for huge pages, that the main thread populates before any other thread starts, and the workers run on stacks in it. See the [README](README.md#approach). Local counts (counts.sh, on Zen 4, so only instruction counts carry over to the box): 227 instructions per record after `2bfc56f`, down from 258 (D69, D70), about 8 fewer once the first two loops merged (row 73), and 0.33 mispredicts per record when last counted (D58).

A separate review tried to break the code of steps 49 and 50: fresh seeds and hostile inputs against the reference (records straddling every position of a piece boundary, names over the table's capacity, records over 1 MiB, thread counts from 1 to 64) gave about 940 comparisons and 7,300 repeat runs with no wrong output, and found one crash on a spec-legal input with names over 32 bytes, since fixed (see the journey). The long-name path it crashed in was replaced later by the locked set (steps 75 to 81).

## How we know it's correct

- Every build is checked by SHA-256 against the naive reference on 15 generator seeds (tiny and sample) and 5 seeds at full scale, plus 20 answer-preserving rewrites of the input and 4 limit cases.
- A constant-hash test build, where every key collides and the table has only 32 homes, so that names run out of slots and take the locked-set path, gives the same answers.
- `quick.sh --threads "1 2 3 7 12 64"` on every build moves every cut, because segment sizes follow the thread count.
- **Determinism drill on the final source:** `quick.sh --repeat 5` pinned with `taskset -c 0-1`: 54/54 cells, each run five times. Then 12 threads on those same two CPUs, with two `yes` competing for them: 54/54.
- The spec's edge cases that needed care, and how each is handled: [README](README.md#correctness).
- **Reproduce it yourself:** `python3 tools/verify.py --tracks c` (Docker, git and Python only). Its report on commit `429da80`, run with `--full` on 2026-11-01 11:40 UTC against the organisers' harness at `adb9316`:

| Track | Generator seeds | Answer-preserving rewrites | Limit cases | 5 identical runs |
|---|---|---|---|---|
| C | 35/35 | 20/20 | 4/4 | yes |

## With another 24 hours

The box showed that line fills per record set T here, more than instructions or the lock, so that's where we'd look.

- **Already tested, both lost (D91).** `T0` for the slot prefetch: K 41.66, 43.96, 43.73, median 43.73. Homes on multiples of 32, so that tag windows never straddle a line: K 43.18, 43.28, 41.48, median 43.18. The chosen source's median is 44.00 over 7 runs, and it has ranged from 42.71 to 44.95 with no change, so T0's 0.6% is a lack of gain rather than a proven loss. Aligned homes are about 1.9% down.
- **Lines.** S is worth about 1% of K per 28 lines, and it's still the largest part of what separates us from the leader. The last pass took out 47 by removing the hand-built mapping and its struct and by merging helpers. What's left that we know of trades T for lines, like the TZCNT loop under [What didn't work](#what-didnt-work).
- **A local machine like the box.** Local cycles on a Zen 4 ranked cache and lock changes wrongly, so each real answer took a board slot. A Golden Cove machine would have let us test far more of them.
