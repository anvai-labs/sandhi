# Gemini routing and cache inventory — 2026-11-11

This supplements the [initial inventory](router-cache-inventory-2026-11-11.md)
after the user explicitly added Gemini to the protocol-neutral goal. It records
local source behavior and synthetic evidence, not live Google or Vertex acceptance.

## Existing implementation

| Surface | Present behavior |
| --- | --- |
| Native ingress | POST /v1beta/models/{model}:generateContent and :streamGenerateContent; model and delivery mode are taken from the path. |
| Credentials | x-goog-api-key or Authorization Bearer virtual keys; query-string keys remain explicitly refused. |
| Discovery | Gemini-shaped model discovery, constrained by the key and declared backend models. |
| Counting | :countTokens runs admission and returns Sandhi's byte-based estimator without generation/reservation; it does not call a vendor tokenizer. |
| Native backend | Explicit Gemini endpoint family, API-key/Bearer transport; M2a tests verify Bearer wire auth independently of provider label. |
| Translation | Neutral text, inline media, tools and selected generation controls; compatible top-level extras are retained in the Gemini bucket. Both buffered and SSE response encoders exist. |
| Cache observations | The parser reads usageMetadata.cachedContentTokenCount and subtracts reads from inclusive promptTokenCount. Reasoning uses thoughtsTokenCount with separate-output semantics. |
| Cache references | Top-level cachedContent survives in the Gemini bucket on compatible routes; no cross-backend portability contract or cached-content resource lifecycle is implemented. |

Source: [ingress and counting](../../crates/sandhi-proxy/src/lib.rs),
[request/response codecs](../../crates/sandhi-proxy/src/codec.rs),
[Gemini transport codec](../../crates/sandhi-providers/src/gemini_typed.rs),
[usage parsing](../../crates/sandhi-core/src/usage.rs).

## Gaps to drive M3–M6

- The decoder emits all functionResponse parts first and then skips the rest of a
  content turn. Adjacent text/media can disappear; original order is not retained.
- A function result is projected from response.output. Arbitrary response objects
  without that field become null in the neutral projection.
- Unsupported parts, fileData and thoughtSignature metadata are not preserved by
  the current neutral projection. inlineData is treated as an image regardless of
  MIME type. These need faithful representation or explicit refusal on translation.
- Native function declarations are decoded permissively; malformed declarations
  can be skipped. Multiple allowed function names are reduced to the first named
  tool choice. These cannot be advertised as lossless conversion.
- cachedContent references and provider-native state need backend/model/account
  provenance and compatible affinity. Their presence in an extras bucket does not
  make them usable by an OpenAI or Anthropic backend.
- Cache policy must distinguish native resource references, gateway placement
  intent and reported read observations. The current Gemini output renderer has no
  separate write category: translated writes remain in inclusive input and in
  Sandhi's neutral/store/accounting evidence.
- The implemented v1beta models routes and generic Bearer transport do not establish
  Vertex project/location routes, ADC, token refresh or a managed Google credential
  lifecycle. Those require a separately admitted endpoint/auth profile.
- Live cache-resource operations, warm/cold measurements, tenant isolation and
  provider-specific minimums/expiry are unqualified.

## Acceptance extension

Gemini is now the fourth ingress dimension of TD-0034, crossed with Chat/open-weight,
Messages, standard Responses, Codex and Gemini backends. Cover buffered and SSE,
path model authorization, both header schemes, counting, discovery, interleaved
parts and tools, native state/media, cached references, usage conservation and
cancellation/restart. Test native SDK agent loops, not only handcrafted HTTP.

Existing Google SDK tests cover native generation/streaming, credentials,
discovery, error rendering and cross-family text to OpenAI. This review also adds
an SDK countTokens case and Gemini SDK → explicit Responses registry route.
The cache accounting matrix expands from 18 to 24 cases by including Gemini
ingress: inclusive input/read/reasoning wire counts and disjoint neutral categories
are checked against SQLite and both allowance scopes. Test results are recorded
in [TD-0034](../td/TD-0034-protocol-neutral-router-cache-accounting.md).

## Additional protocol distinction from current documentation

Google also documents an Interactions API, distinct from generateContent, with a
different conversation and usage surface. It is not implemented or qualified by
this generateContent work. Keep it as a separate endpoint-family admission rather
than treating it as a Gemini URL override. Current documentation also makes
explicit cached-resource availability model-dependent. Sources:
[Interactions/implicit caching](https://aistudio.google.com/docs/caching),
[generateContent caching](https://aistudio.google.com/docs/caching?generate-content=true).
