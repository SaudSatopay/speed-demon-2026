# Track A: Ingest

Per-hour, per-endpoint latency percentiles over a 3.1M-line HTTP request log. Rust, standard library only.

## Where everything is

| What | Where |
|---|---|
| Headline numbers and their board run IDs | [PERF.md](PERF.md), first lines |
| What each phase costs on the box | [PERF.md](PERF.md#where-it-loses) |
| Why each decision was made, reverted ones included | [PERF.md](PERF.md#decision-log) |
| The hot loop | `src/main.rs`, function `parse` |
| Finishing an hour | `src/main.rs`, functions `write_hour` and `format_hour` |
| How correctness was checked | [Correctness](#correctness), below |

## Limits

- It relies on three spec guarantees: every timestamp is 27 bytes inside the fixed 72-hour window, every line is well formed, and a seed has 1,842 endpoints. A line outside the window either aborts, when its hour falls outside the 72, or is counted in a wrong hour, as the hour comes only from the bits that differ inside the window. An endpoint table past 4,094 lines of keys aborts.
- The spec promises no bound on how late a line can come. Hours are claimed in order, each once the reading is 1/48 of the log past its last line so far. A line for an hour already claimed stops the run and starts it again with no early finish, which about doubles T. The worst disorder on our seeds is 1/315 of the log, and 280 full runs of an earlier build (PERF.md, D40) saw no restart.
- Readers never wait for a thread that falls behind, so on a busy machine records pile up: M rises by about 2 MB, while T stays about 1.5 times better than with a wait.
- Threads that lose a race to publish a key leave its lines unused. A hand-built log of 1,842 keys of two lines each, in the same order on every thread, can use up the ids and abort. The generator can't make one.
- T has a slow mode we can't fully explain: on identical code, some board runs read about 0.018 against 0.0158. See [PERF.md](PERF.md#where-it-loses).
- S is 343 code lines, against the reference's 56.

## Approach

The log is cut into segments that 12 threads claim from an atomic counter, and each thread parses its segment straight from the mapped log. Each line is read at fixed offsets where the format allows: the hour comes from one 8-byte load, and the method's length from its 4th byte. One 64-byte block finds the end of the endpoint, and one vector parses the latency. One lock-free table turns endpoints into ids, always comparing the whole key. Each line becomes one 32-bit record in a shared array for its hour, where each thread reserves a page at a time.

The log runs in rough time order, so hours don't wait for the end of the log. Once the reading is 1/48 of the log past an hour, one of four threads claims it, sorts its records with three stable radix passes, and writes its text over the records. Whichever of the four can take the lock appends the ready hours to the file in order, and gives their pages back. So few records are in memory at once, text never sits in memory for long, and all 12 threads stay busy until the reading ends, when the four finish what's left.

The log is mapped read-only over anonymous zero pages, which pad its end, and the parse prefetches the log 1,536 bytes ahead of each line. The timer's clock runs to exit, and exit takes a mapping's pages down one by one, so the threads that are done share that work first, releasing the log's pages with `MADV_DONTNEED`, 8 MiB at a time. Memory is the hours' record arrays, the key table, and the finishers' scratch. The log's own pages are not charged to M, as `/data` is warmed outside the run's cgroup.

## Why it's fast

Run IDs and the full story are in [PERF.md](PERF.md).

1. **No date parsing and no objects per line.** The reference spends about 77% of its time in `strptime`, and 228 MB on Python ints. Here the hour is one multiply of an 8-byte load, and a line is one 32-bit integer whose plain order is the order the percentiles need.
2. **Mapping the log, with a prefetch per line and a shared release.** The parse reads the log's pages in place, with no copy into a buffer. Mapping lost to reading in pieces three times, because the clock runs to exit and exit took the log's pages down on one thread. Threads that finish parsing now release them together. Most board runs of the final build read T 0.0157 to 0.0161, against 0.0185 to 0.0188 for the pread build it replaced. Some still read about 0.018 on identical code, a slow mode that shrank but didn't clearly go when the main thread became a worker. See D68, D74 and D75 in [PERF.md](PERF.md#decision-log).
3. **Hours finished during the read.** Memory peaks when records and text are both held. Finishing an hour as soon as the reading is past it took M from 18.8 to 16.4 MB, and T from 0.0245 to 0.0221, because no thread sits waiting for the output stage.
4. **Almost no slow paths per line.** Keys sit in whole 64-byte lines, so one block compare settles 92% of lines, with no branch on the length of a key that fits. Before this, 13.7% of lines took the slow path; now 8.4% do.
5. **Three linear passes per hour.** A record holds an endpoint id, a latency and an error flag. Two radix passes over the latency and one over the ids, in the endpoints' byte order, leave each endpoint's latencies sorted, with no comparison sort except for the side list of big latencies, below. Only latencies of 524,287 or more, about 3% of lines, take a side list, in the far end of the same array.
6. **Text written over its own records.** The hour's array has room for its text, so an hour's text takes few new pages: on seed 1 it runs past its records in 30 of 72 hours, by 63 KB at most.

## Ideas and where they came from

- Fixed-offset parsing, and the hour from masked bytes: our own reading of the spec's grammar.
- A full-key compare on every hit: the 1BRC contest rejected entries that trusted hashes (Morling, 2024).
- Parsing 8 digits at once: Lemire (2022). Printing 8 digits at once: Khuong (2017).
- Reading in small pieces instead of mapping (our build until D68): found on our Track C, by trying it on the board after local timing couldn't tell.
- Sharing the release of the mapped log's pages among the threads that are done, before exit: our own, from the clock running to exit.
- Finishing hours during the read, and restarting if a line comes too late: our own, from the eight shards the spec describes.
- Timing a phase on a machine we can't profile, by running that phase twice in a labelled `exp:` push: our own.

## Correctness

The spec's edge cases need no special code: duplicate lines count twice, a 499 isn't an error because only a first byte of `5` counts, the hour comes from the timestamp rather than the file's order, and an empty log gives an empty file. Lines that come late for their hour are the case that does need code: a thread records where it last came to each hour under a lock, an hour isn't claimed until every reader is past that by the margin, and a line for an hour already claimed stops the round.

Every build is checked by SHA-256 against the naive reference on 15 generator seeds, 4 answer-preserving rewrites and 5 limit cases. The final build also passed 130,233 repeated runs (seed 20261103) with none wrong. An earlier mapped build (D68) passed address sanitizer with no reports, and thread sanitizer with the right hashes and reports we believe are false alarms but haven't all examined. An adversarial review found no wrong answer on input the spec allows. The tiny and sample seeds, the rewrites and the limit cases run at 1, 2, 3, 7, 12 and 64 threads. A constant-hash test build, where every key collides, must give the same answers. On top of those: repeated runs on 2 to 14 threads, since a lock-free table's races don't show up on a single pass, and logs in other orders: shuffled, reversed, rotated, and with lines moved just past the margin, which pass with no restart. Four more kinds restart on nearly every run: 2% chunks in random order, the first and last lines swapped, and all of one endpoint's lines moved to the end, for a rare key and for the longest. Anyone can reproduce the main check with one command: `python3 tools/verify.py` (see [tools/README.md](../tools/README.md)).

## Dependencies

None. `mmap`, `madvise` and `prctl` are declared directly against glibc, which `std` already links, and the vector code comes from `std::arch`.

## Build and run

- **Container (as scored):** `docker build -t track-a .`, then run it with `/data` mounted read-only. It writes exactly one file, `/out/summary.txt`.
- **Native (for profiling):** `cargo build --release`, then `./target/release/ingest <data dir> <out dir>`.

## AI use

Written with AI assistance (Claude Code and Codex (GPT-6 Luna)). Every decision and number in PERF.md was checked, and can be explained, by the author.
