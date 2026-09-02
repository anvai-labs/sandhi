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
- **Status:** early and released. The metering core, typed provider runtime, four proxy ingress
  dialects, operator surface, durable single-node lease ledger, rate limiting, observability/OTLP,
  transparent metering, idempotent usage recording, and opt-in listener TLS are shipped. The
  concise implementation status and active roadmap live in the
  [documentation map](docs/README.md), so this README does not freeze a release number or duplicate
  25 TD phase tables.
- **Packages:** crates.io `sandhi-core` / `-providers` / `-store` / `-proxy` · PyPI
  `sandhi-gateway` · npm `@anvai-labs/sandhi` *(intentionally unpublished; the Node binding builds
  from source and is tested in CI)*

## Why

Teams that share one provider API key on an internal network can't answer *"who spent
what,"* can't budget per person, and can't rate-limit a runaway user. And every framework
re-implements the same provider adapters + usage parsing — the exact place metering goes
wrong. Sandhi is the single, fast, neutral implementation of both.

## What it does

- **Virtual keys** — one shared upstream key fronts many per-user keys; attribution and
  revocation are per person, not per shared secret.
- **Per-user / per-team attribution** — every call tagged with `subject_id` / `group_id`.
- **Budgets** — per-virtual-key / per-team **token** caps enforced by a lease ledger (reserve a
  conservative *ceiling*, settle by lease id), with calendar-aligned daily/monthly/total
  **windows**, a block-or-**warn** policy, and threshold **alerts**. Set `SANDHI_STORE` and the
  ledger is **durable**: spend, caps, and in-flight leases survive a restart, and dangling leases
  are reclaimed. Without it the ledger is in-memory and a restart resets accrued spend.
- **Rate limits** — per-virtual-key requests/minute, enforced by a token bucket before the budget
  reservation, so a throttled call consumes no budget. The 429 carries `Retry-After` in the
  caller's own dialect. **Per process:** the limiter is in-memory, so with N replicas the effective
  limit is N × the configured value — the same single-node caveat as the ledger below.
  Still open: a shared/HA backend for multi-replica deployments
  ([TD-0007](docs/td/TD-0007-enforcement-ledger-backends.md)).
- **Unified provider transport** — Anthropic, OpenAI-compatible (covers ~20 providers),
  Gemini, Cohere, local vLLM/Ollama, OpenAI Responses — streaming, pooling, retry,
  circuit-breaker, with **usage + cache-split extracted at the source**. (Bedrock is
  parser-only until SigV4 request signing lands; front it with an OpenAI-compatible gateway
  meanwhile.)
- **Latency + reasoning tokens** — `duration_ms` / `time_to_first_token_ms` measured at the
  adapter boundary, and separately-reported reasoning tokens captured where a provider exposes
  them (OpenAI Chat + Responses, Gemini `thoughtsTokenCount`).
- **One neutral usage event** — [`schemas/usage-event.v1.schema.json`](schemas/usage-event.v1.schema.json),
  the boundary object every consumer codes against.

## Two shapes, one core

Sandhi is a Rust core (`sandhi-core` + `sandhi-providers`) exposed two ways:

1. **In-process, via bindings** — PyO3 (`sandhi-gateway` wheel) for Python, a napi native addon for
   TypeScript, a native crate for Rust. No network hop; wrap your existing client or use
   Sandhi's transport.
2. **Reverse-proxy** — the same core + an HTTP listener. **In-path (inline)**: it holds the
   real upstream key server-side, so internal clients point their `base_url` at Sandhi with a
   virtual key and never see the real key. The only shape that serves cross-process /
   cross-host / polyglot / shared-key setups.

### Client compatibility

Point the vendor's own SDK at Sandhi — change the `base_url` and the key, nothing else:

| client | ingress | drop-in | proven by |
|---|---|---|---|
| **OpenAI SDK** | `/v1/chat/completions`, `/v1/responses` | ✅ | `tests/sdk-conformance/` drives `openai-python` in CI |
| **Anthropic SDK** | `/v1/messages` | ✅ | same suite drives `anthropic-python`, authenticating with `x-api-key` as the SDK does |
| **Gemini SDK** | `/v1beta/models/{model}:generateContent` | ✅ *(header auth)* | same suite drives `google-genai`; the credential must be the `x-goog-api-key` **header** — the documented `?key=` query form is refused, since it would put a live virtual key in URLs and access logs |

```python
client = anthropic.Anthropic(base_url="http://sandhi:8787", api_key="vk_…")   # unmodified
client = openai.OpenAI(base_url="http://sandhi:8787/v1", api_key="vk_…")      # unmodified
client = genai.Client(api_key="vk_…", http_options=genai.types.HttpOptions(base_url="http://sandhi:8787"))
```

A Gemini client may resolve to a Gemini upstream for byte-exact transparent forwarding or to
another family through the translation plane. Standard content, tools, inline media, and tool
results translate; provider-specific extensions may not. A streamed cross-family tool call is not
rendered into a Gemini partial-function-call frame, while non-streaming tool calls translate fully
([TD-0010](docs/td/TD-0010-ingress-dialect-parity.md) D4b).

That table is a CI gate, not a claim: the suite starts the real proxy against a mock upstream and
drives it with the vendors' clients, so a dialect that stops being drop-in fails the build. Rows
are added when they pass, never in advance.

> **Prompt-cache safe (by design).** Sandhi preserves per-conversation cache affinity — it
> never collapses users to a single session and carries attribution *outside* the cached
> prompt, so hosted prompt caches keep hitting and self-hosted KV routing stays sticky. The
> **content-faithful forwarding** that guarantees this on the proxy path is **live** as the
> transparent-metering plane of
> [ADR-0004](docs/adr/0004-two-plane-proxy-and-enforcement-boundary.md): when the ingress dialect
> and the upstream are the same family, the proxy forwards the client's bytes verbatim — except a
> minimal envelope normalization that injects usage-metering on OpenAI streams (ADR-0004 D1) — and
> meters the stream as it passes. A hard-capped request that omits its output limit uses translation
> instead so the proxy can inject an enforceable ceiling. Otherwise, provider-specific extras
> (e.g. message-level Anthropic `cache_control` breakpoints) survive untouched. A
> **cross-family** request still re-encodes through the neutral contract — faithful for standard
> fields, but it can drop those extras. The in-process bindings, which never re-encode, are
> unaffected.

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
docs/td/                             # implementation designs and phase status
docs/README.md                       # documentation map + current status index
```

Run the proxy with `SANDHI_STORE=usage.db` to persist events to SQLite and serve a self-hosted
usage **dashboard** at `/dashboard` (per-user / per-team / per-provider totals; neutral units, no
pricing).

## Operating it

For a local Ollama-backed instance with persistent state, run `./scripts/quickstart.sh`. For the
complete production-facing flow—listener/TLS configuration, credential registration, virtual-key
minting, budgets, client setup, metrics, OTLP, security, and troubleshooting—use the
[proxy operator guide](docs/operator/proxy-guide.adoc). `GET /version` exposes public contract
versions; the admin-gated `GET /admin/version` adds enabled capabilities.

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

## Architecture, contracts, and roadmap

The [documentation map](docs/README.md) is the maintained index for ADRs, TD status, generated
schemas, current scope, and the active roadmap. The original first milestones are complete; open
work is tracked by owning TD rather than repeated here.

## Community & contributing

- **Contributing** — see [CONTRIBUTING.md](CONTRIBUTING.md) (setup, the `develop` → `main`
  branch flow, the test/coverage gates).
- **Releases** — see [CHANGELOG.md](CHANGELOG.md) and [RELEASING.md](RELEASING.md).
- **Security** — report vulnerabilities privately; see [SECURITY.md](SECURITY.md).
- **Code of conduct** — [CODE_OF_CONDUCT.md](CODE_OF_CONDUCT.md).

## License

Apache-2.0 — see [LICENSE](LICENSE).
