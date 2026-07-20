#!/usr/bin/env bash
# Regenerate the typify narrow models (ADR-0003 §2/§4 — TD-0001 W3).
#
# NEVER hand-edit the generated files (crates/sandhi-core/src/generated/*.rs). Edit the byte-pinned
# schema under crates/sandhi-core/schemas/ and rerun this. CI drift-checks that the committed output
# matches (.github/workflows/ci.yml → codegen-drift): regenerate here → `git diff --exit-code`.
#
# typify runs as a standalone CLI (never a cargo dependency of the shipped crate), so the shipped
# crate + bindings never link typify — only the committed output, which depends solely on serde
# (ADR-0003 §3). Pinned for reproducible output.
set -euo pipefail

CARGO_TYPIFY_VERSION="0.4.3"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"

if ! cargo install --list | grep -q "cargo-typify v${CARGO_TYPIFY_VERSION}"; then
  echo "installing cargo-typify v${CARGO_TYPIFY_VERSION} ..."
  cargo install cargo-typify --version "${CARGO_TYPIFY_VERSION}" --locked
fi

# schema (source of truth) -> generated Rust (never hand-edited)
declare -A MODELS=(
  ["crates/sandhi-core/schemas/anthropic_message_usage.schema.json"]="crates/sandhi-core/src/generated/anthropic_usage.rs"
)

for schema in "${!MODELS[@]}"; do
  out="${MODELS[$schema]}"
  echo "typify: ${schema} -> ${out}"
  cargo typify --output "${ROOT}/${out}" "${ROOT}/${schema}"
  rustfmt "${ROOT}/${out}"
done

echo "done — generated models are up to date."
