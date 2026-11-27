# ADR-0050: Structured owner-origin timeout frame and partial-usage evidence

- **Status:** Source candidate for the next feature release.
- **Owner:** Streaming body ownership in `sandhi-proxy` (`streaming.rs`, `streaming/qualified.rs`, `stream_response`).
- **Supersedes:** ADR-0047's silent teardown for owner body deadlines at a complete event boundary.
- **Context incident:** The GLM-5.3 body-cap kills left victor/RepoReel with `ProviderConnectionError (provider-failure)` and no way to distinguish "model still reasoning" from "transport broke" (handoff §3, ask 3).

## Decision

1. **The owner can end a timed-out body with a structured SSE error.** It uses the ingress dialect's encoder and emits the terminal marker required by that dialect. It appends the error only at a complete event boundary and before the source has observed an origin terminal or failure. If appending would corrupt the framing, delivery keeps the torn behavior. The error is a transport terminator and carries no settlement authority.
2. **Safe timeout frames can report measured progress.** After the source drops, the owner derives the wire frame and settlement input from one snapshot. Non-final provider-reported usage remains `Partial` and both holds remain unresolved. A receipt still requires qualified `Final` provider evidence. Usage without measurements remains unavailable, and the existing ADR-0048 retention policy applies.
3. **The source still owns its ending.** Provider errors, origin idle timeouts, disconnects, shutdowns and pre-dispatch refusals keep their existing behavior. Native stream attachments and streams through the constrained Codex subscription codec retain their existing endings, including Codex subscriptions translated to Chat Completions.

## Wire shapes

| Plane | Shape |
|---|---|
| Translated (OpenAI chat) | `data: {"error":{"code":"timeout","message":...,"http_status":504,"details":{"timeout_phase":"body","measured_usage":...}}}` + `data: [DONE]` |
| Translated (Anthropic) | dialect encoder `Error` arm (`event: error`) |
| Translated (Gemini) | `{"error":{"code":...,"status":...}}` |
| Translated (Responses) | `event: error` + `{"type":"error",...}` |
| Transparent (same-family raw) | The same ingress-specific error encoding, appended only at a complete SSE boundary |

## Consequences

- Victor (and any client with the existing error-event vocabulary) classifies edge body-cap timeouts as `ProviderTimeoutError` instead of `ProviderConnectionError` — no client-side change needed (verified: victor's error-event handler + `code == "timeout"` mapping).
- The existing ADR-0048 sweep handles aged unresolved liabilities. The timeout frame does not change accounting eligibility.
- `/readyz` still reports 503 while long tracked streams hold in-flight intents — pre-existing conservative semantics, unchanged.
- During rollout, clients should tolerate both structured errors and a torn connection.
