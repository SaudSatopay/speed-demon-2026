# Track C: Join

Aggregates per country, from a hostile 200 MB CSV joined against a region table. Rust, standard library only.

## Where everything is

| What | Where |
|---|---|
| Headline numbers and their board run IDs | [PERF.md](PERF.md), first lines |
| Why each decision was made, reverted ones included | [PERF.md](PERF.md#decision-log) |
| The hot loop | `src/main.rs`, function `aggregate` |
| How correctness was checked | [Correctness](#correctness), below |

## Limits

- The parser reads one byte at a time.
- It relies on two spec guarantees: a mid-file BOM only ever starts a record, and a CR only ever starts a CRLF. A file with no CRLF would still be right, but wouldn't get split across threads.
- The name table is 8.4 MB, which keeps M above the 7.3 MB where more savings stop counting.

## Approach

The region table becomes an array from region id to country index, with indices in byte order, so printing by index sorts the output. Threads claim segments of the mapped CSV from an atomic cursor, and cut them just after a CRLF, which is always a record boundary because the spec promises a CR appears nowhere else. A scalar parser written straight from section 2.1 of the spec walks each segment. It never unescapes anything: a quoted field's raw bytes, with its `""` pairs intact, encode its value one to one, and the value's length is the raw length minus the number of pairs. Each thread sums into its own rows. One lock-free table, shared by all threads, counts distinct names: each slot is one cache line, holding the name's first 24 bytes and one bit per country.

## Why it's fast

See [PERF.md](PERF.md) for run IDs.

1. **No strings.** A field is a slice of the mapping, and nothing is decoded or copied.
2. **Exact cents from the digits alone.** The fraction is always two digits, so an amount is read as one integer, ignoring the point.
3. **The file isn't read into memory.** Mapped dataset pages don't count toward the memory peak.
4. **One cache line per name lookup.** On seed 1, 99.99% of names fit inline in the table, so comparing the key needs no second memory access, and the country bits are in the same line.

## Ideas and where they came from

- Comparing raw bytes instead of unescaping: our own reading of section 2.1.
- Cutting on CRLF instead of counting quotes: our own reading of section 2.2. Ge et al. (SIGMOD 2019) describe the two-pass quote-counting method it replaces.
- A full-key compare on every hash hit: the 1BRC contest rejected entries that trusted hashes (Morling, 2024).

## Correctness

Each of the spec's 20 edge cases falls out of the field rules. An empty unquoted field and `""` both give empty raw bytes, so they're one name. `Betts` and `Betts ` differ in their raw bytes. A BOM is skipped only at a record start, so inside quotes it stays data. Money is printed as a sign plus an absolute value, so `-0.75` keeps its sign and zero never gets one. Every build is checked by SHA-256 against the naive reference on 15 generator seeds, 10 answer-preserving rewrites (line endings, BOMs, empty-field quoting) and 4 limit cases. A constant-hash test build, where every key collides, must give the same answers. Anyone can reproduce this with one command: `python3 tools/verify.py` (see [tools/README.md](../tools/README.md)).

## Dependencies

None. `mmap` is declared directly against glibc, which `std` already links.

## Build and run

- **Container (as scored):** `docker build -t track-c .`, then run it with `/data` mounted read-only. It writes exactly one file, `/out/aggregates.txt`.
- **Native (for profiling):** `cargo build --release`, then `./target/release/join <data dir> <out dir>`.

## AI use

Written with AI assistance (Claude Code). Every decision and number in PERF.md was checked, and can be explained, by the author.
