# Upstream InferFlux co-design asks — issue drafts (ADR-0008 consequences)

Two small, additive wire changes. Sandhi's OpenAI-compat usage parser
(`crates/sandhi-core/src/usage.rs`, `parse_openai_usage`) already understands both
shapes below — the moment InferFlux ships either, per-request cache-split accounting
lights up in Sandhi (and everything downstream of it) with **zero** further changes.

Filing these on the InferFlux tracker is an outward-facing action — confirm before
posting. Drafts below are ready to paste.

---

## Draft 1 — Per-request prompt-cache split in the usage object

**Title:** Report per-request prompt-cache hit/miss tokens in the chat-completions
`usage` object

**Body:**

InferFlux's radix prefix cache already accounts hits (per-request, internally —
`RadixPrefixCache` / `InferenceResult`), but the response body's `usage` reports only
`prompt_tokens` / `completion_tokens` / `total_tokens`
(`server/http/http_server.cpp`, `BuildUsageBody`). The per-request split is currently
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

## Draft 2 — Per-request duration / time-to-first-token in the usage frame

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
