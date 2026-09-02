# ADR-0001: Sandhi architecture — Rust core, bindings, proxy, and the usage-event wire contract

Date: 2026-07-19

## Status

Accepted (founding decision for this repo). Originates from **AnvaiOps ADR-0047**
(authoritative open-core split + cross-repo homing), with sibling consumer decisions
**victor FEP-0020 / ADR-018** and **ProximaDB ADR-067**. This ADR records the *internal*
architecture of `sandhi` itself; ADR-0047 remains authoritative for the OSS↔commercial
boundary.

> **Amended 2026-07-22 by [ADR-0004](0004-two-plane-proxy-and-enforcement-boundary.md),
> reconciled 2026-09-02.** The current four-crate layout, two-plane forwarding rule, and shipped
> status are reflected below. Bedrock remains parser-only until SigV4 transport is admitted.

## Context

Sandhi is an Apache-2.0 OSS **AI usage gateway**: the junction every model call passes
through, counted and attributed. It exists because the provider wire layer + usage
accounting are triplicated across victor (Python), ProximaDB (Rust), and AnvaiOps
(Python), and that triplication sits exactly where metering trust is decided. Sandhi is
the single, fast, neutral implementation those three (and third parties) consume.

The boundary: **Sandhi measures (neutral units); the commercial layer prices** (AnvaiOps).
Sandhi never emits dollars or tier/SKU names.

## Decision

### 1. One Rust core, layered crates

| Crate | Role |
|---|---|
| **`sandhi-core`** | Usage/token accounting (full cache split), virtual-key resolution, budget + rate-limit enforcement, the neutral-event emitter, the wire types. No transport opinion. |
| **`sandhi-providers`** | The unified provider transport (ADR-0047 D10): adapter trait + concrete transports (Anthropic, OpenAI-compatible, Gemini, Cohere, local vLLM/Ollama, OpenAI Responses), streaming parsing, **usage extraction at the source**, pooling, retry, and circuit-breaker. Bedrock is parser-only pending SigV4. Patterns: adapter / strategy / factory / decorator. |
| **`sandhi-store`** | Durable SQLite usage sink and aggregates, the lease ledger, credential-vault metadata, virtual keys, budgets, and alerts. Kept separate so language bindings do not bundle SQLite. |
| **`sandhi-proxy`** | The reverse-proxy binary — `sandhi-core` + `sandhi-providers` + an HTTP/streaming listener. Not a second implementation. |

### 2. Two deployment shapes, one core (ADR-0047 D2)

- **In-process, via bindings** — `bindings/python` (PyO3 → the **`sandhi-gateway`** PyPI
  wheel), `bindings/node` (napi native addon). Zero network hop for same-process callers; ProximaDB
  links `sandhi-core`/`sandhi-providers` natively (no FFI).
- **Reverse-proxy** — the same core + a listener, for cross-process / cross-host / polyglot
  / shared-key use. **In-path (inline)**, never a redirect (ADR-0047 D8): it holds the real
  upstream key server-side, issues virtual keys, meters every token.

### 3. Wire contracts are versioned boundary objects

`schemas/usage-event.v1.schema.json` (ADR-0047 D3) is the single artifact every consumer
codes against. Neutral units only — no dollars, no tier/SKU names. Breaking changes bump the
`schema_version` and coordinate across consumers (same discipline as `victor-codegraph`).

[TD-0002](../td/TD-0002-typed-provider-runtime.md) additionally defines the narrow neutral chat
contract consumed by bindings and proxy ingress codecs. This is not a second provider
implementation: `sandhi-core` owns its types and `sandhi-providers::ProviderRuntime` owns the one
codec/transport path used by every front door.

### 4. Session / prompt-cache / KV affinity is preserved, not flattened (ADR-0047 D9)

Multiplex transport ≠ mix sessions. On the proxy, eligible same-family requests use ADR-0004's
transparent plane and preserve the caller's bytes except for the narrowly specified usage-metering
envelope normalization. A hard-capped request without an explicit upstream output bound is routed
through translation so Sandhi can inject an enforceable ceiling. Cross-family requests decode
through `ChatRequestV1` and re-encode, preserving the neutral contract while provider-specific
extensions may be lost. Attribution stays in
headers/metadata **outside** the cached prompt, and a stable per-conversation `session_id` is
preserved so hosted prompt caches keep hitting and a self-hosted fleet can route a conversation to
its warm instance.

### 5. Host-language provider escape hatch (mandatory)

The provider registry accepts a **host-language adapter** (a Python/TS callback), so a
consumer's custom / air-gapped / community providers register without a Rust contribution.
This preserves victor's Python extensibility as its 28 adapters migrate onto Sandhi
(phased, behind a flag — victor FEP-0020 § Provider transport migration).

### 6. Layout

```
sandhi/
  crates/{sandhi-core, sandhi-providers, sandhi-store, sandhi-proxy}/
  bindings/python/            # PyO3 → PyPI `sandhi-gateway`
  bindings/node/              # napi native addon (npm publication intentionally deferred)
  schemas/                    # generated public contracts
  docs/{adr,td}/              # durable decisions + executable designs
```

## Consequences

- **Positive:** one implementation of transport + accounting; the meter sees usage at the
  source; a compelling "Rust LiteLLM" OSS project; clean OSS boundary (pure mechanism, no
  pricing).
- **Cost:** a cross-repo wire contract to keep stable; PyO3/napi streaming across the FFI
  boundary needs care (solved patterns: async PyO3 / channels; the proxy for full isolation).
- **Status:** implemented and released. The four crates, both bindings, generated contract set,
  typed provider runtime, proxy/operator surface, two-plane forwarding, single-node enforcement,
  telemetry, and opt-in TLS are present. Remaining work is tracked by owning TD in the
  [documentation status index](../README.md), not by this founding ADR.

## References

- AnvaiOps ADR-0047 (authoritative open-core split; D1–D10)
- victor FEP-0020 / ADR-018 (primary adopter + provider-transport migration)
- ProximaDB ADR-067 (native-Rust consumer)
- `schemas/usage-event.v1.schema.json` (the wire contract)
