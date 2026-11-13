# TD-0028: Cache accounting availability and bounded diagnostics

- **Status:** C1–C4 and terminal-stream repair are merged. Historical controlled
  comparisons passed; broad mixed-provider deployment acceptance is separate.
- **Scope:** [Cache co-design](../upstream/inferflux-cache-codesign-2026-09-18.md).
- **Related:** TD-0013, TD-0027 and [ADR-0008](../adr/0008-inferflux-admission-and-session-affinity.md).

## Findings rechecked before implementation

The preserved, credential-free local audit has 18 non-stream usage projections and
nine Sandhi SQLite rows. All nine rows conserve the reported counts: plain repeated
451 prompt = 25 fresh + 426 cached; appended 488 = 62 + 426; tools 587 = 73 + 514;
the recorded JSON/logprobs probes have 436 fresh and zero cached. Gateway requests
followed direct requests, so the first gateway plain/tools request was already warm.

`parse_openai_usage` already subtracts cached input exactly once. The raw forwarder
and OpenAI-compatible adapter share the family parser. The catalog owns the InferFlux
session and client-request-ID headers. Existing complete/SSE fixtures already cover
a fully cached 50-token prompt; no new provider parser or hard-coded routing branch
is needed. Missing and malformed cache fields currently normalize numerically to zero;
that is an availability limitation, not proof of an upstream cache miss.

Evidence does not establish why the earlier Victor member run reported zero cache.
It also does not prove that lookup matches always equal reuse actually executed by
InferFlux; that producer-side investigation remains independently owned upstream.

## Ordered increments and gates

| Slice | Bounded change | Acceptance | State |
|---|---|---|---|
| C1 | Extend existing InferFlux corpus with the 18 sanitized usage objects | Explicit zero/partial/full cache accounting; both forwarding paths; unchanged response bytes and one late terminal SSE usage emission | [PR #268](https://github.com/anvai-labs/sandhi/pull/268) merged after clean independent review and real CI pass |
| C2 | ADR and additive cache-read availability/source contract | Define reported zero, absent, malformed and explicitly unsupported; preserve legacy numeric defaults; parser/event/UsageV2/SQLite/API/generated bindings/schema agreement | [PR #269](https://github.com/anvai-labs/sandhi/pull/269) merged after exact-head CI and clean independent review |
| C3 | Dashboard availability and coverage | `n_reported/n_total` over the same filtered call population; honest cache read/write and neutral-unit labels | Merged in #269; 25 real-browser dashboard regressions pass |
| C4 | Bounded credential-field-free diagnostic lookup/export | Admin-authorized persisted request/session/run projection, source-labelled timings and normalized counters; unavailable historical evidence stated explicitly; no prompt/body capture added | [PR #270](https://github.com/anvai-labs/sandhi/pull/270) merged after clean independent review and exact-head CI; isolated WSL HTTP/CLI validation passed |
| C5 | Joint replay after InferFlux investigation | One actual member trace, same ready model, direct and gateway; cache counts, correlation/session mapping and usage conservation | Actual-member baseline passes; terminal-stream repair #274 merged and unchanged accepted-runtime five-call oracle passes across wire/SQLite/C4/dashboard. Broader mixed-provider acceptance remains separate |

## Qualification summary

This public summary preserves measured behavior and its limits. Raw runtime
records, host topology and intermediate incident logs are retained privately.

| Check | Observed result | Limit |
| --- | --- | --- |
| Non-stream usage corpus | 18 recorded usage objects and nine gateway projections conserved fresh/cache/output counts | Constructed envelopes around usage objects are regression fixtures, not complete live response captures |
| Reporting availability | Reported zero remains distinct from missing, malformed or unsupported reporting | Zero alone does not prove a cache miss |
| Dashboard coverage | Availability is counted over the same filtered call population | Reporting coverage is not cache hit rate |
| Bounded diagnostics | HTTP and CLI projected the same persisted rows without new model calls | This does not establish new provider acceptance |
| Stream lifecycle | A terminal-stream correction passed the controlled five-call comparison across wire, SQLite, diagnostics and dashboard | Broader team runs and origin lifecycle behavior require separate qualification |

Diagnostics tests cover authorization first, indexed exact selectors, byte and row
bounds, malformed stored fields, duplicate IDs and cancellation. Actual SQLite
work retains lifecycle ownership until it finishes; canceling an HTTP waiter must
not make retained database work appear idle.

## Contract decisions implemented by C2

Do not infer unsupported capability from a missing counter or a reported zero. Define
how validated wire presence and an explicit producer/catalog capability declaration
compose, including malformed/inconsistent counts. Specify legacy-row defaults,
per-call versus attempt denominators, missing terminal usage, aborted streams,
multi-call aggregation and unknown future enum values. Source identifiers must be
bounded vocabulary, never raw provider objects or arbitrary diagnostic strings.

Consumer decisions must cover Rust source compatibility as well as additive JSON
compatibility, SQLite migration/backfill behavior, generated Python/Node facades and
schema/contract-minor rules. No version bump or publication is implied by this TD.
Numeric billable-unit arithmetic must not change and must not become dollar pricing.

## Operational and release boundaries

Use isolated state and explicitly selected endpoints for qualification. Preserve
shared caches and active provider services; a cold-cache test must not clear a
shared cache. Default metering must not retain secrets, prompts or full provider
bodies. Record source/runtime provenance privately and publish only the evidence
needed to reproduce a supported contract or understand a measured limitation.

Each change requires its normal review, CI and release gates. Controlled local
acceptance does not establish managed deployment, broad provider compatibility or
publication of a new version.
