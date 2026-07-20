<!-- Sandhi — the metering layer for AI agents -->

# Sandhi

**The metering layer for AI agents.** *(Sanskrit संधि — "junction": where forms meet and combine.)*

Sandhi is an open-source **AI usage gateway** — the junction every model call passes
through, **counted and attributed**. Meter every call, know who spent what across a shared
key, and set per-user budgets — without hand-rolling provider APIs.

> **Sandhi measures; the commercial layer prices.** Sandhi emits neutral **units** (tokens,
> cache split, GPU-seconds) and never dollars — pricing/billing is a separate, downstream
> concern. See [ADR-0001](docs/adr/0001-sandhi-architecture-and-wire-contract.md).

- **License:** Apache-2.0
- **Status:** pre-alpha (design-complete; bootstrap = layout + wire schema + ADRs)
- **Packages:** crate `sandhi` · PyPI `sandhi-gateway` · npm `@anvai-labs/sandhi`

## Why

Teams that share one provider API key on an internal network can't answer *"who spent
what,"* can't budget per person, and can't rate-limit a runaway user. And every framework
re-implements the same provider adapters + usage parsing — the exact place metering goes
wrong. Sandhi is the single, fast, neutral implementation of both.

## What it does

- **Virtual keys** — one shared upstream key fronts many per-user keys; attribution and
  revocation are per person, not per shared secret.
- **Per-user / per-team attribution** — every call tagged with `subject_id` / `group_id`.
- **Budgets + rate limits** — per virtual key / team.
- **Unified provider transport** — Anthropic, OpenAI-compatible (covers ~20 providers),
  Gemini, Bedrock, Cohere, local vLLM/Ollama — streaming, pooling, retry, circuit-breaker,
  with **usage + cache-split extracted at the source**.
- **Local cost display** — from a community price table (visibility, not billing).
- **One neutral usage event** — [`schemas/usage-event.v1.schema.json`](schemas/usage-event.v1.schema.json),
  the boundary object every consumer codes against.

## Two shapes, one core

Sandhi is a Rust core (`sandhi-core` + `sandhi-providers`) exposed two ways:

1. **In-process, via bindings** — PyO3 (`sandhi-gateway` wheel) for Python, napi/wasm for
   TypeScript, a native crate for Rust. No network hop; wrap your existing client or use
   Sandhi's transport.
2. **Reverse-proxy** — the same core + an HTTP listener. **In-path (inline)**: it holds the
   real upstream key server-side, so internal clients point their `base_url` at Sandhi with a
   virtual key and never see the real key. The only shape that serves cross-process /
   cross-host / polyglot / shared-key setups.

> **Prompt-cache safe.** Sandhi preserves per-conversation cache affinity — it forwards the
> cacheable prefix byte-exact and never collapses users to a single session, so hosted
> prompt caches keep hitting and self-hosted KV routing stays sticky. It multiplexes
> transport; it never mixes sessions.

## The usage event

```json
{
  "schema_version": "1", "request_id": "…", "occurred_at": "…",
  "provider": "anthropic", "model": "claude-…", "backend": "external",
  "virtual_key_id": "vk_…", "subject_id": "alice", "group_id": "platform-team",
  "session_id": "conv_…", "route": "…",
  "tokens_in": 0, "tokens_out": 0,
  "cache_creation_tokens": 0, "cache_read_tokens": 0, "gpu_seconds": null
}
```

No dollars, no tier/SKU names. Full schema: [`schemas/usage-event.v1.schema.json`](schemas/usage-event.v1.schema.json).

## Where it fits

Sandhi is part of the **anvai-labs** family, alongside
[Victor](https://github.com/anvai-labs/victor) (agent framework) and
[ProximaDB](https://github.com/anvai-labs/proximaDB) (context database). It is the OSS
*mechanism*; commercial pricing, billing authority, SSO/RBAC governance, and managed
dashboards-at-scale live in the AnvaiOps control plane — the open-core split is recorded in
AnvaiOps ADR-0047.

## Layout

```
crates/sandhi-core/         # metering engine (events, sinks, virtual keys, budgets, parsers)
crates/sandhi-providers/    # unified provider transport + resilience decorator + escape hatch
crates/sandhi-store/        # durable SQLite sink + usage aggregation queries
crates/sandhi-proxy/        # the inline reverse-proxy server + self-hosted dashboard
bindings/python/            # PyO3 → PyPI `sandhi-gateway`
bindings/node/              # napi  → npm `@anvai-labs/sandhi`
schemas/usage-event.v1.schema.json   # the wire contract
docs/adr/                            # architecture decisions
```

Run the proxy with `SANDHI_STORE=usage.db` to persist events to SQLite and serve a self-hosted
usage **dashboard** at `/dashboard` (per-user / per-team / per-provider totals; neutral units, no
pricing).

## Tests & coverage

```
cargo test --workspace                        # core crate tests
cargo llvm-cov --workspace --fail-under-lines 75 \
  --ignore-filename-regex 'src/generated/'    # core line coverage (CI gate)
source ~/code/.venv/bin/activate              # a pyo3-compatible venv (CPython 3.11–3.13)
scripts/coverage-bindings.sh python           # FFI glue coverage (venv above, or COV_PYTHON=…)
scripts/coverage-bindings.sh node             # FFI glue coverage (needs npm)
```

The bindings are separate cargo workspaces built by maturin/napi and driven by a foreign
runtime, so their glue (`bindings/*/src/lib.rs`) never appears in the `--workspace` number.
`scripts/coverage-bindings.sh` instruments the cdylib, runs the binding's own test harness, and
gates the glue file at **≥85% lines** (both bindings sit ~91–96%). CI runs all three. The Python
run force-installs the built wheel, so run it inside a **virtual environment** (never system
Python, which is often too new for pyo3 — the script guards the version); the base interpreter is
`python3` or `$COV_PYTHON`.

## Roadmap (first milestones)

1. `sandhi-core`: usage accounting + the wire-event emitter + virtual-key/budget model.
2. `sandhi-providers`: the OpenAI-compatible adapter (unlocks ~20 providers), then Anthropic
   (validates the cache-split parsing metering depends on).
3. `bindings/python` (`sandhi-gateway`) + the in-process middleware.
4. `sandhi-proxy`: the inline reverse-proxy with virtual keys + budgets.

## License

Apache-2.0 — see [LICENSE](LICENSE).
