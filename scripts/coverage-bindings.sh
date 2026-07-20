#!/usr/bin/env bash
# Line-coverage for the FFI *binding glue* (bindings/<lang>/src/lib.rs).
#
# The main `coverage` CI job runs `cargo llvm-cov --workspace` over crates/ — but the bindings are
# separate cargo workspaces built by maturin/napi and driven by a *foreign* runtime (Python/Node),
# so their glue never appears in that number. This script instruments the cdylib, runs the binding's
# own test harness (which loads the instrumented extension and emits .profraw), and reports coverage
# for the glue file alone — then fails under a floor. Usage:
#
#     scripts/coverage-bindings.sh python   # needs maturin on PATH (or in a venv)
#     scripts/coverage-bindings.sh node     # needs npm on PATH
#
# Requires: cargo-llvm-cov + the llvm-tools-preview component (same as the core coverage job).
set -euo pipefail

LANG_ARG="${1:-}"
FLOOR="${COVERAGE_FLOOR:-85}"   # minimum line coverage % for the glue file
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

# Ignore everything that isn't the binding glue: dependency sources, the rust stdlib, and the
# workspace crates/ (those are gated by the separate `coverage` job). What's left is lib.rs.
IGNORE='(registry|rustc|/crates/|library/std)'

case "$LANG_ARG" in
  python)
    DIR="$ROOT/bindings/python"
    # `maturin build` + `pip install` (not `develop`): works with the active interpreter whether or
    # not a virtualenv is active (CI uses system Python, so `develop` — which requires a venv —
    # would fail). The instrumented .so still lands in target/ where llvm-cov finds it.
    BUILD() {
      rm -rf target/cov-dist
      maturin build --out target/cov-dist
      pip install --force-reinstall --no-deps target/cov-dist/*.whl
    }
    RUN() { python tests/test_gateway.py; }
    ;;
  node)
    DIR="$ROOT/bindings/node"
    BUILD() { npm run build:debug; }
    RUN() { node --test __test__/gateway.test.mjs; }
    ;;
  *)
    echo "usage: $0 <python|node>" >&2
    exit 2
    ;;
esac

cd "$DIR"

# Export instrumentation env (RUSTFLAGS=-C instrument-coverage, LLVM_PROFILE_FILE, …) into THIS shell
# so the maturin/napi build (which shells out to cargo) produces an instrumented extension.
# shellcheck disable=SC1090
eval "$(cargo llvm-cov show-env --export-prefix)"

cargo llvm-cov clean --workspace
BUILD
RUN   # loads the instrumented cdylib → writes .profraw next to it

echo "== binding glue coverage ($LANG_ARG, floor ${FLOOR}% lines) =="
cargo llvm-cov report --ignore-filename-regex "$IGNORE"
cargo llvm-cov report --ignore-filename-regex "$IGNORE" --fail-under-lines "$FLOOR"
