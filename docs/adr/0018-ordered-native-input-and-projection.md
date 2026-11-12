# ADR-0018: Ordered native input and portable projection

Date: 2026-11-11

## Status

Accepted implementation candidate for the first TD-0034 M3 increment. Request-side
preservation and compatible buffered egress are implemented; ADR-0019 adds native
streaming output and Codex terminal-state aggregation. Cache-policy compilation
and account/backend affinity remain unfinished. Validation
is recorded in TD-0034. This does not complete M3 or the overall routing goal.

## Problem

A family body clone was retained in extensions, but re-encoding overwrote its
messages/input from a lossy portable projection. Signed thinking, encrypted
reasoning, ordered native items and message/system cache boundaries disappeared.
Forced translation for an output bound could therefore change a same-family
conversation. Gemini mixed tool-result turns also discarded adjacent text and
projected arbitrary function-result objects as null; Responses media was skipped.

## Decision

Contract minor 11 adds optional NativeInputV1 to ChatRequestV1. Original input stays
in the existing family extension bucket. The metadata names its endpoint family,
a SHA-256 consistency digest and bounded nonportable feature classes. The digest
covers native input fields, portable messages/tools/tool choice and feature
classification. Serialization streams into the digest without allocating another
serialized prompt buffer. Resolved model, output bounds and authenticated
attribution are excluded so admission can apply them without changing history.

The digest is a consistency check, not a signature, authorization grant or proof of
backend/account provenance. HTTP codecs construct the field after projection; they
never adopt a client-supplied field or a passthrough bucket as decoder ownership.
Direct typed hosts may capture native input explicitly and are responsible for
correct projection/feature classification. Native buckets without this metadata
retain the previous encode-authoritative typed-field behavior.

Compatible encoders validate the snapshot and reuse its ordered native input
instead of reconstructing messages, tools and tool choice. Model and generation
controls still come from the admitted typed request. A changed portable projection
or native input invalidates the snapshot and requires decoding again.

On an incompatible endpoint family, portable input can translate normally.
Classified opaque state, resource references, unsupported content and native
controls cause an explicit refusal. Proxy preparation performs this check before
rate, policy, budget reservation or dispatch; provider encoders also check direct
typed calls. This protects the known classified cases, not every possible future
vendor extension. Codex retains its profile constraints and refuses server-side
continuation references rather than treating them as materialized history.
Backend/account pinning within the same protocol remains M4 work.

Compatible buffered egress uses the decoder-owned native response body when its
family response structure is present. This preserves signatures, encrypted state,
media and unknown output items. Aggregation metadata is not a native response body;
ADR-0019 retains Codex terminal responses and compatible native streaming frames.

Gemini projection now splits around function responses in source order. It unwraps
only Sandhi's own single-output wrapper; arbitrary result objects and additional
fields stay intact as JSON tool-result text. Responses projection retains ordered
text, image URLs/details and inline files; provider file references are classified
as nonportable. Native unknown Responses items and built-in tools survive compatible
re-encoding and are refused on incompatible routes.

Plaintext completions and embeddings deliberately clear this metadata when deriving
an admission-only Chat projection. Their dispatch still uses original endpoint
bytes and the documented completion default; they are not native Chat requests.

## Compatibility and limits

Existing typed callers may omit the new field. Native input and undeclared
generation controls that previously disappeared across families can now produce
explicit errors instead of a changed request. The first classification is bounded
and conservative; completing M3 requires expanding content/state coverage.
[ADR-0020](0020-cache-intent-compilation.md) adds cache compilation at minor 13.

The new field does not transfer opaque state between endpoints, create a shared
cache namespace, guarantee cache hits or implement a subscription credential
lifecycle. Existing request/stream bounds, policy inspection of original bytes,
credential expiry and Codex account/origin fences remain in force.

## Validation

Core tests cover snapshot round-trip, mutation refusal, compatible ordered restore,
model/output-bound authority and portable cross-family use. Real gateway tests
cover all four ingress families in buffered/streamed request execution, forced
same-family re-encoding, native input/cache boundaries, compatible buffered output
state, pre-reservation state refusal, Gemini mixed results, Responses media and
forged ownership through client extras. The existing cache-accounting and
subscription tests remain required. Schemas and Python/TypeScript facades are
generated from the updated contract; binding builds/tests are separate gates.

## References

- [Goal, constraints and evidence](../td/TD-0034-protocol-neutral-router-cache-accounting.md)
- [Backend lifecycle](0017-backend-registry-lifecycle.md)
- [Gemini inventory](../reviews/gemini-router-cache-inventory-2026-11-11.md)
