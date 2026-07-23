# TD-0006: Implementation plan — the transparent-metering plane (byte-exact proxy passthrough)

Status: Draft (proposed) — **Step 1 approach revised 2026-07-23 (see banner)**
Date: 2026-07-22
Implements: [ADR-0004](../adr/0004-two-plane-proxy-and-enforcement-boundary.md) D1

> **Revised 2026-07-23 (pressure-test) — the "surface the existing adapters" premise is
> falsified.** The typed adapters cannot be a byte-exact transport: they parse the body to
> `serde_json::Value`, inject `stream`/`stream_options.include_usage`, re-serialize via `.json()`,
> and carry **no response headers**. Corrections, superseding Steps 1–3 below:
> - **Add a dedicated raw forwarder** that owns the `reqwest` POST with `.body(bytes)` and never
>   touches `Provider`/`ChatProvider`. Request bytes and a **curated response-header allowlist**
>   (pass `retry-after` / request-id / rate-limit; strip hop-by-hop + `Authorization`) pass
>   through; the client's **message/content** bytes are preserved.
> - **The promise is "content-faithful, envelope-normalized," not "byte-identical."** Delete the
>   golden byte-identity test. OpenAI streaming *requires* injecting `stream_options.include_usage`
>   to meter at all — treat that (and `stream:true`, `Accept-Encoding: identity`) as documented,
>   cache-neutral **envelope normalizations to the upstream**, and assert usage is non-zero when
>   the client omits the flag.
> - **Force `Accept-Encoding: identity` upstream** (reqwest is built without gzip/brotli) so bytes
>   are plaintext for both sniffing and forwarding, or preserve `Content-Encoding` end-to-end.
> - **Bound and shape the sniffer** — `metered_passthrough`'s line buffer is unbounded and assumes
>   newline-delimited SSE; Gemini's non-SSE JSON-array stream and huge single tool-call lines blow
>   it up and meter zero. Cap the buffer; make usage extraction transport-shape-aware
>   (SSE / NDJSON / single-JSON-array); guard the JSON parse with a `contains(b"usage")` substring
>   check before parsing every delta.
> - **Select the plane from the vault-declared `ProviderFamily`**, never `for_slug` (which defaults
>   unknown slugs to OpenAI-compat and would byte-forward an OpenAI body to an Anthropic upstream).
>   Thread `ProviderFamily` onto `ProviderHandle`. Gemini stream-vs-complete is URL-method-driven,
>   not body-`stream`-driven.
> - **Validate `model` against a strict charset** before it enters the Gemini upstream URL path.
> - **Keep the `Drop`-based finalizer** and, per [ADR-0005](../adr/0005-enforcement-correctness-reservation-ledger-observe-enforce-split.md)
>   D1, settle `Partial` (not zero) on mid-stream disconnect for `Block` scopes.

## Goal

Make the proxy forward byte-exact on the common path so the "prompt-cache safe" / "drop-in
replacement" promises hold, without losing metering. Restate the target precisely:

- **Same-family** (ingress dialect == upstream family): forward the request body **unchanged**,
  stream the response through `metered_passthrough`, extract usage with the existing
  `sniff_usage_line` parsers. No `ChatRequestV1`, no re-encode.
- **Cross-family** (ingress ≠ upstream): keep today's `ChatRequestV1` translation path, now
  explicitly labeled lossy.

## Current state (what the plan changes)

Confirmed by audit (file:line current as of the audit; re-confirm before editing):

- Ingress routes: `sandhi-proxy/src/lib.rs:103-105` map `/v1/chat/completions` →
  `IngressDialect::OpenAi`, `/v1/messages` → `Anthropic`, `/v1/responses` → `Responses`
  (`codec.rs:16-23`). No Gemini/Cohere ingress.
- Every request: `decode_request(dialect, body, meta)` → `ChatRequestV1` (`lib.rs:289`) →
  `ProviderHandle::complete/stream` (`lib.rs:317-319`), which **re-encodes** via the per-family
  codec; response rebuilt by `encode_response` (`lib.rs:432`) and stream frames by
  `encode_stream_event`. `ProviderHandle` explicitly refuses raw transport
  (`typed.rs:97-99`).
- Upstream family is resolved from the virtual key's `upstream_ref` (`lib.rs:256-267`) —
  **independent of the ingress route**, which is exactly what lets us compare the two.
- The byte-exact primitive already exists: `metered_passthrough` +
  `sniff_usage_line` (`sandhi-providers/src/lib.rs`), used by the raw adapters but **not** by
  the proxy.

## Design

### Step 1 — expose a raw transport on the provider handle

`ProviderHandle` currently only offers the typed `complete/stream`. Add a raw sibling that the
transparent plane uses (the adapters underneath already implement `Provider::complete/stream`
returning raw `ProviderResponse`/`ByteStream` with parsed usage — this is surfacing what exists,
not new transport):

```rust
impl ProviderHandle {
    /// Byte-exact forward: body is sent to the upstream unchanged; usage is sniffed from the
    /// response. Caller guarantees `body` is already in the upstream family's native dialect.
    pub async fn complete_raw(&self, model: &str, body: Bytes, meta: Attribution)
        -> Result<ProviderResponse, ProviderError>;
    pub async fn stream_raw(&self, model: &str, body: Bytes, meta: Attribution)
        -> Result<ByteStream, ProviderError>;
}
```

These reuse the resilience decorator (retry/circuit-breaker/timeout) and `metered_passthrough`
already wrapping the adapters. `typed.rs:97-99`'s comment ("raw transports intentionally do not
cross this boundary") is revised: raw transport is allowed **when the caller has proven dialects
match**, which the proxy does structurally.

### Step 2 — a plane selector in the proxy handler

In `handle()` (`lib.rs`), after resolving the vkey and its upstream family, before decoding:

```rust
let ingress_family = dialect.family();              // OpenAi -> OpenAiCompat, etc.
let upstream_family = handle.family();              // from upstream_ref
if ingress_family == upstream_family {
    // TRANSPARENT PLANE: forward raw bytes, sniff usage.
    let est = estimate_reservation_bytes(&raw_body);        // reuse byte/4 (or Step 5)
    ledger.reserve(scope, est)?;
    let resp = handle.complete_raw(model, raw_body, attr).await?;   // or stream_raw
    // usage already parsed by the adapter's sniffer
    emit_event(&resp.usage, ...);
    ledger.reconcile(scope, est, billable(&resp.usage));
    return forward_bytes(resp);   // status + body verbatim
}
// CROSS-FAMILY PLANE: unchanged ChatRequestV1 path (lib.rs:289 onward).
```

Key property: on the transparent plane the client's **exact bytes** reach the upstream, so
Anthropic message-level `cache_control`, OpenAI `logprobs`/`n`/`logit_bias`, Gemini
`safetySettings`, and any field Sandhi never modeled all survive — and the **response and stream
are forwarded verbatim** (fixes the response-regeneration loss too).

`dialect.family()` is a small total function; `IngressDialect::Responses` maps to
`OpenAiResponses` (its own family), so an OpenAI-Chat client routed to a Responses upstream still
correctly takes the cross-family plane.

### Step 3 — add Gemini and Cohere ingress dialects

Add `IngressDialect::Gemini` (`/v1beta/models/{model}:generateContent` and `:streamGenerateContent`)
and `IngressDialect::Cohere` (`/v1/chat`). Each needs:
- a route registration alongside `lib.rs:103-105`;
- a `family()` arm;
- for the **cross-family** plane only, a `decode_request`/`encode_response` implementation (the
  transparent plane needs no codec — that is the point). Ship Gemini ingress transparent-only
  first (Gemini-in/Gemini-out), add its cross-family decoder later if a consumer needs it.

This makes native Gemini and Cohere clients able to point at the proxy at all — today they
cannot.

### Step 4 — first-class `cache_control` on the chat contract (for the cross-family plane)

So breakpoints survive even when translation is unavoidable, add an optional, additive field to
`ChatMessageV1` variants and content parts:

```rust
#[serde(default, skip_serializing_if = "Option::is_none")]
pub cache_control: Option<CacheControlV1>,   // { type: "ephemeral", ttl?: "5m"|"1h" }
```

Additive → non-breaking under TD-0002's v1 policy; regenerate `chat-request.v1.schema.json` and
the binding facades (the `codegen-drift` gate enforces this). The Anthropic encoder then re-grafts
message-level breakpoints the way it already does for system/tool blocks
(`anthropic_typed.rs:113-125,148-159`). Other families ignore the field.

### Step 5 — (optional, from ADR-0004 D4) tighten the estimate and the billable definition

Independent of planes but adjacent: the reservation estimate is `bytes/4`
(`lib.rs:505-518`), and the budget bills `tokens_in + tokens_out` while the event meters the
cache split. Pick one `billable()` definition in `sandhi-core` and use it in both the ledger and
the event; optionally swap the byte heuristic for a model-aware token count on the reserve path.
Not required for the plane split, but cheap to land alongside.

## Testing

- **Golden byte-identity:** for each same-family pair, assert the bytes the upstream mock
  receives equal the client's bytes exactly (wiremock body matcher), and the bytes the client
  receives equal the upstream's exactly. This is the regression guard for D1.
- **Cache-control survival:** an Anthropic request with a message-level `cache_control` breakpoint
  reaches the upstream with the breakpoint intact (transparent plane) and, separately, survives
  the OpenAI-in/Anthropic-out cross-family path once Step 4 lands.
- **Usage parity:** transparent-plane usage equals the adapter-layer `sniff_usage_line` output
  for the same recorded stream (reuse the TD-0001 W1 fixtures — `stream_cache_split.sse` etc.).
- **Plane selection:** a table test over (ingress dialect × upstream family) asserting which
  plane is taken.
- **Streaming O(1):** the transparent plane forwards chunks without buffering the whole body
  (assert via a chunk-boundary fixture, mirroring TD-0001's chunk-split tests).
- Coverage stays ≥75%; clippy `-D warnings`.

## Sequencing

1. Step 1 (raw handle) + Step 2 (plane selector) for the three existing dialects — this alone
   fixes byte-fidelity and the response/stream regeneration for the common case.
2. Step 3 Gemini/Cohere ingress (transparent-only).
3. Step 4 `cache_control` contract field (cross-family fidelity).
4. Step 5 estimate/billable cleanup (fold in with ADR-0004 D4).

## Risks

- **Usage on non-streaming transparent responses** must still be parsed from the returned body
  by the adapter — verify each adapter's non-stream `complete` populates `ParsedUsage` (it does
  today; the sniffer is stream-side, the non-stream parse is the `parse_*_usage` call).
- **Error passthrough:** on the transparent plane, upstream error bodies should forward verbatim
  too (a client debugging a 400 wants the provider's real error), rather than being reshaped into
  `ProviderErrorV1`. Keep reshaping to the cross-family plane.
- **Header hygiene:** forward only the upstream-relevant headers; never leak the real upstream
  credential back to the client, and strip hop-by-hop headers. (Same discipline the current
  path needs, but more visible when bytes pass through.)
