# TD-0027: Three-way origin co-design — InferFlux producer contract, reasoning separation, conformance

- **Status:** **In progress** (release follow-through, 2026-09-17). S1-S5 and Sandhi review
  fixes are promoted to `main` via #261/#262. Exact-main CI passed; immutable `v0.7.0`
  is published and independently verified across all required targets. InferFlux and Victor release candidates
  were rebased onto their current `develop` branches and retain review/CI/publication gates;
  see the [review and delivery gates](../reviews/three-way-release-review-2026-09-16.md).
  Model-free wire tests do not establish loaded-model accuracy, performance, or production readiness.
- **Relates to:** [inferflux-issue-drafts.md](../upstream/inferflux-issue-drafts.md) (drafts 1-7),
  [integration-handoff.md](../upstream/integration-handoff.md) (archived predecessor — that
  document covered a different, earlier integration slice: session affinity, auth
  case-sensitivity, the I1-I3 InferFlux fixes. This TD covers the *origin producer contract* —
  token counts, reasoning separation, request identity, error shape — that came after it),
  TD-0013 (no-re-tokenization principle), TD-0022 (per-call transport context), ADR-0008
  (admission and session affinity), and Victor's
  [InferFlux reasoning-separation consumer contract](https://github.com/anvai-labs/victor/blob/3b5aa2b0dfd0947ea261802158aa39f6091caf90/docs/architecture/inferflux-reasoning-separation-handoff.md).
- **Companion changes:** InferFlux PRs #52 (auth casing + usage telemetry), #170 (gateway-facing
  producer contract), #171 (buffered reasoning separation), #172 (streaming separation), #174
  (gpt-oss/harmony template), and #176 (model-free origin-contract completion); Sandhi PRs
  [#255](https://github.com/anvai-labs/sandhi/pull/255),
  [#256](https://github.com/anvai-labs/sandhi/pull/256),
  [#258](https://github.com/anvai-labs/sandhi/pull/258), and
  [#259](https://github.com/anvai-labs/sandhi/pull/259); Victor PR
  [#1066](https://github.com/anvai-labs/victor/pull/1066).

## Why this TD exists

A three-way audit (2026-09-14) across InferFlux, Sandhi, and Victor found the premise "Sandhi
approximates tokens instead of using exact usage" true only in two narrow sites (the budget
admission ceiling's `bytes/4` estimate, and interrupted-stream fallback) — for completed calls
Sandhi already trusts InferFlux's exact usage. What the audit actually found missing was on the
**InferFlux producer side**: no reasoning/content separation, non-unique completion ids, no
resolved-model field, an inconsistent streaming usage frame, and a non-OpenAI error envelope.
Those gaps are now closed on InferFlux `main`. This TD tracked the remaining Sandhi
consumption-hardening work (W7/W8), the conformance suite that pins the contract against drift,
and the cross-repo records; the completion evidence is recorded below.

## The boundary (unchanged from TD-0008, restated for this contract slice)

**InferFlux owns** (it runs the tokenizer and sees generation structure at the source): exact
token counts (prompt/completion/cached/reasoning), reasoning vs. content separation, resolved
model/backend identity, per-request latency (duration_ms, TTFT), origin error shape.

**Sandhi owns**: budget admission/reservation estimation (a *statistical* ceiling, calibrated
from InferFlux's own settled events — never a second tokenizer call, per TD-0013), meter-side
request correlation, session/KV-affinity header mapping, ingress enforcement and retryability.

**Victor owns**: prompt/message construction, tool/model policy, client-side retry ownership,
reasoning-content consumption and display, pricing.

## What InferFlux shipped (verify against these, don't re-derive)

| Capability | Shipped in | Wire shape |
|---|---|---|
| Unique completion ids + `client_request_id` echo | #170 | `id` field + `client_request_id` body/header echo |
| OpenAI error envelope + `Retry-After`/`X-RateLimit-Remaining` | #170 | `{"error":{"message","type","code"}}`, 429 headers |
| Streaming usage frame always emitted (including stub/no-backend path) | #170 | terminal SSE frame when `stream_options.include_usage=true` |
| TTFT histogram | #170 | `usage.time_to_first_token_ms` (streaming only) |
| Resolved model on completion bodies | #170 | `model` field reports the model that actually served, not the request string |
| Reasoning separation, buffered path | #171 | `message.reasoning_content`, `usage.completion_tokens_details.reasoning_tokens` |
| Reasoning separation, **streaming** path | #172 | `delta.reasoning_content` frames before `delta.content`; terminal usage frame carries the same `reasoning_tokens` |
| A second reasoning-format family: gpt-oss/harmony channels | #174/#176 | **Same wire shape as above** — this is transparent to consumers. `reasoning_content`/`reasoning_tokens` now populate correctly whether the model used `<think>` tags (Qwen3, LFM2.5) or harmony channels (gpt-oss). No Sandhi/Victor-side format-specific parsing is needed. |
| `INFERFLUX_DISABLE_REASONING_SPLIT` kill switch | #171/#172 | tags/channels stay in `content` verbatim when set |
| `INFERFLUX_STUB_COMPLETION` env override | #172/#176 | lets model-free CI (this repo's conformance suite included) exercise a `<think>`-tagged or harmony-shaped canned completion end-to-end without a real model |

Full wire contract: `docs/API_SURFACE.md` in the InferFlux repo, §5 (usage extensions table)
and the reasoning_content/delta rows immediately below it.

## Implementation scorecard (2026-09-17)

“Closed” below means the original implementation lane, not registry publication or production
acceptance. The release checkpoint and remaining roadmap follow the lane record.

| Seam | State | Verdict |
|---|---|---|
| InferFlux origin producer contract | Exact usage/cache/reasoning, identity, error shape, resolved model, and latency fields shipped; model-free gaps closed by InferFlux #176 | **Closed** |
| S1 · Seed record and conformance skeleton | Sandhi #255 merged after the skipped-check routing diagnosis | **Closed** |
| S2 · Pinned origin conformance | Exact InferFlux commit `273780835f120cbe1a8da4860d72905861e71e29`; CPU/stub build driven through the real Sandhi proxy and OpenAI SDK in CI | **Closed** — Sandhi #256 |
| S3 · Reservation calibration | Per-`(provider, model)` EWMA uses only final, measured origin events; cold-start/floor/bounds retained; synthetic low-ratio overshoot regression pinned | **Closed** — Sandhi #258, promoted in #262, published in v0.7.0 |
| S4 · Latency semantics | Origin timing wins independently per field and carries `origin` provenance; Sandhi boundary timing fills only absent fields and carries `boundary` | **Closed** — Sandhi #259 |
| Auth header compatibility | Secured exact-pin e2e succeeds through reqwest's lowercase `authorization`; InferFlux unit coverage also pins header and bearer-scheme casing | **Closed** — InferFlux #52 |
| Victor consumer boundary | Original reasoning separation merged in #1066; later review found inclusion-flag coercion, aggregate billing and timing provenance gaps | **Original lane closed; follow-up pending** — corrected v0.9.5 candidate still needs CI/promotion/publication |

## Lanes — completion record (dependency order)

### Lane S1 — PR #255 landed

PR #255 merged at `c3d0be555b13a6883c53257414f066e1599be734`. The apparently all-skipped
run was the intentionally inert `pull_request_target` mirror (`OWNER_PRIVATE_CI_ENABLED=false`),
not the real required run. The live `pull_request` run did execute; its remaining release failure
was traced to a self-hosted machine carrying an `ubuntu-latest` label despite an older glibc.
The correctly routed run passed, including aggregate `CI Success`, before merge.

### Lane S2 — Extend the conformance suite (W10 in the original plan)

PR #256 merged at `3d04ce69220ab8f4495b95a60d46741736d4a776`. The suite pins InferFlux
`273780835f120cbe1a8da4860d72905861e71e29` (InferFlux #176), builds that exact revision as a
CPU/stub server in per-PR CI, and drives it through the real Sandhi proxy with OpenAI's SDK. It
pins buffered and streaming reasoning for both `<think>` and harmony families, cache usage,
request/trace correlation, attribution non-forwarding, resolved model/error shape, and terminal
usage. A contract-changing PR must update `tests/sdk-conformance/inferflux_pin` in the same PR
or a same-day follow-up.

### Lane S3 — W7: per-model token estimate calibration (Rust, `sandhi-providers`)

PR #258 merged at `704d9c50a5ba03219799f8b89cebae0994dcb536`. Sandhi now keeps an EWMA
chars-per-token estimate per `(provider_slug, model)`, updated only from `Final`, measured usage
(cached input included) and applied only to the input reservation term. Cold start remains 32
samples, bounds remain `[1.5, 8.0]`, and the 75%-of-baseline floor prevents an optimistic
reservation collapse. Synthetic replay checks estimator arithmetic and a low-bytes/token case
pins a reduction in `sandhi_settle_overshoot_tokens_total` growth. These are not captured
request/tokenizer pairs or a real CJK corpus. No tokenizer was added.

### Lane S4 — W8: latency-semantics reconciliation

PR #259 merged at `a93015dcc5ebd10342440770df7c087e6224edfc`. `duration_ms` and
`time_to_first_token_ms` now carry independent `origin`/`boundary` provenance through `UsageV2`,
`UsageEvent`, JSON schemas, SQLite, and both bindings. Typed, metered, buffered proxy, and
streaming proxy paths preserve an origin value exactly; boundary measurement fills only a
missing field. The exact-pin run used InferFlux's secured default config, so its ten passing
origin cases also exercise the shipped case-insensitive auth fix with reqwest's lowercase
header.

### Lane S5 — Records (do last, once S1-S4 are real)

This completion update adds the scorecard and cross-links Victor's merged consumer-side record
from PR #1066 (`3b5aa2b0dfd0947ea261802158aa39f6091caf90`). Victor pins reasoning-event
ordering/separation and `reasoning_tokens`; context replay/trimming remains an explicitly
separate product decision rather than an unclosed producer-contract item.

## Release checkpoint and remaining roadmap

- Sandhi [#261](https://github.com/anvai-labs/sandhi/pull/261) merged review fixes into
  `develop`; [#262](https://github.com/anvai-labs/sandhi/pull/262) promoted them to main
  `e05d7c5f16b1577da2ecb445642cd1ef9ea608a7`. Exact-main
  [push CI](https://github.com/anvai-labs/sandhi/actions/runs/35193468410) passed with
  executed `CI Success` and `Release safeguards`; a skipped mirror is not this evidence.
- Immutable `v0.7.0` points to that commit. All-target
  [release run](https://github.com/anvai-labs/sandhi/actions/runs/35203459727) passed on attempt 2:
  all publishers succeeded on attempt 1; only read-only verification was retried after an npm
  platform-package 404. Independent all-target verification also passed. Protected back-sync
  remains open at this checkpoint.
- The latest-develop release scope includes InferFlux tool-call/reasoning/KV-capacity and
  Victor curated-tool/session-context regressions discovered during the expanded review.
  Fix candidates and CPU/local tests do not imply merged or released fixes.
- Next: back-sync Sandhi evidence; validate Victor against the published binding; complete
  independent review and fresh rebased CI, promotion, native release gates and publication for
  InferFlux v0.3.0 and Victor v0.9.5. Record each exact source and outcome separately.
- Deferred optimization: bound calibration-key cardinality, evaluate resolved-model identity,
  collect real multilingual calibration pairs, and separate latency aggregates by provenance.
  These are follow-ups, not completed S3/S4 guarantees. TD-0026 P01–P03 remain production gates.

TD-0027's historical “W7/W8” labels mean reservation calibration/latency. They do **not** close
TD-0026 W07 scoped security or W08 hierarchical policy, which remain separate planned work.

## Verification

- Sandhi PRs #255, #256, #258, and #259 all merged with aggregate `CI Success` green.
- S4's local and CI gates passed `cargo test --workspace`, fmt, clippy, generated-schema/facade
  drift, Node (24 tests), Python (37 tests), and 88.32% Rust line coverage (86.80% region
  coverage; line threshold 75%).
- The exact pinned InferFlux CPU/stub build passed all ten origin cases locally and the complete
  vendor-SDK/dashboard CI job. This is the authoritative e2e for the model-free contract; GPU
  runners, a loaded model, and Windows packaging are intentionally outside this TD.
- The S3 synthetic replay measures reservation coverage on its selected ratios, not general
  workload accuracy. Origin reasoning separation is pinned for both supported fixture families.
- The release review strengthens the origin suite to 16 cases: a progressive, case-preserving
  recorder, direct mixed-case auth probes, injected attribution headers, and exact rounded
  persisted latency comparisons. The original ten cases did not prove all those properties.
