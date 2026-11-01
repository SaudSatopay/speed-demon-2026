# Track B: Index

Boolean and phrase queries over a 254 MB corpus. Rust, standard library only.

## Where everything is

| What | Where |
|---|---|
| Headline numbers and their board run IDs | [PERF.md](PERF.md), first lines |
| What each phase costs on the box | [PERF.md](PERF.md#where-it-loses) |
| Why each decision was made, reverted ones included | [PERF.md](PERF.md#decision-log) |
| The hot loop | `src/main.rs`, function `scan_ranges`, and in it `look_up`, `mark` and `check_bools` |
| How correctness was checked | [Correctness](#correctness), below |

## Limits

- A query anchored on a common term still stores postings, in bitmaps and chains, which is the largest part of the memory left, about 16 of 42 MB.
- It relies on the spec's document ids, which are consecutive from 0 in the file's order and fit its bound. Past 1,048,512 documents it stops with an assert, as the sums of ids need the room.
- A wrong guess from the 1% sample can cost memory or extra phrase checks, never a wrong answer.
- S is 1,015 code lines against the reference's 91. Each feature has been measured again since it went in, and each still pays for its lines, but it's the most code of any of our tracks.

## Approach

The queries are read first, so only their 75,000 or so terms are ever looked up. Every document matching a BOOL query contains one of a few anchor terms: the rarest AND of its last run, or else each OR term. For nine queries in ten those anchors are rare, so the query is answered during the scan: at an anchor's first occurrence in a document, marks of what the document holds index a truth table, and no posting is stored. Only queries anchored on a common term read postings afterwards, from bitmaps for their common terms and chains of 2-byte offsets for the others. A sample of about 1% of the corpus decides which terms are common, and which term of each phrase is rarest.

Threads claim ranges of the corpus in order, and scan a document at a time in stages: find the tokens, look them all up, mark them, then act on what the marks listed. A lookup takes one cache line and has no branch on what it finds; the few words it can't be sure of get a second look. When a document ends, each occurrence of a phrase's rarest term checks the phrase around it. Then the same threads answer the queries that read postings, and print all the answers in order.

## Why it's fast

Run IDs and the full story are in [PERF.md](PERF.md).

1. **Only the queries' terms are indexed.** The reference builds a set for each of 609,833 terms. Here a token that no query uses costs one lookup and nothing else.
2. **Most BOOL queries never store a posting.** 29,194 of 31,000 are answered in the scan. That took M from 73 to 41 MB locally, under the 44 MB where more savings stop counting.
3. **Stages per document, with no branch per token.** Token edges come from 64-byte masks and a table per byte; keys and hashes of four tokens at a time in AVX2; every token's set prefetched before any is compared, so cache misses overlap. Going from a token at a time to this took T from 0.308 to 0.211 s, then with fewer instructions per token to 0.100 s.
4. **Truth tables instead of formulas.** A BOOL check packs the marks of up to five other terms into a number, whose bit in a 32-bit table says whether the document counts. Three terms take 16 bytes, which covers nine checks in ten. A check itself has no branch on the data; only a tally's first ten hits take a branch, to the smallest ids.
5. **Phrases check only around their rarest term.** No positions are stored, and most tokens trigger no check.
6. **A count, a sum and the smallest ids come straight from the scan or a bitmap.** The reference sorts every result set. Bitmap answers use nibble tables in AVX2 for counts and sums of positions in one pass.

## Ideas and where they came from

- Indexing the queries and streaming the documents: the SIGMOD 2013 programming contest.
- Finding a rare literal first, then verifying: Hyperscan.
- Popcounts by nibble table: Mula, Kurz and Lemire (2018). A sum of ids from such tables: our own derivation.
- Transposing rows of bits by swapping squares: *Hacker's Delight* (Warren).
- A check's marks packed into a number that indexes one truth table: our own, from how `Check`s came out of the anchors.
- Timing a phase on a machine we can't profile, by running that phase twice in a labelled `exp:` push: our own.

## Correctness

The tokenizer works on bytes through range compares, so every byte of 0x80 or above is a separator, even inside a letter, and `Zürich` is `z` and `rich`. A token no query uses still has a place in the document's list, so a phrase can't match across it. Each phrase is anchored on one term, at its position in the phrase, so every match is found at that term, and a document counts once however often the anchor occurs. BOOL queries apply each operator to the running result, strictly in order, as the truth tables are built by running the query's own steps over all combinations of its terms. Any choice of anchors or storage gives the same answers: only the sample's guess of what's common changes, and that changes only the cost.

Every build is checked by SHA-256 against the naive reference on 15 generator seeds, 4 answer-preserving rewrites and 4 limit cases, at 1, 2, 3, 7, 12 and 64 threads. A constant-hash test build, where every key collides, must give the same answers. Anyone can reproduce the grid at the default thread count with one command: `python3 tools/verify.py` (see [tools/README.md](../tools/README.md)); for the sweep, run the image with `SD_THREADS` set.

## Dependencies

None. `mmap` and `mallopt` are declared directly against glibc, which `std` already links, and the vector code comes from `std::arch`.

## Build and run

- **Container (as scored):** `docker build -t track-b .`, then run it with `/data` mounted read-only. It writes exactly one file, `/out/results.txt`.
- **Native (for profiling):** `cargo build --release`, then `./target/release/index <data dir> <out dir>`.

## AI use

Written with AI assistance (Claude Code). Every decision and number in PERF.md was checked, and can be explained, by the author.
