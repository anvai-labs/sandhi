# ADR-0040: Share buffered source qualification across admitted typed calls

- Status: Implemented source candidate; final qualification and review in progress.
- Tracks: [TD-0034](../td/TD-0034-protocol-neutral-router-cache-accounting.md),
  [coordinated migration](0038-coordinated-victor-contract-migration.md) and
  [explicit profiles](0039-shared-embedded-endpoint-profiles.md).

## Decision

Ordinary and accounted typed buffered calls use the same strict source qualifier
for Chat, Messages, Responses and Gemini. Remove the `qualify` switch from those
codecs. A successful projection contains the qualified usage, including optional
reported zero; it cannot use clamped legacy extraction as final measurement.
Disjoint fresh/read/write/output counts and reasoning inclusion retain their
existing source conventions. Detailed Chat audio/prediction counters remain
optional validated subsets and are not added to inclusive parents again.

The shared buffered constructor keeps separately qualified usage for an accounted
owner. If qualification fails, its response usage is unavailable, with zero
placeholders and independent availability/latency metadata. That is no measured
zero and cannot authorize settlement. Ordinary callers receive a terminal
`output_projection_error`. A received response with malformed required output is
also a deterministic refusal; pure decoder failures cannot advertise retryable
transport errors. Network failures before decoding retain their actual category.
The gateway's existing source observer and accounting owner retain their evidence
independently of output delivery.
Keep decoded generation outcome independently of physical HTTP success: qualifying
numeric counters must not rewrite an incomplete or failed Responses result as
successful. Unavailable projections retain the same decoded outcome metadata.

Constrained Codex buffered calls use the existing qualified SSE operation and its
original total/setup/idle bounds. Remove the alternate ordinary aggregation path
and its qualification flag. Bounded native aggregation verifies item coverage and
identity before assembling a complete envelope. Reuse the buffered Responses
decoder to expose terminal-only native text/tools in the canonical view while
preserving the original native envelope and observed reasoning. Portable delivery
keeps its strict ordered projector; sparse terminal envelopes must not erase
previously emitted content. The first aggregation refusal survives a later drain
timeout while the original source keeps its actual transport outcome.

One response-owned qualified stream represents one physical response with retries
disabled. Existing numeric usage carries attempts 1. Missing numeric usage stays
absent; Codex's known physical cardinality does not depend on aggregate Usage
events or manufacture a final zero.

Known executable tool fields retain required identity, name and argument types in
native and portable views. Missing or malformed fields cannot become empty calls,
empty argument strings or silently dropped functions. Duplicate call identities
and excess retained calls refuse explicitly. Other native tool kinds remain in
their original envelope. Gemini retains its optional ID and argument semantics:
missing IDs use the existing response-scoped generator, and unset arguments map
to an empty object. See the [FunctionCall contract](https://aistudio.google.com/api-reference/caching)
and [ProtoJSON null semantics](https://protobuf.dev/programming-guides/json/#null-values).

## Boundaries

This does not change credential authority, Codex endpoint/account/expiry fences,
cache namespace ownership or request cache compilation. No inference is replayed
to repair usage or output. Wire schema v1 remains unchanged; the paired package
targets stay Sandhi 0.17.0 and Victor 0.29.0.

The gateway second-hop counts/cache contract and accounting-default migration
remain separate work. Broader agent/media, cache-lifecycle and issuer/subscription
qualification remain open. The retained accounting waits, native Python exit and
SDK shutdown failures remain reliability/merge gates; later green tests alone do
not identify their causes.
