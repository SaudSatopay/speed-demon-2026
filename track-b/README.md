# Track B: Index

Boolean and phrase queries over a 254 MB corpus. Rust, standard library only.

## Where everything is

| What | Where |
|---|---|
| Headline numbers and their board run IDs | [PERF.md](PERF.md), first lines |
| Why each decision was made, reverted ones included | [PERF.md](PERF.md#decision-log) |
| The hot loops | `src/main.rs`, functions `scan_range` and `Evaluator::run` |
| How correctness was checked | [Correctness](#correctness), below |

## Limits

- Queries anchored on a common term still store postings, in bitmaps and chains, which is most of the memory left.
- Threads get equal byte ranges rather than claiming work, so one slow thread slows the whole scan.
- A wrong guess from the 1% sample can cost memory or extra phrase checks, never a wrong answer.

## Approach

The queries are read first, so only their 75,000 or so terms are indexed. A sample of about 1% of the corpus decides which terms are common enough for a bitmap, and which term of each phrase is rarest. Threads then scan the corpus once, each over its own contiguous range, in 32-byte blocks. Every token is looked up in one cache access and buffered for its document. Common terms set bits directly, and other terms append 2-byte offsets to a chain of small blocks, with no second copy and no merge, because each thread's range follows document order. When a document ends, each occurrence of a phrase's rarest term checks the phrase around it. BOOL queries keep the spec's left-to-right meaning, but reorder each run of ANDs and NOTs, rarest first, which is safe because they all intersect. The result is a sorted list, which becomes a bitmap only when a union with a bitmap makes it large.

## Why it's fast

See [PERF.md](PERF.md) for run IDs.

1. **Only the queries' terms are indexed.** The reference builds a set for each of 609,833 terms.
2. **Phrases check only around their rarest term.** No positions are stored, and most tokens trigger no check.
3. **Most BOOL queries never store a posting.** At an anchor term's first occurrence in a document, each term's stamp gives the whole formula, with no branch.
4. **A count, a sum and the smallest ids come straight from the scan or a bitmap.** The reference sorts every result set.

## Ideas and where they came from

- Indexing the queries and streaming the documents: the SIGMOD 2013 programming contest.
- Finding a rare literal first, then verifying: Hyperscan.
- A sum of ids from popcounts: our own derivation.

## Correctness

The tokenizer works on bytes through a 256-entry table, so every byte of 0x80 or above is a separator, even inside a letter, and `Zürich` is `z` and `rich`. Non-query tokens stay in the document's buffer as a marker, so a phrase can't match across them. Each phrase is anchored on one term, at its position in the phrase, so every match is found at that term, and a document counts once however often the anchor occurs. BOOL queries apply each operator to the running result, strictly in order. Every build is checked by SHA-256 against the naive reference on 15 generator seeds, 8 answer-preserving rewrites and 4 limit cases. A constant-hash test build, where every key collides, must give the same answers. Anyone can reproduce this with one command: `python3 tools/verify.py` (see [tools/README.md](../tools/README.md)).

## Dependencies

None. `mmap` is declared directly against glibc, which `std` already links, and the AVX2 loads come from `std::arch`.

## Build and run

- **Container (as scored):** `docker build -t track-b .`, then run it with `/data` mounted read-only. It writes exactly one file, `/out/results.txt`.
- **Native (for profiling):** `cargo build --release`, then `./target/release/index <data dir> <out dir>`.

## AI use

Written with AI assistance (Claude Code). Every decision and number in PERF.md was checked, and can be explained, by the author.
