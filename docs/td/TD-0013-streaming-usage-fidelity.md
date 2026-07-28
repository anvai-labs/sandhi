# TD-0013: Streaming usage fidelity — settle what the provider reported, not what we guessed

- **Status:** Draft (proposed) — 2026-07-28
- **Relates to:** [ADR-0005](../adr/0005-enforcement-correctness-reservation-ledger-observe-enforce-split.md)
  D1 (whose settle-on-interruption mechanism this refines — no new ADR needed),
  [ADR-0004](../adr/0004-two-plane-proxy-and-enforcement-boundary.md) (the two planes this must fix
  symmetrically), [TD-0009](TD-0009-usage-visibility-surfaces.md) D4 (which already ruled "emit
  mid-stream `Usage` only where the provider actually reports it" and proposed the `basis`
  discriminator — unimplemented until now), [TD-0007](TD-0007-enforcement-ledger-backends.md) C4
  (the invariant D6 protects), [TD-0010](TD-0010-ingress-dialect-parity.md) (the client-visible wire
  shape D7 must not disturb), [TD-0011](TD-0011-first-party-observability.md) (bounded label set)
- **Companion facts:** `grep -rn "usage_running\|UsageCadence\|UsageBasis" crates/*/src` returns
  **nothing** — none of the machinery below exists yet. And
  `grep -n "ceiling" crates/sandhi-proxy/src/lib.rs` returns only pre-flight sites: the mid-stream
  cutoff that `crates/sandhi-core/src/ledger.rs:91-92` claims "enforces that bound" was specified in
  ADR-0005 D1 and never built.

## Why this exists

Sandhi's product claim is that every model call is **counted and attributed**. On the streaming path
we break that claim in a way that is specific, avoidable, and measurable.

When a stream ends without a terminal usage frame — a client disconnect, a dropped connection, an
aborted long generation — the proxy settles the budget lease against `partial_usage(delta_bytes)`, a
`bytes / 4` estimate (`crates/sandhi-proxy/src/lib.rs:1493-1521`). The estimate is not the mistake.
ADR-0005 D1 introduced it to close a real evasion hole: before it, an interrupted stream released
its lease to zero, so a caller could stream-and-abort repeatedly for free.

The mistake is that **the estimate is applied even when an exact, provider-reported number is
already sitting in memory** — and that it only ever estimates *output*.

### The number is computed, held, and then thrown away

`metered_passthrough` (`crates/sandhi-providers/src/lib.rs:314-370`) is the O(1) byte-passthrough
primitive both planes meter through. It keeps a running `ParsedUsage` and feeds every complete
newline-delimited line to the per-family sniffer. Anthropic's sniffer
(`crates/sandhi-providers/src/anthropic.rs:135-160`) fills that accumulator **incrementally, per
category**:

| Anthropic SSE event | fills |
|---|---|
| `message_start` (arrives **before any content**) | `tokens_in`, `cache_creation_tokens`, `cache_read_tokens` |
| `message_delta` | `tokens_out` (cumulative) |

Gemini attaches `usageMetadata` to chunks and is read last-wins (`gemini.rs:118-124`). So by
mid-stream, for those two families, the accumulator holds real per-category counts. And then:

```rust
yield StreamChunk { data: chunk, usage: None, attempts: 1 };            // :355 — every data chunk
yield StreamChunk { data: Bytes::new(), usage: Some(usage), ... };      // :367 — terminal only
```

The accumulator is surfaced **only in a terminal chunk that a disconnect never reaches**. Both
planes inherit this from the same primitive: the transparent plane reads `chunk.usage` directly
(`proxy/src/lib.rs:1141`), and the typed decoders derive their single `ChatStreamEventV1::Usage`
from the same terminal chunk (`anthropic_typed.rs:386-395`). One defect, one fix, two planes.

### The magnitude — an evasion vector, not a rounding error

`partial_usage` sets **only `tokens_out`**. `tokens_in`, `cache_creation_tokens` and
`cache_read_tokens` stay zero, and `billable()` (ADR-0005 D4) counts all four.

On the fixture this repo already ships
(`crates/sandhi-providers/tests/fixtures/anthropic/stream_cache_split.sse`), `message_start`
announces 1024 input + 2048 cache-creation + 4096 cache-read: **7168 real billable tokens, known
before a single content byte streams.** A client that disconnects right after `message_start`
settles `bytes/4` of the text that arrived — typically single digits.

ADR-0005 D1's hole was closed for output and left open for input and cache. On prompt-cache-heavy
workloads — the workload Sandhi exists to serve — the cache split *is* the bill.

## First principles

1. **An estimate is only defensible where no measurement exists.** Substituting a guess for a number
   the provider already handed us is not conservatism; it is discarding evidence. The fallback must
   be the exception the code reaches for last, not the default it applies uniformly.
2. **What a provider reports, and when, is a transport fact.** It belongs in a declaration next to
   `base_url` and `request_id_header`, not in the emergent behaviour of a `match` arm. This is the
   #92 rule — vendor differences are data, never branches in shared code.
3. **A measurement and a guess must not be indistinguishable on the wire.** If a consumer cannot
   tell them apart, every downstream number inherits the ambiguity and nobody can audit it.
4. **Where nothing is reported, the fallback is a policy question, not an accuracy one.** For
   terminal-only families there is no number at disconnect. Settling zero would be the honest
   *measurement* and the wrong *policy* — it reopens the stream-and-abort hole. Say that out loud
   rather than letting an estimate imply precision it does not have.
5. **Fidelity work must not change what the client sees.** Ingress wire shape is a TD-0010 parity
   guarantee; a metering improvement that alters a client's byte stream has broken something more
   important than it fixed.

## Non-goals

- **Not implementing ADR-0005 D1's mid-stream cutoff** (aborting the upstream when cumulative usage
  crosses the reservation ceiling). D6 clamps the *accounting* so the invariant holds; the cutoff is
  a separate change with its own failure modes (half-delivered responses, dialect-shaped aborts).
  Stating this is the point — `crates/sandhi-core/src/ledger.rs:91-92` currently describes that
  cutoff as though it exists.
- **No synthetic usage for terminal-only families.** TD-0009 D4's rule stands: no invented deltas,
  no interpolation between a start and an end we never saw.
- **No re-tokenization.** Sandhi does not ship a tokenizer and will not guess better by counting
  BPE locally; that would be a second meter disagreeing with the provider's.
- No new dependencies, and no dollars (ADR-0001).

## Decisions

**D1 — Usage-reporting cadence is a declared per-family transport fact.** Add
`ProviderFamily::facts() -> &'static FamilyFacts` carrying
`usage_cadence: UsageCadence { Incremental, TerminalOnly }`:

| Family | Cadence | Evidence |
|---|---|---|
| Anthropic | `Incremental` | `message_start` → input + cache split; `message_delta` → cumulative output |
| Gemini | `Incremental` | `usageMetadata` on chunks |
| OpenAI Chat | `TerminalOnly` | usage only in the final chunk, and only with injected `stream_options.include_usage` |
| OpenAI Responses | `TerminalOnly` | `response.completed` |
| Cohere | `TerminalOnly` | `message-end` |
| Ollama | `TerminalOnly` | final NDJSON line (`prompt_eval_count` / `eval_count`) |

Today no per-**family** facts table exists — `ProviderFamily` is a bare six-variant enum and
per-family behaviour is spread across seven `match` sites. This TD adds the seam and exactly one
field. It deliberately does **not** migrate the existing sites (`raw.rs` auth/sniffer/parser/
envelope, `proxy/src/lib.rs` `upstream_path`, `operator.rs` `default_base_url`, `typed.rs`
transport construction); a fact table that grows by accretion is reviewable, one that lands as a
seven-site refactor is not.

**D2 — The declaration must be falsifiable.** A test drives each family's shipped SSE fixture
through `metered_passthrough` and asserts that a family declared `Incremental` genuinely exposes
non-zero usage *before* the terminal chunk, and that a `TerminalOnly` family does not. A transport
fact nobody can refute is a comment, and comments drift. This is the same discipline
`conformance.rs` applies to the ledger contract.

**D3 — Surface the running accumulator; never fabricate one.** `StreamChunk` gains
`usage_running: Option<ParsedUsage>`, set to `Some` only once a sniff has actually mutated the
accumulator — detected by comparing before/after (`ParsedUsage` is `Copy + PartialEq`), so no
sniffer signature changes and no family has to opt in. `ParsedUsage` is five `u64`s; the per-chunk
cost is a 40-byte compare and copy with no allocation, preserving the O(1) passthrough property
ADR-0004 D1 depends on.

**D4 — The fallback is per-category, not per-call.** This is the heart of it. On an interrupted
stream:

- `tokens_in`, `cache_creation_tokens`, `cache_read_tokens` — taken from the reported accumulator,
  or **zero**. They are never estimated. No byte count observed on the response can stand in for
  the tokenization of a prompt, and inventing one would be a second meter.
- `tokens_out` — `max(reported_out, byte_estimate)`. Anthropic's `message_delta` lags the text it
  describes, so between `message_start` and the first delta the reported output is legitimately `0`
  while real output has flowed. Taking the max never settles below today's behaviour and keeps the
  conservative direction ADR-0005 D1 chose.

An all-or-nothing switch ("real numbers if we have them, else the estimate") would throw away the
7168 cache tokens in the example above whenever output happened to be unreported. Per-category is
what actually recovers the dominant term.

**D5 — `basis` distinguishes a measurement from a guess.** Add `UsageBasis { ProviderReported,
Estimated }` as `UsageV2.basis` (serde default `provider_reported`). A call is `Estimated` when
*any* category came from the byte fallback.

This is additive and **orthogonal to `completeness`** — deliberately not a fourth
`UsageCompleteness` variant. `completeness` answers *how complete is this*; `basis` answers *where
did the number come from*. Today `Partial` is silently overloaded across both axes:
`providers/src/metering.rs:137-142` uses it for "real provider counts, stream then errored" while
`proxy/src/lib.rs:1518` uses it for "byte guess". Redefining `Partial` would retroactively change
the meaning of rows already persisted in operators' stores; adding a field does not. This implements
the discriminator TD-0009 D4 proposed and never got.

**D6 — Settlement is clamped to the reserved ceiling.** `crates/sandhi-core/src/ledger.rs:91-92`
states `actual` "is trusted to be ≤ the reserved ceiling (the proxy's mid-stream cutoff enforces
that bound)". There is no cutoff. Settling real input + cache makes an overshoot materially more
likely than settling `bytes/4` of output ever did, and an unclamped settle can push
`spent + reserved` past the limit — the exact invariant TD-0007 C4 asserts and
`conformance.rs` now tests. So: clamp at settle, count the clamp
(`sandhi_settle_clamped_total`), and **correct the comment**. A doc line describing code nobody
wrote is worse than a known gap, because it stops the next reader from looking.

**D7 — Partial usage is accounting-only and never re-encoded.** Mid-stream usage observed for
settlement must not become an extra client-visible SSE frame. The proxy observes it, does not let it
supersede a terminal `Final`, and does not pass it to `encode_stream_event`. The acceptance evidence
is the real-SDK conformance suite passing **unchanged** — 22 tests that assert byte-level client
behaviour across all three dialects.

## Phases

| Phase | Scope | Acceptance |
|---|---|---|
| **P1** | D3 + D4 on both planes; the zeroed-terminal fix | A paced Anthropic stream disconnected after `message_start` settles the real 1024 + 2048 + 4096, not a byte guess — and reverting the change makes that test fail; a stream whose usage is never sniffed settles the accrued partial instead of `0`; the SDK-conformance suite is unchanged and green |
| **P2** | D1 + D2 — cadence as a declared fact | Each family's declared cadence is asserted against its shipped fixture through the production `metered_passthrough` path; a family whose declaration disagrees with its sniffer fails the build |
| **P3** | D5 — `basis` on the wire, plus the operator signal | `usage.basis` round-trips through all five exported schemas and both binding facades with `codegen-drift` green; `sandhi_estimated_tokens_total` carries the TD-0011 bounded label set, with the forbidden-label loop extended to cover it |
| **P4** | D6 clamp + the two adjacent streaming defects | An `actual` above the lease ceiling settles clamped and increments the counter, with the C4 invariant asserted after; the false cutoff claim in `ledger.rs` is corrected; a client's sibling `stream_options` fields survive the typed plane as they already do on the raw plane; a `"usage": null` Responses event cannot zero the accumulator |

P1 is the accuracy fix and is independently shippable. P2 turns it from behaviour into a declared
contract. P3 is the only phase that touches the wire contract, so it lands on its own. P4 is
correctness debt this work surfaced and would otherwise leave slightly worse than it found.

## Pressure test

1. **"This is a rounding-error fix dressed up as an integrity problem."** It is not a rounding
   error. `partial_usage` reports zero for three of the four categories `billable()` counts, and on
   the shipped Anthropic fixture that is 7168 tokens recorded as roughly five. A caller who
   disconnects after `message_start` gets a full cached prompt read for approximately free, on
   repeat. The size of the gap is the argument.
2. **"You are trading O(1) passthrough for accuracy."** No. D3 adds a `Copy` compare of five `u64`s
   per chunk and no allocation; the sniffing, line-buffering and 64 KiB budget are unchanged. The
   accumulator was already being maintained on every line — the only thing that changes is whether
   anyone is allowed to read it before the end.
3. **"`max(reported, estimate)` mixes a measurement with a guess."** It does, and that is why D5
   exists: any call touching the fallback is labelled `Estimated`, so the mixture is visible rather
   than laundered into an authoritative-looking number. The alternative — taking `reported` alone —
   would settle `0` output for the window before Anthropic's first `message_delta`, which is
   strictly worse than today.
4. **"Why not just settle zero for terminal-only families and be honest?"** Because honesty about
   the measurement would be dishonesty about the incentive. Zero re-opens exactly the hole
   ADR-0005 D1 closed. First principle 4 names this as a policy call and D5 makes the policy
   auditable; that is the most that can be true at once.
5. **"A new wire field breaks consumers."** `basis` is additive with a serde default, which is the
   TD-0002 additive policy, and it is the *conservative* option: the alternative considered was
   redefining `UsageCompleteness::Partial`, which would have changed the meaning of data already
   written to operators' stores without changing a single byte of schema.
6. **"Clamping under-charges when the real usage exceeds the ceiling."** Yes — deliberately. The
   ceiling is what the ledger admitted; charging beyond it would let a settle breach the cap the
   admission decision was made against, and C4 says the invariant holds at every step, not on
   average. `sandhi_settle_clamped_total` makes the trade visible so a deployment can widen its
   ceilings if it is clamping often.
7. **"The disconnect test already exists."** It does, and it cannot fail for the right reason:
   `tests/proxy.rs:613-659` serves the whole SSE body in one write with a `content-length`, so the
   first frame usually already carries the terminal usage and the estimated-partial branch is never
   reached. It asserts only that one event was emitted, never the settled quantity. P1's acceptance
   requires a *paced* upstream, because a test that cannot fail proves nothing.

## Open questions

- Should `Incremental` families emit usage progress to the *client* as well (a mid-stream
  `message_delta`-shaped frame on Anthropic ingress)? D7 says no for now — parity beats visibility
  until someone asks — but TD-0009's live-usage surface may want it, and that is the natural place
  to decide.
- Is `max(reported_out, byte_estimate)` the right rule, or should the byte estimate apply only until
  the *first* reported output and then be dropped entirely? The latter is cleaner in principle and
  under-counts when a provider's cumulative output lags badly. Leaning `max` until we have evidence
  of a family whose reporting is far enough behind to matter.
- Should the cache split be reserved as well as settled? A reservation ceiling built from
  `input_estimate` (bytes/4 of the request) roughly covers a cached prompt today, but only because
  the client re-sends the full prompt each turn. If a future provider supports server-side prompt
  handles, the ceiling would badly under-reserve and D6's clamp would start firing constantly.
- Does `ParsedUsage` need audio and prediction categories? They exist on `UsageV2` but are populated
  only by the typed OpenAI decoder, so the transparent plane reports `None` for them on every
  family. Out of scope here, but it is the same shape of gap.
