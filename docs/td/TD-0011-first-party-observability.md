# TD-0011: First-party observability — telemetry about the gateway, not a second meter

- **Status:** Accepted (2026-07-26). **P1, P2, P3 and P4 complete.** P3 (OTLP) landed behind the
  non-default `otel-otlp` cargo feature — see the **P3 amendment** below.
- **Relates to:** ADR-0001 (measure-vs-price), ADR-0004 D4 (dashboard gating), ADR-0005
  (enforcement ledger), TD-0009 (usage aggregate + cardinality discipline), TD-0008 (Victor
  co-design boundary)
- **Companion facts:** `grep -rn "tracing::\|opentelemetry\|prometheus" crates/*/src` returns
  **nothing** — a gateway whose purpose is measurement currently exports no telemetry about itself

## Why this exists

Sandhi can tell you, exactly, how many tokens `alice` spent on `claude-sonnet-4` yesterday. It
cannot tell you that p95 latency to Anthropic tripled an hour ago, that the circuit breaker is
open, that 12% of reservations are being denied, or that lease reclaims spiked after a restart.
Those are the questions an operator asks when the gateway is the thing that is wrong, and today the
only way to answer them is to read a JSONL sink by hand.

An engineering audit of this repo named it the largest gap. It is also the gap most likely to be
filled badly, because "add Prometheus" invites two specific mistakes this TD exists to prevent:
recounting usage in a second place, and unbounded label cardinality.

## First principles

1. **The meter and the telemetry are different things.** `UsageEvent` is the accounting atom;
   `UsageAggregateV1` is its view (TD-0009). Telemetry is about the **gateway's own behaviour** —
   latency, error rates, circuit state, reservation accuracy, plane selection. If a number can be
   billed on, it belongs to the meter and telemetry may only *derive* it, never recount it.
2. **Cardinality is a correctness property, not a tuning knob.** A metric labelled by `subject_id`
   or `session_id` is a memory leak with a dashboard attached. Per-subject attribution already has
   a home (the aggregate); metrics get bounded labels only.
3. **A library emits, an application decides.** `sandhi-core` and `sandhi-providers` are libraries
   that Victor links in-process. They must emit through a facade and install no subscriber, no
   exporter, and no background task — the host owns that choice.
4. **Telemetry is not a billing side-channel.** Neutral units only, and never a label or value that
   reintroduces dollars (ADR-0001).

## Non-goals

- **No dollars, no pricing, no SKU names** in any metric, label, or span attribute. The same line
  the README's phantom "cost display" crossed.
- **No per-subject / per-session / per-virtual-key metric labels.** Ever. See D2.
- **No telemetry stack in the language bindings.** The wheels stay dependency-light for exactly the
  reason `sandhi-store` is a separate crate (TD-0009 D1).
- Not a replacement for the usage sink, the aggregate, or the dashboard.

## Decisions

**D1 — `tracing` as a facade in the libraries; subscribers only in the binary.** `sandhi-core`,
`sandhi-providers` and `sandhi-store` take a `tracing` dependency and emit spans/events. They
install nothing. `sandhi-proxy`'s binary installs the subscriber. Victor, linking the libraries
in-process, captures Sandhi's spans in *its* logging with no configuration and no duplicate
runtime — which is the co-design outcome TD-0008 would ask for.

**D2 — A closed, bounded label set, enforced by a test.** Permitted labels: `provider`, `model`,
`dialect`, `plane` (`transparent` | `translation`), `outcome`, `policy` (`block` | `warn`), and
`backend`. That set is bounded by the catalog and the code, not by traffic. `subject_id`,
`group_id`, `session_id`, `virtual_key_id`, `request_id`, and anything caller-supplied are
**forbidden as labels** — a test asserts the metric registry never contains them, because a
convention that lives only in a doc will be broken by the first person who wants a per-user graph.
They remain available on *spans* (bounded lifetime, sampled) and in the aggregate.

**D3 — Metrics are derived from the event, never recounted.** Token counters are incremented from
the same `billable_parts` quantity the ledger settles on (#78's lesson: three implementations of
one number drifted into a 2× gap). A metric that cannot be derived from an event or a ledger
transition does not belong to accounting and must be named so it cannot be mistaken for it.

**D4 — Pull first, push behind a feature.** The proxy exposes `GET /metrics` in Prometheus text
format: no new infrastructure, no exporter configuration, works with what operators already run.
OTLP export lives behind a non-default cargo feature so the default build stays lean, and so a
deployment that wants traces opts in deliberately.

**D5 — `/metrics` is gated like the dashboard.** It reveals traffic shape, model mix, and failure
patterns. It follows ADR-0004 D4 exactly: admin-bearer-gated when an admin token is configured,
open only with an explicit opt-out env or when no token exists. It must never contain a virtual
key, a prompt fragment, or an upstream error body.

**D6 — What to emit, chosen for what only Sandhi knows.** A generic HTTP dashboard is not worth
building; these are the signals no sidecar can compute:

| signal | why it is Sandhi's to emit |
|---|---|
| reserve-vs-settle ratio | measures the ADR-0005 D1 ceiling's accuracy — over-reservation silently starves callers |
| cap denials / warn-policy admits | distinguishes "enforced" from "would have enforced" (TD-0003 P2) |
| lease reclaims | the crash-recovery signal; a spike means leases are leaking |
| cache-read vs fresh-input ratio | prompt-cache effectiveness, the thing the metering split exists to expose |
| plane selection (transparent vs translation) | ADR-0004 adoption signal — how much traffic still re-encodes |
| TTFT + duration histograms | already measured at the adapter boundary (#68); currently only stored |
| circuit state, retry counts | `ResilientProvider`'s internal state, invisible from outside |

## Phases

| Phase | Scope | Acceptance |
|---|---|---|
| **P1** ✅ | `tracing` events at the points only Sandhi can see; subscriber installed in the binary only | **Met.** A compile-time test asserts the three library crates cannot depend on `tracing-subscriber`; three tests drive the shipped binary and assert the plane event fires, that request telemetry carries no credential or attribution, and that a subscriber is actually installed |
| **P2** ✅ | `GET /metrics` (Prometheus text), the D6 signal set, D5 gating | **Met, and stronger than specified.** The label set is a *type* (`metrics::Labels`), so a forbidden dimension is unrepresentable rather than merely tested; the render test guards the output as a second line; a real request through the proxy lands in the registry with the right plane/dialect and no secret; `/metrics` reuses the dashboard's gate (401 without the admin bearer, 200 with it). Registry is hand-rolled — no new dependency |
| **P3** ✅ | OTLP export behind a non-default feature | **Met.** The `otel-otlp` feature (default off, `sandhi-proxy` only) exports `gen_ai.*` spans + metrics over OTLP/HTTP. The default build is unchanged — a CI guard asserts `opentelemetry` is absent from `cargo tree` without the feature, and the D1 compile-time test forbids it in the library crates. Attribution never leaves the process: the gen_ai span is built directly via the OTel Tracer API through a closed attribute allowlist (the `tracing_opentelemetry` bridge is deliberately **not** installed — see the amendment), and a red test asserts `subject_id`/`group_id`/`session_id`/`virtual_key_id`/`request_id` are absent from exported spans **and** metrics. |
| **P4** ✅ | Operator guidance: example scrape config, the four alerts worth having | **Met.** README "Operating it": log filtering, a gated scrape config, and four alerts (capacity leaking, enforcement off, callers refused, upstream degrading) — each expression checked against a series the code actually emits |

P1 is independently useful and touches no wire contract. P2 is where the cardinality discipline
has to hold. P3 must not move the default build's dependency graph.

## P3 amendment — the OTLP export path overrides D2 to a stricter, symmetric boundary

P3 landed as the `otel-otlp` cargo feature (default off, `sandhi-proxy` only). Four decisions
override or sharpen what D1/D2/D4 anticipated, recorded here so the implementation is not a
mystery:

1. **Attribution is forbidden on exported spans *and* metrics — stricter than D2.** D2 permits
   `subject_id`/`group_id`/`session_id`/`virtual_key_id`/`request_id` on *in-process* `tracing`
   spans ("bounded lifetime, sampled"). OTLP sends spans **off-process**, past the trust boundary,
   where those protections no longer hold — so the export path applies the metric rule to spans too.
   The boundary is structural: span/metric attribute keys are literals in `otel.rs`, produced only
   by a closed allowlist type (mirroring `metrics::Labels`); a `UsageEvent` field *name* can never
   become an attribute *key*, and the recorder only ever observes `UsageV2` + a provider slug + a
   model name (never the attribution fields, which live in `RequestMetadataV1`). A red test drives a
   request carrying the full attribution set through the dispatch→finalize chokepoint and asserts
   none of it reaches the exported span or metric.

2. **The `tracing_opentelemetry` bridge is deliberately not installed.** Layering it would bridge
   the proxy's existing `tracing::` events into exported spans verbatim — and `scope` (which encodes
   `vk:<id>`) and `request_id` appear in those events. There are no `#[instrument]`/`span!` sites in
   the tree, so the bridge's only effect would be to export exactly the events that leak. The gen_ai
   span is instead created directly via the OTel `Tracer` API. (If span correlation is ever wanted,
   the bridge must ship with a per-target filter that excludes `sandhi_store` and anything carrying
   `scope`.)

3. **The cache split is exported on span attributes only.** `gen_ai.client.token.usage` carries
   `gen_ai.token.type ∈ {input, output}` — there is no metric dimension for the prompt-cache split.
   `gen_ai.usage.cache_creation.input_tokens` / `cache_read.input_tokens` therefore live on the span.
   A faithful export of the cache split (the reason the metering split exists) *requires* spans, not
   just metrics. The token metric is **not** the billable quantity — billable stays in
   `sandhi_tokens_total{kind="billable"}`. `gen_ai.usage.input_tokens` = `tokens_in +
   cache_read_tokens` (semconv: "input SHOULD include cached"; cache_creation stays its own
   attribute, a write rather than consumed input).

4. **Measure-vs-price holds: no `gen_ai.*.cost.*`.** Neutral units only — never dollars, tiers, or
   SKU names (ADR-0001). The OTel export is `gen_ai.system` (deprecated → `gen_ai.provider.name` in
   newer semconv; emit the former, which collectors parse today), `gen_ai.request.model`,
   `gen_ai.operation.name = "chat"`, the usage attributes, and `gen_ai.response.id` (the provider's
   completion id — `upstream_request_id`, never Sandhi's `request_id`).

Dependency note: `opentelemetry`/`opentelemetry_sdk`/`opentelemetry-otlp` at 0.32, HTTP/protobuf over
the async reqwest client (not gRPC/tonic). `opentelemetry-otlp` 0.32's reqwest client pulls reqwest
0.13 while the proxy is on 0.12, so the binary carries both majors — accepted to keep the change
isolated (unifying on 0.13 is a separate scope). The OTel 0.32 MSRV (1.75) matches the workspace; the
feature does not move the default build's MSRV.

## Pressure test

1. **"This duplicates the usage event."** It would, if metrics recounted tokens independently —
   which is why D3 derives them from the same quantity the ledger settles. The distinction to hold:
   the event answers *who spent what* (durable, exact, billable); metrics answer *is the gateway
   healthy* (sampled, aggregate, disposable). If a metric ever becomes the source of truth for
   spend, this design has failed.
2. **"Bounded labels will not survive the first feature request."** Correct, if it is only a
   convention — so D2 makes it a test over the registry. The first person who wants a per-user graph
   should be pointed at the aggregate, which was built for exactly that and is bounded by an
   explicit cap with an overflow bucket (TD-0009 D2).
3. **"A `/metrics` endpoint is a new attack surface."** It is, and it is more sensitive than it
   looks: traffic shape and model mix are commercially interesting. D5 reuses the dashboard's gate
   rather than inventing a second policy, and the no-secrets requirement gets a test — the same way
   #77's redaction did.
4. **"`tracing` in the libraries bloats the wheels."** `tracing` is a facade: a few thousand lines,
   no runtime, no subscriber. The heavy parts (subscriber, OTLP, protobuf) live in the binary and
   behind a feature. If measurement shows otherwise, P1 can be feature-gated in the libraries too —
   but measure before adding the knob (ADR-052 discipline, imported via TD-0008).
5. **"Prometheus is the wrong choice; OTel is the standard."** Pull-based metrics need no collector
   and are what most self-hosted operators already run; OTel needs an endpoint, a protocol choice,
   and a running collector. D4 ships the one with no prerequisites first and makes the other opt-in
   — the reverse order would leave the common case unserved for longer.
6. **"Sampling makes the numbers wrong."** Only for spans, and deliberately: traces answer "what
   happened in this request", not "how much". Counters and histograms are unsampled. Anything
   billable stays with the meter, which samples nothing.
7. **"This is a lot of surface for a self-hosted gateway."** P1 alone would answer most of the
   audit's complaint, and each later phase is independently droppable. The sequencing is chosen so
   stopping after any phase leaves something coherent.

## Open questions

- ~~Does the metrics registry live in `sandhi-core` or only in `sandhi-proxy`?~~ **Resolved in P2:
  proxy-only.** No in-process consumer asked, and the bindings already have the aggregate snapshot
  for what they need (TD-0009 P2). Moving it later is additive; moving it back would not be.
- Should `plane` be a metric label or a span attribute? As a label it is bounded and genuinely
  useful for the ADR-0004 adoption question; the risk is that it invites `dialect`×`plane`×`model`
  fan-out. Measure the series count on a realistic model mix before committing.
- Is there a case for emitting the reserve-vs-settle *distribution* rather than a ratio? A ratio
  hides bimodality, which is exactly the shape a bad ceiling heuristic produces.
