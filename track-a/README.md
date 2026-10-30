# Track A: Ingest

Per-hour, per-endpoint latency percentiles over a 3.1M-line HTTP request log. Rust, standard library only.

## Where everything is

| What | Where |
|---|---|
| Headline numbers and their board run IDs | [PERF.md](PERF.md), first lines |
| Why each decision was made, reverted ones included | [PERF.md](PERF.md#decision-log) |
| The hot loop | `src/main.rs`, function `aggregate` |
| How correctness was checked | [Correctness](#correctness), below |

## Limits

- It relies on two spec guarantees: every timestamp is 27 bytes inside the fixed 72-hour window, and every line is well formed. A line outside that window aborts the program instead of being counted wrongly.
- It's single-threaded for now.

## Approach

The log is memory-mapped and walked once. Each line is read at fixed offsets where the format allows it: the hour comes from four timestamp bytes, and the method's length from its 4th byte. An open-addressing table turns each endpoint into a dense id, always comparing the full key. Each (hour, endpoint) group keeps a growing list of latencies. At the end, the endpoints are sorted once as bytes, then each hour prints its groups in that order, sorting each group's list and reading the three percentiles from it. The output goes to the file in 64 KB pieces, so it never sits in memory as well.

## Why it's fast

See [PERF.md](PERF.md) for run IDs.

1. **No date parsing.** The reference spends about 77% of its time in `strptime`. Here the hour is a few bit operations on four bytes.
2. **No per-line objects.** A line costs no allocation, except when a latency list grows.
3. **The log isn't read into memory.** Mapped dataset pages don't count toward the memory peak.

## Ideas and where they came from

- Fixed-offset parsing: our own reading of the spec's grammar.
- A full-key compare on every hash hit: the 1BRC contest rejected entries that trusted hashes (Morling, 2024).

## Correctness

The spec's edge cases need no special code: duplicate lines count twice, a 499 isn't an error because only a first byte of `5` counts, the hour comes from the timestamp rather than the file's order, and an empty log gives an empty file. Every build is checked by SHA-256 against the naive reference on 15 generator seeds, 8 answer-preserving rewrites and 5 limit cases. A constant-hash test build, where every key collides, must give the same answers. Anyone can reproduce this with one command: `python3 tools/verify.py` (see [tools/README.md](../tools/README.md)).

## Dependencies

None. `mmap` is declared directly against glibc, which `std` already links.

## Build and run

- **Container (as scored):** `docker build -t track-a .`, then run it with `/data` mounted read-only. It writes exactly one file, `/out/summary.txt`.
- **Native (for profiling):** `cargo build --release`, then `./target/release/ingest <data dir> <out dir>`.

## AI use

Written with AI assistance (Claude Code). Every decision and number in PERF.md was checked, and can be explained, by the author.
