#!/usr/bin/env python3
"""Interleaved A/B benchmark of baseline vs patched q10-harness. Stdlib only.
Env: BASELINE_HARNESS PATCHED_HARNESS GRAPH GRAPH_OPT QUERY_FILE RESULTS_ROOT SMOKE=1
"""
import json, os, platform, statistics, subprocess, sys, datetime

E = os.environ
BASE, PATCH = E["BASELINE_HARNESS"], E["PATCHED_HARNESS"]
G, GOPT, QF = E["GRAPH"], E["GRAPH_OPT"], E["QUERY_FILE"]
SMOKE = E.get("SMOKE") == "1"
IDS = "6597069775789,8796093029267,24189255819727"
stamp = datetime.datetime.now(datetime.timezone.utc).strftime("%Y%m%dT%H%M%SZ")
OUT = os.path.join(E["RESULTS_ROOT"], stamp)
os.makedirs(OUT, exist_ok=True)
raw = open(os.path.join(OUT, "raw.jsonl"), "w")


def n(full, small):
    return small if SMOKE else full


def run(binary, graph, query, extra, label):
    cmd = [binary, graph, QF, query, "--expect-ids", IDS] + extra
    p = subprocess.run(cmd, capture_output=True, text=True)
    if p.returncode != 0:
        sys.exit(f"FAILED ({label}): {' '.join(cmd)}\n{p.stdout}\n{p.stderr}")
    line = next((l for l in p.stdout.splitlines() if l.startswith("JSON ")), None)
    if line is None:
        sys.exit(f"no JSON line ({label}):\n{p.stdout}")
    d = json.loads(line[5:])
    raw.write(json.dumps({"label": label, "cmd": cmd, "result": d}) + "\n")
    raw.flush()
    return d


def med(xs):
    return statistics.median(xs) if xs else float("nan")


rows = []


def ab(name, query, graph, rounds, extra, pick):
    res = {"baseline": [], "patched": []}
    for r in range(rounds):
        for side, b in (("baseline", BASE), ("patched", PATCH)):
            d = run(b, graph, query, extra, f"{name}/{side}/r{r}")
            res[side].append(pick(d))
    mb, mp = med(res["baseline"]), med(res["patched"])
    rows.append((name, res["baseline"], res["patched"], mb / mp if mp else float("nan")))


p50 = lambda d: d["stats"]["p50_ms"]
first = lambda d: d["first_ms"]
w = ["--warmup", str(n(5, 1)), "--iters", str(n(50, 2))]
ab("Q10 warm p50 (ms)", "q10", G, n(3, 1), w, p50)
ab("Q10 warm first (ms)", "q10", G, n(3, 1), w, first)
ab("Cypher-order warm p50 (ms)", "q10_cypher_order", G, 1,
   ["--warmup", str(n(1, 0)), "--iters", str(n(3, 1))], p50)
cold = ["--warmup", "0", "--iters", "0"]
ab("Fresh-process first query, sf1 (ms)", "q10", G, n(3, 1), cold, first)
ab("Fresh-process first query, sf1_opt (ms)", "q10", GOPT, n(3, 1), cold, first)

# Patched only: indexed vs CSR-resident.
wi = ["--warmup", str(n(10, 1)), "--iters", str(n(100, 2))]
idx, csr = [], []
for r in range(n(3, 1)):
    idx.append(p50(run(PATCH, G, "q10", wi, f"indexed/r{r}")))
    csr.append(p50(run(PATCH, G, "q10", wi + ["--prewarm", "q10_cypher_order"], f"csr/r{r}")))

fmt = lambda xs: ", ".join(f"{x:.2f}" for x in xs)
lines = ["| metric | baseline | patched | median speedup |", "|---|---|---|---|"]
for name, b, p, s in rows:
    lines.append(f"| {name} | {fmt(b)} | {fmt(p)} | {s:.2f}x |")
lines.append(f"| Patched Q10 p50, indexed vs CSR-resident (ms) | indexed: {fmt(idx)} | CSR: {fmt(csr)} | "
             f"{med(idx) / med(csr) if med(csr) else float('nan'):.2f}x |")
table = "\n".join(lines)
print(table)

def sh(c):
    try:
        return subprocess.run(c, shell=True, capture_output=True, text=True).stdout.strip()
    except Exception:
        return ""

facts = {
    "date_utc": stamp, "uname": " ".join(platform.uname()),
    "cpu": sh("sysctl -n machdep.cpu.brand_string 2>/dev/null || grep -m1 'model name' /proc/cpuinfo"),
    "memory": sh("sysctl -n hw.memsize 2>/dev/null || grep MemTotal /proc/meminfo"),
    "loadavg": os.getloadavg(), "smoke": SMOKE,
}
json.dump(facts, open(os.path.join(OUT, "machine.json"), "w"), indent=2)
open(os.path.join(OUT, "table.md"), "w").write(table + "\n")
print(f"\nresults: {OUT}")
