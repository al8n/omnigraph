#!/usr/bin/env bash
# Reproduce the Q10 benchmark. Run from a checkout of the fork branch.
#   reproduce.sh fetch | build | load | bench | ladybug | all
# Prerequisites: rustup (rust-toolchain.toml pins 1.97.1), protoc, git, curl,
# zstd, python3, and uv (ladybug step only; python3.13 is fetched by uv).
# Env: WORK (default $PWD/work), BASELINE_REV, CARGO_BIN (default cargo),
# TARGET_ROOT (default $WORK/target), SMOKE=1 for tiny bench iteration counts.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
HW="$(cd "$SCRIPT_DIR/.." && pwd)"
REPO="$(git rev-parse --show-toplevel)"
WORK="${WORK:-$PWD/work}"
CARGO_BIN="${CARGO_BIN:-cargo}"
TARGET_ROOT="${TARGET_ROOT:-$WORK/target}"
GBL_REV=199852076ba8dceb345e18eeb2f8ac04c73f3ea1
DATA_URL=https://datasets.ldbcouncil.org/snb-interactive-v1/social_network-sf1-CsvBasic-StringDateFormatter.tar.zst
DATA_SHA=5d74f5b0b0570eb593b3317e14ffab3d5dd181efb7e2c947e0ed0409c377b23d
GBL="$WORK/graph-benchmark-ldbc"
G="$WORK/graphs/sf1.omni"
GOPT="$WORK/graphs/sf1_opt.omni"

sha256_of() { if command -v sha256sum >/dev/null; then sha256sum "$1" | cut -d' ' -f1; else shasum -a 256 "$1" | cut -d' ' -f1; fi; }

fetch() {
  mkdir -p "$WORK"
  local base="${BASELINE_REV:-$(git -C "$REPO" log -1 --format=%H --grep='^q10-harness:')}"
  [ -n "$base" ] || { echo "cannot find q10-harness: commit; set BASELINE_REV" >&2; exit 1; }
  [ -e "$WORK/patched" ]  || git -C "$REPO" worktree add --detach "$WORK/patched" HEAD
  [ -e "$WORK/baseline" ] || git -C "$REPO" worktree add --detach "$WORK/baseline" "$base"
  if [ ! -d "$GBL/.git" ]; then
    git clone https://github.com/prrao87/graph-benchmark-ldbc "$GBL"
    git -C "$GBL" checkout "$GBL_REV"
  fi
  if [ ! -d "$GBL/csv" ]; then
    mkdir -p "$WORK/data"
    local ar="$WORK/data/sf1.tar.zst"
    [ -f "$ar" ] || curl -fL -o "$ar" "$DATA_URL"
    [ "$(sha256_of "$ar")" = "$DATA_SHA" ] || { echo "sha256 mismatch for $ar" >&2; exit 1; }
    rm -rf "$WORK/data/extract"; mkdir -p "$WORK/data/extract"
    zstd -dc "$ar" | tar -xf - -C "$WORK/data/extract"
    local top; top="$(find "$WORK/data/extract" -mindepth 1 -maxdepth 1 -type d)"
    [ "$(printf '%s\n' "$top" | wc -l)" -eq 1 ] || { echo "expected one top dir" >&2; exit 1; }
    mv "$top" "$GBL/csv"
  fi
}

build() {
  local name
  for name in baseline patched; do
    ( cd "$WORK/$name" && RUSTFLAGS='' CARGO_TARGET_DIR="$TARGET_ROOT-$name" \
      "$CARGO_BIN" build --release --locked -p omnigraph-cli -p q10-harness \
      --config profile.release.strip=false \
      --config 'profile.release.debug="line-tables-only"' )
  done
}

step() { # step <label> <cmd...>
  local label="$1" t0; shift; t0=$(date +%s)
  "$@"; echo "[step] $label: $(( $(date +%s) - t0 ))s"
}

load() {
  local og="$TARGET_ROOT-baseline/release/omnigraph" csv="$GBL/csv" item f kind
  [ -x "$og" ] || { echo "missing $og (run build)" >&2; exit 1; }
  rm -rf "$G" "$GOPT"; mkdir -p "$WORK/graphs"
  step init "$og" init --schema "$HW/inputs/ldbc_q10.pg" "$G"
  for item in static/tag_0_0.csv:node:Tag dynamic/person_0_0.csv:node:Person \
      dynamic/post_0_0.csv:node:Post dynamic/comment_0_0.csv:node:Comment \
      dynamic/post_hasTag_tag_0_0.csv:edge:PostHasTag \
      dynamic/comment_replyOf_post_0_0.csv:edge:ReplyOfPost \
      dynamic/comment_hasCreator_person_0_0.csv:edge:CommentHasCreator; do
    f="${item%%:*}"; kind="${item#*:}"
    step "load $f" "$og" mutate --target "$G" \
      --set load_source_max_bytes=268435456 --set query_memory_bytes=8589934592 \
      -e "load csv \"file://$(cd "$csv" && pwd)/$f\" into $kind mode overwrite delimiter \"|\" header"
  done
  step "index build" "$og" index build --target "$G"
  cp -R "$G" "$GOPT"
  step optimize "$og" optimize --target "$GOPT"
}

bench() {
  BASELINE_HARNESS="${BASELINE_HARNESS:-$TARGET_ROOT-baseline/release/q10-harness}" \
  PATCHED_HARNESS="${PATCHED_HARNESS:-$TARGET_ROOT-patched/release/q10-harness}" \
  GRAPH="${GRAPH:-$G}" GRAPH_OPT="${GRAPH_OPT:-$GOPT}" \
  QUERY_FILE="${QUERY_FILE:-$HW/inputs/q10.gq}" RESULTS_ROOT="${RESULTS_ROOT:-$WORK/results}" \
    python3 -I "$SCRIPT_DIR/bench.py"
}

ladybug() {
  local py="$WORK/venv-ladybug/bin/python"
  if [ ! -x "$py" ]; then
    uv venv --python 3.13 "$WORK/venv-ladybug" || python3.13 -m venv "$WORK/venv-ladybug"
  fi
  local idx=(); [ -z "${UV_INDEX_URL:-}" ] || idx=(--index-url "$UV_INDEX_URL")
  uv pip install --python "$py" --only-binary :all: --require-hashes \
    ${idx[@]+"${idx[@]}"} -r "$SCRIPT_DIR/ladybug-requirements.txt"
  cd "$GBL/ladybugdb"
  [ -e ldbc_snb_sf1.lbdb ] || "$py" -I build_graph.py
  "$py" -I -m pytest benchmark_query.py -k test_benchmark_query10 -p no:faulthandler \
    --benchmark-min-rounds=200 --benchmark-min-time=0.000005 --benchmark-max-time=1.0 \
    --benchmark-timer=time.perf_counter --benchmark-calibration-precision=10 \
    --benchmark-warmup=off --benchmark-disable-gc
}

case "${1:-}" in
  fetch) fetch ;;
  build) build ;;
  load) load ;;
  bench) bench ;;
  ladybug) ladybug ;;
  all) fetch; build; load; bench; ladybug ;;
  *) echo "usage: $0 fetch|build|load|bench|ladybug|all" >&2; exit 2 ;;
esac
