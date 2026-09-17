# Upstream InferFlux co-design asks — issue drafts (ADR-0008 consequences)

Two small, additive wire changes. Sandhi's OpenAI-compat usage parser
(`crates/sandhi-core/src/usage.rs`, `parse_openai_usage`) already understands both
shapes below — the moment InferFlux ships either, per-request cache-split accounting
lights up in Sandhi (and everything downstream of it) with **zero** further changes.

Filing these on the InferFlux tracker is an outward-facing action — confirm before
posting. Drafts below are ready to paste.

Status (2026-09-14): Drafts 1-3 SHIPPED in InferFlux develop.
Drafts 4-7 (below) are the next co-design asks.

---

## Draft 1 — SHIPPED: Per-request prompt-cache split in the usage object

**Title:** Report per-request prompt-cache hit/miss tokens in the chat-completions
`usage` object

**Body:**

InferFlux's radix prefix cache already accounts hits (per-request, internally —
`RadixPrefixCache` / `InferenceResult`), but the response body's `usage` reports only
`prompt_tokens` / `completion_tokens` / `total_tokens` (built inline in `BuildCompletionBody`,
`server/http/http_server.cpp` ~728, and the streaming terminal usage frame ~3284). The
per-request split is currently
visible only as server-global aggregates on `GET /v1/admin/cache` (`hits`, `misses`,
`matched_tokens`, `kv_reuse_tokens`), which cannot be attributed to a single call.

**Ask:** thread the per-request cached-prompt token count from the scheduler into the
usage object on both `/v1/chat/completions` and `/v1/completions` (and the streaming
terminal usage frame), using either of the two shapes OpenAI-compat consumers already
parse:

```jsonc
// Option A — OpenAI's nested form (preferred: widest tooling support):
"usage": {
  "prompt_tokens": 1000,          // total prompt, including cached (unchanged)
  "completion_tokens": 250,
  "prompt_tokens_details": { "cached_tokens": 640 }
}

// Option B — DeepSeek's top-level hit/miss form:
"usage": {
  "prompt_tokens": 1000,
  "completion_tokens": 250,
  "prompt_cache_hit_tokens": 640,
  "prompt_cache_miss_tokens": 360
}
```

Semantics to preserve: `prompt_tokens` stays *inclusive* of the cached portion; the
fresh-input count is derived by the consumer. Zero when the cache is disabled or the
request missed (not omitted — an explicit 0 is easier to reason about than absence).

**Why it matters:** metering gateways (Sandhi) and cost dashboards currently cannot
distinguish a prefill from a cache hit per call, so per-conversation accounting
over-reports billable input for exactly the workload InferFlux optimizes (multi-turn
agent loops replaying context). Sandhi parses both shapes today; this is the single
blocking fact for accurate per-request cache attribution.

---

## Draft 2 — SHIPPED: Per-request duration / time-to-first-token in the usage frame

**Title:** Include per-request `duration_ms` and `time_to_first_token_ms` in the
usage object (or a sibling field)

**Body:**

Prefill/decode/queue/forward latencies exist today only as Prometheus histograms on
`/metrics` (`server/metrics/`). A per-call consumer (client SDK, metering gateway)
cannot correlate a specific request with its latency.

**Ask:** carry per-request timings on the response, e.g.:

```jsonc
"usage": {
  "prompt_tokens": 1000,
  "completion_tokens": 250,
  "duration_ms": 1840,              // request accepted → last token
  "time_to_first_token_ms": 210     // request accepted → first streamed token
}
```

`time_to_first_token_ms` only for streaming; both are already measured internally by
the scheduler (`InferenceResult` timings) — this is exposure, not new instrumentation.

**Why it matters:** TTFT is the primary perceived-latency metric for interactive
agents, and per-request (not aggregate) latency is what per-conversation dashboards
need. Neutral units only (milliseconds) — no cost/pricing fields; downstream
consumers own any valuation.

---

---

## Draft 3 — Case-sensitive `Authorization` header parsing (compatibility bug)

**Title:** `Authorization` header is matched case-sensitively — RFC 9110 violation
that blocks Rust/HTTP2 clients from any secured deployment

**Body:**

Found while wiring the Sandhi gateway to a live `inferfluxd` (config with `api_keys`
set). Reproduction against `POST /v1/chat/completions` **and** `GET /v1/models`,
same key, same body — only the header spelling changes:

| Header spelling | Result |
|---|---|
| `Authorization: Bearer <key>` | ✅ authenticated (200 / expected route) |
| `authorization: Bearer <key>` | ❌ `401 {"error":"unauthorized"}` |
| `AUTHORIZATION: Bearer <key>` | ❌ `401 {"error":"unauthorized"}` |

HTTP/1.1 field names are case-insensitive (RFC 9110 §5.1). Hyper/reqwest — the HTTP
stack behind most Rust clients, including Sandhi — always sends lowercase
`authorization`, so **every secured InferFlux deployment is unreachable from the
Rust client ecosystem** (and any other client that lowercases, which is the
dominant convention post-HTTP/2). Likely a `headers.find("Authorization")`-style
exact match in the request parser; the fix is a case-insensitive lookup (and worth
sweeping the other parsed headers — `x-inferflux-session-id`,
`x-inferflux-client-request-id`, `traceparent` — for the same class of bug).

---

*Context for maintainers: these asks come out of the Sandhi ↔ InferFlux
integration (Sandhi ADR-0008 — InferFlux admitted as an OpenAI-compat catalog
provider with `x-inferflux-session-id` session-affinity mapping). Sandhi measures
latency itself at the gateway regardless; draft 2 removes the discrepancy between
gateway-measured and server-measured timings. Draft 3 was verified against a live
`inferfluxd` v0.1.0 build on 2026-08-31.*

---

## Draft 4 — Reasoning separation (`reasoning_content` + `reasoning_tokens`)

**Title:** Separate `<think>` reasoning from user-facing content and report reasoning tokens

**Body:**

Reasoning models (Qwen3, LFM2.5) emit `<think>...</think>` blocks that currently
leak into the user-visible `content` field. Gateways and agent frameworks need
the reasoning text separated so `content` carries only the answer.

**Ask:** at the completion body builder, split `<think>` blocks from `content`:

```jsonc
"choices": [{
  "message": {
    "role": "assistant",
    "content": "The answer is 255.",
    "reasoning_content": "15 * 17 = 15 * 10 + 15 * 7 = 150 + 105 = 255."
  }
}],
"usage": {
  "completion_tokens_details": { "reasoning_tokens": 42 }
}
```

Non-breaking: when no `<think>` block is present, `reasoning_content` and
`reasoning_tokens` are omitted.

---

## Draft 5 — Unique completion ids + `client_request_id` echo

**Title:** Unique completion ids and `client_request_id` echo for request-response correlation

**Body:**

Completion ids use `<prefix><epoch-seconds>`, which collides for concurrent
requests. And `client_request_id` (accepted from body or the
`x-inferflux-client-request-id` header) is stored but never echoed.

**Ask:** id = `<prefix><epoch-ms>-<counter>` (unique per call, survives same-ms
bursts). Echo `client_request_id` in the response body and as a response header
so gateways can join request-response pairs without guessing.

---

## Draft 6 — Resolved model on completion bodies

**Title:** Report the resolved model (post-capability-fallback) on completion bodies

**Body:**

Completion bodies currently echo the requested model string (or `"unknown"`).
When capability fallback changes the backend, the gateway cannot tell which
model actually served. `GET /v1/embeddings` already returns the resolved id.

**Ask:** set the completion body's `model` field to the resolved model id
(matching the embeddings endpoint's existing behavior).

---

## Draft 7 — Always-emit streaming usage + OpenAI error envelope + rate-limit headers

**Title:** Three gateway-facing contract fixes (usage chunk on stub path, error envelope, rate headers)

**Body:**

Three small changes that make InferFlux a drop-in origin for metering gateways:

1. Emit the terminal streaming usage chunk on every path including the
   no-backend/stub path (currently the stub path omits it even when
   `stream_options.include_usage=true`).
2. Use the OpenAI error envelope `{"error":{"message","type","code"}}` instead
   of `{"error":"string"}` so SDK clients parse errors correctly.
3. Add `Retry-After` and `X-RateLimit-Remaining: 0` headers to 429 responses.
