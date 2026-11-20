# Q10 after the engine change, 2026-11-19

Change: `engine-runtime-corrections.patch` (on Omnigraph `d20eba74`):
1. `engine/graph.rs` `decide_expand_start`: the runtime re-costing prices the
   CSR by what this run built (`graph_index.is_built()`), not by the planner's
   `csr_cached` prediction.
2. `engine/operators/hash_join.rs` + `engine/lower.rs`: a HashJoin that
   declares `fallback: id_lookup` holds its probe (at most 4,096 rows and
   1/64 of the pool) and answers by id lookup when the probe ends below the
   planner's own rule (`rows × HASH_JOIN_RATIO < destination rows`); larger
   probes drain the build as planned.

A/B, interleaved runs of the baseline and patched `q10-harness` builds
(same profile), every run verified (3 ids). Load average 12–15 (other
cargo builds and tests ran on the machine; no quiet window).

| metric | baseline | patched | speedup |
|---|---|---|---|
| Q10 warm p50, 3 rounds × 50 | 165.3 / 157.6 / 160.8 ms | 8.5 / 8.0 / 7.9 ms | 20x |
| first query in process | 1430 / 1269 / 1283 ms | 40 / 20 / 22 ms | 58x |
| fresh process first query (index build) | 1268 / 1277 / 1301 ms | 27 / 22 / 20 ms | 59x |
| fresh process first query (after optimize) | 700 / 703 / 694 ms | 27 / 20 / 19 ms | 36x |
| Cypher-order Q10 warm p50 | 1202 ms | 1184 ms | 1.0x |

Ladybug on the same machine: ~2.1 ms (pytest-benchmark, 200 rounds).
First patch version held up to 1/4 of the pool and regressed the
Cypher-order query to ~4.5 s (build refused → id lookup over 1M probe rows);
the 4,096-row / 1/64-pool cap fixed it.

## Step 2: drop reads of unused traversal destinations (planner)

`planner-drop-unused-destination-reads.patch`: `projection_pushdown` removes a
destination `TableScan` whose binding is never filtered, returned or read
beyond `__id` (named, directed, min_hops ≥ 1 Expand; not inside an AntiJoin
tree), because endpoint integrity (cascading deletes, load checks) already
guarantees the node exists; the plan validator accepts the Expand as the
binding's source. Q10 no longer reads Post or Comment.

Final A/B, baseline vs both steps (`all-engine-changes.patch`), interleaved,
every run verified, load average 10–14:

| metric | baseline | final | speedup |
|---|---|---|---|
| Q10 warm p50, 3 rounds × 50 | 163.0 / 163.1 / 162.3 ms | 5.6 / 6.1 / 7.7 ms | 27x |
| first query in process | 1504 / 1259 / 1270 ms | 110 / 17 / 17 ms | 75x |
| fresh process first query (index build) | 1240 / 1228 / 1262 ms | 17 / 14 / 18 ms | 72x |
| fresh process first query (after optimize) | 713 / 681 / 678 ms | 24 / 16 / 13 ms | 42x |
| Cypher-order Q10 warm p50 | 1198 ms | 849 ms | 1.4x |

Not run: the GQT and planner test suites (plan goldens that pin a destination
scan will need updating). Known side effects to check: the dropped scan no
longer caps later Expands' row estimates; the plan validator accepts an
Expand as a node binding's source.

## Step 3: reuse a CSR the runtime cache already holds

The Expand re-costing treats the CSR as free when this query built it or the
cross-query runtime cache already holds it for the same edge-table versions
(`RuntimeCache::cached_graph_index`, a non-building lookup;
`GraphIndexHandle::is_warm`). `all-engine-changes.patch` now holds all three
steps. Same binary, interleaved, `--prewarm q10_cypher_order` loading the CSR
before timing (models a long-running process with the CSR resident):

| Q10 warm, 3 rounds × 100 | p50 | p95 |
|---|---|---|
| indexed hops (no CSR resident) | 6.08 / 6.12 / 6.10 ms | 6.85 / 7.30 / 7.10 ms |
| CSR resident | 4.82 / 4.86 / 4.36 ms | 6.32 / 5.52 / 5.74 ms |

Cold paths are unchanged (no CSR resident → indexed hops).
