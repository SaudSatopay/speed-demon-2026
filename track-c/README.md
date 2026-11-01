# Track C: Join

Aggregates per country, from a hostile 200 MB CSV joined against a region table. Rust, standard library only.

## Where everything is

| What | Where |
|---|---|
| Headline numbers and their board run IDs | [PERF.md](PERF.md), first lines |
| Why each decision was made, reverted ones included | [PERF.md](PERF.md#decision-log) |
| What the scoring box taught us, with run IDs | [PERF.md](PERF.md#what-the-box-taught-us) |
| The hot loop | `src/main.rs`, functions `separators`, `parse_records` and `add_records` |
| How correctness was checked | [Correctness](#correctness), below |

## Limits

- It relies on sections 2.2 and 2.3 of the spec: a mid-file BOM only ever starts a record, a CR only ever starts a CRLF, numbers fit in 8 bytes, and there are at most 240 countries. A file with no CRLF would still be right, but wouldn't get split across threads. Names may hold any bytes but zero.
- Positions in a piece fit 32 bits, so a single record can't reach 4 GB.
- A name longer than 32 bytes, or one that finds no free slot, is counted in a locked set instead of the table. That is right but slower, and no generated name has been seen beyond 25 bytes.
- The table, tags and join table are 8.2 MB, which keeps M above the 7.3 MB where more savings stop counting.
- The code calls PEXT, BZHI, the carry-less multiply, PREFETCHW and GFNI directly, so it only runs on a CPU that has them, as the scoring box's does. `.cargo/config.toml` sets the build to match.

## Approach

Threads claim segments of the file from a shared counter, 24 segments per thread, and each segment is cut just after a CRLF, which is always a record boundary because the spec promises a CR appears nowhere else. A thread reads its segment in 32 KiB pieces into its own buffer. Inside a piece there are two stages. The first walks fixed 64-byte blocks: a carry-less multiply of the quote mask gives the parity of quotes at every byte, which tells inside from outside, and every comma and line end outside quotes goes on a list. Positions are written eight at a time: six PEXTs gather the bits of the positions of a block's separators, and one GFNI matrix multiply turns those bits into eight position bytes. The second stage reads each record from its five listed positions, in two passes over a chunk of 16 blocks. The first pass parses the id, region and amount in one vector, prefetches the join table entry, hashes the name, then guesses its slot from the tags and prefetches that line for writing. The second adds to the country's row, counts quotes, checks the guess with one vector compare, and sets the name's bit for the country.

The region table becomes an array from region id to country index, with indices given out as codes first appear; the threads fill it together before any record is parsed, and the output is sorted when it's written. Nothing is unescaped: a quoted field's raw bytes, with its `""` pairs intact, encode its value one to one, and the value's length follows from counting its quotes. One lock-free table, shared by all threads, counts distinct names. A slot is one cache line: the name, up to 32 bytes, padded with ones, and one bit per country. A name's home is one of 110,000 slots, and one 32-byte vector of tags covers the 32 slots from there, so a single load says both whether the name is there and which slot is free; a full window sends the search on by 32. A name longer than 32 bytes, or one that finds every window to the end of the table full, gets no slot and goes to a locked set instead, so the table never restarts.

All the memory is one mapping: slots, tags, join table and the workers' stacks. The main thread populates it with one `madvise` call before any other thread starts, and isn't pinned to a CPU. The workers run on stacks in that mapping, through raw `pthread_create`, so on a generated file at 12 threads they take no page faults and never use the heap. (Fewer threads make the parts of `regions.csv` bigger than the read buffer, and a name with no slot or a record longer than a piece also allocates: slower, not wrong.)

## Why it's fast

See [PERF.md](PERF.md) for run IDs.

1. **No chain from record to record.** A parser that finds each field from the end of the last is limited by that chain of loads, about 75 cycles per record here, not by its instruction count. Listing separators from fixed blocks breaks it.
2. **No branch for most of the hostile parts.** Quote parity comes from arithmetic, so `""` pairs, commas in quotes and LFs in quotes need no branch. Mispredicts were 0.33 per record when last counted (local, PERF.md D58).
3. **Three numbers in one vector,** and an amount's point skipped by its weights: a saturating subtract turns the sign and the point into zero digits.
4. **Reads instead of a mapping.** Each thread's buffer stays in the caches near its core, and there are no page faults or TLB misses on small file pages. 32 KiB is the measured best on the box: 16 KiB costs too many reads, and 64 KiB and 128 KiB were no faster and used more memory.
5. **One cache line per name lookup,** guessed from the tags and prefetched for ownership before the locked write. A single `LOCK BTS` finds the country's bit. The tags themselves aren't prefetched: a separate pass that did bought nothing on the box (PERF.md, "What the box taught us").
6. **No page charged from a worker's CPU.** `memory.peak` counts each CPU's unused charge batch, up to 252 KB per CPU that charges anything, so this took M from 12.9 to 10.6 MB on the board (journey steps 24 and 25).
7. **No std runtime before `main`,** which saves about 15 system calls that the timer counts.

## Ideas and where they came from

- Comparing raw bytes instead of unescaping: our own reading of section 2.1.
- Carry-less multiply for quote parity and block-at-a-time structure: simdjson (Langdale and Lemire, 2019).
- Parallel digit parsing with `maddubs` and `madd`: Lemire's write-ups of Mula's method.
- Cutting on CRLF instead of counting quotes: our own reading of section 2.2. Ge et al. (SIGMOD 2019) describe the two-pass quote-counting method it replaces.
- A full-key compare on every hash hit: the 1BRC contest rejected entries that trusted hashes (Morling, 2024).
- Memory charge batches: found with a kernel trace of `try_charge_memcg`, then checked in the kernel's memory-group code.

## Correctness

Each of the spec's 20 edge cases falls out of the field rules. An empty unquoted field and `""` both give empty raw bytes, so they're one name. `Betts` and `Betts ` differ in their raw bytes. A BOM is skipped only at a record start, so inside quotes it stays data. Quote parity runs from a record boundary, so a comma or LF in quotes never reaches the list. Money is printed as a sign plus an absolute value, so `-0.75` keeps its sign and zero never gets one. Every build is checked by SHA-256 against the naive reference on 15 generator seeds, 20 answer-preserving rewrites (line endings, BOMs, empty-field quoting) and 4 limit cases. A constant-hash test build, where every key collides and the table has only 32 homes, so that names run out of slots and take the locked-set path, must give the same answers. Anyone can reproduce this with one command: `python3 tools/verify.py` (see [tools/README.md](../tools/README.md)).

## Dependencies

None. `mmap`, `madvise` and the `pthread` calls are declared directly against glibc, which `std` already links, and the vector operations come from `std::arch`.

## Build and run

- **Container (as scored):** `docker build -t track-c .`, then run it with `/data` mounted read-only. It writes exactly one file, `/out/aggregates.txt`.
- **Native (for profiling):** `cargo build --release`, then `./target/release/join <data dir> <out dir>`.

## AI use

Written with AI assistance (Claude Code and Codex (GPT-6 Luna)). Every decision and number in PERF.md was checked, and can be explained, by the author.
