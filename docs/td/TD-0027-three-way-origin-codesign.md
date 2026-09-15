# TD-0027: Three-way origin co-design — InferFlux producer contract, reasoning separation, conformance

- **Status:** **In progress** (2026-09-15). InferFlux-side production work (Phase 1 & 2 of the
  original plan) is fully shipped and on InferFlux `main`. Sandhi's own Phase 0/3/5 work is
  partially started: issue-draft updates and a conformance skeleton exist on branch
  `co-design/three-way-contract` (PR #255), open but CI-red. Victor's side (Phase 0.3, Phase 4)
  has not started at all — verified by searching Victor's PR/commit history for any trace of
  this effort; nothing found there past the docs update this repo made.
- **Relates to:** [inferflux-issue-drafts.md](../upstream/inferflux-issue-drafts.md) (drafts 1-7),
  [integration-handoff.md](../upstream/integration-handoff.md) (archived predecessor — that
  document covered a different, earlier integration slice: session affinity, auth
  case-sensitivity, the I1-I3 InferFlux fixes. This TD covers the *origin producer contract* —
  token counts, reasoning separation, request identity, error shape — that came after it),
  TD-0013 (no-re-tokenization principle), TD-0022 (per-call transport context), ADR-0008
  (admission and session affinity).
- **Companion changes:** InferFlux PR #170 (gateway-facing producer contract), #171 (reasoning
  separation, buffered path), #172 (llama.cpp v0.2.0 + reasoning separation extended to
  streaming), #174 (gpt-oss/harmony chat template + a second reasoning-channel format) — all
  merged and promoted to InferFlux `main` as of 2026-09-15. Sandhi PR #255 (this repo, open,
  CI-red — see Lane S below).

## Why this TD exists

A three-way audit (2026-09-14) across InferFlux, Sandhi, and Victor found the premise "Sandhi
approximates tokens instead of using exact usage" true only in two narrow sites (the budget
admission ceiling's `bytes/4` estimate, and interrupted-stream fallback) — for completed calls
Sandhi already trusts InferFlux's exact usage. What the audit actually found missing was on the
**InferFlux producer side**: no reasoning/content separation, non-unique completion ids, no
resolved-model field, an inconsistent streaming usage frame, and a non-OpenAI error envelope.
Those gaps are now closed on InferFlux `main`. This TD tracks what's left: Sandhi's own
consumption-hardening work (W7/W8), the conformance suite that pins the contract against drift,
and the cross-repo records the original plan called for.

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
| A second reasoning-format family: gpt-oss/harmony channels | #174 | **Same wire shape as above** — this is transparent to consumers. `reasoning_content`/`reasoning_tokens` now populate correctly whether the model used `<think>` tags (Qwen3, LFM2.5) or harmony channels (gpt-oss). No Sandhi/Victor-side change needed for this specifically; noted here so a conformance fixture built against one family isn't assumed to cover the other. |
| `INFERFLUX_DISABLE_REASONING_SPLIT` kill switch | #171/#172 | tags/channels stay in `content` verbatim when set |
| `INFERFLUX_STUB_COMPLETION` env override | #172 | lets model-free CI (this repo's conformance suite included) exercise a `<think>`-tagged or harmony-shaped canned completion end-to-end without a real model |

Full wire contract: `docs/API_SURFACE.md` in the InferFlux repo, §5 (usage extensions table)
and the reasoning_content/delta rows immediately below it.

## LANES — what's left, in dependency order

### Lane S1 — Land PR #255 (small, do first)

The PR (`co-design/three-way-contract` → `develop`) is content-complete: it marks InferFlux
drafts 1-3 shipped, adds drafts 4-7 for the capabilities above, and adds
`tests/sdk-conformance/test_inferflux_origin.py` (a skip-gated suite that launches `inferfluxd`
in stub mode and pins usage-echo, streaming-usage, and error-envelope shapes through the
`openai` SDK). It is **already in sync with `develop`** (2 commits ahead, 0 behind as of
2026-09-15 — no rebase needed). Two CI checks are red:

- `lint-title` failed against the OLD PR title ("Three-way co-design: issue-draft update +
  InferFlux-origin conformance skeleton" — no conventional-commit prefix). **The title has
  since been changed** to "docs: InferFlux issue-draft update + origin conformance skeleton",
  which should satisfy the linter — but CI has not been re-run since the title fix. Re-run
  (`gh pr checks 255` after a re-run, or push an empty commit / `gh pr edit` no-op to
  retrigger) and confirm.
- `CI Success` (the aggregate gate) failed because every constituent job in that run reports
  `skipped`, not passed — including `Route CI trust (gatekeeper detection)` and
  `Authorize owner private CI`. This smells like a trust/gatekeeper routing decision that
  didn't fire for this branch/run rather than a real test failure — investigate why the gate
  skipped everything (check the gatekeeper workflow's trigger conditions against this PR's
  branch name/author) before assuming a simple re-run fixes it.

*Done when:* `CI Success` is green and #255 is merged to `develop`.

### Lane S2 — Extend the conformance suite (W10 in the original plan)

Once #255 lands, `tests/sdk-conformance/test_inferflux_origin.py` is the seed. Per the original
plan, extend it to pin: reasoning separation on **both** the buffered and streaming planes
(use `INFERFLUX_STUB_COMPLETION` with a `<think>`-tagged canned string for one fixture and a
harmony-channel-shaped one for another — see the table above, both should produce identical
`reasoning_content`/`reasoning_tokens` wire shape), cached_tokens propagation, header round-trips
(`client_request_id`, `traceparent`), and attribution-never-forwards. Add a pin file
(`tests/sdk-conformance/inferflux_pin`) recording the InferFlux ref this suite is built against,
and wire CI to build InferFlux at that pin (CPU stub build, ccache — nightly fallback if that's
too slow for per-PR CI). Rule going forward: a PR changing a pinned contract updates the pin
file in the same or a same-day follow-up PR.

### Lane S3 — W7: per-model token estimate calibration (Rust, `sandhi-providers`)

**Recommended approach (from the original audit, still valid): EWMA chars-per-token per
`(provider_slug, model)`, calibrated from historical `Final`/`basis=Measured` `UsageEvent`s —
input term only.** Cold start (<32 samples) keeps today's `(len+3)/4`; floor at 0.75× of
today's estimate; bounds [1.5, 8.0]. This is a *statistics-over-authoritative-events* change,
not a second tokenizer call — honors TD-0013. Test: replay InferFlux fixtures + provider
corpus, assert reservation ≥ settled usage at least as often as today; a CJK-heavy corpus
should show `sandhi_settle_overshoot_tokens_total` growth drop vs. baseline.

### Lane S4 — W8: latency-semantics reconciliation

InferFlux now reports `duration_ms`/`time_to_first_token_ms` as origin-measured (§ above).
Reconcile: origin-reported wins and is tagged as such; Sandhi's own boundary-measured timing
becomes the fallback only when the origin field is absent (older InferFlux versions, or a
non-InferFlux provider). Close this out alongside the already-verified auth
case-insensitivity fix (draft 3, already marked shipped).

### Lane S5 — Records (do last, once S1-S4 are real)

Update this TD's status line to "Complete" with a scorecard (mirror TD-0008's format). Cross-link
Victor's consumer-side record once it exists (see the Victor handoff, delivered separately —
`docs/co-design/HANDOFF-2026-09-15.md` in the victor repo covers its Phase 0.3/Phase 4 items).

## Verification

- `cargo test --workspace`, `cargo fmt --check`, `cargo clippy -- -D warnings`, coverage ≥75%
  (this repo's usual gate) for any Rust changes (S3/S4).
- `tests/sdk-conformance/` suite green with `INFERFLUX_SERVER_BIN` pointed at a built
  `inferfluxd` (CPU stub build is sufficient — no GPU/model needed for any pin in this TD).
- Manual e2e (once per lane, not CI-gated): real InferFlux behind the sandhi proxy — confirm a
  reasoning-model response (either family) shows separated `reasoning_content` end to end
  through the transparent plane, and that a CJK-heavy prompt shows the calibration's effect on
  `sandhi_settle_overshoot_tokens_total`.
