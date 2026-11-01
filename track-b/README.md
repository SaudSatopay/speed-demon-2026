# Track B: Index

Boolean and phrase queries over a 254 MB corpus. Rust, standard library only.

## Where everything is

| What | Where |
|---|---|
| Headline numbers and their board run IDs | [PERF.md](PERF.md), first lines |
| What each phase cost on the box, on an earlier build | [PERF.md](PERF.md#where-it-loses) |
| Why each decision was made, reverted ones included | [PERF.md](PERF.md#decision-log) |
| The hot loop | `src/main.rs`, function `scan_and_answer`, and in it `look_up`, `mark`, `check_bools` and `answer_block` |
| How correctness was checked | [Correctness](#correctness), below |

## Limits

- Sums of ids are kept in 40 bits per tally, so they hold while N(N-1)/2 is below 2^40, which is up to 1,482,910 documents. Past that a sum spills into the count, and nothing checks for it. The spec's full scale has 250,000 documents, whose sums stay under 2^35.
- It relies on the spec's document ids, which are consecutive from 0 in the file's order, and on its bounds of 6 terms in a BOOL query and 8 in a phrase. A phrase of 9 terms could never match.
- A wrong guess from the 1% sample can cost time, never a wrong answer.
- S is 1,038 code lines against the reference's 91. Each feature has been measured again since it went in, and each still pays for its lines, but it's the most code of any of our tracks.

## Approach

The queries are read first, so only their 75,000 or so terms are ever looked up. No posting is stored: one scan of the corpus answers every query as it goes. Every document matching a BOOL query contains one of a few anchor terms: the rarest AND of its last run, or else each OR term. A sample of about 1% of the corpus says which terms are common. For most queries the anchors are rare, and at an anchor's first occurrence in a document, marks of what the document holds index a truth table. A query whose anchors hold about N/32 documents in all goes by blocks instead: for each of its terms, a thread keeps one bit per document of a block of up to 255 documents, and when the block is full, runs the query over these columns in vectors, 256 documents at a time. A phrase is anchored on its rarest term, and checked around each occurrence of it when its document ends.

Threads claim ranges of the corpus in order, and scan a document at a time in stages: find the tokens, look them all up, mark them, then act on what the marks listed. A lookup takes one cache line and has no branch on what it finds; the few words it can't be sure of get a second look. Then the same threads add up what they counted, and print all the answers in order.

## Why it's fast

Run IDs and the full story are in [PERF.md](PERF.md).

1. **Only the queries' terms are indexed.** The reference builds a set for each of 609,833 terms. Here a token that no query uses costs one lookup and nothing else.
2. **No posting is stored.** Queries with rare anchors are answered at those anchors, and the rest by blocks of documents, over bits that a thread reuses for each block. That took M from 42 to 27 MB locally, which scored nothing, as M was already under the 44.0 MB where more savings stop counting. The changes after it, which cut the bytes a thread drags through L2 per document and per block, took T from 0.0996 to 0.0787 on the board.
3. **Stages per document, with no branch per token.** Token edges come from 64-byte masks and a table per byte; keys and hashes of four tokens at a time in AVX2; every token's set prefetched before any is compared, so cache misses overlap. Going from a token at a time to this took T from 0.308 to 0.211 s, then with fewer instructions per token to 0.100 s, and without postings and with stamps to 0.078 s.
4. **Truth tables instead of formulas.** A BOOL check packs the marks of up to five other terms into a number, whose bit in a 32-bit table says whether the document counts. Three terms take 8 bytes, which covers 27,994 of the 31,615 checks on seed 1. A check itself has no branch on the data; only a tally's first ten hits take a branch, to the smallest ids.
5. **Marks that never need clearing.** A mark is a byte equal to the document's number in its block, so a new document sees none of the last one's, and a block's end clears them all.
6. **Phrases check only around their rarest term.** No positions are stored, and most tokens trigger no check.
7. **A count, a sum and the smallest ids come straight from the scan.** The reference sorts every result set. Block queries use nibble tables in AVX2 for counts and sums of positions in one pass.

## Ideas and where they came from

- Indexing the queries and streaming the documents: the SIGMOD 2013 programming contest.
- Finding a rare literal first, then verifying: Hyperscan.
- Popcounts by nibble table: Mula, Kurz and Lemire (2018). A sum of ids from such tables: our own derivation.
- Transposing rows of bits by swapping squares: *Hacker's Delight* (Warren).
- A check's marks packed into a number that indexes one truth table: our own, from how `Check`s came out of the anchors.
- Timing a phase on a machine we can't profile, by running that phase twice in a labelled `exp:` push: our own.

## Correctness

The tokenizer works on bytes through range compares, so every byte of 0x80 or above is a separator, even inside a letter, and `Zürich` is `z` and `rich`. A token no query uses still has a place in the document's list, so a phrase can't match across it. Each phrase is anchored on one term, at its position in the phrase, so every match is found at that term, and a document counts once however often the anchor occurs. BOOL queries apply each operator to the running result, strictly in order: the truth tables are built by running the query's own steps over all combinations of its terms, and a block query runs those steps over its columns. Any choice of anchors or storage gives the same answers: only the sample's guess of what's common changes, and that changes only the cost.

Every build is checked by SHA-256 against the naive reference on 15 generator seeds, 4 answer-preserving rewrites and 4 limit cases, at 1, 2, 3, 7, 12 and 64 threads. A constant-hash test build, where every key collides, must give the same answers. Anyone can reproduce the grid at the default thread count with one command: `python3 tools/verify.py` (see [tools/README.md](../tools/README.md)); for the sweep, run the image with `SD_THREADS` set.

## Dependencies

None. `mmap`, `madvise` and `mallopt` are declared directly against glibc, which `std` already links, and the vector code comes from `std::arch`.

## Build and run

- **Container (as scored):** `docker build -t track-b .`, then run it with `/data` mounted read-only. It writes exactly one file, `/out/results.txt`.
- **Native (for profiling):** `cargo build --release`, then `./target/release/index <data dir> <out dir>`.

## AI use

Written with AI assistance (Claude Code). Every decision and number in PERF.md was checked, and can be explained, by the author.
