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
- It also relies on the file's last record ending in its CRLF terminator (section 2.2). A file whose last record had none would lose that record, and `quick.sh` has no such file, so it wouldn't catch it.
- Positions in a part fit 32 bits, so a single record can't reach 4 GB, nor can a part: at 12 per thread, that is a file far past 200 MB.
- There are stacks for 64 threads besides the main one, so `SD_THREADS` above 65 stops with a panic.
- A name longer than 32 bytes, or one that finds no free slot in 64 windows, is counted in a locked set instead of the table. That is right but slower, and no generated name has been seen beyond 25 bytes.
- The table, tags, join table and worker stacks fill four huge pages, 8.4 MB, which keeps M above the 7.3 MB where more savings stop counting. The 104,724 homes are the most that fit in them.
- The code calls PEXT, BZHI, the carry-less multiply, PREFETCHW and GFNI directly, so it only runs on a CPU that has them, as the scoring box's does. `.cargo/config.toml` sets the build to match.

## Approach

Threads claim parts of the file from a shared counter, 12 per thread, and each part is cut just after a CRLF, which is always a record boundary because the spec promises a CR appears nowhere else. The file is mapped, starting on a 2 MiB boundary, and each thread parses its part where it lies, with no copy. Inside a part there are two stages. The first walks fixed 64-byte blocks: a carry-less multiply of the quote mask gives the parity of quotes at every byte, which tells inside from outside, and every comma and line end outside quotes goes on a list. Positions are written eight at a time: six PEXTs gather the bits of the positions of a block's separators, and one GFNI matrix multiply turns those bits into eight position bytes. The second stage reads each record from its five listed positions, in two passes over a chunk of 16 blocks. The first pass parses the id, region and amount in one vector, prefetches the join table entry, hashes the name, then guesses its slot from the tags and prefetches that line for writing. The second adds to the country's row, counts quotes, checks the guess with one vector compare, and sets the name's bit for the country.

The region table becomes an array from region id to country index, with indices given out as codes first appear; the threads fill it together before any record is parsed, and the output is sorted when it's written. Nothing is unescaped: a quoted field's raw bytes, with its `""` pairs intact, encode its value one to one, and the value's length follows from counting its quotes. One lock-free table, shared by all threads, counts distinct names. A slot is one cache line: the name, up to 32 bytes, padded with ones, and one bit per country. A name's home is one of 104,724 slots, and each slot has a 16-bit tag, zero while it's free and otherwise 16 bits of its name's hash. The 64 bytes of tags from a home cover its 32 slots, so two vector compares say both whether the name is there and which slot is free. A name takes the first free slot in its window. Only when all 32 are full does it move on, by a distance its tag sets, so that names that spill don't fill the windows next door, and it gives up after 64 windows. A name longer than 32 bytes, or one that gives up, gets no slot and goes to a locked set instead, so the table never restarts.

All the memory is one static: slots, tags, join table, the table of country codes and the workers' stacks. It's zeros in the program's image, and its 2 MiB alignment gets it huge pages. The main thread populates it with one `madvise` call before any other thread starts, covering whole huge pages only, and touches one page in every 2 MiB of the mapped files so that it makes their page tables. It isn't pinned to a CPU. The workers run on stacks in that static, through raw `pthread_create`, so on a generated file at 12 threads they take no page faults on their own memory and never use the heap. They do fault on the file's pages; mapping it on a 2 MiB boundary lets the kernel map them in large pieces, which cut page faults from 3,094 to 651 locally, though T on the box didn't move. (A name with no slot allocates a copy of itself, which is slower, not wrong.) When the parts run out, the threads release the records file's pages together with `MADV_DONTNEED`, in shares of whole 2 MiB, because the exit would otherwise release them all on one thread, on the clock. The run ends in `_exit`, because the handlers that `exit` runs write the first byte of the zeroed part of the image, which would cost a whole huge page.

## Why it's fast

See [PERF.md](PERF.md) for run IDs.

1. **No chain from record to record.** A parser that finds each field from the end of the last is limited by that chain of loads, about 75 cycles per record here, not by its instruction count. Listing separators from fixed blocks breaks it.
2. **No branch for most of the hostile parts.** Quote parity comes from arithmetic, so `""` pairs, commas in quotes and LFs in quotes need no branch. Mispredicts were 0.33 per record when last counted (local, PERF.md D58).
3. **Three numbers in one vector,** and an amount's point skipped by its weights: a saturating subtract turns the sign and the point into zero digits.
4. **A mapped file, and its pages released together.** A copy of the file would take as long as much of the parse, so the threads read it where it lies. When they're done they give its pages back between them: without that, T was 0.0228 on the board against 0.0192, because the exit released the pages on one thread (PERF.md, journey steps 119 and 120). Fine parts, 12 per thread, keep the threads that finish first from sitting idle long.
5. **One cache line per name lookup,** guessed from the tags and prefetched for ownership before the locked write. A single `LOCK BTS` finds the country's bit. The tags are 16 bits because 8-bit ones collided often enough to send about 4% of records to the slow path (PERF.md, steps 107 and 108). They aren't prefetched: a separate pass that did bought nothing on the box (PERF.md, "What the box taught us").
6. **No page charged from a worker's CPU.** `memory.peak` counts each CPU's unused charge batch, up to 252 KB per CPU that charges anything, so this took M from 12.9 to 10.6 MB on the board (journey steps 24 and 25).
7. **No std runtime before `main`,** which saves about 15 system calls that the timer counts.

## Ideas and where they came from

- Comparing raw bytes instead of unescaping: our own reading of section 2.1.
- Carry-less multiply for quote parity and block-at-a-time structure: simdjson (Langdale and Lemire, 2019).
- Parallel digit parsing with `maddubs` and `madd`: Lemire's write-ups of Mula's method.
- Cutting on CRLF instead of counting quotes: our own reading of section 2.2. Ge et al. (SIGMOD 2019) describe the two-pass quote-counting method it replaces.
- A full-key compare on every hash hit: the 1BRC contest rejected entries that trusted hashes (Morling, 2024).
- Memory charge batches: found with a kernel trace of `try_charge_memcg`, then checked in the kernel's memory-group code.
- Releasing the mapped pages from the worker threads: a finding from Track A, where the exit's release of a mapping was worth 2.7 ms (PERF.md, step 95).

## Correctness

Each of the spec's 20 edge cases falls out of the field rules. An empty unquoted field and `""` both give empty raw bytes, so they're one name. `Betts` and `Betts ` differ in their raw bytes. A BOM is skipped only at a record start, so inside quotes it stays data. Quote parity runs from a record boundary, so a comma or LF in quotes never reaches the list. Money is printed as a sign plus an absolute value, so `-0.75` keeps its sign and zero never gets one. Every build is checked by SHA-256 against the naive reference on 15 generator seeds, 20 answer-preserving rewrites (line endings, BOMs, empty-field quoting) and 4 limit cases. A constant-hash test build, where every key collides and the table has only 32 homes, so that names run out of slots and take the locked-set path, must give the same answers. The one thing none of these can see is a last record with no terminator, which the spec rules out (see [Limits](#limits)). Anyone can reproduce this with one command: `python3 tools/verify.py` (see [tools/README.md](../tools/README.md)).

## Dependencies

None. `mmap`, `madvise`, `_exit` and the `pthread` calls are declared directly against glibc, which `std` already links, and the vector operations come from `std::arch`.

## Build and run

- **Container (as scored):** `docker build -t track-c .`, then run it with `/data` mounted read-only. It writes exactly one file, `/out/aggregates.txt`.
- **Native (for profiling):** `cargo build --release`, then `./target/release/join <data dir> <out dir>`.

## AI use

Written with AI assistance (Claude Code and Codex (GPT-6 Luna)). Every decision and number in PERF.md was checked, and can be explained, by the author.
