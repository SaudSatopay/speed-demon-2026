# tools/: how we checked correctness, and how you can too

Nothing here is part of any track's solution, and none of it is scored: the rules exclude tools outside the track folders. These are the checks we used to keep every output byte-identical to the organisers' naive reference. They let you reproduce that claim with one command.

## One command

```bash
python3 tools/verify.py
```

It needs Docker, git and Python 3.9 or later. A cold start took 16 minutes on our PC, mostly generating the data and computing the reference's answers. Add `--full` for full scale on five seeds, which is slower. It writes a report to `.out/verify/verify-report.md` and exits non-zero on any mismatch. `--tracks c` checks one track.

## What it checks

For each track folder, it builds the image offline, as the board does. Then it runs the image in the board's sandbox: no network, a read-only root, a 1 GB `/tmp`, 8 GB of memory and an unprivileged user.

| Datasets | Per track | What they catch |
|---|---|---|
| Generator seeds 1, 2, 3, 7, 11, 13, 17, 19, 23, 29, 31, 37, 41, 42, 43, at tiny and sample scale | 30 | Anything tuned to a single seed |
| Answer-preserving rewrites of seeds 2 and 7 (`mutate.py`) | A, B, D: 4; C: 20 | Line endings (one C variant has only two CRs in 20 MB), BOM placement, empty-field quoting, line and edge order, letter case, spacing |
| Synthetic files at each spec's limits (`limits.py`) | A 5, B 4, C 4, D 6 | Empty inputs, every endpoint and token length, percentile ranks at their edges, sums past 2^32, max-weight chains, every kind of unreachable query |
| Seed 1 at sample scale, run five times | 1 | Nondeterminism: final scoring needs one identical output across five runs |

Every expected answer comes from the organisers' own naive reference, run on their own generators' data. Both run in the allowlisted `python:3.13-slim` image, offline. Every synthetic file is checked against its spec's input grammar before use, so a mismatch is a real bug, not an artefact of the test.

## Files

- `verify.py`: the one-command check.
- `mutate.py`: the answer-preserving rewrites. It's byte-identical to the copy we ran during the event, which also banked the same datasets for our local loop.
- `limits.py`: the limit-case builders and grammar checks. It's also byte-identical to the copy we used.

During the event the same datasets ran on every build: natively in seconds, then in the board's sandbox before every push. Each track's PERF.md describes that loop.
