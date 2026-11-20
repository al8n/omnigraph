# LDBC SNB Q10 on Omnigraph: benchmark and engine fixes

**Result.** On SF1, Q10 went from 163 ms to 6.1 ms warm (27×) and from 1.27 s
to 17 ms as a process's first query (75×), returning the same 3 people on every
run. Ladybug takes 2.1 ms on the same machine; most of the remaining 2.3–2.9×
is per-read overhead.

**Setup.** M1 Max (10 cores, 32 GB), macOS, AC power; LDBC SNB Interactive v1
SF1. Omnigraph 0.15.1 at `d20eba74`, release profile, holding the 7 tables Q10
reads (`load csv`, `index build`); Ladybug 0.22.1 built by the benchmark repo's
script. Q10 is written Tag-first in `.gq`, with a literal Cypher-order copy as a
regression guard. The expected answer comes from the raw CSVs (tag 71 → 4 posts
→ 21 comments → 3 people).

**Method.** A harness (`crates/q10-harness`) times only `Session::query`,
results materialized as Arrow: 5–10 warm-ups, 50–200 runs, median. First
queries run in fresh processes; every run asserts the 3 ids. Baseline and
patched binaries run interleaved (A/B/A/B), so load drift hits both. Compiles
were paused for the baseline and CSR-dictionary windows; the A/B runs shared
the machine (load average 10–14), and background load is recorded with the
results. Ladybug uses the repo's pytest-benchmark flags with 200 rounds.
Diagnosis: `explain` with per-node actuals, Instruments Time Profiler.

**Findings.**
1. Parse, compile and plan were under 1% of the baseline time.
2. `name = "Cate_Blanchett"` is not a key, so it is estimated at all 16,080 Tags
   (actual 1); downstream estimates reach 724k and 1.4M (actual 4 and 21). Every
   Expand is planned as CSR, and `$post` is joined by a HashJoin that reads all
   1,003,605 Posts for 4 matches: the warm cost.
3. The plan declares runtime corrections for exactly this
   (`alternatives: [indexed_scan]`, `fallback: id_lookup`); both missed. The
   Expand re-costing priced the CSR as free on the planner's prediction that an
   earlier Expand would build it, but that Expand had switched to indexed, so
   hops 2–3 built a 3.8M-edge CSR for frontiers of 4 and 21 (1.1 s, mostly
   hashing and allocating String ids). The HashJoin falls back only on a memory
   refusal.
4. `@unique` never reaches the cost model: the engine's `PlanSource` never
   overrides `is_unique_property`.
5. `$post` and `$c` are read only to confirm they exist, which endpoint
   integrity (insert and load checks, cascading deletes) already guarantees.

**Changes** (engine and planner, ~400 lines, no format or API change):
1. Expand re-costing prices the CSR by what the run has built, as
   `docs/dev/execution.md` describes, not by the plan's prediction.
2. A HashJoin declaring `id_lookup` holds a small probe (≤ 4,096 rows, ≤ 1/64
   of the pool). A probe that ends below the planner's own threshold (rows × 8 ≥
   destination rows) is answered by id lookup; a larger one builds the hash as
   planned. A first version holding up to 1/4 of the pool slowed the
   Cypher-order query 4×; the A/B caught it.
3. The planner drops destination reads nothing uses (unfiltered, only `__id`
   demanded, one named directed edge type, outside `not {}`).
4. Re-costing also counts a CSR held by the cross-query cache as free, so a
   long-running process walks the hops in memory. This reads process state,
   outside the documented rule that re-decisions use only plan inputs and run
   observations: rows are unchanged, but a replay can record a different
   `ran`. Recording residency on the plan at planning time would keep the rule.
5. Graph-index dictionaries use ahash, manifest-count pre-sizing and `SmolStr`
   ids: the SF1 CSR builds 33% faster (997 → 666 ms) and loads 17% faster after
   `optimize`, with 70–80 MB less memory. Q10 itself builds no CSR.

| Q10, median (changes 1–4) | baseline | after | speedup |
|---|---|---|---|
| warm, in process | 163 ms | 6.1 ms | 27× |
| warm, CSR resident (long-running process) | 163 ms | 4.8 ms | 34× |
| first query in a process | 1.27 s | 17 ms | 75× |
| first query after `optimize` | 680 ms | 16 ms | 42× |
| Cypher-order (roots at all 2M comments), warm | 1.20 s | 0.85 s | 1.4× |
| Ladybug, same machine | 2.1 ms | | |

**Not verified.** Only Q10 and its Cypher-order variant were measured, and the
GQT, planner and engine suites were not run: plan goldens that pin a
destination scan will need updating, and change 4 may trip the plan-replay
tests.

**Next.**
- **Per-read overhead** (the remaining 2.3–2.9×): 5 Lance reads of ~1 ms each
  (2 with the CSR resident). Each small lookup builds a Scanner and a
  DataFusion plan (~20% of CPU), reopens files (~8%) and hops to the blocking
  pool (~22%). BTREE → row ids → `take_rows` for id lookups and indexed hops,
  inline decoding of small batches and reader reuse: about 800–1,500 lines with
  tests.
- **Selectivity** for non-key equality (distinct or index-backed counts) and
  `@unique` in the estimate: the root cause of finding 2.
- **CSR:** one copy per id (a byte arena with a `HashTable<u32>`), and an
  equality index for String properties (`@index` on a String builds a
  full-text index).
- **Hardening:** run the suites, add a GQT case per runtime switch, and make
  the harness an `omnigraph-bench` scenario.

---

## Reproduce

Everything below runs from a checkout of this branch (`homework/q10/` is the folder holding this file).

**Prerequisites:** rustup (`rust-toolchain.toml` pins 1.97.1), `protoc`, `git`, `curl`, `zstd`, `python3`, and `uv` (only for the Ladybug comparison).

```
homework/q10/scripts/reproduce.sh all
```

All work happens under `WORK` (default `./work`). Rough cost: two release builds (baseline and patched), about 30 to 60 minutes each; about 3 GB for the dataset and graphs; the full pipeline takes hours.

Single steps, each skips work already done where it can:

- `reproduce.sh fetch`: creates `baseline` and `patched` git worktrees (baseline is the commit titled `q10-harness:`, i.e. the unmodified engine plus the timing tool; override with `BASELINE_REV`), clones `graph-benchmark-ldbc` at a pinned commit, downloads and sha256-verifies the LDBC SF1 CSVs.
- `reproduce.sh build`: release builds of `omnigraph` and `q10-harness` for both trees (`CARGO_BIN`, `TARGET_ROOT` override cargo and target dirs).
- `reproduce.sh load`: builds `sf1.omni` and an optimized copy `sf1_opt.omni` with the baseline binary, printing seconds per step.
- `reproduce.sh bench`: runs `scripts/bench.py`, which interleaves baseline and patched runs (A/B/A/B) to reduce drift. It prints a markdown table (metric, baseline values, patched values, median speedup) and writes raw JSON lines and machine facts (uname, CPU, memory, load, date) to `$WORK/results/<UTC timestamp>/`. `SMOKE=1` uses tiny iteration counts for a quick check.
- `reproduce.sh ladybug`: creates a Python 3.13 venv with hash-pinned wheels (`scripts/ladybug-requirements.txt`; honors `UV_INDEX_URL`), builds the Ladybug graph and runs its Q10 benchmark.

Absolute numbers vary by machine, disk and load; compare the baseline/patched ratios from the same run, which the A/B interleaving is designed to make meaningful. The patched tree is the branch head, so it includes change 5, which the report's Q10 table predates (`results/2026-11-20/` holds its Q10 control runs).

## What is where

Branch commits: the `q10-harness` timing tool; Expand re-costing (changes 1 and 4); the HashJoin small probe (change 2); the planner's destination drop (change 3); graph-index dictionaries (change 5); this folder.

- `inputs/`: schemas (`ldbc_q10.pg`, `ldbc_q10_unique.pg`) and queries (`q10.gq`, `explain_q10.gq`, `explain_q10_cypher_order.gq`).
- `scripts/`: `reproduce.sh`, `bench.py`, `ladybug-requirements.txt`.
- `results/`: raw results and patches from 2026-11-18 to 2026-11-20 (profiler `.trace` bundles omitted for size).
