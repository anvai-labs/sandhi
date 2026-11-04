# ADR-0013: Guarded native-extensions passthrough and translation-plane prompt caching

Date: 2026-11-03

## Status

**Accepted for implementation.** Implemented on the translation plane with unit and
cross-crate seam tests; merge, deployment and release gates remain distinct.

Relates to [ADR-0004](0004-two-plane-proxy-and-enforcement-boundary.md) (translation
plane), [ADR-0001](0001-sandhi-architecture-and-wire-contract.md) (neutral contract),
and [ADR-0008](0008-inferflux-admission-and-session-affinity.md) (catalog-owned facts,
client-owned policy).

## Verified problem and non-goals

A third-party client speaking the OpenAI-compat dialect
(`POST /v1/chat/completions`) to an Anthropic upstream crosses the translation
plane: `decode_openai_request` builds a `ChatRequestV1`, and
`encode_anthropic_request` re-encodes it. The OpenAI body has no
`cache_control` field, and the decoder stored the raw body only in
`extensions["openai"]` — a bucket the Anthropic encoder never reads. Every
such request therefore reached the wire with zero prompt-cache markers: full
price on every read, silently. Typed clients (Victor) were unaffected because
they supply `extensions["anthropic"]` themselves.

The change must not make Sandhi a policy layer for prompt shape, weaken the
decoded contract's validation, or let a passthrough client override anything
the proxy has already validated (model allowlist, attribution, sampling
fields, message content).

## D1. Reserved `sandhi_extensions` passthrough field (opt-in, guarded)

`decode_openai_request` admits a reserved top-level body field:

```json
{
  "model": "claude-...",
  "messages": [...],
  "sandhi_extensions": { "anthropic": { "top_k": 5 } }
}
```

Guard rails, enforced at decode time with dialect-shaped errors:

- The field must be a JSON object mapping family slugs to JSON objects
  (slug charset: lowercase letters, digits, `-`, `_`).
- The decode-owned `openai` bucket is rejected — the ingress body clone stays
  authoritative.
- The field is removed from the body before the clone, so it never rides in
  `extensions["openai"]`.
- Buckets merge into `ChatRequestV1.extensions` after the openai bucket.

Safety of the merge rests on an invariant every family encoder already
follows: the encoder clones its family bucket as the base body and then
**inserts the typed fields** (model, messages, tools, sampling params,
thinking) **after the clone**. A passthrough bucket can therefore only add
native extras a family accepts beyond the contract — it can never override
the decoded, validated request. Unknown family slugs are inert by
construction: no encoder reads a bucket it does not own.

This is the general mechanism by which ANY third-party OpenAI-compat client
exercises family-native request optimizations (Anthropic `cache_control`
placement, provider `top_k`/`metadata`, Gemini `safetySettings`), not just
the native/typed clients.

## D2. Auto `cache_control` on the Anthropic translation path (default-on, suppressible)

When `encode_anthropic_request` sees **no** client-supplied
`extensions["anthropic"]` bucket, it marks the cache boundary itself:
`{"type": "ephemeral"}` on the **last system block** and the **last tool** —
the stable-prefix boundary (tools → system) Anthropic documents, leaving the
conversation tail uncached. This mirrors the placement policy typed clients
(Victor, FEP-0011) apply through the bucket today.

- Default-on: a plain third-party request gains prompt caching with no client
  change. Markers below Anthropic's minimum cacheable prefix are inert
  upstream.
- Client bucket present — including the empty object — **suppresses** auto
  placement: marker placement stays client-owned
  (`"sandhi_extensions": {"anthropic": {}}` is the documented opt-out).
- Same-family (transparent-plane) routes are untouched: byte-exact forwarding
  already preserves client-supplied native markers.
- Non-Anthropic upstreams are untouched: no other family has explicit
  client-visible cache markers on this path.

## Consumer decisions

| Change | Consumer | Decision |
|---|---|---|
| Reserved `sandhi_extensions` ingress field | OpenAI-compat clients | Opt-in; absent field = byte-identical behavior |
| Auto cache markers, no bucket | Third-party translation-plane callers | Default-on caching; empty bucket opts out |
| Encoder reads bucket, then typed fields win | Passthrough clients | Buckets add native extras only; contract fields are encode-authoritative |

## Testing

- `codec.rs`: passthrough admission, shape/ownership validation, no leak into
  the openai body clone, and the cross-crate seam test
  `openai_to_anthropic_translation_keeps_prompt_caching_alive` (ingress decode
  → `encode_anthropic_request`, via the `typed` re-export) pinning both the
  auto path and client-owned placement.
- `anthropic_typed.rs`: marker placement (last system block, last tool only),
  suppression by client bucket, no-op without cacheables, and the existing
  index/name merge tests for client-supplied markers.
