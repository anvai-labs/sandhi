# ADR-0014: Dialect parity — token counting endpoints and streaming fidelity

Date: 2026-11-03

## Status

**Accepted for implementation.** Implemented with unit and end-to-end proxy
tests; merge, deployment and release gates remain distinct.

Relates to [ADR-0004](0004-two-plane-proxy-and-enforcement-boundary.md)
(translation plane), [ADR-0013](0013-native-extensions-passthrough-and-auto-cache-control.md)
(reserved passthrough), and [ADR-0008](0008-inferflux-admission-and-session-affinity.md)
(neutral catalog facts).

## Verified problem and non-goals

Agent clients speak their vendor's dialect and call more than the generate
endpoint. Claude Code calls `POST /v1/messages/count_tokens` on every context
trim (the gateway 404'd); Antigravity-class Gemini clients stream tool calls
(the Gemini SSE encoder silently DROPPED tool-call events — there is no partial
frame in that protocol, so nothing was emitted); the Anthropic egress stream
never opened content blocks (deltas without `content_block_start`, thinking and
text sharing index 0); Codex-class Responses clients re-read the terminal
`response.completed` frame, whose `output` was always `[]`; OpenAI completion
objects lacked the spec-required `created` timestamp that the official SDKs
declare mandatory.

Non-goals: no new metered surface (counting is not generation), no vendor
tokenizer fidelity (the estimate is the gateway's own reservation estimate),
no change to the OpenAI-dialect translation paths, and no `?key=` query-param
auth on the Gemini dialect (header schemes only, as today).

## D1. Token-counting endpoints answer from the request body alone

`POST /v1/messages/count_tokens` → `{"input_tokens": N}` and
`POST /v1beta/models/{model}:countTokens` → `{"totalTokens": N}` run the FULL
pre-dispatch pipeline — credential → virtual key → provider resolution →
`prepare` (decode validation, model allowlist, policy input gate, per-key rate
limit) — and then answer without dispatching: no lease, no generation, no
`UsageEvent`. The estimate is the same `TokenEstimator::estimate` over the
request bytes the gateway reserves against, keeping counting consistent with
budget enforcement. No upstream is dialed (pinned by a test pointing the
upstream at a dead port).

## D2. Streaming fidelity: stateful per-stream egress encoders

The neutral `ChatStreamEventV1` stream is event-per-event; three dialects need
cross-event state, now held by `codec::StreamEventEncoder` (one instance per
response stream):

- **Anthropic**: content-block lifecycle — `content_block_start`/`stop` with
  unique indices across prose and tools, thinking vs text as distinct block
  types, `input_json_delta` on the owning block, a spec-shaped `ping` after
  `message_start`, and open blocks closed on Finish/Error.
- **Gemini**: tool calls buffered across start/argument deltas and emitted as
  ONE complete `functionCall` part at `ToolCallEnd` (the protocol has no
  partial frame — previously these events were silently dropped, breaking any
  agentic Gemini client on cross-family routes). Stream errors now render as
  `{"error": ...}` data frames instead of vanishing.
- **Responses**: streamed text and buffered function calls fold into the
  terminal `response.completed` frame's `output` (full items, `status`),
  and `response.output_item.done` carries the complete item — what Codex-class
  clients re-read.

The OpenAI Chat upstream decoder retains `finish_reason` until trailing usage
has been decoded. It publishes the neutral Finish at `[DONE]` (without waiting
for HTTP EOF), or after flushing a clean EOF tail for compatible transports.
This keeps final usage ahead of translated Responses/Anthropic terminal
envelopes. A transport error before completion does not publish the retained
successful Finish.

Same-family (transparent-plane) routes remain byte-exact and untouched.

## D3. OpenAI Chat Completions: `created` on completions and chunks

Completion objects and every chunk now carry the spec-required `created` unix
timestamp (SDK client models declare it mandatory). No other OpenAI shape
changed; the OpenAI dialect remains the preferred ingress and its translation
into Anthropic/Gemini upstreams is unchanged.

## D4. Translated traffic is labeled, never native

Every `UsageEvent` carries the ingress `route` (`/v1/chat/completions`,
`/v1/messages`, `/v1beta/...`), so an OpenAI-dialect request translated to an
Anthropic/Gemini upstream is distinguishable from native-dialect traffic on the
same upstream (pinned). The translation planes themselves are unchanged.

## Testing

- `codec.rs`: block-lifecycle sequence (thinking→text→tool, indices, ping,
  stop reasons), Gemini whole-part assembly + error frame, Responses terminal
  folding + full `output_item.done`.
- `tests/proxy.rs`: counting endpoints answer without dispatch (dead-port
  upstream, empty sink), auth enforced with dialect-shaped 401s, and the route
  label pin.
