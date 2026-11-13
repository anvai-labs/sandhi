# Shared buffered source qualification checkpoint

The source candidate implements [ADR-0040](../adr/0040-shared-buffered-source-qualification.md).
Ordinary and accounted buffered calls share strict source usage qualification for
Chat, Messages, Responses and Gemini. Constrained Codex calls reuse their qualified
SSE operation. Invalid or absent counters cannot become measured final zeros;
ordinary callers receive a terminal output-projection refusal without replay.

Known function calls retain required identity, name and object argument semantics.
Malformed functions and duplicate identities refuse rather than becoming empty or
silently dropped calls. Gemini retains optional unset ID/argument behavior. Native
Codex aggregation exposes complete terminal-only text/tools while preserving its
validated envelope. The first projection refusal survives a later drain failure.
Buffered Chat also preserves optional audio/prediction counter subsets, including
reported zero, without adding them to their inclusive parents again.

## Final candidate validation

| Validation | Result |
| --- | --- |
| Rust workspace | 1,228 passed; six opt-in ignores across 52 suites |
| Python binding, with fault tracing | 271 passed |
| Node binding | 69 passed |
| Actual vendor SDK buffered fidelity/refusal/recovery | 45 passed |
| Victor providers and selected agent/core resilience | 1,916 passed; five skipped |
| Default and ONNX/SentinelPass all-target Clippy | Passed |

The Python suite includes 140 local HTTP cases covering five source paths, native
and portable delivery, usage refusal, deterministic malformed-output refusal and
known function fields. Measured results and scope are recorded in the
[sanitized artifact](artifacts/shared-buffered-qualification-2026-11-12.json).
These are synthetic source-contract tests, not live cache-lifecycle evidence.

Success fixtures now declare the counters required by their standard source
profile. Codex fixtures supply actual terminal output and consistent item identity.
No production qualifier was weakened to preserve success-shaped old fixtures.

The review follow-up adds 24 default-retry HTTP cases for completed malformed or
duplicate-key JSON across all four buffered HTTP families and invalid Chat tool
discriminators. Each refuses deterministically after exactly one physical call.
Byte-read I/O errors keep their transport classification.
Qualified and unavailable response projections also preserve the decoder's
generation outcome; HTTP success cannot erase an incomplete or failed Responses
status. Unknown statuses remain outside the numeric qualification contract.
Eight local HTTP Codex subcases cover both delivery views, both incomplete reasons
and valid/invalid numeric usage. They retain the outcome through aggregation and
the unavailable fallback, with one source request each. Portable aggregation
carries terminal metadata without replacing earlier content with sparse output.

## Retained failed evidence

An earlier complete vendor SDK run finished **563 passed, one failed, three
skipped**. Its unresolved-usage TLS shutdown cell exceeded the existing six-second
process wait. The [shutdown report](sdk-shutdown-timeout-2026-11-12.md) preserves
the exact request/attempt/reservation join: unavailable usage, both liabilities
unsettled, no charge or receipt. The later focused 45-case pass does not cover or
diagnose this shutdown occurrence.

An earlier Python binding run finished **146 passed, one failed** after a child
reported the expected TLS hostname rejection and then exited with SIGSEGV.
[Native-exit evidence](python-native-tls-exit-2026-11-12.md) retains that failure
and bounded comparisons which did not reproduce it. The final binding pass does
not identify its cause. Both remain reliability/merge gates alongside the earlier
accounting waits.

## Live and remaining boundaries

Two tiny direct GLM buffered calls passed before the final review fixes: Chat with
the terminal-usage profile and Messages with the implicit-cache profile. Each
reported fresh 16, read 0, write 0, output 3 and one attempt. The artifact pins that
earlier wheel separately; these calls do not qualify the final candidate package,
gateway receipts, warm cache, savings or subscription execution.

The gateway peer counts/cache contract, accounting-default retirement, broader
media/agent and cache lifecycle matrices, issuer/subscription qualification and
supported release packaging remain open. Streaming Chat detailed audio/prediction
projection is outside this buffered change. Sandhi 0.17.0 and Victor 0.29.0 remain
prepared package targets; no package publication or merge is authorized by this
checkpoint.
