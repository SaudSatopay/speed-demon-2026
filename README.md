# ALL STAR · Speed Demon 2026

Four rewrites of the organisers' naive Python references, one per track. Each is written in Rust with only the standard library (zero dependencies) and built offline into its own Docker image. A solo entry by Saud Satopay.

Each track's output is compared byte for byte (SHA-256) with the reference's by one command ([below](#check-our-claims)). It covers 171 datasets, plus five repeated runs of each track.

## The tracks

| Track | What it does | Reference on the box (T · M · S) | Details |
|---|---|---|---|
| A · Ingest | Per-hour, per-endpoint latency percentiles over a 3.1M-line request log | 32.05 s · 228 MB · 56 | [README](track-a/README.md) · [PERF.md](track-a/PERF.md) |
| B · Index | Boolean and phrase queries over a 254 MB corpus of 250,000 documents | 56.35 s · 2,200 MB · 91 | [README](track-b/README.md) · [PERF.md](track-b/PERF.md) |
| C · Join | A hostile 200 MB CSV joined to a region table and summed in integer cents | 14.44 s · 367 MB · 108 | [README](track-c/README.md) · [PERF.md](track-c/PERF.md) |
| D · Traverse | 2,000 shortest paths on a directed graph of 1.74M nodes and 5M edges | 72.69 s · 893 MB · 43 | [README](track-d/README.md) · [PERF.md](track-d/PERF.md) |

T is wall time, M peak memory and S the code lines the board counts, all as the board measures them. K and speed points are the board's own scores for a run; each PERF.md explains how M and S enter them.

## Board numbers

This is the only place in this README where numbers change. They come from the live board's feed at **2026-11-02 11:49:02 UTC (Mon 17:19 IST)** and the board's run history, and none is a local timing. Ranks are by K among the teams scored on that track at that time. Each row is the track's best scored run so far; the last column shows the latest run, to show how much one source moves between runs.

| Track | Best run | Commit | T (s) | M (MB) | S | K | Speed points | Rank by K | Latest run: K |
|---|---|---|---|---|---|---|---|---|---|
| A | `run-a3d80d9d43be` | `c438b24` | 0.0156 | 16.1 | 343 | 58.58 | 88.4 | 3 of 136 | 56.94 (`run-e9f3b90435fa`) |
| B | `run-4d59d4dfba18` | `4d9e164` | 0.0643 | 29.2 | 1076 | 44.70 | 82.5 | 2 of 109 | 43.81 (`run-deac1404c01f`) |
| C | `run-d8ffdb6e4d14` | `ba16c53` | 0.0189 | 10.0 | 367 | 47.28 | 83.7 | 7 of 116 | 44.20 (`run-db358eb1bf1f`) |
| D | `run-ba733dba1abf` | `f579765` | 0.1444 | 42.2 | 620 | 27.68 | 72.1 | 16 of 116 | 24.02 (`run-ce33b2f0836c`) |
| Overall | | | | | | | 84.84 | 3 of 115 | |

The overall row is the board's: speed points only, over the 115 teams with three scored tracks. The judges' share of the score comes after the freeze.

A best run can come from an earlier commit than the head of this branch; the commit column says which. The headline at the top of a PERF.md can lag this table until the freeze. Where they differ, this table is newer. Every step in a PERF.md names the board run that measured it, and the failed experiments are kept too.

## Check our claims

| Claim | How to check it |
|---|---|
| Every output matches the naive reference byte for byte | `python3 tools/verify.py`. It needs Docker, git and Python 3.9 or later, and took about 16 minutes from cold on our PC. It runs the organisers' own generators and reference on 15 seeds at 2 scales (120 datasets across the four tracks), 32 answer-preserving rewrites, 19 limit cases, and 5 repeated runs per track ([tools/README.md](tools/README.md)) |
| The times and memory above | The run IDs, on the board and in each PERF.md |
| Zero dependencies | Each track's `Cargo.toml` has an empty `[dependencies]` |
| Nothing is precomputed | Each program reads only the data directory it is given (`/data` by default), and final scoring uses a seed nobody has seen |

## Limits

One line per track. The full list is in each track's README, under Limits.

- **A** relies on the spec's guarantees: 27-byte timestamps inside the fixed 72-hour window, well-formed lines, and 1,842 endpoints. Outside them it aborts, or can count a line in the wrong hour. A line that comes later than 1/48 of the log restarts the run, which about doubles T; no generated seed does this. Its T has a slow mode on identical code that we can't fully explain.
- **B** relies on document ids that run consecutively from 0, and on at most 6 terms in a BOOL query and 8 in a phrase. Its sums of ids keep 40 bits, which holds up to 1,482,910 documents, and nothing checks for more. The spec's full scale has 250,000. It is also the longest of our tracks.
- **C** relies on sections 2.2 and 2.3 of the spec: how a BOM and a CR can appear, numbers that fit in 8 bytes, at most 240 countries, and a last record that ends in CRLF. A name over 32 bytes takes a slower path. It calls PEXT, BZHI, carry-less multiply, PREFETCHW and GFNI directly, so it only runs on a CPU that has them, as the scoring box's does.
- **D** stops at an assertion if the graph keeps more than 4.19M nodes after simplifying; the full-scale graph has 1.74M before. Its code has not changed since Nov 1, and its numbering can differ a little between runs, which costs time and never a wrong answer.
- **All four** are built for x86-64-v3 (AVX2) on Linux with glibc, and are timed only on the organisers' box.

## What we have not verified

- **Other hardware.** Timings that count come from the board's box. Our own PC (a Zen 4 7600X) ranked candidate builds and was not used for any figure above. Nothing was run on other CPUs.
- **Slow runs.** On the board, A shows a slow mode on identical code, and C has shown one in about 1 run in 10 since Nov 1. For C, we think it is box noise but haven't proven it.
- **Sanitizers.** Address and thread sanitizers ran on an earlier build of A, not its final one. The thread sanitizer reported things we believe are false alarms but haven't all examined.
- **Late lines in A.** The restart path for a late line is exercised only by logs we built by hand, since no generated seed triggers it.
- **A file with no final terminator in C.** It would lose its last record, and none of our test files has one. The spec rules it out.
- **Phase timings.** Where B and A lose time on the box was measured on earlier designs. We haven't timed the current ones that way.
- **This table.** It is a snapshot. Ranks and K move with every run by anyone, and we make no claim about the final ranking.
- **The check report.** `tools/verify.py` writes its report to `.out/`, which is not committed, so the claim above can be reproduced but isn't shown here.

## How we worked

- **Correct first.** A bank of datasets ran on every build: generator seeds, answer-preserving rewrites of them (line endings, BOMs, quoting, order) and synthetic files at each spec's limits. It ran natively in seconds, then in the board's sandbox. A pre-push gate was set up to refuse a push that failed its Dockerfile check, its offline build or the correctness grid.
- **Measured like the box.** The local scorer uses the organisers' timer from exec to exit, cgroup `memory.peak`, a trimmed mean of repeated runs, and the box's huge-page setting. Local numbers only ranked candidate builds, and `ab.sh` counted a change only if it beat twice the run-to-run spread.
- **One idea per push,** so each board run measures one change. The dev machine is WSL2 with no hardware counters, so instruction and branch-miss counts come from cachegrind, and are cited in the PERF.md entries.

## Shared design choices

- **Rust, standard library only.** Each image builds on `rust:1-slim-trixie`, pinned by digest, with `cargo build --release --offline`. A, B and D run on `debian:13-slim` in a second stage; C has one stage and runs where it was built. `mmap`, `madvise` and a few other calls are declared by hand against glibc, which `std` already links, so there is no `libc` crate.
- **Threads.** The count is `SD_THREADS` if set, otherwise `available_parallelism()`, which respects the container's CPU set, capped at 12. A, B and D use `std::thread::scope`; C creates its threads with `pthread` calls it declares itself. Work is handed out from atomic counters.
- **Build flags** are in each track's `.cargo/config.toml`: `target-cpu=x86-64-v3` on all four. A, B and C also link glibc statically, as T counts from exec, and switch off three tuning features meant for much older CPUs. C adds `pclmulqdq`, `prfchw` and `gfni`.
- **One output file.** Each program writes exactly one file into `/out` (`summary.txt`, `results.txt`, `aggregates.txt` or `distances.txt`), in pieces.

## Before the event

Reading the specs against the harness source turned up two places where the docs and the code disagreed: the dataset the board uses for Track B, and the final-scoring timeout. We reported both on Discord, and the organisers fixed them the same day ([da9d695](https://github.com/hackathon-raptors/speed-demon-2026/commit/da9d695)). No solution code was written before kickoff. `tools/` is test tooling outside the track folders, as the rules allow.

Our first push came 4 minutes 50 seconds after kickoff. GitHub's activity log shows the repo unchanged from the starter until then (15:04:50 UTC, commit `90b27c9`). The session log shows the first Rust source written at 15:02:45 UTC. What existed before kickoff was prose design notes, test tooling, and build files with no track logic (`Cargo.toml`, `.cargo/config.toml`, the `Dockerfile`), which rule 05 allows. An AI agent wrote v1 from those notes. We can share the full session log with the organisers.

## AI use

Written with AI assistance (Claude Code and Codex (GPT-6 Luna)). Every decision and number in the PERF.md files was checked, and can be explained, by the author.

## Licence

MIT: see [LICENSE](LICENSE).
