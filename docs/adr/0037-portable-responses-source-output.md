# ADR 0037: Preserve Responses source output before translating delivery

Status: implemented locally; exact-tip review and public CI are checkpoint gates.

## Problem and decision

Qualified Responses and Codex streams previously decoded visible deltas but
could silently omit terminal-only text/tools or interim opaque reasoning state.
Buffered Codex retained only the terminal response envelope. A partial encrypted
item from an earlier event was consequently lost if the terminal omitted it.
The strict portable Chat decoder also rejected the provider's normal string
`obfuscation` padding field.

Qualified portable Responses delivery now uses complete SSE events and an
ordered bounded item projection. It materializes terminal-only text and calls,
reconciles repeated snapshots with emitted prefixes, checks identities and item
positions, and refuses meaningful native fields, unsupported item kinds or
contradictory output. Typed string padding is recognized in both Chat and
Responses portable decoders. Upstream padding remains enabled. Re-encoding a
different protocol does not claim to preserve the provider's padding properties.

```mermaid
flowchart LR
    S[One owned provider HTTP response] --> O[Source observer and bounded transport]
    O --> P[Ordered portable projection]
    O --> N[Existing native frame delivery]
    P --> C[Client protocol encoder]
    N --> C
    O --> U[Fresh / read / write / output / reasoning facts]
    U --> L[Original client and upstream reservations]
    L --> R[One paired settlement receipt]
```

The projection owns no transport, credentials, dispatch or accounting lease.
On a source-projection refusal it keeps polling the same bounded source to its
actual completion or transport error. It emits no successful finish. Later
observed source counters remain available to the original settlement owner;
neither refusal nor translation invents a cache hit, zero usage or another call.
Cancellation and the existing transport deadlines still bound this work.

## Native delivery and bounds

Native streaming retains original frames, including opaque fields and transport
metadata, through the established decoder. Qualified buffered native Codex
assembles complete `output_item.done` snapshots into the terminal response when
the source omits them. Added encrypted state is incomplete and is never promoted
to replayable state. If no complete item or terminal counterpart covers it,
buffered delivery refuses explicitly. A complete replacement may change the
partial encrypted string. Complete snapshots must agree on their contents and
positions; holes and conflicting identities refuse.

Buffered native assembly also checks that materialized text, refusals, reasoning
and arguments are covered by the complete response. Unhandled native event kinds
and rich part metadata that the assembler cannot verify require native
streaming. This is a bounded assembly contract, not support for every Responses
event type. No synthetic native stream frames are created. Legacy canonical
delivery keeps its established path; the stronger materialized-output coverage
check applies to qualified buffered delivery.

Portable retention is at most 8 MiB of accumulated payload/identities, 1,024
items and 1,024 parts. Native buffered snapshot/materialization retention has
its own 8 MiB ceiling and 1,024-entry bounds. Both preflight retained fragments
before storing them. The established native decoder now also bounds retained
tool arguments and call count, so draining it cannot grow that state forever.
Buffered canonical aggregation remains separately bounded. These are individual
state limits, not a claim that total process memory is 8 MiB.

Portable streaming preserves ordered text, refusal, materialized reasoning and
complete function calls. A later item waits for an earlier open item. Buffered
portable output additionally refuses ordering that the flat canonical response
cannot represent, such as a function call followed by visible text. An egress
encoder refusal or an early native decoder/transport error can still stop before
future source counts arrive; only actually observed facts are authoritative.

## Evidence and limits

Regression tests first failed on the published implementation for dropped
terminal text, accepted interim encrypted state and rejected normal padding.
The resulting checks cover ordinary and Codex profiles, original native-frame
equality, complete encrypted snapshots, call-ID drift, later-chunk source counts,
parallel completion order, prefix consistency, invalid lifecycles, byte splits,
multiline Unicode SSE and pre-mutation retention limits.

Fifteen current-SDK gateway cases use OpenAI Chat, Anthropic Messages and Gemini
clients against a synthetic Responses HTTP origin. They cover terminal-only
text/tools, normal padding, opaque interim state and conflicting tool identities.
Three tool-result continuations return the original call identity in a second
physical request. Delivery refusal after SSE headers aborts the stream instead
of changing HTTP status; both boundaries must avoid replay. The database checks
join actual source facts and require paired settlement at five neutral units
per completed physical call. Those units are a policy valuation, not currency
or measured price savings.

The provider pins remain OpenAI 3.49.0, Anthropic 2.8.0 and Google GenAI 3.2.0,
rechecked against PyPI on 2026-11-12. No version bump was needed. A fresh fetch
also found the working branch contains the current `develop` baseline. These
synthetic tests do not qualify a live Codex subscription, Gemini issuer, cache
TTL, media path or provider cache-price discount. The broader qualification and
spontaneous accounting-wait work remains open in TD-0034.

Independent review rejected candidate `9a7cc77`: complete per-item snapshots
could omit previously emitted parts, and buffered native materialization did not
bind delta item IDs to complete item IDs. Three independent actual-HTTP probes
confirmed successful delivery of those contradictions while retaining one
physical dispatch and final source counts. The corrected projection requires
complete part coverage; native assembly binds identities and requires their
presence in the replay envelope. Terminal clone bytes enter the retention budget,
post-terminal assembly events refuse, and meaningful unretained event metadata
refuses before storage. The rejected candidate has no clean review record.

The pre-review full SDK run was intentionally interrupted after source fixes:
248 passed and two skipped; it is partial evidence and cannot qualify the final
source snapshot. The final-source suite and exact-tip re-review remain required.

Protocol references:
[Responses streaming events](https://developers.openai.com/api/reference/resources/responses/streaming-events),
[Responses creation and padding](https://developers.openai.com/api/reference/resources/responses/methods/create),
and [Chat streaming events](https://developers.openai.com/api/reference/resources/chat/subresources/completions/streaming-events).
