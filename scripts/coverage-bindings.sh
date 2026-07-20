#!/usr/bin/env bash
# Line-coverage for the FFI *binding glue* (bindings/<lang>/src/lib.rs).
#
# The main `coverage` CI job runs `cargo llvm-cov --workspace` over crates/ — but the bindings are
# separate cargo workspaces built by maturin/napi and driven by a *foreign* runtime (Python/Node),
# so their glue never appears in that number. This script instruments the cdylib, runs the binding's
# own test harness (which loads the instrumented extension and emits .profraw), and reports coverage
# for the glue file alone — then fails under a floor. Usage:
#
#     scripts/coverage-bindings.sh python   # run inside a pyo3-compatible venv (see below)
#     scripts/coverage-bindings.sh node     # needs npm on PATH + cargo-llvm-cov
#
# Python interpreter: the pyo3 0.22 abi3-py311 build needs CPython **3.11–3.13** with `maturin`
# installed. It uses the active interpreter (`python3`); override with `COV_PYTHON=/path/to/python`.
# The build wheel is force-installed into that interpreter, so use a **virtual environment**, never
# system Python — locally that is `~/code/.venv` (3.12): `source ~/code/.venv/bin/activate` first,
# or `COV_PYTHON=~/code/.venv/bin/python scripts/coverage-bindings.sh python`. (Bare system Python
# is often too new — e.g. 3.14 — and breaks the pyo3 build; the script guards the version.) In CI
# the setup-python step provides an isolated 3.12 as `python3`.
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
    PY="${COV_PYTHON:-python3}"
    # Fail fast if the interpreter can't build the pyo3 abi3-py311 extension (needs 3.11–3.13).
    PREFLIGHT() {
      command -v "$PY" >/dev/null || { echo "error: '$PY' not found (set COV_PYTHON)" >&2; exit 3; }
      local v; v="$("$PY" -c 'import sys; print("%d.%d" % sys.version_info[:2])')"
      case "$v" in
        3.11 | 3.12 | 3.13) ;;
        *) echo "error: Python $v unsupported for the pyo3 build (need 3.11–3.13); activate a venv like ~/code/.venv or set COV_PYTHON" >&2; exit 3 ;;
      esac
      "$PY" -m maturin --version >/dev/null 2>&1 || { echo "error: maturin not installed for $PY — 'pip install maturin' in your venv" >&2; exit 3; }
    }
    # Build the instrumented wheel with the venv's interpreter + force-install it there (not
    # `maturin develop`, which additionally requires the venv to be *activated*). Isolation comes
    # from the caller's venv, so this never touches system Python.
    BUILD() {
      rm -rf target/cov-dist
      "$PY" -m maturin build --out target/cov-dist
      "$PY" -m pip install -q --force-reinstall --no-deps target/cov-dist/*.whl
    }
    RUN() { "$PY" tests/test_gateway.py; }
    ;;
  node)
    DIR="$ROOT/bindings/node"
    PREFLIGHT() { :; }
    BUILD() { npm run build:debug; }
    RUN() { node --test __test__/gateway.test.mjs; }
    ;;
  *)
    echo "usage: $0 <python|node>" >&2
    exit 2
    ;;
esac

cd "$DIR"
PREFLIGHT

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
