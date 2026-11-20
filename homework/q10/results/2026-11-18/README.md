# Q10 baseline measurement, 2026-11-18

First measurement spike: Omnigraph runs LDBC SNB Q10 correctly, and its speed
is compared with Ladybug on the same machine and data. No engine change yet.

## Environment

- Apple M1 Max (10 cores), 32 GiB, macOS 27.0, on AC power.
- Quiet window: compiles on the machine were paused
  (22:41–22:46 CST); no cargo or rustc ran during the timed runs.
- Uncontrolled background load stayed: a long-running Python process (one
  core at 100%), WindowServer (~50%), Spotlight indexing (mds/mdworker),
  a VM (~16–27%). Load average 8–23. OS page cache warm in every run.
- Omnigraph 0.15.1 at `d20eba74` (2026-11-18), Rust 1.97.1, release profile
  (opt-level 2, thin LTO, 16 codegen units) built with `RUSTFLAGS=` empty plus
  `strip=false` and `debug="line-tables-only"` so the profiler has symbols.
- graph-benchmark-ldbc at `19985207` (2026-11-15). Ladybug 0.22.1,
  Python 3.13.14, polars 1.37.1, pyarrow 23.0.0, pytest 9.0.2,
  pytest-benchmark 5.2.3: wheels verified against the repo's `uv.lock` hashes.

## Data

`social_network-sf1-CsvBasic-StringDateFormatter.tar.zst` (sha256
`5d74f5b0b0570eb593b3317e14ffab3d5dd181efb7e2c947e0ed0409c377b23d`).
Omnigraph loads only the 7 tables Q10 reads (`../ldbc_q10.pg`); Ladybug
loads all 31 files with the repo's `build_graph.py`.

| Omnigraph `load csv` (overwrite) | rows | time |
|---|---|---|
| Tag | 16,080 | 0.05 s |
| Person | 9,892 | 0.05 s |
| Post | 1,003,605 | 1.89 s |
| Comment | 2,052,169 | 0.83 s |
| PostHasTag | 713,258 | 0.25 s |
| ReplyOfPost | 1,011,420 | 0.33 s |
| CommentHasCreator | 2,052,169 | 0.45 s |
| `index build` (18 indexes) | | 4.31 s |

The Post and Comment loads need `--set query_memory_bytes` above the 150 MiB
default (Comment needs about 2.3 GB); a smaller budget is refused before any
effect. Ladybug's full build (31 files + 6 ART indexes) took 10.0 s.

## Correctness

Computed straight from the CSVs: tag 71 (`Cate_Blanchett`) → 4 posts →
21 direct comments → 3 creators. Both engines return exactly
6597069775789, 8796093029267, 24189255819727; every timed run was verified.

## Timings (ms)

| What | median | other |
|---|---|---|
| Omnigraph, in-process warm, Tag-first `q10`, n=200, trial 1 | 148.1 | mean 150.3, min 128.8, p95 177.7 |
| same, trial 2 | 140.6 | mean 143.3, min 122.5, p95 165.5 |
| Omnigraph, first query in a fresh process (incl. CSR build) | 1,276 / 1,346 | open 4–5 ms |
| Omnigraph, Cypher-order `q10_cypher_order`, warm, n=20 | 1,144 | first query 2,214 |
| Omnigraph CLI, fresh process per run, n=10 | 1,253 | min 1,229 |
| Omnigraph `explain` only, first in process (compile + gather + plan) | 9.2 | |
| Ladybug, repo's pytest-benchmark flags (5 rounds) | 2.59 | mean 2.64 |
| Ladybug, same with min 200 rounds, trial 1 / trial 2 | 2.08 / 2.11 | mean 2.11 / 2.18 |
| Ladybug, execute + get_as_arrow + close, n=500 | 2.65 | p95 2.95; first after open+ANALYZE 78.5 |

Timer boundaries differ: the Omnigraph harness (`crates/q10-harness` in the
local clone) times only `Session::query` (results materialized as Arrow
batches); Ladybug's pytest timer also covers polars conversion and `print`.

## What the engine does (explain + per-node actuals)

- Root is the Tag scan (first declared binding). `Tag.name @index` built a
  full-text index, so the equality filter scans sequentially (16,080 rows).
- The estimate for `name = "Cate_Blanchett"` stays at the full table (16,080):
  only `@key`/`@unique` equality is estimated as one row. Frontier estimates
  grow to 723,600 and 1,447,200 (real: 1, 4, 21), so every Expand is planned
  as `csr` and `$post` is joined with a HashJoin over the whole Post table.
- Actuals: the first hop switched to `indexed_scan` at run time (4 rows);
  the Post hash-join build side read 1,003,605 rows for 4 matches; the other
  two hops ran on CSR (21 rows each); Person was read whole (9,892 rows).

## Where the time goes (Instruments Time Profiler, `profile_*_summary.txt`)

- Warm query (~145 ms): execution, not planning. Scans 38%, Lance decoding
  29% (FSST string decompression 10%), hash join 9%, thread hand-off
  (`cvwait`/`cvsignal`) ~29%; Expand on the cached CSR 0.4%; planner,
  compiler and snapshot capture together under 1%.
- First query (~1.3 s): building the CSR inside the Expand operator (~69%),
  dominated by string ids: SipHash of `String` keys (~21%), `memcmp` (8%),
  malloc/free (~13%), Lance string decoding (~11%). Dropping the cached CSR at
  process exit costs another ~7%.
