"""Interleaved A/B/C of the CSR-build variants. Stdlib only.

Each run is a fresh process; every run must return the 3 expected ids.
Cypher-order (builds or loads the CSR): first_ms, p50 of the next 3 runs,
peak RSS. Tag-first Q10 warm p50 is the control (it builds no CSR).
"""
import json, re, statistics, subprocess, sys

T = "/Volumes/rollroll/rust/targets/modernrelay-omnigraph/homework-main/variants"
VARIANTS = [
    ("v0", f"{T}/v0-four-changes/q10-harness"),
    ("A", f"{T}/vA-ahash-presize/q10-harness"),
    ("B", f"{T}/vB-smolstr/q10-harness"),
]
Q = "/Users/al/Developer/omigraph-homework/q10"
QF = f"{Q}/q10.gq"
IDS = "6597069775789,8796093029267,24189255819727"
ROUNDS = int(sys.argv[1]) if len(sys.argv) > 1 else 5
RAW = sys.argv[2] if len(sys.argv) > 2 else "csr_ab_raw.jsonl"


def run(binary, graph, query, extra):
    cmd = ["/usr/bin/time", "-l", binary, graph, QF, query, "--expect-ids", IDS] + extra
    p = subprocess.run(cmd, capture_output=True, text=True)
    if p.returncode != 0:
        sys.exit(f"FAILED: {' '.join(cmd)}\n{p.stdout}\n{p.stderr}")
    line = next(l for l in p.stdout.splitlines() if l.startswith("JSON "))
    d = json.loads(line[5:])
    m = re.search(r"(\d+)\s+maximum resident set size", p.stderr)
    d["max_rss_mb"] = int(m.group(1)) / 2**20 if m else float("nan")
    return d


res = {}
with open(RAW, "w") as raw:
    for r in range(ROUNDS):
        order = VARIANTS[r % 3:] + VARIANTS[: r % 3]
        for name, binary in order:
            for g in ("sf1", "sf1_opt"):
                d = run(binary, f"{Q}/{g}.omni", "q10_cypher_order", ["--warmup", "0", "--iters", "3"])
                key = (name, g)
                res.setdefault(key, []).append((d["first_ms"], d["stats"]["p50_ms"], d["max_rss_mb"]))
                raw.write(json.dumps({"round": r, "variant": name, "graph": g, "query": "q10_cypher_order", "result": d}) + "\n")
            d = run(binary, f"{Q}/sf1.omni", "q10", ["--warmup", "5", "--iters", "50"])
            res.setdefault((name, "q10"), []).append((d["stats"]["p50_ms"], float("nan"), d["max_rss_mb"]))
            raw.write(json.dumps({"round": r, "variant": name, "graph": "sf1", "query": "q10", "result": d}) + "\n")
            raw.flush()
        print(f"round {r} done", flush=True)

med = statistics.median
print("\nCypher-order, fresh process (median over rounds)")
print(f"{'variant':8}{'graph':9}{'first_ms':>10}{'warm_p50':>10}{'first-warm':>12}{'rss_MB':>9}   first_ms per round")
for g in ("sf1", "sf1_opt"):
    for name, _ in VARIANTS:
        xs = res[(name, g)]
        f, w, m = med(x[0] for x in xs), med(x[1] for x in xs), med(x[2] for x in xs)
        print(f"{name:8}{g:9}{f:10.0f}{w:10.0f}{f - w:12.0f}{m:9.0f}   {[round(x[0]) for x in xs]}")
print("\nControl: Tag-first Q10 warm p50 on sf1 (ms)")
for name, _ in VARIANTS:
    xs = res[(name, "q10")]
    print(f"{name:8}{med(x[0] for x in xs):8.2f}   {[round(x[0], 2) for x in xs]}")
