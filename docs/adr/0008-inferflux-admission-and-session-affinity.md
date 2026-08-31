# ADR-0008: InferFlux admission as catalog data, and session affinity as a transport fact

Date: 2026-08-31

## Status

**Accepted.** InferFlux is admitted as an OpenAI-compat catalog row — not a provider family —
and the catalog grows its second strategy-via-data header field, `session_header`, which both
proxy planes map the neutral session id onto.

Relates to [ADR-0001](0001-sandhi-architecture-and-wire-contract.md) §4 (attribution rides
outside the cached prompt), [ADR-0003](0003-provider-adapter-authoring-and-codegen.md)
(adapter authoring; "vendor differences are data"), [ADR-0004](0004-two-plane-proxy-and-enforcement-boundary.md)
D1 (the two planes), and [TD-0004](../td/TD-0004-catalog-governance-dual-mode.md)
(catalog governance: data yes, policy no).

## Why this exists

InferFlux (`anvai-labs/inferflux`, sister project) is a self-hosted C++17 inference server
(v0.1.0) for on-prem/edge workloads — an Ollama/LM Studio alternative with continuous
batching, a radix prefix cache, and speculative decoding. Multi-agent workflows (victor) will
run on it, and every one of those calls must pass through Sandhi counted and attributed like
any other provider.

The obvious template — "integrate it like Ollama" — is wrong, and rejecting it is the first
decision here. Ollama is integrated as a **native family** because it speaks its *own* wire
(NDJSON `/api/chat`, `prompt_eval_count`/`eval_count`). InferFlux speaks the **OpenAI Chat
Completions dialect** (`POST /v1/chat/completions`, OpenAI SSE framing, usage only when the
client sends `stream_options.include_usage` — which both Sandhi planes already inject). It
needs no adapter, no codec, no family, and no schema change.

The second question is throughput. InferFlux's optional session-handle layer keys
KV/prefix-cache reuse per conversation (`x-inferflux-session-id` header or `session_id` body
field; server flag `runtime.scheduler.session_handles.enabled`, TTL 300 s, max 1024
sessions). Multi-agent loops replay context turn after turn; affinity to the cache is the
difference between a prefill and a cache hit. Sandhi already carries the conversation key
out-of-band — `ProviderRequest::session_id`, "the conversation key for cache/KV affinity" —
but no adapter forwards it. This ADR decides how it crosses to the wire without dragging
attribution with it.

## Decision

### D1. Admission is a catalog data row, not a family.

One `OpenAiCompatProviderSpec` entry: slug `inferflux`, base URL
`http://127.0.0.1:8080/v1` (its default bind), no aliases, no curated models, no
`EndpointFamilyV1` variant, no codegen. `ProviderFamily::for_slug` already falls through to
`OpenAiCompat`; the proxy's `default_base_url` and the bindings' `known_openai_compat` read
the spec table. A native family would fork the OpenAI codec for zero capability gained —
exactly the branch-in-shared-code the catalog exists to prevent.

Bearer auth is optional server-side (anonymous read+generate when no keys are configured), so
the empty-secret convention used for Ollama applies unchanged; a secured deployment resolves
its real key from the vault.

### D2. The curated lineup is empty, on principle.

InferFlux model ids are **operator config** (its `registry.yaml`), not vendor facts — there is
no fixed lineup to curate, the same stance the catalog already takes for aggregators
(Together, OpenRouter). Consumers that want a listing use the proxy's `/v1/models` (the
virtual key's permitted intersection) or edit their own policy tier (victor's YAML). Sandhi
must not manufacture volatile capabilities merely because a provider shares a wire.

### D3. Session affinity is a spec fact — the second strategy-via-data header field.

`OpenAiCompatProviderSpec` grows `session_header: Option<&'static str>`, set to
`Some("x-inferflux-session-id")` for InferFlux and `None` everywhere else (the struct has no
`Default`, so the compiler enforces the explicit choice per vendor). This follows
`request_id_header` exactly: the second case where a vendor's wire difference is expressed as
a table cell rather than a code branch, in the lineage of `FamilyFacts`/`UsageCadence`
(declared once, in one place, and *tested* rather than trusted).

Both planes consume the same neutral source — `ProviderRequest::session_id` (typed plane,
resolved from `ChatRequestV1.metadata.session_id`) and the proxy's request metadata
(transparent plane, from `x-sandhi-session` ingress or explicit metadata) — and inject it as
a **request header only**. The body is untouched: cache hits depend on byte-stable prompts,
so affinity that mutated the body would destroy the thing it exists to create.

### D4. Only the session key crosses; attribution never does.

The header carries the conversation key and nothing else. `subject_id`/`group_id`/virtual-key
identity is key-authoritative metering input consumed by `usage_event()`, and adapters never
read `ProviderRequest::attribution` — enforced by construction, and pinned by a proxy
negative test (`inferflux_attribution_never_reaches_the_upstream`) that fails if any
`x-sandhi-*` attribution header or body key reaches the upstream. This is ADR-0001 §4 applied
at the egress boundary.

### D5. Sandhi sends the header unconditionally; InferFlux's flag stays server-side.

The session-handle layer is feature-flagged **off** upstream by default, and a server with
the flag off ignores the header. Sandhi does no capability gating — a per-request descriptor
lookup on the hot path for a header the server safely discards is not a trade. Operators who
want KV reuse enable `scheduler.session_handles.enabled` on the InferFlux side.

### D6. Upstream request correlation: a third spec fact, caller-owned injection.

`client_request_id_header` (`x-inferflux-client-request-id` for InferFlux, `None` elsewhere)
names the vendor's per-request correlation header. The value is the id the **proxy mints at
admission** — no longer lazily at event assembly — so the same string is (a) sent upstream,
where InferFlux keys its per-request logs/metrics on it, and (b) the usage event's
`request_id` fallback. One id, two ledgers, 1:1 correlation.

Two deliberate asymmetries with D3. First, **injection is caller-owned**: session affinity is
adapter-injected because the conversation key lives in neutral request metadata, but the
correlation id is *minted by the caller* (the proxy), so it rides TD-0022's per-call wire
headers — the spec fact supplies only the vendor's header name. Second, **mint-early**
changes timing, not value space: every successful event already got an id; minting it at
admission is what makes it usable mid-flight, and it composes with TD-0021 G20 (the inert
`idempotency-key`): the dedup lookup G20 wants needs exactly a per-call id minted before the
upstream call. G20 itself remains that TD's item.

Two recorded caveats. The injection idiom lives at two altitudes (the proxy's
`per_call_wire_headers` on the typed plane, the adapter's builder-resolved injection on the
transparent plane) and must evolve in lockstep; a `ProviderRequest::correlation_id` field
mirroring `session_id` would collapse them if a third consumer appears. And the event's id
precedence prefers `upstream_request_id` when a provider supplies one — nothing populates it
on the success path today, but a future wiring that does would break the 1:1 join for
providers that *also* declare a correlation header, and must be reconsidered then.

## Consequences

- **Positive.** Admission cost one table row plus tests, and both planes gained a mechanism
  (`session_header`) any future affinity-keyed vendor reuses without code changes. The
  transparent plane serves InferFlux calls byte-exact (same-family OpenAI dialect) while the
  affinity header rides alongside — the highest-throughput path Sandhi has, with cache
  affinity on top.
- **Positive.** The usage contract needs nothing new: `include_usage` injection already
  guarantees the terminal usage frame, and `parse_openai_usage` already understands both
  OpenAI's `prompt_tokens_details.cached_tokens` and DeepSeek-style hit/miss counts — if
  InferFlux ships either, the cache split meters with **zero** Sandhi changes (asked for
  upstream; today the whole prompt meters as fresh input, which the fixture pins).
- **Negative/expectation.** `billable_tokens()` will read high next to cache-discounting
  providers until that split ships, and InferFlux emits no request-id response header (only a
  child `traceparent`), so `upstream_request_id` stays Sandhi-minted. A client sending
  `n>1`/`best_of>1` with `stream:true` gets InferFlux's 400 surfaced as an upstream error —
  correct, but worth knowing. InferFlux also closes the connection after every stream, so
  latency includes a per-request TCP handshake.
- **Neutral.** Latency (`duration_ms`, TTFT) is measured by Sandhi itself; InferFlux reports
  timing only in Prometheus today. No dollars anywhere, on either side — the measure-vs-price
  line is held.

## Pressure test

1. **"It's a sister project — special-case it."** The kindest treatment is the standard one:
   a first-class catalog slug with zero bespoke code is *more* support than a forked adapter
   family, because it inherits every fix to the shared codec for free.
2. **"Session affinity in a header violates 'attribution rides outside the wire'."** It
   doesn't — that invariant is about the *cached prompt* (body), and its purpose is exactly
   cache reuse; a transport header keyed to the conversation is the sanctioned channel. The
   line that matters — subject/group never reaching the upstream — is D4, tested negatively.
3. **"Gate the header on a capability flag so we don't send dead bytes."** Dead bytes are one
   header on a request that already exists; gating costs a hot-path descriptor lookup and a
   config surface. When the server ignores it, nobody pays; when it doesn't, everybody wins.
4. **"A per-spec capabilities override for `prompt_cache_usage: false` would be more
   honest."** The blanket value is a statement about the dialect (this wire *can* express a
   cache split), and the gap is temporary — the moment upstream ships the field, an override
   would be wrong and need removing. Consumers whose policy needs today's truth (victor's
   `cache.supported: false`) own that layer.
5. **"Why not map affinity onto the body `session_id` field InferFlux also accepts?"** Body
   mutation breaks prompt-cache byte-stability for every other turn shape, splits the
   mechanism across two planes (the transparent plane promises envelope-only normalization),
   and the header works on both planes uniformly.

## References

- [ADR-0001](0001-sandhi-architecture-and-wire-contract.md) §4 — attribution outside the
  cached prompt; the invariant D4 enforces at the egress seam.
- [ADR-0003](0003-provider-adapter-authoring-and-codegen.md) — hand-written adapters are for
  native dialects; OpenAI-dialect vendors are catalog data.
- [ADR-0004](0004-two-plane-proxy-and-enforcement-boundary.md) D1 — the transparent plane the
  same-family InferFlux call rides.
- [TD-0004](../td/TD-0004-catalog-governance-dual-mode.md) — catalog data vs consumer
  policy, which D2 applies to operator-defined model ids.
- [TD-0013](../td/TD-0013-streaming-usage-fidelity.md) — the `include_usage` cadence both
  planes already inject; InferFlux's terminal-only usage frame conforms.
