# TD-0021: Co-design seam v2 — the promises the proxy path makes and does not keep

- **Status:** Draft (proposed), 2026-08-31. Owns gaps **G20, G21, G22, G30**.
- **Relates to:** [TD-0008](TD-0008-victor-codesign-boundary.md) (the seam this extends from the
  in-process bindings to the HTTP path), [ADR-0005](../adr/0005-enforcement-correctness-reservation-ledger-observe-enforce-split.md) D7
  (the neutral identity fields, one of which is inert), [ADR-0001](../adr/0001-sandhi-architecture-and-wire-contract.md)
  (the wire contract), [TD-0010](TD-0010-ingress-dialect-parity.md) D2 (dialect-shaped errors, the
  discipline G30 completes).

## Why this exists

TD-0008 did careful work on the **in-process** seam: a version handshake, feature detection, typed
errors, schema-pinned binding facades, an event-consumption conformance suite. That work assumed the
consumer links Sandhi as a library.

The **proxy path has none of it.** A consumer pointing an SDK at Sandhi over HTTP — which the README
calls "the only shape that serves cross-process / cross-host / polyglot / shared-key setups" — gets
no version handshake, no capability discovery, and no way to detect a contract change except by
observing broken behaviour. TD-0008's own framing applies exactly: each of these is a **co-design
gap** — a contract capability on one side with no counterpart on the other.

Three concrete instances, plus one loose end.

**G20 — `idempotency-key` is read, persisted, and never used.** *(Design narrowed 2026-09-01: #179 now mints request ids at admission for every call, so the dedup record gains a stable correlation key for free — the G20 design no longer needs to choose an id source.)* It is extracted at
`sandhi-proxy/src/lib.rs:1526`, carried on `RequestMetadataV1` (`sandhi-core/src/chat.rs:147`),
stamped onto the usage event (`sandhi-core/src/event.rs:107`), and persisted. ADR-0005 D7 names its
purpose as *"`idempotency-key` for reconcile-once."* There is no dedup lookup anywhere in the
repository. A client that retries after an ambiguous timeout — the case the header exists for, and
the case `resilience.rs:145` explicitly warns about ("a timeout does not…") — is metered twice. This
is the exact false-affirmative pattern TD-0012 was written to eliminate: a field accepted, echoed,
and inert.

**G21 — no contract version on the HTTP path.** `wire_contract_version()` and
`chat_contract_version()` are exported by both bindings (`bindings/python/src/lib.rs:475,484`;
`bindings/node/src/lib.rs:430,439`), and TD-0008 records a real incident caused by a consumer *not
calling* the handshake that existed. An HTTP consumer cannot call it at all — there is no equivalent.
The proxy's four ingress dialects are vendor-shaped by design (TD-0010), so a Sandhi-aware consumer
has no way to ask *which Sandhi contract am I talking to*.

**G22 — a scorecard row that may be stale.** TD-0008 lists Node typed-error parity as pending
"follow-up C." `bindings/node/src/lib.rs:76` defines `typed_provider_error`. Either the row is out of
date or the parity is partial. Verify and correct the record — a scorecard that has drifted from the
code is worse than no scorecard.

**G30 — error construction is not single-sourced.** Four independent constructors build
client-facing errors: `provider_error` (`lib.rs:2616`), `rate_limited_error` (`:2679`),
`ingress_error` (`:2691`) and the bare `error` helper (`:2710`), alongside the raw plane's own
construction. TD-0010 D2 established that every error must
render in the caller's dialect and that is well-tested — but the *shape* is enforced by convention
across several construction sites rather than by one.

## First principles

1. **A field the API accepts and ignores is a defect** (TD-0012's rule). Either honour
   `idempotency-key` or reject it.
2. **The proxy path deserves the same seam discipline as the binding path.** TD-0008's conclusions
   were not scoped to FFI; they were about a contract with two sides.
3. **Version negotiation must not break vendor SDK compatibility.** The whole premise of TD-0010 is
   that an unmodified vendor SDK works. Any handshake must be additive and ignorable.
4. **Metering must be exactly-once per logical call.** ADR-0005 D2 made *settle* idempotent by lease
   id. The same reasoning applies one layer up: a client retry of the same logical call must not
   produce two usage events.
5. **A stale scorecard erodes the artefact.** TD-0008's table is only useful while it is true.

## Non-goals

- **No change to the ingress dialects.** Vendor SDKs continue to work unmodified; every addition here
  is an optional header or an optional endpoint.
- **No general-purpose response cache.** G20 is about *metering* exactly once, not about replaying a
  response body. Those are different problems with different storage and different privacy
  properties, and conflating them would put prompt content in a cache this TD does not want to own.
- **No new error taxonomy.** G30 consolidates construction; the vocabulary is TD-0010 D2's and stays.
- **No binding changes beyond G22's verification.**

## Decisions

**D1 — `idempotency-key` deduplicates the *lease and the usage event*, not the response.** Within a
bounded window, a repeat of `(virtual_key_id, idempotency_key)` reuses the original call's settlement
rather than creating a second lease and a second event. The client still gets its upstream call and
its response — Sandhi is not caching model output — but the *meter* records the logical call once.
Rejected: full response caching, for the §Non-goals reason.

**D2 — Dedup state is bounded and lives with the ledger.** A dedup record shares the enforcement
ledger's durability (the settlement it protects is durable, so the dedup must be too) with a TTL
comparable to the lease TTL (`RESERVATION_TTL_SECS`, 15 minutes at `proxy/src/ledger.rs`). Beyond the
window a repeat is a new logical call, which is correct: a retry an hour later is a new call.
Rejected: in-memory dedup — it would silently stop working across the restart that a crash-retry
scenario makes most likely.

**D3 — If the window has expired or dedup is unavailable, count the call.** Under uncertainty,
Sandhi records the measurement. ADR-0005 D3's split applies: over-counting is a visible, correctable
discrepancy; under-counting silently loses revenue-relevant truth and, per TD-0013 D6, "Sandhi's
product is the measurement."

**D4 — `GET /version` plus an optional response header.** An unauthenticated `GET /version` returns
the wire and chat contract versions and the supported ingress dialects; every proxied response
carries `x-sandhi-contract-version`. Both are additive and invisible to a vendor SDK. Rejected:
negotiation via `Accept` — it would collide with the dialects' own content negotiation and could
change what an unmodified SDK receives.

**D5 — The proxy advertises capabilities, not just a version.** `GET /version` reports which ingress
dialects are wired and which optional features are on (transparent plane, durable ledger, alerts,
OTLP). TD-0008's `hasattr` feature detection was the right instinct; this is its HTTP form, and it
lets a consumer degrade deliberately instead of by trial and error.

**D6 — Single-source error construction (G30).** One `IngressError` type owns status, code, dialect
rendering, and the TD-0008 D redaction decision; the four current construction sites become
constructors on it. This is a refactor with no behaviour change, and its value is that the *next*
error path cannot forget the redaction rule.

**D7 — Re-verify TD-0008's scorecard against the code and amend it in place.** G22 specifically, and
every other row while the file is open. Verification is the deliverable, whatever it finds.

## Phases

| Phase | Scope | Acceptance (the failing test to write first) |
|---|---|---|
| **P1** | D7 — scorecard verification — **DONE** (2026-09-01): every row re-verified; G22 closed for real — `SandhiProviderError` rewired on all three Node paths with an instanceof conformance test | Each TD-0008 row is confirmed against code with a citation, or amended. Node typed-error parity is settled either way |
| **P2** | D4 + D5 — version and capability discovery — **DONE** (2026-09-01): ungated `GET /version` (versions + dialects), gated `GET /admin/version` (capability booleans), `x-sandhi-contract-version` via one end-of-chain middleware (success AND errors, R3) | `GET /version` returns wire/chat contract versions, wired dialects, and active features; every proxied response carries `x-sandhi-contract-version`; an unmodified vendor SDK request is byte-identical apart from that header |
| **P3** | D6 — single-source errors — **DONE** (2026-09-01): `IngressError` (codec.rs) owns status/code/dialect rendering + the redaction decision; `ingress_error`/`provider_error` are thin delegates (all 29 call sites unchanged); acceptance test drives the type directly across all four dialects | Every client-facing error is constructed through one type; the existing dialect-shaping tests (`tests/proxy.rs:1138`, `:247`) pass unchanged; a redaction test proves the *default* path cannot leak an upstream body regardless of construction site |
| **P4** | D1 + D2 + D3 — idempotent metering | Two requests with the same `(vkey, idempotency-key)` inside the window produce **one** lease and **one** usage event; outside the window, two; with dedup unavailable, two (D3's fail-toward-counting), and the fallback is counted in a metric |

P1 is an afternoon and improves an artefact other sessions read. P4 is the substantive one and should
follow [TD-0016](TD-0016-enforcement-throughput-ceiling.md) P1, since it adds a ledger-adjacent write
to the hot path that the current single-mutex ledger would serialise.

## Pressure test

1. **"Idempotency dedup adds a durable write to the hot path to fix a rare case."** The write is
   small and shares the ledger's transaction, and the case is not rare: it is exactly what happens
   during an upstream incident, when every client retries at once and double-metering is least
   affordable. It is also *already promised* — ADR-0005 D7 says "reconcile-once" — so the choice is
   between honouring the promise and withdrawing it.
2. **"Nobody sends `idempotency-key` to a proxy."** The OpenAI and Anthropic SDKs both send one on
   retries by default. Sandhi already receives, stores, and ignores them today.
3. **"A version header leaks implementation detail to clients."** It leaks a contract version, which
   is what a contract is for. TD-0008 records a production incident caused by the *absence* of a
   handshake the bindings already had; the HTTP path is the one with more consumers and less
   coupling, so it needs it more.
4. **"D3 means a retry storm can over-count."** Bounded by the dedup window, visible in a metric, and
   the deliberate direction of error. The inverse — failing open into under-counting — would silently
   destroy the measurement, which per TD-0013 D6 is the product.
5. **"G30 is a refactor with no user-visible benefit."** Its benefit is that TD-0008 D's redaction
   default becomes structural rather than remembered. Three construction sites is three chances for
   the next error path to leak an upstream body containing prompt fragments to a different tenant.
6. **"P1 is busywork."** TD-0008's scorecard is cited by other documents as the state of the seam.
   One row already looks stale. A reference artefact that has quietly drifted from the code is worse
   than one that never existed, because people trust it.

## Resolved

**R1 — The dedup key is `(virtual_key_id, idempotency-key)`, with no request-body hash.** A body
hash would additionally catch a client reusing one key for a *different* call — but that is a client
bug, and the cost of detecting it is hashing every request body on the hot path for every request
that carries the header. Record the residual risk instead: a client that reuses an idempotency key
across different calls will have the second call metered as the first. That is the client's error to
fix, and the header's own semantics say so.

**R2 — Two endpoints, not one shape with two gates.** Ungated `GET /version` returns the wire and
chat contract versions; the capability detail (which optional features are enabled) goes behind the
existing ADR-0004 D4 admin gate. The draft's instinct — "ungated for versions, gated for capability
detail" — was right, and its own objection ("two shapes for one endpoint, which is its own smell")
is answered by making them two endpoints rather than one endpoint with conditional content.

**R3 — Yes, `x-sandhi-contract-version` appears on error responses too.** A version mismatch is most
likely to *present* as an error, which is precisely when a consumer most needs to know which
contract it is talking to. Omitting it there would withhold the header in the one case it exists
for.

**R4 — TD-0008's scorecard is kept honest by a conformance test, not a manual amend — and P1 now
starts from a finding rather than a verification.** G22 was resolved while planning this pass:
Python exposes a real exception class (`pyo3::create_exception!(… SandhiProviderError …)`,
`bindings/python/src/lib.rs:73-86`), while Node's `typed_provider_error`
(`bindings/node/src/lib.rs:76-79`) returns a generic `napi::Error::from_reason(json_string)` — no
distinct class, so a JS consumer cannot `instanceof` it and must parse the message to branch. **The
scorecard row is correct and the parity gap is real.** P1's job is therefore to close it and to add
the test that stops the scorecard drifting again, rather than to re-check rows by hand.

## Still open

Nothing. Every question this TD raised is decided above.
