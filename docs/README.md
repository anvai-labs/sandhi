# Sandhi documentation map

This page is the index for Sandhi's architecture, contracts, and delivery status. It separates
current product truth from historical rationale so that a proposal's original context is not
mistaken for shipped behavior.

## Which document is authoritative?

| Question | Source of truth |
|---|---|
| What can I run today? | The root [README](../README.md) and [proxy operator guide](operator/proxy-guide.adoc) |
| What is the supported architecture and scope? | Accepted records in [`adr/`](adr/) |
| What is complete or still open? | This index and the status line at the top of each record in [`td/`](td/) |
| What is the public data contract? | Rust types in `sandhi-core`; generated JSON Schemas in [`../schemas/`](../schemas/) |
| What changed in a release? | [CHANGELOG](../CHANGELOG.md) and [release guide](../RELEASING.md) |
| How do sibling repositories integrate? | [`upstream/`](upstream/) snapshots; these are non-normative and may be historical |

ADRs record durable decisions. Their context sections describe the repository at the time the
decision was made and are not implementation-status pages. TDs record execution: the status line
and phase table take precedence over narrative written before implementation. If code and prose
disagree, code plus its tests win until the record is reconciled.

## Current scope

Sandhi is an L7 AI usage gateway and provider-transport library, not a general L4 proxy:

- The proxy accepts four HTTP ingress dialects: OpenAI Chat, OpenAI Responses, Anthropic Messages,
  and Gemini. Cohere and Ollama are available as upstream codecs but do not have proxy ingress
  dialects.
- Eligible same-family proxy calls use the transparent metering plane; cross-family calls use the
  neutral chat contract and may lose explicitly provider-specific extensions. A hard-capped call
  with no explicit output limit also uses translation so Sandhi can inject an enforceable ceiling.
- The listener is deliberately HTTP/1.1-only. It supports plain HTTP for loopback/trusted hops and
  opt-in TLS termination. HTTP/2, HTTP/3, raw TCP forwarding, and WebSocket sessions are not shipped.
- Budget and rate-limit enforcement is proxy-only and single-node. The in-process bindings meter
  usage but do not enforce the proxy's lease ledger.
- Sandhi measures neutral usage units. Pricing, billing, identity governance, and commercial
  policy remain downstream.

The rationale and the evidence gate for changing this scope live in
[ADR-0006](adr/0006-layer-boundary-and-protocol-scope.md). The HTTP/1-only listener decision is
[ADR-0009](adr/0009-http1-only-listener.md).

## Contract map

The committed schemas are generated from Rust with `scripts/gen-chat-contract-schemas.sh`; do not
edit them by hand. `scripts/gen-binding-contract-facades.py` derives the Python and TypeScript
facades from the same set.

| Contract | Purpose |
|---|---|
| [`usage-event.v1`](../schemas/usage-event.v1.schema.json) | One attributed, settled model-call measurement |
| [`usage.v2`](../schemas/usage.v2.schema.json) | Neutral token/cache/reasoning measurement embedded in chat contracts |
| [`usage-aggregate.v1`](../schemas/usage-aggregate.v1.schema.json) | Shared aggregate shape used by core, store, proxy, and bindings |
| [`run-cost-tree.v1`](../schemas/run-cost-tree.v1.schema.json) | Per-run/per-step usage rollup |
| [`chat-request.v1`](../schemas/chat-request.v1.schema.json) | Provider-neutral typed request |
| [`chat-response.v1`](../schemas/chat-response.v1.schema.json) | Provider-neutral completed response |
| [`chat-stream-event.v1`](../schemas/chat-stream-event.v1.schema.json) | Provider-neutral streaming event |
| [`provider-descriptor.v1`](../schemas/provider-descriptor.v1.schema.json) | Provider and model transport facts |
| [`provider-error.v1`](../schemas/provider-error.v1.schema.json) | Structured provider-boundary failure |

The wire and chat major versions are both `1`; they are exposed independently by the bindings and
by `GET /version` so a future split cannot silently invalidate a consumer handshake.

## Architecture decisions

All current ADRs are accepted; ADR-0007 is an accepted **negative** decision.

| ADR | Decision |
|---|---|
| [0001](adr/0001-sandhi-architecture-and-wire-contract.md) | Layered Rust core, bindings, proxy, and neutral wire contract |
| [0002](adr/0002-provider-transport-scope-and-modality-admission.md) | Chat-completion transport scope and the admission gate for new modalities |
| [0003](adr/0003-provider-adapter-authoring-and-codegen.md) | Hand-written transports; generated models/oracles only where bounded |
| [0004](adr/0004-two-plane-proxy-and-enforcement-boundary.md) | Transparent and translation planes; custody-based enforcement boundary |
| [0005](adr/0005-enforcement-correctness-reservation-ledger-observe-enforce-split.md) | Lease-based atomic enforcement and observe/enforce separation |
| [0006](adr/0006-layer-boundary-and-protocol-scope.md) | Stay L7 unless a measured admission gate is cleared |
| [0007](adr/0007-embedding-modality-admission.md) | Embeddings are not admitted on current evidence |
| [0008](adr/0008-inferflux-admission-and-session-affinity.md) | OpenAI-compatible catalog admission and session-affinity facts |
| [0009](adr/0009-http1-only-listener.md) | No h2c sniffing; cleartext listener is HTTP/1 only |

## Technical-design status

Status was reconciled with `develop` on 2026-09-02. “Complete” means the TD's required scope is
shipped; a deliberately transferred or optional follow-up is named in the TD instead of keeping it
perpetually open.

| TD | State | Remaining scope |
|---|---|---|
| [0001](td/TD-0001-provider-adapter-qa-and-codegen.md) | Complete | — |
| [0002](td/TD-0002-typed-provider-runtime.md) | Complete | Consumer-repository cleanup is outside Sandhi's completion gate |
| [0003](td/TD-0003-operator-surface-keys-budgets-attribution.md) | Complete | — |
| [0004](td/TD-0004-catalog-governance-dual-mode.md) | In progress | Shared-governance core and optional in-process durable surface; policy engine is TD-0005 |
| [0005](td/TD-0005-declarative-policy-engine.md) | Proposed | Policy document, engine, and distribution |
| [0006](td/TD-0006-two-plane-proxy-transparent-metering.md) | Complete | — |
| [0007](td/TD-0007-enforcement-ledger-backends.md) | In progress | Shared/HA backend selection and implementation |
| [0008](td/TD-0008-victor-codesign-boundary.md) | Complete | — |
| [0009](td/TD-0009-usage-visibility-surfaces.md) | Complete | Optional binding-local durable history only if demanded |
| [0010](td/TD-0010-ingress-dialect-parity.md) | Complete | — |
| [0011](td/TD-0011-first-party-observability.md) | Complete | — |
| [0012](td/TD-0012-rate-limit-enforcement.md) | Complete | Shared limiter transferred to TD-0007 |
| [0013](td/TD-0013-streaming-usage-fidelity.md) | Complete | — |
| [0014](td/TD-0014-data-plane-resource-safety.md) | In progress | P4 tenant fairness/sharded limiter; documented measurement follow-ups |
| [0015](td/TD-0015-performance-baseline-and-fault-injection.md) | In progress | P2–P5 load, codec, fault, and CI harnesses |
| [0016](td/TD-0016-enforcement-throughput-ceiling.md) | In progress | P2–P6; P0/P1 are complete |
| [0017](td/TD-0017-transport-security-and-ingress-protocol-breadth.md) | In progress | P2 rotation, P3 ALPN/context, evidence-gated P4 HTTP/2 |
| [0018](td/TD-0018-duplex-session-metering.md) | Proposed | Spike, session abstraction, and WebSocket ingress |
| [0019](td/TD-0019-ingress-codec-untrusted-input-hardening.md) | Proposed | Property/fuzz suites and lossy-field inventory |
| [0020](td/TD-0020-operational-readiness-and-transport-observability.md) | In progress | Readiness, remaining gauges, pool, shedding, and DNS work |
| [0021](td/TD-0021-co-design-seam-v2-proxy-path-contract.md) | Complete | — |
| [0022](td/TD-0022-per-call-transport-context.md) | Complete | — |
| [0023](td/TD-0023-release-automation.md) | Complete | npm remains intentionally unconfigured/unpublished |
| [0024](td/TD-0024-reservation-retention-and-rollup.md) | Proposed | Bounded reservation history and rollups |
| [0025](td/TD-0025-ingress-funnel-and-family-registry.md) | Proposed | Family registry and funnel decomposition |

The compact active roadmap is therefore:

1. Establish proxy/load/fault measurements (TD-0015), then use them to choose fairness,
   operational, and throughput changes (TD-0014/0016/0020).
2. Complete TLS lifecycle work; add HTTP/2 only with a named use case and the ADR-0009 timeout
   invariants (TD-0017).
3. Harden untrusted codecs and bound long-lived ledger history (TD-0019/0024).
4. Reduce family/codec co-edit risk (TD-0025).
5. Run the duplex-session spike before scheduling a WebSocket/session implementation (TD-0018).

## Maintenance rules

- Update a TD's status line and phase marker in the same PR that completes a phase.
- Update this index when a TD changes lifecycle state or transfers scope.
- Put operational knobs and examples in the operator guide; keep the root README concise.
- Treat upstream handoffs as dated snapshots. Add a supersession banner instead of rewriting their
  historical instructions as if they were a current runbook.
- Prefer links to types, tests, and stable symbols over source line numbers; line references are
  evidence snapshots and drift as implementation files move.
