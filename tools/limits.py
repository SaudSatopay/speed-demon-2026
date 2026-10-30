#!/usr/bin/env python3
"""Speed Demon 2026 prep: a bank of synthetic inputs at the specs' limits, for the correctness grid.

The generator's seeds all come from one distribution, as 1BRC's samples did before its 10K-key file
caught the entries tuned to them. These files push every limit the specs allow instead:
  A  an empty file, every endpoint length 1-125 with near-twin keys, percentile ranks around
     n = 19/20/99/100, every latency digit count, every hour boundary in reverse, all 1,842 x 72 groups
  B  every token length 1-32 next to every separator, BOOL chains over 70,000 documents, phrase
     shapes (repeats, overlaps, separators, non-query tokens between terms), set-word boundaries
  C  names and notes at every length and quoting style, amount limits and sums past 2^32, a one-record
     file, distinct-name rules and unknown regions
  D  one node, no queries, weights at the bucket and packing boundaries, a max-weight chain, every
     unreachable kind including heavy-spur islands, zero-weight cycles and chains

    python3 ~/speed-demon/limits.py            # build any missing cases and bank them
    python3 ~/speed-demon/limits.py --force    # rebuild them all

Each case is banked at scale "tiny" as seed 200000 + case, so quick.sh, the pre-push gate and
score.sh's grid run it. The official naive reference computes every expected answer. Each file is
checked against its spec's input grammar before banking, so a failure is a real bug, not a false
alarm. ~/speed-demon/data/limits.tsv lists the cases. This is prep tooling, not solution code.
"""
import argparse
import hashlib
import random
import re
import shutil
import string
import subprocess
from datetime import datetime, timedelta, timezone
from pathlib import Path

from mutate import BANK, FIELD_RE, OUTPUT, SD_HOME, c_parse, read_tsv, run_reference, write_tsv

MANIFEST = SD_HOME / "data" / "limits.tsv"
SCALE = "tiny"
U32_MAX = 4294967295

# ---- Track A (specs/a-ingest.md, sections 2 and 3) ------------------------------------------------
A_START = datetime(2026, 10, 30, 18, tzinfo=timezone.utc)
A_METHODS = ["GET", "POST", "PUT", "PATCH", "DELETE", "HEAD", "OPTIONS"]
A_ECHARS = string.ascii_letters + string.digits + "/-_.{}:"
A_FIXED = ["/", "/health", "/healthz", "/v2/checkout", "/v2/orders", "/v2/orders-export", "/v2/orders.csv",
           "/v2/orders/{id}", "/v2/orders/{id}/items", "/v2/ordersByDate", "/v2/orders_legacy", "/v2/orderstatus",
           "/internal/v1/tenants/{tenant_id}/projects/{project_id}/pipelines/{pipeline_id}/runs/{run_id}"
           "/artifacts/{artifact_id}:download"]
A_LINE = re.compile(rb"(\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d\.\d{6}Z) (?:GET|POST|PUT|PATCH|DELETE|HEAD|OPTIONS) "
                    rb"/[A-Za-z0-9/_.{}:-]{0,124} [1-5]\d\d (0|[1-9]\d{0,9})")


def a_ts(hour, rng=None, at=None):
    """A timestamp in window hour 0-71: random within the hour, or exactly at (minute, second, us)."""
    m, s, us = at if at else (rng.randrange(60), rng.randrange(60), rng.randrange(1_000_000))
    t = A_START + timedelta(hours=hour, minutes=m, seconds=s, microseconds=us)
    return t.strftime("%Y-%m-%dT%H:%M:%S.") + f"{t.microsecond:06d}Z"


def a_line(ts, method, endpoint, status, latency):
    return f"{ts} {method} {endpoint} {status} {latency}\n"


def a_endpoint(rng, length):
    return "/" + "".join(rng.choice(A_ECHARS) for _ in range(length - 1))


def a_latency(rng, digits):
    lo = 0 if digits == 1 else 10 ** (digits - 1)
    return rng.randint(lo, min(10 ** digits - 1, U32_MAX))


def a_file(lines, rng):
    rng.shuffle(lines)
    return {"requests.log": "".join(lines).encode()}


def a_empty(rng):
    return {"requests.log": b""}  # spec 3.7: an empty log gives an empty summary


def a_endpoint_lengths(rng):
    endpoints = A_FIXED + [a_endpoint(rng, n) for n in range(1, 126)]
    base = "/" + "a" * 124  # 125 bytes, plus twins that differ from it at a single byte
    endpoints += [base] + [base[:p] + "b" + base[p + 1:] for p in (1, 7, 8, 15, 16, 17, 31, 32, 33, 63, 64, 65, 95, 96, 124)]
    endpoints += ["/abc", "/abc/", "/abcd", "/ab", "/abc{", "/ABC"]  # prefixes and case: all distinct keys
    lines = []
    for i, ep in enumerate(dict.fromkeys(endpoints)):
        for k in range(14):  # every method, every latency width; the last two share hour 5 with every key
            hour = rng.randrange(72) if k < 12 else 5
            lines.append(a_line(a_ts(hour, rng), A_METHODS[k % 7], ep, rng.randrange(100, 600), a_latency(rng, 1 + (i + k) % 10)))
    return a_file(lines, rng)


def a_percentile_ranks(rng):
    lines = []
    for n in (1, 2, 3, 19, 20, 21, 99, 100, 101, 199, 200, 1000):  # where p95/p99 stop being the max
        for dup in (False, True):
            ep, hour = f"/rank/{n}/{'dup' if dup else 'distinct'}", rng.randrange(72)
            values = [rng.choice([0, 7, U32_MAX]) for _ in range(n)] if dup else rng.sample(range(0, U32_MAX + 1, 9973), n)
            for v in values:
                lines.append(a_line(a_ts(hour, rng), rng.choice(A_METHODS), ep, rng.choice([100, 199, 200, 499, 500, 501, 599]), v))
    for digits in range(1, 11):  # the smallest and largest latency of every width
        lo, hi = (0 if digits == 1 else 10 ** (digits - 1)), min(10 ** digits - 1, U32_MAX)
        for v in (lo, lo + 1, hi - 1, hi):
            lines.append(a_line(a_ts(digits, rng), "GET", f"/digits/{digits}", 200, v))
    for errors, n in ((0, 5), (5, 5), (1, 3), (1, 7), (2, 3)):  # error_ppm rounding
        for j in range(n):
            lines.append(a_line(a_ts(40, rng), "POST", f"/ppm/{errors}-of-{n}", 503 if j < errors else 499, 1000 + j))
    return a_file(lines, rng)


def a_hour_boundaries(rng):
    lines = [a_line(a_ts(hour, at=at), "GET", ep, 200 + hour, hour * 1000 + at[0])
             for hour in range(72) for ep in ("/edge", "/v2/orders", "/v2/orders/{id}")
             for at in ((0, 0, 0), (30, 0, 0), (59, 59, 999_999))]
    return {"requests.log": "".join(reversed(lines)).encode()}  # newest first: every hour goes backwards


def a_all_groups(rng):
    endpoints, seen = list(A_FIXED), set(A_FIXED)
    while len(endpoints) < 1842:  # the catalogue size every seed has
        ep = a_endpoint(rng, rng.randrange(2, 126))
        if ep not in seen:
            seen.add(ep)
            endpoints.append(ep)
    return a_file([a_line(a_ts(h, rng), rng.choice(A_METHODS), ep, rng.randrange(100, 600), rng.randint(0, U32_MAX))
                   for h in range(72) for ep in endpoints], rng)


def a_check(files):
    data = files["requests.log"]
    if not data:
        return
    assert data.endswith(b"\n")
    for line in data[:-1].split(b"\n"):
        m = A_LINE.fullmatch(line)
        assert m and b"2026-10-30T18:00:00.000000Z" <= m.group(1) <= b"2026-11-02T17:59:59.999999Z", line
        assert int(m.group(2)) <= U32_MAX, line


# ---- Track B (specs/b-index.md, section 2) --------------------------------------------------------
B_SEPARATORS = [c for c in map(chr, range(0x20, 0x7F)) if not c.isalnum()] + ["é", "€", "𝔄", "\u00a0", "Ñ", "İ"]
B_QUERY = re.compile(rb'BOOL [a-z0-9]{1,32}(?: (?:AND|OR|NOT) [a-z0-9]{1,32}){0,5}|PHRASE "[a-z0-9]{1,32}(?: [a-z0-9]{1,32}){1,7}"')


def b_token(rng, length):
    return "".join(rng.choice(string.ascii_lowercase + string.digits) for _ in range(length))


def b_join(rng, tokens):
    """Tokens joined by 1-3 random separators, some upper-cased (tokenizing lower-cases ASCII)."""
    out = []
    for i, tok in enumerate(tokens):
        if i:
            out.append("".join(rng.choice(B_SEPARATORS) for _ in range(rng.randint(1, 3))))
        out.append(tok.upper() if rng.random() < 0.2 else tok)
    return "".join(out)


def b_tokens(text):
    return [t.decode() for t in re.findall(rb"[a-z0-9]+", text.encode().lower())]  # spec 2.2 exactly


def b_files(texts, queries):
    return {"docs.txt": "".join(f"{i}\t{t}\n" for i, t in enumerate(texts)).encode(),
            "queries.txt": "".join(q + "\n" for q in queries).encode()}


def b_token_lengths(rng):
    vocab = [b_token(rng, n) for n in range(1, 33) for _ in range(2)]
    texts = []
    for _ in range(3000):
        text = b_join(rng, [rng.choice(vocab) for _ in range(rng.randint(0, 12))])
        if rng.random() < 0.3:
            text = rng.choice(B_SEPARATORS) + text
        if rng.random() < 0.3:
            text += rng.choice(B_SEPARATORS)
        texts.append(text)
    queries = []
    for tok in vocab:
        twin = tok[:-1] + ("z" if tok[-1] != "z" else "y")  # differs only in its last byte
        queries += [f"BOOL {tok}", f"BOOL {twin}", f"BOOL {tok} OR {twin} AND {rng.choice(vocab)}"]
    for _ in range(600):  # phrases cut from real documents, so most of them match
        toks = b_tokens(rng.choice(texts))
        if len(toks) >= 2:
            n = rng.randint(2, min(8, len(toks)))
            start = rng.randrange(len(toks) - n + 1)
            queries.append('PHRASE "' + " ".join(toks[start:start + n]) + '"')
    return b_files(texts, queries)


def b_bool_chains(rng):
    words = [b_token(rng, rng.randint(2, 9)) for _ in range(36)] + ["and", "or", "not", "007", "7"]
    weights = [1 / (rank + 1) for rank in range(len(words))]  # from almost every document to almost none
    texts = [b_join(rng, rng.choices(words, weights, k=rng.randint(0, 8))) for _ in range(70_000)]
    queries = []
    for _ in range(4000):
        terms = [rng.choice(words + ["zzabsent", "q1"]) for _ in range(rng.randint(1, 6))]
        queries.append("BOOL " + terms[0] + "".join(f" {rng.choice(['AND', 'OR', 'NOT'])} {t}" for t in terms[1:]))
    return b_files(texts, queries + queries[:50])  # repeated lines get their own output lines


def b_phrases(rng):
    words = ["x", "y", "a", "b", "the", "bail", "bond", "zz9"]
    texts = ["x x x x", "a b a b a", "x y x y", "the bail bond", "bail-bond, bail; bond", "bond bail", "zürich rich",
             "", "...;;;", "bail é bond", "THE BAIL BOND", "bail filler bond", "x"]
    texts += [b_join(rng, rng.choices(words + ["filler"], k=rng.randint(0, 20))) for _ in range(2000)]
    queries = ['PHRASE "x x"', 'PHRASE "x x x"', 'PHRASE "x x x x x"', 'PHRASE "a b a"', 'PHRASE "b a b a"',
               'PHRASE "bail bond"', 'PHRASE "the bail bond"', 'PHRASE "bond bail"', 'PHRASE "z rich"',
               'PHRASE "rich rich"', 'PHRASE "bail filler"', 'PHRASE "x y x y x y x y"']
    queries += ['PHRASE "' + " ".join(rng.choices(words + ["filler"], k=rng.randint(2, 8))) + '"' for _ in range(500)]
    return b_files(texts, queries + [f"BOOL {w}" for w in words])


def b_id_boundaries(rng):
    n = 70_000
    marks = {63, 64, 65, 511, 512, 513, 4095, 4096, 4097, 65535, 65536, 65537, n - 1}  # word, block and chunk edges
    texts = []
    for i in range(n):
        toks = ["all"] + ["even"] * (i % 2 == 0) + ["m64"] * (i % 64 == 63) + ["first"] * (i < 10)
        texts.append(b_join(rng, toks + ["last"] * (i >= n - 10) + ["mark"] * (i in marks)))
    queries = ["BOOL all", "BOOL even", "BOOL m64", "BOOL mark", "BOOL first OR last", "BOOL all NOT even",
               "BOOL all NOT all", "BOOL mark OR first OR last", "BOOL even AND m64", "BOOL all AND mark NOT even",
               "BOOL m64 OR mark AND even", "BOOL last NOT mark", "BOOL nothing OR mark", "BOOL nothing AND all",
               'PHRASE "all even"', 'PHRASE "all m64"', 'PHRASE "even m64"', 'PHRASE "all mark"']
    return b_files(texts, queries)


def b_check(files):
    docs = files["docs.txt"]
    assert docs.endswith(b"\n")
    for i, line in enumerate(docs[:-1].split(b"\n")):
        doc_id, tab, text = line.partition(b"\t")
        assert doc_id == str(i).encode() and tab and not re.search(rb"[\t\r\n]", text), line
        text.decode("utf-8")
        assert all(len(t) <= 32 for t in re.findall(rb"[a-z0-9]+", text.lower())), line
    queries = files["queries.txt"]
    assert queries.endswith(b"\n")
    for line in queries[:-1].split(b"\n"):
        assert B_QUERY.fullmatch(line), line


# ---- Track C (specs/c-join.md, sections 2.1 to 2.3) -----------------------------------------------
C_AMOUNT = re.compile(rb"-?(?:0|[1-9]\d*)\.\d\d")


def c_cents(cents):
    return f"{'-' if cents < 0 else ''}{abs(cents) // 100}.{abs(cents) % 100:02d}"


def c_text_field(rng, value):
    if value == "":
        return rng.choice(["", '""'])  # both spellings of the empty value
    return '"' + value.replace('"', '""') + '"'  # non-empty names and notes are always quoted


def c_name(rng, length, style=0):
    """Letters and single spaces, starting with a letter; style 1 adds a comma, style 2 an inner quote."""
    parts = []
    while len(" ".join(parts)) < length:
        parts.append("".join(rng.choice(string.ascii_letters) for _ in range(rng.randint(1, 8))))
    name = " ".join(parts)[:length].rstrip(" ")
    name += "x" * (length - len(name))
    if style == 1 and " " in name:
        name = name.replace(" ", ", ", 1)
    if style == 2 and name.count(" ") >= 2:
        first, inner, rest = name.split(" ", 2)
        name = f'{first} "{inner}" {rest}'
    return name


def c_note(rng, words, style=0):
    """Lowercase words separated by single spaces or LFs (at most 3, never first or last), with an
    optional comma, quoted word, lone quote, leading or trailing quote character, or trailing space."""
    ws = ["".join(rng.choice(string.ascii_lowercase) for _ in range(rng.randint(1, 9))) for _ in range(words)]
    if style == 1 and len(ws) > 2:
        ws[1] = f'"{ws[1]}"'
    if style == 2 and len(ws) > 1:
        ws[0] += ","
    if style == 3:
        ws[-1] += '"'
    if style == 4:
        ws[0] = '"' + ws[0]
    if style == 5 and len(ws) > 2:
        ws[1] += '"'
    note, lfs = ws[0], 0
    for w in ws[1:]:
        lf = lfs < 3 and rng.random() < 0.2
        lfs += lf
        note += ("\n" if lf else " ") + w
    return note + " " * (style == 6)


def c_files(rng, rows, regions, first_id=1, max_gap=11):
    """records.csv from (name, note, region_id, cents) rows: ids strictly increasing, terminators mixed,
    some BOMs at record starts (never right after the header), CRLF on the last record."""
    out, rid = ["\ufeffid,name,note,region_id,amount\r\n"], first_id
    for i, (name, note, region, cents) in enumerate(rows):
        bom = "\ufeff" if i > 0 and rng.random() < 0.05 else ""
        term = "\r\n" if i == len(rows) - 1 or rng.random() < 0.5 else "\n"
        out.append(f"{bom}{rid},{c_text_field(rng, name)},{c_text_field(rng, note)},{region},{c_cents(cents)}{term}")
        rid += rng.randint(1, max_gap)
    lookup = "region_id,country\n" + "".join(f"{r},{c}\n" for r, c in sorted(regions.items()))
    return {"records.csv": "".join(out).encode(), "regions.csv": lookup.encode()}


def c_field_lengths(rng):
    regions = {r: rng.choice(["AAA", "BBB", "CCC", "DDD"]) for r in sorted(rng.sample(range(1, 10_000), 2000))}
    keys, rows = list(regions), []
    for _ in range(4):
        for length in range(1, 81):
            for style in range(3):
                for trailing in (0, 1, 3):
                    note = c_note(rng, rng.randint(1, 12), rng.randrange(7))
                    rows.append((c_name(rng, length, style) + " " * trailing, note, rng.choice(keys), rng.randint(-99999, 9999999)))
    rows += [("", "", rng.choice(keys), 0) for _ in range(20)]
    rng.shuffle(rows)
    return c_files(rng, rows, regions)


def c_amounts(rng):
    regions = {1: "BIG", 2: "ZER", 3: "NEG", 4: "MIX", 999_999: "TOP"}
    edges = [0, 1, 9, 10, 99, 100, 999, 1000, 9999, 10000, 99999, 100000, 999999, 1000000, 9999999,
             -1, -9, -10, -99, -100, -999, -1000, -9999, -10000, -99999]
    rows = [(c_name(rng, 6), "", 4, cents) for cents in edges for _ in range(3)]
    rows += [(c_name(rng, 5), "", 1, 9999999) for _ in range(60_000)]  # a sum near 6e11 cents, past 2^32
    for _ in range(500):  # a country whose sum is exactly 0.00
        cents = rng.randint(1, 99999)
        rows += [("Zed", "", 2, cents), ("Zed", "", 2, -cents)]
    rows += [("Neg", "", 3, -rng.randint(1, 99)) for _ in range(300)]  # every amount in -0.99 to -0.01
    rows += [("Top", "", 999_999, 1) for _ in range(3)]  # the largest region id in the lookup
    rows += [("Lost", "", r, 5) for r in (5, 6, 999_998, 1_000_000, 9_999_999)]  # holes and above the range
    rng.shuffle(rows)
    return c_files(rng, rows, regions, first_id=39_000_000)  # 8-digit ids, still below 4 x 10^7


def c_one_record(rng):
    return c_files(rng, [("Solo", "only one", 7, 12345)], {7: "ONE"})


def c_distinct_names(rng):
    regions = {10: "AAA", 20: "BBB", 30: "CCC"}  # CCC has regions but no records, so no line
    rows = [(name, rng.choice(["", "a b", "x\ny"]), 10, 100)
            for name in ["Betts", "Betts ", "Betts  ", 'Ann "Annie" Lee', "Kaphar, Titus", "", "B"] for _ in range(3)]
    rows += [("", "", 20, 1), ("", "", 20, 2)]  # the empty name, written either way, counts once
    rows += [("Ghost", "", 999, 50) for _ in range(4)]  # an unknown region: _UNMATCHED
    rng.shuffle(rows)
    return c_files(rng, rows, regions)


def c_check(files):
    data = files["records.csv"]
    assert data.startswith(b"\xef\xbb\xbfid,name,note,region_id,amount\r\n") and data.endswith(b"\r\n")
    _, records = c_parse(data)
    assert records and not records[0][0], "no BOM may follow the header"
    prev = 0
    for _, body, _ in records:
        assert b"\r" not in body and b"\xef" not in body, body
        fields, pos = [], 0
        while True:
            m = FIELD_RE.match(body, pos)
            fields.append(m.group())
            pos = m.end()
            if pos == len(body):
                break
            pos += 1
        rid, name, note, region, amount = fields
        assert re.fullmatch(rb"[1-9]\d*", rid) and prev < int(rid) < 40_000_000, body
        assert re.fullmatch(rb"[1-9]\d*", region) and int(region) <= 9_999_999, body
        assert C_AMOUNT.fullmatch(amount) and amount != b"-0.00", body
        assert -99999 <= int(amount.replace(b".", b"")) <= 9999999, body
        assert name in (b"", b'""') or (name.startswith(b'"') and b"\n" not in name and name[1:2] != b" "), body
        assert note in (b"", b'""') or note.startswith(b'"'), body
        prev = int(rid)
    lines = files["regions.csv"].split(b"\n")
    assert lines[0] == b"region_id,country" and lines[-1] == b""
    ids = [int(line.split(b",")[0]) for line in lines[1:-1]]
    assert ids == sorted(set(ids)) and all(re.fullmatch(rb"[1-9]\d*,[A-Z]{3}", line) for line in lines[1:-1])


# ---- Track D (specs/d-traverse.md, section 2) -----------------------------------------------------
def d_files(n, edges, queries):
    return {"graph.txt": (f"{n} {len(edges)}\n" + "".join(f"{u} {v} {w}\n" for u, v, w in edges)).encode(),
            "queries.txt": "".join(f"{s} {t}\n" for s, t in queries).encode()}


def d_one_node(rng):
    return d_files(1, [], [(0, 0)] * 3)


def d_no_queries(rng):
    return d_files(3, [(0, 1, 5), (1, 2, 7)], [])  # zero query lines: an empty output


def d_weight_boundaries(rng):
    n = 3000
    special = [0, 1, 2047, 2048, 2049, 4095, 4096, 4097, 65535, 65536, 1048575, 1048576, U32_MAX - 1, U32_MAX]
    edges = [(rng.randrange(n), rng.randrange(n), rng.choice(special) if rng.random() < 0.3 else rng.randint(1, 500))
             for _ in range(12_000)]
    for _ in range(300):  # parallel pairs, lighter first or heavier first, plus an exact duplicate
        u, v = rng.randrange(n), rng.randrange(n)
        light, heavy = sorted(rng.sample(special, 2))
        edges += [(u, v, light), (u, v, heavy)] if rng.random() < 0.5 else [(u, v, heavy), (u, v, light)]
        edges.append(edges[-1])
    edges += [(i, i, rng.choice(special)) for i in rng.sample(range(n), 50)]  # self-loops
    rng.shuffle(edges)
    return d_files(n, edges, [(rng.randrange(n), rng.randrange(n)) for _ in range(2000)] + [(5, 5), (17, 17)])


def d_max_chain(rng):
    edges = [(i, i + 1, U32_MAX) for i in range(1000)]
    edges += [(i, i + 1, U32_MAX - 1) for i in range(0, 1000, 7)]  # a slightly lighter parallel edge
    edges += [(i, i + 2, 0) for i in range(500, 600, 3)]  # zero-weight shortcuts
    rng.shuffle(edges)
    queries = [(0, k) for k in range(0, 1001, 37)] + [(k, 0) for k in range(1, 1001, 101)] + [(0, 1000), (999, 1000)]
    return d_files(1001, edges, queries)  # distances up to about 1000 x 2^32


def d_unreachable_kinds(rng):
    """A strongly connected main body plus every unreachable kind in the spec: no-in sources with heavy
    edges, no-out targets, isolated nodes, and 14-node islands whose two-edge spur weighs near 2^32
    (the shape that makes min-key side alternation settle the whole main body)."""
    body, edges = 2000, []
    for u in range(body):
        edges += [(u, (u + 1) % body, rng.randint(1, 500)), ((u + 1) % body, u, rng.randint(1, 500))]
    edges += [(rng.randrange(body), rng.randrange(body), rng.randint(1, 4000)) for _ in range(4000)]
    n = body
    no_in, no_out, isolated = range(n, n + 3), range(n + 3, n + 6), range(n + 6, n + 9)
    n += 9
    for x in no_in:
        edges += [(x, rng.randrange(body), U32_MAX - rng.randrange(1 << 20)) for _ in range(2)]
    for x in no_out:
        edges += [(rng.randrange(body), x, rng.randint(1, 500)) for _ in range(2)]
    islands = []
    for _ in range(2):
        base = n
        n += 14
        islands.append(base)
        for i in range(12):  # a two-way ring of 12
            j = base + (i + 1) % 12
            edges += [(base + i, j, rng.randint(1, 50)), (j, base + i, rng.randint(1, 50))]
        edges += [(base, base + 12, U32_MAX - 3), (base + 12, base + 13, U32_MAX - 5)]  # the heavy spur
    rng.shuffle(edges)
    queries = []
    for s in islands:
        queries += [(s, rng.randrange(body)), (s + 3, rng.randrange(body)), (s, s + 13)]
    for x in no_in:
        queries += [(rng.randrange(body), x), (x, rng.randrange(body))]
    for x in no_out:
        queries += [(x, rng.randrange(body)), (rng.randrange(body), x)]
    for x in isolated:
        queries += [(x, rng.randrange(body)), (rng.randrange(body), x), (x, x)]
    queries += [(rng.randrange(body), rng.randrange(body)) for _ in range(200)]
    rng.shuffle(queries)
    return d_files(n, edges, queries)


def d_zero_weights(rng):
    n = 500
    edges = [(i, (i + 1) % 50, 0) for i in range(50)]  # a zero-weight cycle
    edges += [(i, i + 1, 0) for i in range(50, 300)]  # a long zero-weight chain
    edges += [(rng.randrange(n), rng.randrange(n), rng.choice([0, 0, 1, 2, 5000])) for _ in range(1500)]
    rng.shuffle(edges)
    return d_files(n, edges, [(0, 49), (49, 0), (50, 299), (299, 50), (10, 10)] + [(rng.randrange(n), rng.randrange(n)) for _ in range(500)])


def d_check(files):
    graph = files["graph.txt"]
    head, _, rest = graph.partition(b"\n")
    n, m = map(int, head.split())
    edges = rest.split(b"\n")[:-1] if rest else []
    assert graph.endswith(b"\n") and n >= 1 and len(edges) == m
    for line in edges:
        u, v, w = map(int, line.split())
        assert 0 <= u < n and 0 <= v < n and 0 <= w <= U32_MAX, line
    queries = files["queries.txt"]
    for line in (queries[:-1].split(b"\n") if queries else []):
        s, t = map(int, line.split())
        assert 0 <= s < n and 0 <= t < n, line


CASES = {  # track: (case number, name, builder); each case is banked as seed 200000 + case number
    "a": [(1, "empty-file", a_empty), (2, "every-endpoint-length", a_endpoint_lengths),
          (3, "percentile-ranks-and-digits", a_percentile_ranks), (4, "every-hour-boundary-reversed", a_hour_boundaries),
          (5, "all-1842-endpoints-x-72-hours", a_all_groups)],
    "b": [(1, "every-token-length", b_token_lengths), (2, "bool-chains-70k-docs", b_bool_chains),
          (3, "phrase-shapes", b_phrases), (4, "set-word-boundaries", b_id_boundaries)],
    "c": [(1, "name-note-lengths-and-quoting", c_field_lengths), (2, "amount-limits-and-big-sums", c_amounts),
          (3, "one-record", c_one_record), (4, "distinct-names-and-unmatched", c_distinct_names)],
    "d": [(1, "one-node", d_one_node), (2, "no-queries", d_no_queries), (3, "weight-boundaries", d_weight_boundaries),
          (4, "max-weight-chain", d_max_chain), (5, "unreachable-kinds", d_unreachable_kinds), (6, "zero-weights", d_zero_weights)],
}
CHECK = {"a": a_check, "b": b_check, "c": c_check, "d": d_check}


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("--tracks", nargs="+", default=list("abcd"), choices=list("abcd"))
    ap.add_argument("--force", action="store_true", help="rebuild cases that are already banked")
    args = ap.parse_args()

    bank_header, bank = read_tsv(BANK, 5)
    assert bank_header, f"no hash bank at {BANK}: run hash_bank.sh first"
    _, manifest = read_tsv(MANIFEST, 5)
    for track in args.tracks:
        for case, name, build in CASES[track]:
            seed = 200000 + case
            ddir = SD_HOME / "data" / f"s{seed}" / f"{track}-{SCALE}"
            expected = Path(f"{ddir}.expected") / OUTPUT[track]
            if not args.force and (seed, track, SCALE) in bank and expected.exists():
                continue
            files = build(random.Random(f"{track}{case}"))
            CHECK[track](files)  # the spec's input grammar, before anything is banked
            shutil.rmtree(ddir, ignore_errors=True)
            ddir.mkdir(parents=True)
            for fname, data in files.items():
                (ddir / fname).write_bytes(data)
            try:
                output, secs = run_reference(track, ddir)
            except subprocess.CalledProcessError as exc:
                print(f"FAILED   {track} {name}: the reference rejected it ({exc.returncode})")
                continue
            expected.parent.mkdir(exist_ok=True)
            expected.write_bytes(output)
            bank[(seed, track, SCALE)] = [hashlib.sha256(output).hexdigest(), f"{secs:.1f}"]
            manifest[(seed, track, SCALE)] = [str(case), name]
            size = sum(len(d) for d in files.values())
            print(f"ok       {track} seed {seed} {name}: input {size:,} B, output {len(output):,} B ({secs:.1f} s)")
    write_tsv(BANK, bank_header, bank)
    write_tsv(MANIFEST, "seed\ttrack\tscale\tcase\tname", manifest)
    print(f"bank: {len(bank)} rows; limit cases: {len(manifest)} in {MANIFEST}")


if __name__ == "__main__":
    main()
