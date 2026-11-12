# Qualified Codex accounting: source acceptance

The workspace candidate implements [ADR-0027](../adr/0027-qualified-codex-buffered-accounting.md).
These are synthetic source tests with disposable HTTP origins and SQLite stores;
they do not qualify a live Codex account or deployment.

## Evidence exercised

- The provider HTTP matrix crosses native-output enabled/disabled with valid,
  incomplete/failed, missing/malformed, progress-only, post-terminal, truncated,
  duplicate, identity-mismatched, multiline/BOM/CRLF and unframed terminal events.
  It verifies one send, unchanged account/credential ownership, constrained wire
  fields, retained canonical counts and refusal of a reused attempt context.
- Retry-enabled configurations and expired leases refuse before HTTP, with zero
  observed dispatches. Native encrypted reasoning output remains available only
  when requested.
- A deterministic decoder-failure test places final usage in the same observed
  chunk or a later unpolled chunk. Only the former retains final totals; the latter
  retains preceding partial counts. Neither case polls another inference request.
- A virtual-clock deadline test includes setup time and recurring heartbeats,
  checks original versus scoped whole-call bounds, and observes one timeout terminal
  with either partial or final source usage.
- The in-process gateway matrix crosses four ingress shapes with seven evidence
  cases: final, partial, missing, post-terminal, failed, unavailable and explicit
  zero. Its 28 HTTP calls join frozen Codex profile/translation context, one physical
  attempt, canonical usage and both reservation states. Valid final totals settle
  both; failed model output can still have final accounting; partial/unavailable
  totals leave both held. Explicit zero settles zero, preserving reported zero.

Relevant commands:

```sh
cargo test --locked -p sandhi-providers --test qualified_codex
cargo test --locked -p sandhi-providers decoder_failure_retains
cargo test --locked -p sandhi-providers qualified_buffered_deadline
cargo test --locked -p sandhi-proxy --test proxy qualified_codex::
cargo test --locked --workspace
cargo clippy --locked --workspace --all-targets -- -D warnings
```

Final verification passed 1,098 Rust tests (six opt-in ignored), warnings-denied
workspace/all-targets Clippy, formatting/diff checks, 62 SDK regressions, strict
MkDocs, 13 documentation/brand unit tests and the brand checker. The provider HTTP
matrix makes 24 calls; the gateway matrix makes 28 calls. Tests do not consume a
live subscription credential.

The fixed-origin production factory remains covered separately by endpoint and
subscription tests. Local transport injection is confined to the test composition;
the shipped gateway gains no arbitrary subscription-origin override.

## Failures retained and their limits

An initial broad run exposed a cancellation regression: unfinished keepalive
comments were classified as unfinished data. Source closure now distinguishes them.
Dedicated partial-data/comment drop cases and the existing disconnect/deadline/
shutdown drill verify this distinction; data framing remains conservative.

That same run also returned an intermittent settlement 502 on call 138 of the
existing 200-call stress test. It retained final 10-input/2-output observations and
one process-local owner. No static accounting reason was captured in that run.
The [retained failure artifact](artifacts/codex-accounting-source-failures-2026-11-11.json)
preserves both failures. A subsequent isolated 200-call run and the 15-case owned
accounting group passed with logging enabled, as did a later full workspace run.
Those passes do **not** establish the stress failure's cause or prove it resolved.
The broader M5/M6 reliability gate remains open.

Full SDK regressions passed 62 cases across tracked protocols and backend registry
tests. The [GLM tracked Responses pair](zai-router-tracked-responses-stream-2026-11-11.md)
is separate live evidence for standard Responses, not proof of Codex subscription
behavior. Remaining goal requirements are recorded in
[TD-0034](../td/TD-0034-protocol-neutral-router-cache-accounting.md).
