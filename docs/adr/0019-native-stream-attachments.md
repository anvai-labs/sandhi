# ADR-0019: Opt-in native stream attachments

Date: 2026-11-11

## Status

Implemented candidate for TD-0034 M3. Source and real binding validation are
recorded in TD-0034. Cache compilation, routing affinity and durable execution
accounting remain separate unfinished work.

## Problem

Canonical text/tool events cannot carry every provider's output state. Rebuilding
compatible SSE from them dropped Anthropic signatures, Responses encrypted items,
Gemini thought signatures and unknown Chat fields. Codex buffered aggregation
also discarded the terminal response needed for the next turn.

## Decision

Contract minor 12 adds a default-off `include_native_stream` request flag and a
`native_frame` event containing endpoint family and one bounded SSE event. Chat,
Messages, Responses and Gemini built-in transports support it. Other transports
reject an explicit request rather than silently dropping the requested fidelity.
Existing typed consumers retain their event set when the flag is omitted.

Canonical events remain the accounting projection. Native frames preserve field
order, opaque state, SSE event/id/retry/comments and unknown fields; line endings
normalize to LF. The proxy enables this mode only for compatible typed ingress
and backend families, validates each attachment's family/size, forwards native
frames and suppresses duplicate canonical rendering. It does not append another
Chat DONE marker. HTTP clients cannot set this internal transport choice through
native extras. Credential/profile and dispatch-expiry constraints still apply.

Complete events support multiline data, LF, CRLF and bare CR across arbitrary
byte boundaries. Native events have an 8 MiB aggregate limit, including many short
lines. Canonical metering is parsed from complete logical data before the native
frame becomes visible; transport failure retains partial evidence and does not
produce successful completion. Original protocol error frames are forwarded once,
then fail the typed stream. Recognized protocol completion can finish before HTTP
EOF; Gemini uses finish reason plus usage, or successful EOF when usage is absent.
Truncated EOF without protocol completion fails rather than fabricating Finish
or promoting partial counts to Final. Gemini also recognizes a native prompt-block
reason and keeps absent numeric usage unavailable.

Codex buffered delivery aggregates its mandatory stream while retaining only the
native terminal response, rather than the whole event history. Canonical text and
tool accumulation is independently bounded at 8 MiB. The native terminal response
preserves encrypted output items for a subsequent compatible turn. This is not a
license to transfer continuation state to another backend/account.

## Consumer decisions and limits

The proxy renders native frames or canonical content, never both. Accounting
consumes canonical usage and content estimates, never sums values from native
JSON again. Python and Node expose the opt-in frames unchanged and exercise the
real binding with the flag omitted/enabled. External consumers remain default-off
until they implement this choice; release coordination is recorded in TD-0008.

Native passthrough preserves provider wire behavior, including provider model IDs
and unsupported future output fields. It does not make such fields portable or
establish backend/account provenance. Synthetic fixtures qualify framing,
preservation and accounting paths; they do not qualify live cache hits or account
subscription behavior. Malformed/overlapping numeric usage across all families,
every dispatch's durable attribution and unknown liability remain M5 work.

## References

- [Ordered input and projection](0018-ordered-native-input-and-projection.md)
- [Goal and evidence](../td/TD-0034-protocol-neutral-router-cache-accounting.md)
- [Consumer decisions](../td/TD-0008-victor-codesign-boundary.md)
