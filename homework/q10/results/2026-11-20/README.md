# Cheaper graph-index dictionaries, 2026-11-20

Change (commit `642f9ae3`, after the Q10 work): the CSR/CSC dictionary
(`graph_index::TypeIndex`) hashes ids with `ahash` instead of SipHash, is
sized from manifest row counts (node count, capped by the incident edge
rows), and stores ids as `SmolStr` (inline up to 23 bytes, so these ids never
allocate). The persisted artifact format is unchanged. Q10 itself builds no
CSR; this targets queries that do.

Variants, same flags and toolchain, only `omnigraph-engine` and the harness
rebuilt:
- `v0`: the four Q10 changes (`6c76977e`).
- `A`: `v0` plus ahash and pre-sizing, keys still `String` (`vA-ahash-presize.patch`).
- `B`: `A` plus `SmolStr` ids (= `642f9ae3`).

## Method

- Quiet window 20:11–20:14 CST: no cargo or rustc on the machine.
  Uncontrolled background load: PerfPowerServices ~49%, WindowServer ~48%,
  a VM ~32%, Docker ~13%; load average 7–10.
- Every run is a fresh process and must return the 3 expected ids.
- Cypher-order Q10 (`q10_cypher_order`, starts from all 2M comments, so it
  builds or loads the CSR): `--warmup 0 --iters 3`. `first_ms` includes the
  CSR build (`sf1.omni`) or the persisted-artifact load (`sf1_opt.omni`);
  `first − warm` subtracts the median of the 3 following runs. Peak RSS from
  `/usr/bin/time -l`.
- 5 rounds, variant order rotated each round (`csr_ab.py 5`). Medians below;
  per-round values in `csr_ab_raw.jsonl`.

## Results

| Cypher-order, fresh process | v0 | A | B |
|---|---|---|---|
| `sf1` first query | 1,723 ms | 1,378 ms | 1,354 ms |
| `sf1` first − warm (≈ CSR build) | 997 ms | 695 ms (−30%) | 666 ms (−33%) |
| `sf1_opt` first − warm (≈ artifact load) | 491 ms | 464 ms (−5%) | 406 ms (−17%) |
| warm median (CSR resident) | 726 ms | 683 ms | 688 ms |
| peak RSS `sf1` / `sf1_opt` | 572 / 520 MB | 561 / 520 MB | 489 / 450 MB |

Hashing and pre-sizing carry most of the build gain; `SmolStr` carries most of
the load gain (the load inserts each id once, so the per-id allocation is a
larger share) and the memory drop (one heap copy fewer per id).

## Control: Tag-first Q10 (builds no CSR)

| warm p50 | v0 | A | B |
|---|---|---|---|
| in the 5-round run (`--warmup 5 --iters 50`) | 4.85 ms | 4.80 ms | 5.20 ms |
| separate 10-round run (`--warmup 5 --iters 100`, `q10_control_raw.jsonl`) | 4.75 ms | 5.03 ms | 5.08 ms |

Inconclusive: −1% to +7% across the two runs. No mechanism was found (the
indexed path creates 3–5 dictionaries of a few dozen ids per query). A code
layout shift from recompiling the engine (release builds are not incremental;
16 codegen units, thin LTO) is the working guess, not verified. The Q10 table
in the report was measured on `v0`.
