# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What Sandhi is

Sandhi is an open-source **AI usage gateway**: the junction every model call passes through, **counted and attributed**. It emits neutral **units** (tokens, cache-creation/cache-read split, GPU-seconds) and **never dollars** — pricing/billing is a downstream concern owned by the commercial AnvaiOps control plane. Keep this measure-vs-price boundary intact: nothing in this repo should emit dollars, tiers, or SKU names. See `docs/adr/0001-sandhi-architecture-and-wire-contract.md`.

A Rust core exposed two ways: **in-process** via PyO3/napi bindings, and as an **inline reverse-proxy** that holds the real upstream key server-side while clients present per-user **virtual keys**.

## Commands

```bash
# Core workspace (crates/*) — what CI gates on:
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo test --workspace <name>        # run a single test by substring
cargo test -p sandhi-core            # one crate
cargo llvm-cov --workspace --fail-under-lines 75 --ignore-filename-regex 'src/generated/'   # coverage gate (≥75%)

# Run the proxy (persist + serve dashboard at /dashboard):
SANDHI_STORE=usage.db cargo run -p sandhi-proxy --bin sandhi-proxy
cargo run -p sandhi-proxy --bin sandhi -- --help    # the `sandhi` operator CLI

# Bindings are SEPARATE cargo workspaces (own Cargo.toml/lock), not `--workspace` members:
cargo fmt --manifest-path bindings/python/Cargo.toml --check
cargo clippy --manifest-path bindings/python/Cargo.toml --all-targets -- -D warnings
maturin build -m bindings/python/Cargo.toml --out dist && python -m pytest bindings/python/tests/ -q
(cd bindings/node && npx napi build --platform && npm test)
scripts/coverage-bindings.sh python   # FFI-glue coverage, gated ≥85% (needs a pyo3-compatible venv, CPython 3.11–3.13)
scripts/coverage-bindings.sh node
```

Activate the git hooks once: `git config core.hooksPath .githooks` (commit-msg attribution check + pre-push `cargo fmt`).

## Architecture — the crate boundaries carry meaning

The workspace is split so the language bindings' wheels stay small and dependency-clean. Respect these boundaries:

- **`sandhi-core`** — the metering engine. Neutral units only: usage accounting incl. the prompt-cache split (`usage.rs`), virtual-key resolution (`keys.rs`), budget/rate-limit enforcement (`budget.rs`), the `UsageEvent` wire type + `Sink` (`event.rs`, `sink.rs`), and the versioned provider-neutral chat contract (`chat.rs` → `ChatRequestV1`/`ChatResponseV1`/`UsageV2`). **No transport dependencies** — no `reqwest`, no HTTP. The usage *parsers* live here (not in `sandhi-providers`) because they are metering primitives.
- **`sandhi-providers`** — unified provider transport. One `Provider` adapter per family (Anthropic, OpenAI-compatible ≈20 providers, Gemini, Cohere, Ollama/local, OpenAI Responses); Bedrock is parser-only until SigV4 lands. Layered patterns: **adapter** (per provider) → **decorator** (`MeteredProvider`, `ResilientProvider` = circuit-breaker + retry + timeout) → **typed runtime** (`typed.rs`: `ProviderRuntime`/`ProviderHandle` normalize the neutral `ChatRequestV1` through per-family codecs). Adapters return raw `ParsedUsage` + the response; **the caller assembles the `UsageEvent`** with request id / timestamp / attribution — adapters never fabricate those. `catalog.rs` holds stable *transport* facts (slug, aliases, endpoint routing) only — not a model/capability catalog.
- **`sandhi-store`** — durable SQLite sink + usage-aggregation queries, plus the TD-0003 operator tables: the credential **`vault`** (metadata in SQLite; secrets in the OS keyring / SentinelPass, selected by `SANDHI_VAULT_BACKEND`) and the **`vkeys`** virtual-key store (hashes + scope, rehydrated on startup). Kept out of `sandhi-core` so binding wheels never bundle SQLite.
- **`sandhi-proxy`** — the inline egress gate. Two binaries: `sandhi-proxy` (the axum server) and `sandhi` (the operator CLI, a thin HTTP client to `/admin/*`). Request flow: resolve virtual key → budget reserve → **decode ingress dialect to `ChatRequestV1` → typed runtime re-encodes to the upstream → decode/re-encode the response** → emit one `UsageEvent` → reconcile budget. `operator.rs` is the admin API + startup key rehydration.

Data-flow invariant: **attribution rides outside the cached prompt** — `subject_id`/`group_id`/`session_id` live in `RequestMetadataV1`, never in the wire body. Preserve `session_id` end-to-end; never flatten users to one session.

Two proxy-path caveats that contradict older prose — see [ADR-0004](docs/adr/0004-two-plane-proxy-and-enforcement-boundary.md):
- The proxy does **not** currently forward byte-exact. `metered_passthrough` (`sandhi-providers/src/lib.rs`) is the O(1) byte-passthrough primitive at the *adapter* layer; the proxy's typed ingress pipeline re-encodes instead. ADR-0004 re-draws this as a two-plane design (transparent metering for same-dialect, opt-in translation for cross-family). There is **no Gemini/Cohere ingress dialect** yet — only `/v1/chat/completions`, `/v1/messages`, `/v1/responses`.
- Enforcement is **proxy-only** and runs on the ADR-0005 **lease ledger** (`ProxyLedger` in `sandhi-proxy/src/ledger.rs`): reserve a conservative *ceiling* → settle by lease id against the cache-inclusive `billable()` (D4). It is **durable + crash-safe when `SANDHI_STORE` is set** — spend, caps, and in-flight leases survive a restart, spend is measured over calendar-aligned windows (D5), and dangling leases are reclaimed (D2) — and volatile in-memory otherwise. Caps honor daily/monthly/total **windows** + a block/**warn** policy (`Warn` is a soft cap: admits over the limit, still tracks spend for **alerts**, TD-0003 P2), with per-tier **fail-open/closed** on a backend error (D6). The per-key **model allowlist is enforced** (`vk.permits_model`, P4). Still open: **per-minute rate limits** are stored but not enforced, and a shared/HA (Redis) ledger backend. The P4 dashboard read endpoints are **unauthed by design** (masked-only, self-hosted trust). Declarative policy over this substrate is TD-0005.

## Codegen — never hand-edit generated files

- `crates/sandhi-core/src/generated/*.rs` are produced by `scripts/gen-provider-models.sh` (typify, run as a standalone CLI so the shipped crate never links typify) from byte-pinned schemas in `crates/sandhi-core/schemas/`. Edit the schema and regenerate. These are excluded from coverage.
- `schemas/*.v1.schema.json` (the wire contract, e.g. `usage-event.v1`, `chat-*.v1`) are exported from Rust+schemars via `scripts/gen-chat-contract-schemas.sh`. Rust is authoritative.
- The Python/TS contract facades (`bindings/*/…`) are generated by `scripts/gen-binding-contract-facades.py`; each embeds a schema digest.
- CI job **`codegen-drift`** regenerates all three and fails on any `git diff`. After touching a schema or contract type, rerun the generators and commit the output.

## Conventions

- **No AI-agent authorship attribution** in commits/PRs — no `Co-Authored-By: Claude/…`, no "Generated with", no robot emoji, no model signature. Enforced by the `commit-msg` hook **and** server-side CI (not bypassable). (Mentioning `CLAUDE.md` or the Anthropic/OpenAI *APIs* is fine.)
- PRs target **`develop`** (protected; the aggregate **`CI Success`** check must be green). `main` is the release trunk.
- New behavior lands with tests; line coverage must stay ≥75%. Decisions go in `docs/adr/NNNN-slug.md`; larger technical designs in `docs/td/TD-NNNN-*.md`.
- CI is path-filtered: docs-only changes skip compile/coverage/bindings but `CI Success` still reports.
