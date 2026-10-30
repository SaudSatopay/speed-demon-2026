#!/usr/bin/env python3
"""Reproduce this repo's correctness claims with one command.

For every track folder in this repo, build the image offline, the way the board does, and run it in
the board's sandbox on:
  * 15 generator seeds at tiny and sample scale (add --full for full scale on seeds 1, 2, 3, 7, 42);
  * 32 answer-preserving rewrites of seeds 2 and 7: line endings, BOMs, empty-field quoting, line and
    edge order, letter case, spacing (mutate.py);
  * 19 synthetic files at the specs' limits: empty inputs, every key length, sums past 2^32, every
    unreachable kind (limits.py);
  * the seed-1 sample five times over, which must give one identical output.
Each output's SHA-256 is compared with what the organisers' naive reference produces on the same data.
The official generators and references run in the allowlisted python:3.13-slim image, offline.

    python3 tools/verify.py                  # every track folder, about 15 minutes
    python3 tools/verify.py --tracks c       # one track
    python3 tools/verify.py --full           # also full scale for 5 seeds (slower)

Needs Docker, git and Python 3.9 or later. Work files go to .out/verify/ (git-ignored), and a Markdown
report is written to .out/verify/verify-report.md.
"""
import argparse
import datetime
import hashlib
import os
import random
import shutil
import subprocess
import sys
import time
from pathlib import Path

KIT = Path(__file__).resolve().parent
REPO = KIT.parent
sys.path.insert(0, str(KIT))
import limits  # noqa: E402  (the limit-case builders and grammar checks)
import mutate  # noqa: E402  (the answer-preserving rewrites)

HARNESS_URL = "https://github.com/hackathon-raptors/speed-demon-2026"
PYTHON_IMAGE = "python:3.13-slim@sha256:d98cc29d3266e535dd9da1099688d9cb3ca4753813571e071763d17725460962"
SEEDS = [1, 2, 3, 7, 11, 13, 17, 19, 23, 29, 31, 37, 41, 42, 43]
FULL_SEEDS = [1, 2, 3, 7, 42]
VARIANT_BASES = [2, 7]
SANDBOX = ["--network=none", "--read-only", "--tmpfs", "/tmp:rw,size=1g,exec", "--memory=8g", "--memory-swap=8g",
           "--pids-limit=512", "--cap-drop=ALL", "--security-opt=no-new-privileges", "--user", "65534:65534"]


def run(args, **kw):
    return subprocess.run([str(a) for a in args], check=True, **kw)


def sha256(path):
    try:
        return hashlib.sha256(Path(path).read_bytes()).hexdigest()
    except OSError:
        return None


def organiser_script(harness, script, mounts):
    """Run one of the organisers' Python scripts in the allowlisted image, offline, as this user."""
    cmd = ["docker", "run", "--rm", "--network=none", "--user", f"{os.getuid()}:{os.getgid()}", "-v", f"{harness}:/h:ro"]
    for host, guest, mode in mounts:
        cmd += ["-v", f"{host}:{guest}:{mode}"]
    run(cmd + [PYTHON_IMAGE, "python3.13", *script], stdout=subprocess.DEVNULL)


def generate(harness, track, seed, scale, dest):
    if not (dest.is_dir() and any(dest.iterdir())):
        dest.mkdir(parents=True, exist_ok=True)
        organiser_script(harness, [f"/h/generators/{mutate.MODULE[track]}.py", "--seed", seed, "--scale", scale, "--out", "/out"],
                         [(dest, "/out", "rw")])
    return dest


def write_dataset(dest, files, copy_from=None):
    if dest.is_dir():
        shutil.rmtree(dest)
    dest.mkdir(parents=True)
    for name, data in files.items():
        (dest / name).write_bytes(data)
    for src in (copy_from.iterdir() if copy_from else []):
        if src.name not in files:
            shutil.copy2(src, dest / src.name)
    return dest


def datasets(harness, data, track, full):
    """Yield (kind, label, folder) for every dataset this track is checked on."""
    for seed in SEEDS:
        for scale in ["tiny", "sample"] + (["full"] if full and seed in FULL_SEEDS else []):
            yield "seeds", f"seed {seed} {scale}", generate(harness, track, seed, scale, data / f"seed-{seed}" / f"{track}-{scale}")
    for base in VARIANT_BASES:
        for scale in mutate.DEFAULT_SCALES[track]:
            base_dir = generate(harness, track, base, scale, data / f"seed-{base}" / f"{track}-{scale}")
            for kind, name, rewrite in mutate.VARIANTS[track]:
                files = {mutate.REWRITTEN[track]: (base_dir / mutate.REWRITTEN[track]).read_bytes()}
                rewrite(files, random.Random(100000 + 100 * base + kind))  # the same seeds as our own bank
                dest = write_dataset(data / f"variant-{base}-{name}" / f"{track}-{scale}", files, copy_from=base_dir)
                yield "rewrites", f"seed {base} {scale} + {name}", dest
    for case, name, build in limits.CASES[track]:
        files = build(random.Random(f"{track}{case}"))
        limits.CHECK[track](files)  # each spec's input grammar, before use
        yield "limits", name, write_dataset(data / f"limit-{name}" / track, files)


def expected_hash(harness, track, folder, work, cache):
    key = str(folder.relative_to(work))
    if key not in cache:
        out = work / "ref-out"
        shutil.rmtree(out, ignore_errors=True)
        out.mkdir()
        organiser_script(harness, [f"/h/reference/{mutate.MODULE[track]}.py", "--data", "/data", "--out", "/out"],
                         [(folder, "/data", "ro"), (out, "/out", "rw")])
        cache[key] = sha256(out / mutate.OUTPUT[track])
        with open(work / "expected.tsv", "a") as f:
            f.write(f"{key}\t{cache[key]}\n")
    return cache[key]


def submission_hash(tag, track, folder, work):
    out = work / "out"
    shutil.rmtree(out, ignore_errors=True)
    out.mkdir()
    os.chmod(out, 0o777)  # the sandbox runs as user 65534
    result = subprocess.run(["docker", "run", "--rm", *SANDBOX, "-e", f"TRACK={track}",  # the run contract's one variable
                             "-v", f"{folder}:/data:ro", "-v", f"{out}:/out:rw", tag],
                            stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, text=True)
    if result.returncode != 0:
        return None, f"exit {result.returncode}: {result.stderr.strip()[-200:]}"
    files = sorted(p.name for p in out.iterdir())
    if files != [mutate.OUTPUT[track]]:
        return None, f"/out must hold exactly {mutate.OUTPUT[track]}, found {files}"
    return sha256(out / mutate.OUTPUT[track]), ""


def git_head(path):
    try:
        head = run(["git", "-C", path, "rev-parse", "--short", "HEAD"], capture_output=True, text=True).stdout.strip()
        dirty = run(["git", "-C", path, "status", "--porcelain"], capture_output=True, text=True).stdout.strip()
        return head + ("+uncommitted" if dirty else "")
    except (subprocess.CalledProcessError, FileNotFoundError):
        return "unknown"


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("--tracks", nargs="+", choices=list("abcd"), help="default: every track-* folder present")
    ap.add_argument("--full", action="store_true", help="also check full scale for seeds 1, 2, 3, 7 and 42")
    ap.add_argument("--harness", help="a local clone of hackathon-raptors/speed-demon-2026 (default: clone one)")
    ap.add_argument("--work", default=str(REPO / ".out" / "verify"), help="work folder (default: .out/verify)")
    args = ap.parse_args()

    work = Path(args.work).resolve()
    work.mkdir(parents=True, exist_ok=True)
    harness = Path(args.harness).resolve() if args.harness else work / "speed-demon-2026"
    if not (harness / "reference").is_dir():
        print(f"cloning {HARNESS_URL} ...")
        run(["git", "clone", "-q", "--depth", "1", HARNESS_URL, harness])
    tracks = args.tracks or [t for t in "abcd" if (REPO / f"track-{t}" / "Dockerfile").is_file()]
    cache = {}
    if (work / "expected.tsv").exists():
        for line in (work / "expected.tsv").read_text().splitlines():
            key, digest = line.split("\t")
            cache[key] = digest

    started, rows, failures = time.time(), [], []
    for track in tracks:
        tag = f"sd-verify/track-{track}"
        print(f"== track {track.upper()}: building track-{track} offline")
        run(["docker", "build", "-q", "--network=none", "-t", tag, REPO / f"track-{track}"], stdout=subprocess.DEVNULL)
        tally = {"seeds": [0, 0], "rewrites": [0, 0], "limits": [0, 0]}
        for kind, label, folder in datasets(harness, work / "data", track, args.full):
            want = expected_hash(harness, track, folder, work, cache)
            got, why = submission_hash(tag, track, folder, work)
            tally[kind][1] += 1
            if got == want:
                tally[kind][0] += 1
            else:
                failures.append(f"track {track.upper()}, {label}: " + (why or f"sha256 {got[:12]}, the reference gives {want[:12]}"))
                print(f"   FAIL {label}")
        seed1 = work / "data" / "seed-1" / f"{track}-sample"
        five = {submission_hash(tag, track, seed1, work)[0] for _ in range(5)}
        same = len(five) == 1 and five == {expected_hash(harness, track, seed1, work, cache)}
        if not same:
            failures.append(f"track {track.upper()}: 5 runs on seed 1 sample gave {len(five)} distinct output(s)"
                            + ("" if len(five) > 1 else ", not the reference's"))
        rows.append((track, tally, same))
        print("   " + "   ".join(f"{k} {p}/{n}" for k, (p, n) in tally.items()) + f"   5 identical runs: {'yes' if same else 'NO'}")

    total = sum(n for _, t, _ in rows for _, n in t.values())
    passed = sum(p for _, t, _ in rows for p, _ in t.values())
    stamp = datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%d %H:%M UTC")
    report = [f"# Correctness check of commit {git_head(REPO)}", "",
              f"Run {stamp} with `python3 tools/verify.py{' --full' if args.full else ''}` against the organisers' harness at "
              f"{git_head(harness)}, in {time.time() - started:.0f} s.", "",
              "| Track | Generator seeds | Answer-preserving rewrites | Limit cases | 5 identical runs |", "|---|---|---|---|---|"]
    for track, tally, same in rows:
        cells = [f"{p}/{n}" for p, n in tally.values()]
        report.append(f"| {track.upper()} | {cells[0]} | {cells[1]} | {cells[2]} | {'yes' if same else 'NO'} |")
    report += ["", f"**{passed}/{total} outputs matched the naive reference's SHA-256.**"]
    report += (["", "Failures:", ""] + [f"- {f}" for f in failures]) if failures else []
    (work / "verify-report.md").write_text("\n".join(report) + "\n")
    print("\n" + "\n".join(report))
    print(f"\nreport: {work / 'verify-report.md'}")
    sys.exit(1 if failures else 0)


if __name__ == "__main__":
    main()
