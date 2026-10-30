#!/usr/bin/env python3
"""Speed Demon 2026 prep: answer-preserving variants of banked datasets, for the correctness grid.

Each variant rewrites a banked dataset in a way the spec allows and that must leave the answer
unchanged: line order, line endings, BOM placement, how an empty field is quoted, letter case,
spacing. The organisers' naive reference then computes the expected output for the variant, so a
solution that disagrees is wrong, whatever the generator happened to emit for that seed.

    python3 ~/speed-demon/mutate.py                          # bases 2 and 7; C also at tiny
    python3 ~/speed-demon/mutate.py --bases 3 --tracks c --scales "tiny sample"

Variants are banked as extra seeds, 100000 + 100 * base + kind (kinds in VARIANTS), so quick.sh,
the pre-push gate and score.sh's grid run them with everything else. ~/speed-demon/data/variants.tsv
says what each one is. Every rewrite stays inside the generator's guarantees (each spec's "what the
generator writes" section), so a failure on a variant is a real bug, not a false alarm.
Run inside WSL after hash_bank.sh. This is prep tooling, not solution code.
"""
import argparse
import hashlib
import os
import random
import re
import shutil
import subprocess
import tempfile
import time
from pathlib import Path

SD_HOME = Path(os.environ.get("SD_HOME", Path.home() / "speed-demon"))
HARNESS = SD_HOME / "speed-demon-2026"
BANK = SD_HOME / "hash-bank.tsv"
MANIFEST = SD_HOME / "data" / "variants.tsv"
PYTHON = shutil.which("python3.13") or str(Path.home() / ".local/bin/python3.13")
MODULE = {"a": "a_ingest", "b": "b_index", "c": "c_join", "d": "d_traverse"}
OUTPUT = {"a": "summary.txt", "b": "results.txt", "c": "aggregates.txt", "d": "distances.txt"}
BOM = b"\xef\xbb\xbf"


def lines_of(data):
    assert data.endswith(b"\n"), "expected the file to end with LF"
    return data[:-1].split(b"\n")


def joined(lines):
    return b"\n".join(lines) + b"\n"


# Track A: line order is free (spec: "no bound on how far a line can be out of order").
def a_shuffle(files, rng):
    lines = lines_of(files["requests.log"])
    rng.shuffle(lines)
    files["requests.log"] = joined(lines)


def a_reverse(files, rng):
    files["requests.log"] = joined(lines_of(files["requests.log"])[::-1])


# Track B: a token is a run of ASCII letters and digits after ASCII lower-casing, so swapping the
# case of ASCII letters, or doubling ASCII spaces, leaves every token and its position unchanged.
def b_text(files, change):
    out = []
    for line in lines_of(files["docs.txt"]):
        doc_id, tab, text = line.partition(b"\t")
        out.append(doc_id + tab + change(text))
    files["docs.txt"] = joined(out)


def b_swapcase(files, rng):
    b_text(files, bytes.swapcase)


def b_double_spaces(files, rng):
    b_text(files, lambda text: text.replace(b" ", b"  "))


# Track C: split records.csv by its grammar (spec 2.1). Every CR starts a CRLF terminator and no
# field holds a CR (spec 2.2), so this finds exactly the records the reference finds.
FIELD = rb'"(?:[^"]|"")*"|[^,"\r\n]*'
FIELD_RE = re.compile(FIELD)
RECORD = re.compile(rb"(\xef\xbb\xbf)?((?:" + FIELD + rb")(?:,(?:" + FIELD + rb"))*)(\r\n|\n)")


def c_parse(data):
    assert data.startswith(BOM), "records.csv must start with a BOM"
    pos = data.index(b"\r\n") + 2
    header, records = data[:pos], []
    while pos < len(data):
        m = RECORD.match(data, pos)
        assert m, f"unparsed bytes at offset {pos}"
        records.append([m.group(1) is not None, m.group(2), m.group(3)])  # bom, body, terminator
        pos = m.end()
    return header, records


def c_mutate(change):
    def mutate(files, rng):
        header, records = c_parse(files["records.csv"])
        for i, record in enumerate(records):
            change(i, i == len(records) - 1, record)
        body = b"".join((BOM if bom else b"") + fields + term for bom, fields, term in records)
        files["records.csv"] = header + body
    return mutate


def all_crlf(i, last, record):
    record[2] = b"\r\n"


def lf_except_last(i, last, record):
    record[2] = b"\r\n" if last else b"\n"  # the last record always ends with CRLF (spec 2.2)


def bom_every_record(i, last, record):
    record[0] = i > 0  # a BOM never directly follows the header (spec 2.2)


def bom_none(i, last, record):
    record[0] = False


def swap_empty_quoting(i, last, record):
    fields, pos, body = [], 0, record[1]
    while True:
        m = FIELD_RE.match(body, pos)
        fields.append(m.group())
        pos = m.end()
        if pos == len(body):
            break
        pos += 1  # the comma
    assert len(fields) == 5, f"record {i} has {len(fields)} fields"
    for k in (1, 2):  # name and note: an empty field and "" are the same value (spec 2.1)
        fields[k] = {b"": b'""', b'""': b""}.get(fields[k], fields[k])
    record[1] = b",".join(fields)


# Track D: edge lines are "in no particular order", so any permutation is the same graph. Reversing
# also flips which of two parallel edges comes first.
def d_edges(files, order):
    head, _, rest = files["graph.txt"].partition(b"\n")
    edges = lines_of(rest) if rest else []
    order(edges)
    files["graph.txt"] = head + b"\n" + (joined(edges) if edges else b"")


VARIANTS = {  # track: (kind, name, rewrite)
    "a": [(1, "shuffle-lines", a_shuffle), (2, "reverse-lines", a_reverse)],
    "b": [(1, "swap-case", b_swapcase), (2, "double-spaces", b_double_spaces)],
    "c": [
        (1, "all-crlf", c_mutate(all_crlf)),
        (2, "lf-except-last", c_mutate(lf_except_last)),
        (3, "bom-every-record", c_mutate(bom_every_record)),
        (4, "bom-none", c_mutate(bom_none)),
        (5, "swap-empty-quoting", c_mutate(swap_empty_quoting)),
    ],
    "d": [
        (1, "shuffle-edges", lambda files, rng: d_edges(files, rng.shuffle)),
        (2, "reverse-edges", lambda files, rng: d_edges(files, list.reverse)),
    ],
}
REWRITTEN = {"a": "requests.log", "b": "docs.txt", "c": "records.csv", "d": "graph.txt"}
DEFAULT_SCALES = {"a": ["sample"], "b": ["sample"], "c": ["tiny", "sample"], "d": ["sample"]}


def read_tsv(path, width):
    lines = path.read_text().splitlines() if path.exists() else []
    rows = {}
    for line in lines[1:]:
        cols = line.split("\t")
        rows[(int(cols[0]), cols[1], cols[2])] = cols[3:width]
    return (lines[0] if lines else None), rows


def write_tsv(path, header, rows):
    tmp = path.with_name(path.name + ".tmp")
    body = "".join("\t".join([str(s), t, sc, *rest]) + "\n" for (s, t, sc), rest in sorted(rows.items()))
    tmp.write_text(header + "\n" + body)
    tmp.replace(path)


def build(track, base_dir, vdir, rewrite, rng):
    vdir.mkdir(parents=True, exist_ok=True)
    name = REWRITTEN[track]
    files = {name: (base_dir / name).read_bytes()}
    rewrite(files, rng)
    (vdir / name).write_bytes(files[name])
    for src in base_dir.iterdir():  # the untouched inputs: hard links, which also work inside docker
        dst = vdir / src.name
        if src.name != name and not dst.exists():
            try:
                os.link(src, dst)
            except OSError:
                shutil.copy2(src, dst)


def run_reference(track, data_dir):
    with tempfile.TemporaryDirectory(dir="/dev/shm") as out:
        start = time.monotonic()
        subprocess.run([PYTHON, str(HARNESS / "reference" / f"{MODULE[track]}.py"),
                        "--data", str(data_dir), "--out", out], check=True)
        return (Path(out) / OUTPUT[track]).read_bytes(), time.monotonic() - start


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("--bases", type=int, nargs="+", default=[2, 7], help="banked seeds to rewrite")
    ap.add_argument("--tracks", nargs="+", default=list("abcd"), choices=list("abcd"))
    ap.add_argument("--scales", help='e.g. "tiny sample" (default: sample, plus tiny for C)')
    args = ap.parse_args()

    bank_header, bank = read_tsv(BANK, 5)
    assert bank_header, f"no hash bank at {BANK}: run hash_bank.sh first"
    _, manifest = read_tsv(MANIFEST, 7)
    for track in args.tracks:
        for base in args.bases:
            for scale in (args.scales.split() if args.scales else DEFAULT_SCALES[track]):
                base_dir = (SD_HOME / "data" / f"s{base}" / f"{track}-{scale}").resolve()
                if (base, track, scale) not in bank or not base_dir.is_dir():
                    print(f"skip     {track} {scale} base {base}: not banked (run hash_bank.sh {base} first)")
                    continue
                base_sha = bank[(base, track, scale)][0]
                for kind, name, rewrite in VARIANTS[track]:
                    seed = 100000 + 100 * base + kind
                    vdir = SD_HOME / "data" / f"s{seed}" / f"{track}-{scale}"
                    expected = Path(f"{vdir}.expected") / OUTPUT[track]
                    if (seed, track, scale) in bank and (scale == "full" or expected.exists()):
                        continue
                    build(track, base_dir, vdir, rewrite, random.Random(seed))
                    output, secs = run_reference(track, vdir)
                    sha = hashlib.sha256(output).hexdigest()
                    if scale != "full":
                        expected.parent.mkdir(exist_ok=True)
                        expected.write_bytes(output)
                    same = "same" if sha == base_sha else "CHANGED"
                    bank[(seed, track, scale)] = [sha, f"{secs:.1f}"]
                    manifest[(seed, track, scale)] = [str(base), str(kind), name, same]
                    note = "" if same == "same" else "   <- the answer moved: check this rewrite"
                    print(f"ok       {track} {scale} seed {seed} = base {base} + {name}: {sha[:12]} ({secs:.1f} s){note}")
    write_tsv(BANK, bank_header, bank)
    write_tsv(MANIFEST, "seed\ttrack\tscale\tbase_seed\tkind\tvariant\tanswer_vs_base", manifest)
    print(f"bank: {len(bank)} rows; variants: {len(manifest)} in {MANIFEST}")


if __name__ == "__main__":
    main()
