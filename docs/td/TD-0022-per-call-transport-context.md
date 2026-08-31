# TD-0022: Per-call transport context — wire headers that change every turn

- **Status:** Implemented, 2026-08-31. Closes the ecosystem backlog item tracked since
  sandhi PR #149 ("per-request metadata in the sandhi binding unblocks victor's
  `x-sandhi-step-id`").
- **Relates to:** [ADR-0005](../adr/0005-enforcement-correctness-reservation-ledger-observe-enforce-split.md)
  D7 (the neutral identity fields), [ADR-0008](../adr/0008-inferflux-admission-and-session-affinity.md)
  D3 (headers as the sanctioned out-of-band channel), [TD-0008](TD-0008-victor-codesign-boundary.md)
  (the in-process seam this extends), [TD-0021](TD-0021-co-design-seam-v2-proxy-path-contract.md)
  (the HTTP-path seam; adjacent, not overlapping).

## Why this exists

Gateway mode (a consumer pointing the typed runtime at the sandhi proxy with a virtual key)
stamps gateway-path identity — `x-sandhi-run-id`, `x-sandhi-step-id` — as **wire headers at
handle construction**. But a handle is deliberately cached per conversation (pool, circuit
state), while a **step id changes every turn**. The only workaround was rebuilding the
transport each turn, which the design explicitly refuses. Result: step-level cost-tree nodes
(ADR-0005 D7's `step_id`/`parent_id`) were unreachable on the FFI path — the field was
accepted, persisted, and unpopulatable. The same defect class as TD-0021's G20: a contract
capability with no counterpart on the other side.

## Decisions

### D1 — Per-call wire headers ride the request, not the transport.

`ProviderRequest::extra_headers` (`with_extra_headers`) carries a per-call header map from
the FFI seam (`complete_json`/`stream_json` optional `wire_headers_json` in both bindings →
`ProviderHandle::complete_with`/`stream_with` → `ChatProvider::{complete,stream}` now take
`call_headers`) down to every adapter. The trait parameter is **not** optional to implement:
every family's codec threads it (compiler-enforced — no family can silently drop it), which
is the discipline the trait change buys over a default-ignoring `…_with` default method.

### D2 — One merge, single-sourced, transport-owned names untouchable.

`merge_call_headers(base, call)` overlays the call's headers on the transport's static set;
`strip_transport_owned` removes `Authorization`, `Host`, `Content-Type`, `Accept-Encoding`
from any caller-supplied set (static or per-call). A library consumer can never override the
vaulted credential or the framing. In the OpenAI-compat adapter the catalog-declared session
affinity header (ADR-0008 D3) is inserted **after** the merge, so the authoritative
per-request affinity value also survives per-call spoofs.

### D3 — The four families that dropped static headers now honor them.

`ProviderRuntime::transport()` passed `config.headers` only to the OpenAI-compat and
Responses adapters; Anthropic, Cohere, Gemini, and Ollama constructed without them, so any
transport-configured header — including gateway-mode `x-sandhi-run-id` — silently never
reached those upstreams. All four adapters gain `with_headers` + the per-call merge. This is
a bug fix that falls out of D1's uniformity rather than a scope add.

## Consequences

- **Positive.** Victor (and any FFI consumer) can send turn-scoped gateway identity without
  handle churn — the step-level cost tree becomes populatable, and the handle cache stays
  per-conversation. Cross-family gateway routing now actually delivers its headers.
- **Positive.** The stripping rule is stated once (`strip_transport_owned`), tested at the
  merge level and end-to-end through both bindings against a live local server (the
  `authorization: Bearer attacker` case).
- **Negative/accepted.** `ChatProvider::{complete,stream}` gained a parameter — a breaking
  change for any external implementor of the trait. The crate is workspace-internal
  (unpublished), and the break is the point: it forces every codec through the context.
- **Neutral.** Per-call headers are transport data, never contract data — nothing here
  touches `ChatRequestV1` or any schema; codegen-drift is unaffected.

## Pressure test

1. **"Put them in `ChatRequestV1.metadata` instead — no trait change."** Metadata is the
   *wire contract*; per-call HTTP headers are transport concerns, and smuggling them through
   the contract invites exactly the body/header confusion ADR-0001 §4 forbids. The request
   type the adapters already consume (`ProviderRequest`) is the right vehicle.
2. **"Default-implement the trait method to ignore headers."** Then a codec that forgets to
   override silently drops gateway identity — the TD-0021 G20 defect class, reintroduced on
   purpose. The signature change makes the compiler the reviewer.
3. **"This lets callers inject arbitrary headers."** Within the strip rule, yes — same as
   the static `with_headers` that preceded it (OpenRouter's `HTTP-Referer` is the motivating
   case). The credential and framing stay transport-owned; everything else is caller data
   the proxy upstream can already receive.
