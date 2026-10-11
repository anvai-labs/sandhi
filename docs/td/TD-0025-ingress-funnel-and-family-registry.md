# TD-0025: Ingress funnel decomposition and the family registry

- **Status:** Scoped P3 preparation/boundary increment landed in [PR #365](https://github.com/anvai-labs/sandhi/pull/365); broader P3 remains partial and P1/P2/P4 remain proposed. Owns **G32** in the [ADR-0006](../adr/0006-layer-boundary-and-protocol-scope.md)
  gap register. Each phase lands as an independently revertable, zero-behavior PR. The scoped
  P3 preparation extraction is independent of the proposed family registry.
- **Relates to:** [ADR-0004](../adr/0004-two-plane-proxy-and-enforcement-boundary.md) (the
  two-plane dispatch this funnel implements), [TD-0010](TD-0010-ingress-dialect-parity.md) (D5:
  real-SDK conformance is the proof that must stay green), [TD-0022](TD-0022-per-call-transport-context.md)
  (#196 — the demonstrated cost of the current surface), [TD-0021](TD-0021-co-design-seam-v2-proxy-path-contract.md)
  (P3's `IngressError` unification is a prerequisite for the error-shaping stage), the
  2026-09-01 design audit (findings P1–P10, P17).

## Original audit motivation

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

## 2026-10-11 graph audit: scoped P3 increment

Victor `dacac4d1` indexed Sandhi `a2b0f0c`: 189 files, 107 Rust files,
194 resolved cross-file imports and 72 calls. These are static lower bounds;
macros, conditional code and ambiguous receivers remain unresolved. Cargo's four
crate dependency graph is acyclic. Provider-facade fan-in is expected API reuse,
not a reason to split the provider crate or change languages.

| Item | Design and acceptance | Status |
|---|---|---|
| Shared access helpers | Move the existing comparison, HTTP error and mutation-admission primitives out of the admin handler; lifecycle admission accepts only its owner. Preserve error bodies and hold guards through publication. | ✅ Merged in [PR #365](https://github.com/anvai-labs/sandhi/pull/365) (`b34772b0`) |
| Typed request preparation | Borrow only deadline, rate-limit, metrics and policy capabilities. Preserve auth → decode → allowlist → attribution → rate limit → policy → reservation → dispatch order. | ✅ Merged in [PR #365](https://github.com/anvai-labs/sandhi/pull/365) (`b34772b0`) |
| Explicit integration dependencies | Remove production wildcard imports in buffered/qualified-stream integration; retain canonical accounting and lifecycle owners. | ✅ Merged in [PR #365](https://github.com/anvai-labs/sandhi/pull/365) (`b34772b0`) |
| Verification | Reuse existing auth, proxy, cancellation, settlement and recovery owners. Extend missing shutdown/credential ordering coverage without duplicating suites; run workspace tests, Clippy, coverage, conformance, docs and independent review. | 875 workspace tests (6 ignored), Clippy, 89.54% line coverage, 315 SDK tests (19 skipped), strict docs + 3 docs tests passed |

Merged into `develop` as `b34772b0` on 2026-10-11, from baseline `a2b0f0c`
and independently reviewed head `d104f80a3b941786cd6eb1265b3407fa5864d61d`.
No deployment, public API, wire-contract or default changes were made. This increment does not close
P1/P2/P4, claim distributed enforcement, or establish mixed-team C5 acceptance.

The design uses narrow contexts as recommended by
[Axum's state guidance](https://docs.rs/axum/0.7.9/axum/extract/struct.State.html),
without changing public `ProxyState` fields. It preserves
[Tokio's short synchronous critical-section pattern](https://tokio.rs/tokio/tutorial/shared-state):
no mutex guards across awaits and no unmeasured lock/language rewrite. Success is
fewer dependencies at each internal boundary with identical externally observable
behavior; no runtime speedup is claimed from source or graph size.

Local verification used the unchanged canonical suites, with the existing credential
non-enumeration test extended across running/quiescing/draining/stopped phases and
the existing comparison test moved with its helper. The 231-test pre-change baseline
and extended characterization passed before extraction. Independent working-diff
review and the final exact-commit review found no blocker; all applicable hosted CI
gates passed before merge.

The first SDK attempt was stopped for a shared-build-cache fixture path mismatch;
correcting local binary links and using a short macOS socket directory yielded the
315-test pass. The 19 skips are 16 InferFlux-origin cases, two opt-in AgentBrowser cases and
one opt-in three-repository Victor module; this is not live InferFlux or C5 acceptance. No product behavior was changed to make fixtures pass. Review and
published PR evidence, rather than temporary machine logs, determine merge status.

Hosted CI initially rejected the pinned OIDC adapter fingerprint after its single
helper-import change. Independent reassessment confirmed identical authentication
operations and comparison implementation; the record was updated without extending
its expiry or advisory scope. The fingerprint/dependency-route gate and all three
cargo-deny graphs passed, as did 349 release-safeguard tests (147 subtests). No CI
check was disabled or relaxed. After clean exact-commit independent review and green
hosted CI, #365 was squash-merged using the explicitly authorized admin bypass of
the required PR approval. This records a scoped implementation milestone, not full
TD-0025 completion or a release/deployment acceptance result.
