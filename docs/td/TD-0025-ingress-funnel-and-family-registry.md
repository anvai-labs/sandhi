# TD-0025: Ingress funnel decomposition and the family registry

- **Status:** Draft (proposed). Owns **G32** in the [ADR-0006](../adr/0006-layer-boundary-and-protocol-scope.md)
  gap register. Nothing is scheduled; the phases are ordered so each lands as an independently
  revertable, zero-behavior PR.
- **Relates to:** [ADR-0004](../adr/0004-two-plane-proxy-and-enforcement-boundary.md) (the
  two-plane dispatch this funnel implements), [TD-0010](TD-0010-ingress-dialect-parity.md) (D5:
  real-SDK conformance is the proof that must stay green), [TD-0022](TD-0022-per-call-transport-context.md)
  (#196 — the demonstrated cost of the current surface), [TD-0021](TD-0021-co-design-seam-v2-proxy-path-contract.md)
  (P3's `IngressError` unification is a prerequisite for the error-shaping stage), the
  2026-09-01 design audit (findings P1–P10, P17).

## Why this exists

The audit measured the co-edit surface that the #196 incident demonstrated the hard way: adding
or changing a provider family currently touches **≥13 `match`-on-family sites** (adapter
construction, usage sniff + parse, ingress decode, response + stream-event encode, `for_slug`,
`FamilyFacts`, `ingress_family`, `upstream_path`, `apply_auth`, the operator's handle builder,
and both bindings' `provider()` dispatchers), plus six copy-paste `ProviderRuntime` factories and
six `Typed*` adapters whose `complete`/`stream` bodies are structurally identical modulo codec
function names. The repo already contains the counter-pattern it chose for vendor facts —
`OpenAiCompatProviderSpec` as data, `FamilyFacts` as an accreting table, "vendor differences are
DATA, not code branches" — but family↔codec binding never got the same treatment.

On the proxy side, `handle()` is a ~315-line funnel whose ordering contract (auth → allowlist →
budget → dispatch) lives in a comment; three of the four ingress handlers are identical modulo
the dialect constant; the plane×stream dispatch re-implements the terminal protocol (set_outcome
→ finalize → error shape) at six sites; and the two stream-body loops (transparent, typed) carry
the same lifecycle contract — permit-in-body, open-stream gauge, running partial usage, delta
bytes, finalize — by hand, in two different chunk shapes.

None of this is a runtime bug today. It is the surface that made #196's silent per-family
divergence possible — and each new family multiplies the places to forget.

## Design

**D1 — the family registry.** One declarative binding of a `ProviderFamily` to its codec
surface: constructors, `sniff_usage_line`/`parse_usage`, ingress `decode`, response and
stream-event encoders, `default_base_url` (landed), auth application, path derivation. Adding a
family becomes: one `FamilyCodec` implementation + one registry row. The 13 match sites become
registry lookups. Vendored facts stay where they are (catalog specs); this registry binds
*codecs*, not vendor data.

**D2 — generic `TypedAdapter<C: FamilyCodec>`.** The twelve structurally-identical
`complete`/`stream` bodies collapse into one generic adapter; the two real deviations become
codec hooks (`aggregate_stream` for the Responses ChatGPT profile, `apply_constraints` for the
OpenAiCompat special case). Published-crate caution: `TypedAnthropic` & co. are `pub` types —
the concrete named types should remain as type aliases or thin re-exports so external `impl
ChatProvider` consumers keep compiling.

**D3 — funnel decomposition.** Each numbered comment-step of `handle()` becomes a stage with a
typed output (credential → resolved key → provider → decoded request → admitted call), so the
ordering contract is enforced by construction order rather than maintained in prose. The
terminal protocol moves into one responder wrapper; `resolve_for_discovery` reuses the head
stages instead of re-implementing them.

**D4 — one stream-metering guard, two chunk mappers.** The duplicated stream lifecycle (permit,
gauge, partial-usage, delta bytes, finalize) becomes a single guard type; the transparent and
typed loops keep their own chunk mapping and feed the same guard. With it comes the differential
test the audit found missing: **the same fixture metered through both planes must produce the
same `UsageEvent`** — the strongest available regression net for everything above.

**D5 — non-goals.** No new dialect, no ingress-surface change, no policy engine (TD-0005), no
metric or label changes, no behavior change of any kind in any phase — the proof of correctness
for every phase is: existing tests green unmodified, the TD-0010 real-SDK conformance suite
green, and the new plane-differential test green.

## Phases

| Phase | Deliverable | Risk |
|---|---|---|
| P1 | Family registry (D1) — mechanical, match sites become lookups | Low; compiler-verified exhaustiveness lost → gain an exhaustive registry test |
| P2 | `TypedAdapter<C>` (D2) with concrete-type back-compat | Low-mid; published-crate API care |
| P3 | Funnel stages (D3), after TD-0021 P3's `IngressError` lands | Mid; largest diff, pure moves |
| P4 | Stream guard + plane-differential metering test (D4) | Mid; the differential test is net-new coverage |
