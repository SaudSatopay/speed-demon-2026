# ALL STAR · Speed Demon 2026

Four rewrites of the organisers' naive Python references. Each is written in Rust with only the standard library (zero dependencies) and built offline into its own Docker image. Every output matches the reference's byte for byte on all 171 datasets in our check, and on five repeated runs. You can verify that with one command ([below](#check-our-claims)). A solo entry by Saud Satopay.

| Track | What it does | Reference on the box | Ours on the box (board run) | K | Speed points | Details |
|---|---|---|---|---|---|---|
| A · Ingest | Per-hour, per-endpoint latency percentiles | 32.05 s · 228 MB | 0.0189 s · 16.5 MB (`run-18e65c063a24`) | 54.20 | 86.7 | [README](track-a/README.md) · [PERF.md](track-a/PERF.md) |
| B · Index | Boolean and phrase queries over 250K documents | 56.35 s · 2,200 MB | 0.0983 s · 41.6 MB (`run-f39141d33527`) | 36.97 | 78.4 | [README](track-b/README.md) · [PERF.md](track-b/PERF.md) |
| C · Join | A hostile CSV joined and summed in integer cents | 14.44 s · 367 MB | 0.0227 s · 10.3 MB (`run-40b0e94343ad`) | 41.19 | 80.7 | [README](track-c/README.md) · [PERF.md](track-c/PERF.md) |
| D · Traverse | 2,000 shortest paths on a 5M-edge graph | 72.69 s · 893 MB | 0.1454 s · 42.2 MB (`run-a8cb2e6350b9`) | 27.59 | 72.0 | [README](track-d/README.md) · [PERF.md](track-d/PERF.md) |

Every number above is from the [live board](https://speeddemon.dev/board), measured on the organisers' machine; none is a local timing. Each is the track's best run, and the same code's time varies by a few percent between runs. Each step that produced it is a numbered entry in that track's PERF.md, with its run ID, and the failed experiments are kept too.

## Check our claims

| Claim | How to check it |
|---|---|
| Every output matches the naive reference byte for byte | `python3 tools/verify.py`. It needs only Docker, git and Python, and takes about 16 minutes from cold. It runs the organisers' own generators and reference on 15 seeds at 2 scales, 32 answer-preserving rewrites, 19 limit cases, and 5 repeated runs per track ([tools/README.md](tools/README.md)) |
| The times and memory above | The run IDs, on the board and in each PERF.md |
| Zero dependencies | Each track's `Cargo.toml` has an empty `[dependencies]` |
| Nothing is precomputed | Each program reads only `/data`, and final scoring uses a seed nobody has seen |

## Limits

- **A:** it relies on the spec's fixed timestamps, well-formed lines and 1,842 endpoints, and aborts rather than answering wrongly past those. A line more than 1/48 of the log late restarts the run, which doubles T; no seed does this.
- **B:** it relies on the spec's dense document ids, and stops with an assert past 1,048,575 documents. Queries anchored on a common term still store postings, most of the memory left.
- **C:** it relies on section 2.2 of the spec: a mid-file BOM only ever starts a record, a CR only ever starts a CRLF, numbers fit in 8 bytes, and there are at most 240 countries. A file with no CRLF would still be right, but wouldn't get split across threads.
- **D:** a guard rejects a graph whose renumbered label pages pass 16 bits. A query whose distances pass 2^32 runs twice, first in 32 bits and then in 64.

## How we worked

- **Correct first.** A bank of datasets ran on every build: generator seeds, answer-preserving rewrites of them (line endings, BOMs, quoting, order) and synthetic files at each spec's limits. It ran natively in seconds, then in the board's sandbox before every push. A pre-push gate refused any push that failed. Lock-free tables also got repeated runs at varying thread counts, as a race doesn't show on one pass, and two of them were caught that way.
- **Measured like the box, and judged by the box.** The local scorer uses the official runner and timer, cgroup `memory.peak`, the middle-3 mean of 5 runs, and the box's huge-page setting. Hardware counters ranked changes that cut instructions. Local numbers misled us on anything to do with the kernel or with cache layout, as the dev machine is a virtual machine with smaller caches, so those went to the board as labelled `exp:` pushes.
- **Timing phases on a machine we can't profile.** An `exp:` push runs one phase twice on throwaway state. T minus the usual T is that phase's wall time on the box. That's how we learned that reading, not parsing, was the next limit on A.
- **One idea per push,** so each board run proves one claim. Instruction and branch-miss counts back the PERF.md entries.

## Shared design choices

- **Rust, standard library only.** `mmap`, `madvise` and a few others are declared directly against glibc, with no `libc` crate.
- **Threads.** They run under `std::thread::scope`. The count comes from `available_parallelism()`, which respects the container's CPU set. Work is dealt out in chunks from an atomic counter, and results merge in a fixed order, so the output never depends on scheduling.
- **Built for `x86-64-v3`** (AVX2, no AVX-512, like the box) in `.cargo/config.toml`. Track C adds carry-less multiply for its quote masks and PREFETCHW. Tracks A, B and C link glibc statically, as T counts from exec.
- **One output file.** It's written in pieces into `/out`, never also held whole in memory.
- **Two-stage Dockerfiles** from allowlisted, digest-pinned images.

## Before the event

Reading the specs against the harness source turned up two places where the docs and the code disagreed: the dataset the board uses for Track B, and the final-scoring timeout. We reported both on Discord, and the organisers fixed them the same day ([da9d695](https://github.com/hackathon-raptors/speed-demon-2026/commit/da9d695)). No solution code was written before kickoff. `tools/` is test tooling outside the track folders, as the rules allow.

Our first push came 4 minutes 50 seconds after kickoff, which deserves an explanation. GitHub's activity log shows the repo unchanged from the starter until then (15:04:50 UTC, commit `90b27c9`). The session log shows the first Rust source written at 15:02:45 UTC. What existed before kickoff was prose design notes, test tooling, and build files with no track logic (`Cargo.toml`, `.cargo/config.toml`, the `Dockerfile`), which rule 05 allows. An AI agent wrote v1 from those notes, and the bank of tests checked it in seconds. We can share the full session log with the organisers.

## AI use

Written with AI assistance (Claude Code and Codex (GPT-6 Luna)). Every design decision and every number here and in the PERF.md files was checked, and can be explained, by the author.

## Licence

MIT: see [LICENSE](LICENSE).
