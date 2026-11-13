# Sandhi cache accounting and dashboard co-design handoff

Status: **Dated investigation snapshot; Sandhi C1–C4 merged, joint replay C5 open.** Owner: Sandhi.
Companion project: [InferFlux](https://github.com/anvai-labs/inferflux).

Execution tracking: [TD-0028](../td/TD-0028-cache-accounting-availability.md).
The first regression increment preserves the sanitized non-stream usage projections;
its constructed SSE replay is not a live streaming capture. Contract, dashboard,
diagnostic-export and joint live gates remain separate. The investigation below is
a dated snapshot, not an assertion that those checkouts still serve the same source.
The implementation and qualification boundaries are recorded in TD-0028;
the historical missing-field behavior and checkout identities below are not current
implementation/deployment claims.

```mermaid
flowchart LR
  V[Victor member session] --> S[Sandhi transparent forwarding]
  S --> I[InferFlux usage.cached_tokens]
  I --> U[Canonical usage parser]
  U --> D[SQLite and dashboard]
```

| Boundary | Current contract | Co-design responsibility |
|---|---|---|
| OpenAI-compatible usage | Inclusive prompt minus cached = fresh input | Preserve exactly once, both planes and SSE |
| Missing cache field | Currently normalized to zero | Distinguish unavailable from explicit zero additively |
| Member identity | x-sandhi-session → x-inferflux-session-id | Keep canonical catalog mapping and request correlation |
| Dashboard attribution | Same call population has identical aggregates | Explain grouping and display availability/sample coverage |

## Verified evidence (2026-09-18)

The 08:20:40 dashboard snapshot showed InferFlux 40 calls / 72,970 fresh input /
4,361 output / 0 cache-read; ZAI 63 calls / 23,643 fresh input / 27,141 output /
140,416 cache-read. Identical subject/group totals are expected: every call used
victor-local / multiagent-validation. Provider/model attribution is already split.

**Caching is implemented in InferFlux, reported on its wire, and recorded by Sandhi.**
A controlled probe disproved a blanket “InferFlux cache unsupported” diagnosis.

| Request shape | Direct InferFlux cached tokens | Through Sandhi cached tokens | Observation |
|---|---:|---:|---|
| Plain chat, first / repeat | 0 / 426 | 426 / 426 | Gateway followed direct calls, so its first request was already warm |
| Plain chat, appended turn | 426 | 426 | Incremental prompt reused a prefix |
| Tools present, first / repeat | 0 / 514 | 514 / 514 | Tools alone do not disable caching |
| response_format=json_object, repeated | 0 / 0 | 0 / 0 | Structured path bypasses prefix reuse |
| logprobs=true, repeated | 0 / 0 | 0 / 0 | Logprob path bypasses prefix reuse |

Plain repeated response usage: prompt_tokens=451, completion_tokens=1,
prompt_tokens_details.cached_tokens=426, total_tokens=452. Sandhi SQLite stored
fresh tokens_in=25, tokens_out=1, cache_read_tokens=426. Appended turn stored
62 fresh + 426 cached + 1 output. Three gateway rows therefore contain 1,278 cached
tokens. The two tool-enabled gateway rows each contain 514 cache-read tokens.
No inference from latency was used to manufacture cache counts.

Metrics before/after these 18 requests: inferflux_kv_prefix_reuse_total 20→28;
inferflux_kv_prefix_reuse_tokens_total 47081→50747. The backend-labelled
inferflux_prefix_hits_total / misses / matched_tokens / partial_hits all stayed zero.
These are different counter families; zero legacy prefix counters do not mean no KV reuse.

Probe IDs: cache-audit-5e98e7ffc5 and cache-paths-992460b7bf. Sandhi rows use suffixes
-sandhi, -tools-sandhi, -json-sandhi, -logprobs-sandhi. The reproducible payload is
80 repetitions of “alpha beta gamma delta. ” in a system message, a short user
request, qwen3-coder-30b, temperature=0, max_tokens=32, stream=false. Repeat unchanged,
then append the previous assistant content and another short user turn. The tools
probe adds one read(path:string) function; the JSON and logprob probes independently
set response_format={type:json_object} or logprobs=true. Do not clear a shared cache
to force a cold sample; use a unique prefix and record warm/cold provenance.

## Qualification environment

The controlled comparison used the same ready InferFlux model directly and through
Sandhi, with separate provider/model attribution. Runtime configuration, source
and executable provenance were verified during qualification; deployment-specific
addresses, process identifiers and private artifact locations are retained by the
maintainers rather than serving as setup instructions.

Public source and consumer changes remain traceable through
[Sandhi #265](https://github.com/anvai-labs/sandhi/pull/265) and
[Victor #1115](https://github.com/anvai-labs/victor/pull/1115).
A separate model battery is recorded in
[Victor #1118](https://github.com/anvai-labs/victor/pull/1118).
This comparison did not qualify a distributed tracing deployment.

## Proven limits and unresolved original symptom

The earlier Victor member run had zero cache-read across its InferFlux calls.
Its original full upstream request/response bodies were not retained in this audit;
therefore this handoff does NOT assign a definitive root cause to those 40 zeros.
Changing prompt prefixes, cache/sequence pressure, or an alternate execution path
must be measured on a replay. Tools alone are disproven as the general cause.
The mixed team itself passed: six Qwen members + one GLM reviewer, seven distinct
sessions, fourteen Python deliverables + review.json, seven passing pytest tests,
and exact per-member input/output/total reconciliation in 308.15 seconds.
Victor PR: https://github.com/anvai-labs/victor/pull/1115.

## Sandhi work items, in PR-sized order

1. **Capture regression fixtures from this live path.** Extend the existing InferFlux
   corpus with explicit zero and positive cached-token usage, non-stream and terminal
   SSE usage. The existing fixtures already include cached_tokens; do not create a
   competing provider parser. Assert transparent raw bytes and neutral accounting.
2. **Design additive availability provenance.** parse_openai_usage currently maps
   a missing prompt_tokens_details.cached_tokens to zero. Preserve existing numeric
   defaults but add a validated availability/source signal through UsageV2/event,
   SQLite migration, API, bindings/schema generation, and dashboard. Explicit zero,
   absent field, malformed field, and unsupported provider must be distinguishable.
   Write consumer decisions before changing contracts; use the repo's ADR/TD process.
3. **Dashboard clarity.** Show “not reported” or reporting coverage when appropriate;
   a mixed aggregate needs n_reported/n_total rather than labelling every zero a miss.
   Explain cache read versus cache write and neutral billable units. OpenAI-compatible
   cache writes being zero is not evidence that no KV state was created. Retain
   existing neutral-unit arithmetic; cache-read is not a dollar discount calculation.
4. **Bounded diagnostic export.** Provide a credential-free request/session/run lookup
   containing reported usage fields, normalized counters, origin/gateway duration
   source, provider/model and correlation IDs. Prompt/body capture must remain opt-in
   and bounded. Account for late streaming usage and aborted streams explicitly.
5. **Joint gate after InferFlux investigation.** Replay one actual Victor member trace
   against the same ready model directly and through Sandhi, checking cache counts,
   request IDs, session affinity, and all usage sums. Do not infer cache savings from
   elapsed time or silently substitute estimates for upstream counters.

## Source and validation pointers

- crates/sandhi-core/src/usage.rs: parse_openai_usage (~146); inclusive cache split
  (~179–200); missing field defaults to zero. DeepSeek hit/miss is an existing branch.
- crates/sandhi-providers/src/raw.rs: forward_metered_with_headers and metered SSE;
  family parser is shared with typed adapters. Keep one derivation/normalization.
- crates/sandhi-providers/src/catalog.rs: InferFlux session_header and
  client_request_id_header facts (~225–243), not a new hard-coded gateway branch.
- crates/sandhi-providers/tests/fixtures/inferflux/{complete.json,stream.sse};
  tests/provider_corpus.rs; core usage tests; proxy SQLite/dashboard tests.
- Existing live binary fix had 23 raw-forwarding tests and all CI gates pass.
  This handoff itself changes no runtime code. Future changes: targeted cargo tests,
  cargo fmt --check, clippy and workspace/schema/contract gates as required by repo.

Acceptance: explicit cache hit survives direct→gateway→SQLite→dashboard; absent usage
is visibly different from zero without breaking existing numeric consumers; latency
source/sample counts remain honest; no secrets or prompts enter default metering logs.
