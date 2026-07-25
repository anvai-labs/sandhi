# TD-0008: Victor–Sandhi co-design boundary — cohesion/coupling review and hardening plan

- **Status:** Active (2026-07-25)
- **Relates to:** ADR-0001 (wire contract), ADR-0002 (transport scope), TD-0002 (typed runtime),
  TD-0004 (catalog governance), victor FEP-0020/ADR-018
- **Companion changes:** sandhi#65 (upstream error body), victor#652 (wire handshake +
  body surfacing), victor#647 (reasoning-delta consumption + provider-4xx diagnostics)

## Why this review exists

A production incident (victor session `modality-doc-review-fixes-b4e87728`, 2026-07-24) exposed
three seam defects in one turn: a Moonshot 400 surfaced as an opaque `upstream status 400`
(Sandhi drops upstream bodies), the triggering reasoning-token exhaustion was invisible
(Sandhi forwarded `reasoning_delta`s that Victor silently ignored), and diagnosis required
cross-repo log forensics. None of these was a bug in either repo alone — each was a
**co-design gap**: contract capability on one side, no consumption or no data on the other.

This TD records the deliberate cohesion/coupling balance of the boundary, scores today's
state, and tracks the hardening plan.

## The boundary, stated once

**Sandhi owns** (high cohesion around the wire): provider transport (HTTP/SSE, retries,
circuit breaker, timeouts), the neutral versioned chat contract (`ChatRequestV1` /
`ChatResponseV1` / `ChatStreamEventV1` / `ProviderErrorV1` / `UsageV2`), usage measurement at
the point of the call, transport facts in the catalog (slug, aliases, endpoint routing —
no models, no pricing), and credential *use* (never storage).

**Victor owns** (high cohesion around the agent): prompt/message construction, tool policy,
model policy and user-facing discovery, credential resolution (keyring/OAuth), retry
*ownership* semantics (never replay a billed call), pricing, and everything above the
neutral contract.

**Deliberately loose couplings** (these are features, not gaps):
- Version pin `sandhi-gateway==0.1.2` exact — a transport is a correctness dependency;
  drift is opt-in, never accidental.
- Feature detection (`hasattr`) for new binding surfaces — old bindings degrade to Victor's
  fallbacks (catalog → SDK/static lists) instead of crashing.
- JSON-schema'd wire types with schema-sha256-pinned binding facades — the contract is data,
  not shared code; Node/Python/future consumers stay independent.

## Scorecard (2026-07-25)

| Seam | State | Verdict |
|---|---|---|
| Catalog data-vs-policy split (TD-0004) | Sandhi ships transport facts, Victor shapes policy, fallback tiers work | **Right** |
| Metering (one event per logical call, Drop-guarded; measure/price split) | Shipped, decorator-composed | **Right** |
| Neutral event vocabulary (reasoning/refusal/usage/finish) | Schema'd, generated facades, drift-pinned tests | **Right** |
| Upstream error diagnostics | Body was structurally dropped → opaque 4xx | **Fixed** — sandhi#65 + victor#652 |
| Contract-version handshake | `wire_contract_version()` existed, Victor never called it | **Fixed** — victor#652 |
| Event consumption completeness | `reasoning_delta` ignored until victor#647; `refusal_delta` still unconsumed | **Gap** (P1) |
| Error transit shape | `ProviderErrorV1` as JSON-in-a-string inside `PyRuntimeError`, re-parsed by regexp on the consumer | **Gap** (P2) |
| Streaming FFI hot path | Per-event Rust serialize + Python parse (typed layer; transport itself is O(1) pass-through) | **Measure first** (P3) |
| Node binding parity | Missing `provider_descriptor_json` / `provider_spec` / `chat_contract_schema_json` | **Gap** (P4) |
| Anthropic/Google full typed-handle migration | Verified complete: SDK wire deleted, residual SDK use is discovery/credentials (Victor-owned by design) | **Closed** (P5) |
| Cross-repo conformance | One-directional (schema pinning + generated facades); no test that a consumer *consumes* every event kind | **Gap** (P1) |

## Hardening plan

**P1 — Contract-consumption conformance (the incident class).** A contract is balanced only
when every event kind a producer emits has a deliberate consumer decision: consume, surface,
or *explicitly* ignore. Add to Victor a seam test that instantiates the typed transport
against a scripted event stream containing **every** `ChatStreamEventV1` variant and asserts
each one lands somewhere observable (content, metadata, status, log) — so a future variant
addition fails a test instead of silently vanishing (reasoning did this; refusal still does).
Victor: route `metadata["refusal"]` to a surfaced status. Sandhi: document that new event
variants REQUIRE a consumer-decision entry here before release.

**P2 — Typed errors across the FFI.** Replace parse-`str(exc)`-as-JSON with a structured
error: either a dedicated Python exception type carrying the `ProviderErrorV1` dict, or an
`error` terminal event on the stream (already in the schema) with the iterator raising only
on binding failures. Removes the brittle string round-trip without changing retry ownership.

**P3 — Hot-path batching. CLOSED (2026-07-25): not the dominant cost term.** Measured
(Python 3.12, M-series): consumer-side `json.loads` on representative `ChatStreamEventV1`
payloads (text/reasoning/tool-args/usage) averages **1.11 µs/event**; bounding the full
serialize+parse round trip conservatively at 2× gives **2.23 µs/event**. At a realistic
chat stream rate of 50–200 events/s that is **0.01–0.04 %** of wall clock; even at an
implausible 1,000 events/s it is 0.22 %. Model/network latency dominates by 4–5 orders of
magnitude, and the transport layer under the typed binding is already O(1) byte
pass-through. **Decision: no batching or typed-object redesign; re-open only if a
non-chat modality pushes event rates ≥100× current.**

**P4 — Node parity.** Export `provider_descriptor_json`, `provider_spec`,
`chat_contract_schema_json` from the Node binding; parity is what keeps the contract
consumer-count honest.

**P5 — Finish the typed migration (victor gap #2). CLOSED (2026-07-25): verified
already complete.** Code audit: both `AnthropicProvider.chat()/stream()` and
`GoogleProvider.chat()/stream()` raise `NotImplementedError` ("owned by the Sandhi typed
variant") — the SDK wire paths are deleted, and `resolve_transport_class` maps both to
their Sandhi typed variants with fail-closed semantics. The residual SDK usage is exactly
what the boundary statement assigns to Victor: `AsyncAnthropic` for **model discovery
only** (victor#632, catalog policy tier) and `google-genai` for **credential acquisition**
(victor#631, OAuth/ADC resolved Victor-side and passed as a bearer to the typed Gemini
handle). "Sandhi owns transport" is unconditional for both families.

## Operating rules going forward

1. **New event variants / contract fields ship with a consumer-decision row in this TD** —
   producer-side capability without consumer-side intent is how silent gaps form.
2. **Diagnostics are part of the contract.** Anything the boundary can reject (4xx, contract
   validation, circuit state) must carry enough context for the consumer to explain itself
   without cross-repo forensics.
3. **Loose where versions move, tight where correctness lives.** Feature-detect optional
   surfaces; hard-pin the transport; never silently fall back across the FFI on execution
   paths (a replay can double-bill).
4. **Speed claims need traces** (ADR-052 discipline, imported): no boundary redesign for
   performance without a filed measurement.
