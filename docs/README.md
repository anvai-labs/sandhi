# Sandhi documentation map

This page is the index for Sandhi's architecture, contracts, and delivery status. It separates
current product truth from historical rationale so that a proposal's original context is not
mistaken for shipped behavior.

[Start with the visual operator overview](index.md) or use the reference map below.

## Which document is authoritative?

| Question | Source of truth |
|---|---|
| What can I run today? | The root [README](../README.md) and [proxy operator guide](operator/proxy-guide.adoc) |
| How do users, groups and long-running clients authenticate? | [Identity and agent credentials](operator/identity-groups.md) (feature candidate; live migration pending) |
| How is prompt egress inspected? | [Policy MVP](operator/policy-evaluation.md) (candidate); [evaluator backends and Python/MLflow](operator/python-ml-evaluators.md) (Python, optional ONNX and remote-service candidates); [resource-limited container profile](../templates/python-evaluator/DEPLOYMENT.md) |
| How does Sandhi improve repeated-context efficiency? | [Visual cache routing and token guide](product/cache-aware-routing.md): marker placement, fresh/read/write accounting, cost boundaries and the complementary ONNX egress mechanism |
| What has been tested with live provider caching? | [Z.AI qualification](reviews/zai-router-live-2026-11-11.md): 24 final protocol/mode cells and 48 cache calls; retained defects/timeouts and remaining gates |
| Has tracked routing been tested with live caching? | [Tracked buffered Z.AI matrix](reviews/zai-router-tracked-buffered-2026-11-11.md): 24 successful logical-route calls joined to usage, attempt evidence and paired receipts; earlier failures retained |
| How do I rehearse recovery safely? | [Isolated single-node recovery drill](operator/recovery-drill.md); no automatic production restore guarantee |
| What is the supported architecture and scope? | Accepted records in [`adr/`](adr/) |
| What is complete or still open? | This index and the status line at the top of each record in [`td/`](td/) |
| What is needed for multi-backend routing and cache management? | [Active goal and delivery gates (TD-0034)](td/TD-0034-protocol-neutral-router-cache-accounting.md); [baseline inventory and first fixes](reviews/router-cache-inventory-2026-11-11.md); implemented routing/source contracts are distinct from release/deployment and broader external-resource lifecycle gates |
| How do I operate the dashboard? | [Operator console](operator/dashboard.md): navigable workspaces, routing inventory, token reuse and bounded usage evidence |
| How do I configure backends and logical models? | [Backend operator guide](operator/backends.md); fixed and logical routes, compatible affinity, cache declarations and credential references |
| How do embedded clients select compatible endpoint usage conventions? | [Shared endpoint profiles](adr/0039-shared-embedded-endpoint-profiles.md) and [explicit gateway peer](adr/0041-explicit-gateway-peer-contract.md): client dialect, source convention and one server cache compiler |
| What store and execution contract does HTTP require? | [Mandatory durable owner](adr/0043-mandatory-durable-http-execution.md) and [0.17.0 upgrade](releases/v0.17.0.md): original file/shard1, removed flag, paired holds and shared serving lifecycle |
| What authorizes native continuation? | [Source-issued native ownership](adr/0044-native-reference-ownership.md) and [backend operator guide](operator/backends.md#native-reference-ownership): same-ledger proof, original deadlines and no caller-ID adoption |
| How do buffered calls preserve source accounting and tool fields? | [Shared buffered qualification](adr/0040-shared-buffered-source-qualification.md): ordinary/accounted source semantics, terminal-only Codex aggregation and deterministic refusal |
| Does the routing goal include native Gemini clients? | Yes; [Gemini inventory and fidelity gaps](reviews/gemini-router-cache-inventory-2026-11-11.md) and the four-ingress matrix in TD-0034 |
| What is the public data contract? | Rust types in `sandhi-core`; generated JSON Schemas in [`../schemas/`](../schemas/) |
| What changed in a release? | [0.17.0 migration and qualification](releases/v0.17.0.md), [0.15.0 release evidence](releases/v0.15.0.md), [0.14.0 release and adoption gates](releases/v0.14.0.md), [0.13.0 release](releases/v0.13.0.md), [0.12.0 release evidence](releases/v0.12.0.md), [CHANGELOG](../CHANGELOG.md) and [release guide](../RELEASING.md) |
| How do sibling repositories integrate? | [`upstream/`](upstream/) snapshots; these are non-normative and may be historical |
| What product evolution is proposed and how is it tracked? | [TD-0026 delivery plan](td/TD-0026-gateway-product-evolution.md), [product specification](product/gateway-vision-and-requirements.md), and [2026-09-04 review](reviews/gateway-review-2026-09-04.md); proposals, not shipped claims |
| What evidence gates the M1 checkpoint? | [Workload and acceptance](product/m1-acceptance.md); UA01 accepts engineering evidence for this release and defers hands-on usability until before production |
| Which acceptance decisions remain, and why? | [M1 decision packet](product/m1-acceptance-decisions.md); automated evidence, operating-limit choices and a focused user-review script |
| What is the release state? | [Published releases](https://github.com/anvai-labs/sandhi/releases) and [CHANGELOG](../CHANGELOG.md); [release safeguards](product/release-safeguards.md) records the historical v0.7.0 milestone, not the current installed version |
| What is the three-way co-design state? | [TD-0027](td/TD-0027-three-way-origin-codesign.md) and [delivery review](reviews/three-way-release-review-2026-09-16.md); original lanes integrated, cross-repository release follow-through in progress |
| What adversarial checks cover the first checkpoint? | [2026-09-05 checkpoint review](reviews/checkpoint-adversarial-review-2026-09-05.md); findings, corrections and remaining integration gates |

ADRs record durable decisions. Their context sections describe the repository at the time the
decision was made and are not implementation-status pages. TDs record execution: the status line
and phase table take precedence over narrative written before implementation. If code and prose
disagree, code plus its tests win until the record is reconciled.

## Current scope

Sandhi is an L7 AI usage gateway and provider-transport library, not a general L4 proxy:

- The proxy accepts four HTTP protocol families: OpenAI (chat, plaintext completions and embeddings), OpenAI Responses, Anthropic Messages,
  and Gemini. Cohere and Ollama are available as upstream codecs but do not have proxy ingress
  dialects.
- Eligible same-family proxy calls use the transparent metering plane; cross-family calls use the
  neutral chat contract, preserving representable content and refusing meaningful native state
  or controls that cannot be faithfully projected. No silent extension loss is permitted. A chat call without an explicit
  output limit uses canonical encoding to inject the operator-owned default under
  both Block and Warn policies. Plaintext completions
  instead receive an explicit max_tokens=16 default and remain on their native route.
- The listener is deliberately HTTP/1.1-only. It supports plain HTTP for loopback/trusted hops and
  opt-in TLS termination. HTTP/2, HTTP/3, raw TCP forwarding, and WebSocket sessions are not shipped.
- Budget and rate-limit enforcement is proxy-only and single-node. The in-process bindings meter
  usage but do not enforce the proxy's lease ledger.
- Sandhi measures neutral usage units. Pricing, billing, identity governance, and commercial
  policy remain downstream.
- Budget admission uses estimates, not strict total-token bounds. Reasoning inclusion is explicit
  in contract minor 7; see [metering semantics, compatibility and guarantees](product/metering-and-budget-guarantees.md).
- The current [HTTP execution contract](operator/buffered-accounting.md) requires
  the original single file-backed paired owner for every generation operation. See [attempt accounting and evidence](product/attempt-accounting-and-evidence.md)
  for historical phase evidence. Current serving/recovery and upgrade requirements are in
  [0.17.0](releases/v0.17.0.md), ADR-0043 and ADR-0044; unsupported external lifecycle work
  remains separate.

The rationale and the evidence gate for changing this scope live in
[ADR-0006](adr/0006-layer-boundary-and-protocol-scope.md). The HTTP/1-only listener decision is
[ADR-0009](adr/0009-http1-only-listener.md).

## Contract map

The committed schemas are generated from Rust with `scripts/gen-chat-contract-schemas.sh`; do not
edit them by hand. `scripts/gen-binding-contract-facades.py` derives the Python and TypeScript
facades from the same set.

| Contract | Purpose |
|---|---|
| [`usage-event.v1`](../schemas/usage-event.v1.schema.json) | One attributed model-call observation; observations do not prove committed settlement |
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

ADRs 0001–0009 are accepted; ADR-0007 is an accepted **negative** decision.
ADR-0010 defines the implemented additive cache observation/coverage contract (minor 9);
TD-0028 keeps its merge/deployment gates separate from the remaining diagnostic and replay scope.

| ADR | Decision |
|---|---|
| [0001](adr/0001-sandhi-architecture-and-wire-contract.md) | Layered Rust core, bindings, proxy, and neutral wire contract |
| [0016](adr/0016-explicit-backend-endpoint-binding.md) | Explicit endpoint family/profile factory seam |
| [0017](adr/0017-backend-registry-lifecycle.md) | Non-secret backend registry, revision-conditional lifecycle and credential-dependent reconstruction |
| [0018](adr/0018-ordered-native-input-and-projection.md) | Codec-owned ordered input, projection consistency and compatible buffered native output |
| [0019](adr/0019-native-stream-attachments.md) | Default-off native SSE attachments and bounded Codex terminal-response aggregation; canonical accounting remains authoritative |
| [0020](adr/0020-cache-intent-compilation.md) | Explicit cache intent, declared backend/model controls and scoped keys; no invented backend capability |
| [0021](adr/0021-logical-routing-and-compatible-affinity.md) | Authorized logical models, deterministic selection and durable compatible affinity; no automatic inference replay |
| [0022](adr/0022-durable-execution-context.md) | Atomic admitted backend/cache/policy context with original-weight fencing; historical absent context stays absent |
| [0023](adr/0023-paired-budget-settlement.md) | Atomic paired admission/settlement and conservative unknown liability through expiry/restart |
| [0024](adr/0024-durable-physical-attempt-evidence.md) | Persisted single-attempt identity and atomic source transport/usage evidence |
| [0025](adr/0025-qualified-buffered-protocol-accounting.md) | Qualified buffered four-dialect matrix and explicit compatible source usage profiles |
| [0026](adr/0026-qualified-responses-stream-accounting.md) | Bounded source-qualified Responses streaming, partial progress and original token evidence |
| [0027](adr/0027-qualified-codex-buffered-accounting.md) | Constrained Codex SSE aggregation, original deadlines and source usage independent of delivery |
| [0028](adr/0028-qualified-translated-stream-accounting.md) | Qualified native/portable streaming with source-owned paired accounting and bounded projection |
| [0029](adr/0029-explicit-terminal-chat-usage-profile.md) | Explicit generic and vendor final-choice Chat usage conventions; strict standard qualifier and binding fences retained |
| [0039](adr/0039-shared-embedded-endpoint-profiles.md) | Shared embedded endpoint factory/profiles; gateway peers use an independent declared client dialect |
| [0040](adr/0040-shared-buffered-source-qualification.md) | Shared strict buffered source usage and portable/native fidelity; no measured zero from absence |
| [0041](adr/0041-explicit-gateway-peer-contract.md) | Explicit immediate peer role, server-owned emitted counts convention and one cache compiler |
| [0042](adr/0042-provider-identity-and-budget-namespace.md) | Diagnostic labels separate from immutable persisted budget namespace; old caps/spend/holds/pins preserved |
| [0043](adr/0043-mandatory-durable-http-execution.md) | Required original single-file HTTP owner, defaults, paired liability and ledger-free FFI boundary |
| [0044](adr/0044-native-reference-ownership.md) | Same-ledger source-issued native proof, original request/shutdown bounds and no implicit external adoption |
| [0002](adr/0002-provider-transport-scope-and-modality-admission.md) | Chat-completion transport scope and the admission gate for new modalities |
| [0003](adr/0003-provider-adapter-authoring-and-codegen.md) | Hand-written transports; generated models/oracles only where bounded |
| [0004](adr/0004-two-plane-proxy-and-enforcement-boundary.md) | Transparent and translation planes; custody-based enforcement boundary |
| [0005](adr/0005-enforcement-correctness-reservation-ledger-observe-enforce-split.md) | Lease-based atomic enforcement and observe/enforce separation |
| [0006](adr/0006-layer-boundary-and-protocol-scope.md) | Stay L7 unless a measured admission gate is cleared |
| [0007](adr/0007-embedding-modality-admission.md) | Historical negative admission; current native OpenAI-compatible Embeddings follows ADR-0043 and the governed text guide |
| [0008](adr/0008-inferflux-admission-and-session-affinity.md) | OpenAI-compatible catalog admission and session-affinity facts |
| [0009](adr/0009-http1-only-listener.md) | No h2c sniffing; cleartext listener is HTTP/1 only |
| [0010](adr/0010-cache-read-reporting-availability.md) | Accepted: additive cache-read observation and same-population reporting coverage |
| [0011](adr/0011-bounded-persisted-usage-diagnostics.md) | Accepted design: bounded persisted usage lookup; post-0.8.0 explicit OIDC diagnostics permission, token mode remains admin-only; no new capture or retention |
| [0012](adr/0012-databricks-model-serving-provider.md) | Databricks Model Serving admitted as an OpenAI-compatible provider |
| [0013](adr/0013-native-extensions-passthrough-and-auto-cache-control.md) | Guarded `sandhi_extensions` ingress passthrough; auto `cache_control` on the Anthropic translation path |
| [0014](adr/0014-dialect-parity-counting-and-streaming-fidelity.md) | Token-count endpoints; stateful Anthropic/Gemini/Responses stream encoders; OpenAI `created`; translated-traffic route labels |
| [0015](adr/0015-upstream-budgets-back-leg-caps.md) | Historical upstream-cap seam; immutable stored upstream_budget_scope now preserves authority independently of labels |

## Technical-design status

This dated phase inventory was reconciled with `develop` on 2026-09-17 for TD-0026/0027.
For current 0.17.0 activation and migration, use [ADR-0043](adr/0043-mandatory-durable-http-execution.md),
[ADR-0044](adr/0044-native-reference-ownership.md) and the [release checklist](releases/v0.17.0.md);
older phase descriptions do not enable removed serving modes.
“Complete” means the TD's required scope is
shipped; a deliberately transferred or optional follow-up is named in the TD instead of keeping it
perpetually open.

| TD | State | Remaining scope |
|---|---|---|
| [0034](td/TD-0034-protocol-neutral-router-cache-accounting.md) | Active goal | Explicit backend/operator lifecycle, loss-aware conversation/cache handling, authorized routing/affinity and uniform execution accounting; source/live qualification tracked separately |
| [0032](td/TD-0032-weighted-allowance-policy.md) | W01 released in 0.14.0 | Opt-in weighted allowance arithmetic; reporting provenance, admission-bound policy snapshots, durable enforcement and live acceptance pending |
| [0001](td/TD-0001-provider-adapter-qa-and-codegen.md) | Complete | — |
| [0002](td/TD-0002-typed-provider-runtime.md) | Complete | Consumer-repository cleanup is outside Sandhi's completion gate |
| [0003](td/TD-0003-operator-surface-keys-budgets-attribution.md) | Complete | — |
| [0004](td/TD-0004-catalog-governance-dual-mode.md) | In progress | Shared-governance core and optional in-process durable surface; policy engine is TD-0005 |
| [0005](td/TD-0005-declarative-policy-engine.md) | In progress | Opt-in [policy MVP](operator/policy-evaluation.md): regex/threshold/lexical checks, verified identity selection, durable metadata receipts and dispatch denial; [Python workers/offline MLflow](operator/python-ml-evaluators.md) implemented as candidates; [embedded ONNX](operator/embedded-onnx.md) profile and multi-model remote service/replica adapter implemented; semantic models, payload quarantine and signed distribution remain proposed |
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
| [0017](td/TD-0017-transport-security-and-ingress-protocol-breadth.md) | In progress | P1 + D7 dual-scheme listeners shipped; P2 rotation, P3 ALPN/context, evidence-gated P4 HTTP/2 |
| [0018](td/TD-0018-duplex-session-metering.md) | Proposed | Spike, session abstraction, and WebSocket ingress |
| [0019](td/TD-0019-ingress-codec-untrusted-input-hardening.md) | Proposed | Property/fuzz suites and lossy-field inventory |
| [0020](td/TD-0020-operational-readiness-and-transport-observability.md) | In progress | W06a/b/c and W06d automation verified and integrated; actual-user acceptance pending. Remaining gauges, pool, shedding and DNS work still open |
| [0021](td/TD-0021-co-design-seam-v2-proxy-path-contract.md) | Complete | — |
| [0022](td/TD-0022-per-call-transport-context.md) | Complete | — |
| [0023](td/TD-0023-release-automation.md) | Complete | Publish mechanics implemented; safeguarded all-target v0.6.0 execution tracked separately in TD-0026 |
| [0024](td/TD-0024-reservation-retention-and-rollup.md) | Proposed | Bounded reservation history and rollups |
| [0025](td/TD-0025-ingress-funnel-and-family-registry.md) | Partial P3 landed ([#365](https://github.com/anvai-labs/sandhi/pull/365)) | Family registry and funnel decomposition |
| [0026](td/TD-0026-gateway-product-evolution.md) | In progress | W05b opt-in diagnostics shipped; W05c owned/frozen-observation settlement plus single-file tracked terminal bridge plus owned admission/dispatch handoff and bounded blocking-result/prepared-admission ownership and W05d admission-intent/immutable-terminal-usage/bounded-recovery-inventory/opt-in dispatch-fence storage foundations implemented; additive buffered usage qualification and limited owned buffered HTTP/correlated admission/bounded recovery plus tracked process-crash/restart, terminal-publication contention and synthetic buffered TLS/OIDC composition and grant-revoked restart drills implemented; isolated released 0.12.0 buffered qualification with deployed OIDC and three provider routes passed; strict OpenAI Chat stream event qualification, bounded complete-event framing and opt-in raw response ownership/immutable observation implemented as source prerequisites; opt-in bounded transparent streaming HTTP reuses durable terminal settlement and retained recovery, with standalone streaming crash/restart, contention and TLS/OIDC lifecycle source evidence; managed tracked deployment, broad lifecycle acceptance, observation amendment, export and P01–P03 remain open |
| [0027](td/TD-0027-three-way-origin-codesign.md) | In progress | Sandhi v0.7.0 published/verified and evidence synchronized; InferFlux v0.3.0 packaging correction awaits approval/native verification; focused Victor v0.9.5 awaits approval/CI/promotion; sibling publication remains open |
| [0028](td/TD-0028-cache-accounting-availability.md) | In progress | C1–C4 and C5 terminal-stream repair merged; buffered-only [operator deadlines](operator/buffered-deadlines.md) implemented with unchanged defaults; opt-in Rust [stream body ownership](operator/stream-body-lifetime.md) implemented; standalone [streaming route policy](operator/streaming-deadlines.md) implemented; bounded settlement and originating Mac six-Qwen/one-ZAI acceptance remain open |
| [0029](td/TD-0029-oidc-sso-authorization.md) | In progress | OIDC sessions and roles; feature candidate adds verified groups, renewable clients and durable user keys; existing browser acceptance recorded; new live group/broker acceptance and release pending |

| [0030](td/TD-0030-governed-text-endpoints.md) | In progress | Plaintext embeddings/completions implemented and tested; live acceptance and LAN cutover pending |

TD-0026 slice completion means implementation/local verification. Its separate C01/C02 checkpoint
table tracks remote CI, merge and release; C01 [PR #230](https://github.com/anvai-labs/sandhi/pull/230)
merged into `develop` as `8f56b91` after clean review and green hosted CI, with an explicit
owner-authorized review bypass. That checkpoint did not include release or main promotion;
C02 now records the reviewed main promotion in [PR #243](https://github.com/anvai-labs/sandhi/pull/243),
the completed exact-main CI/tag and successful all-target publication/verification.
W06b is separately checkpointed in [PR #231](https://github.com/anvai-labs/sandhi/pull/231)
(C01b), verified locally against current `develop` with its own CI/merge evidence on the PR;
it does not complete M1 readiness/recovery acceptance.

The proposed [product evolution plan](td/TD-0026-gateway-product-evolution.md) connects UX,
accounting, operations, security and credential lifecycle to these existing designs. Its first
milestone addresses confirmed correctness gaps; its later fleet and protocol work retains the
accepted scope and evidence gates. The [SentinelPass co-design](upstream/sentinelpass-gateway-codesign.md)
defines proposed shared contracts and reciprocal capabilities from pinned source review.
The [browser–gateway–vault co-design](upstream/browser-gateway-vault-codesign.md) adds
AgentBrowser smoke coverage and proposed destination-bound secrets and correlated action evidence;
it does not imply a live three-product integration.

The immediate delivery work is TD-0027's InferFlux v0.3.0 packaging correction and focused
Victor v0.9.5 approval/CI/promotion/publication gates. Further merges require the second account's
approving review. Sandhi v0.7.0 and its evidence synchronization are complete.
It does not close TD-0026's authoritative attempt accounting, scoped security, hierarchical
policy, live broker interoperability or hands-on production acceptance. Calibration identity,
bounded key storage and provenance-separated latency analytics remain explicit optimization
follow-ups in the co-design review, not shipped guarantees.

The longer-term implementation roadmap, which TD-0026 proposes reprioritizing, is:

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

Consolidation candidate: [gateway security review and reviewer path](operator/GATEWAY-SECURITY-REVIEW-2026-09-27.md).

- [Optional sensitive-text policy bundle](operator/sensitive-policy.md): reproducible CPU ONNX demo, audited defaults, protected operator status, and measured limits.

- [Governed text endpoints](operator/text-endpoints.md): plaintext embeddings/completions support and reservation limits.
