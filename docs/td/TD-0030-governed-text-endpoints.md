# TD-0030 — Governed plaintext embeddings and completions

Status: Implemented on the feature branch; deployment acceptance pending.

## Decision

Add two OpenAI-compatible ingress routes to the existing enforcement pipeline.
Keep transport raw and restrict upstream family so endpoint-native bodies and responses
cannot accidentally pass through a chat codec. Use a canonical request solely for
shared admission/accounting, with trusted route metadata and explicit policy input format.

Reject token-ID arrays and unknown fields in this MVP. A tokenizer-aware contract is
needed to inspect encoded content; accepting opaque arrays under text policy would
create an inspection bypass. Preserve the existing chat policy API as a wrapper over
its explicit Chat format. No wire schema or language-binding contract changes.

Bound completion alternatives/batches and reserve total generated output. Insert an
explicit default max_tokens=16 when missing; reserve no embedding output tokens.
Count repeated suffix context for each prompt and alternatives before estimating input.
Actual usage is authoritative at settlement, while tokenization remains estimated.

## Alternatives rejected

- Separate proxy handlers with duplicated auth/ledger logic: would drift from existing
  authorization, lifecycle, receipt and accounting guarantees.
- Forward-only URL aliases: would route without correct inspection and reservations.
- Translate embeddings/completions into upstream chat: changes semantics and response shape.
- Allow token IDs whenever inspection is configured: text classifiers cannot establish
  what those IDs decode to under arbitrary upstream tokenizers.

## Evidence and limitations

Integration tests first failed with 404, then pass for correct upstream paths and bytes,
usage/identity attribution, authorization denials, content block/quarantine across batches
and options, audit exhaustion, budget sizing, malformed controls, rate limiting and SSE.
Broader workspace and live InferFlux acceptance are required before production cutover.
Raw transport retains its existing timeout, cancellation, response-bound and usage limits.
These additions do not certify arbitrary provider controls or implement continuous vault
revocation, other policy dialects, exact tokenization, or distributed budgets.
