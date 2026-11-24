# ADR-0047: Streaming deadline hot-apply and the token-scaled body estimate

- **Status:** Source candidate for the next feature release.
- **Owner:** Streaming admission policy in `sandhi-proxy` (`deadlines.rs`, `preparation.rs`, `operator.rs`).
- **Context incident:** Two GLM-5.3 critic calls (`max_tokens` 32768, reasoning high) through the
  DS3 edge were killed by the 420 s body cap exactly 420.001 s after the first stream event —
  HTTP 200 from the upstream, zero reported usage, reservations unsettled. At the measured
  ~50 tok/s a 32K-token completion legitimately needs ~650 s, so the fixed body cap was shorter
  than the model's natural completion time for this prompt class.

## Decision

Three changes to the streaming deadline policy, none touching the wire contract, the ledger, or
the buffered plane:

1. **Token-scaled body estimate.** When a streaming request resolves to the *global default* —
   no explicit endpoint default and no exact-model override — the effective body limit is raised
   to `max(resolved body, min(max_output_tokens × ms_per_output_token × headroom_factor,
   max_body_ms))`. The parameters live in the `streaming_deadlines` section
   (`estimate.ms_per_output_token`, default 20; `estimate.headroom_factor`, default 2.0;
   `estimate.max_body_ms`, default `ceiling_ms − default.setup_ms`) so tuning is a config edit,
   not a code change. Precedence is explicit and one-directional: **the estimate only ever
   raises the global default; explicit Model/Endpoint overrides are always authoritative and
   are never scaled.** A `max_body_ms` at or below the default body disables scaling (escape
   hatch). Arithmetic is fixed-point (milli-factor) saturating `u128` — no float in the final
   value. `GET /v1/models` keeps advertising the raw policy limits; the effective body can be
   higher, up to `max_body_ms`, and `x-sandhi-stream-body-budget-ms` preflight is checked
   against the effective (possibly raised) limit.

2. **Base default body rises 420 s → 540 s.** Setup 30 s + body 540 s = 570 s ≤ 840 s ceiling;
   body + 60 s settlement headroom = 600 s ≤ the 900 s reservation TTL. The estimate covers the
   genuinely long completions; the higher floor widens the margin for mid-size ones.

3. **Streaming deadlines hot-apply.** This reverses the documented startup-only stance for the
   streaming section specifically (buffered deadlines and listener TLS remain restart-required,
   TD-0017 P2). `POST /admin/config/apply` validates the declared endpoint references against
   the provider registry *as of that apply* — a file adding a provider and a deadline reference
   together succeeds — and swaps the active policy. An unregistered reference refuses the
   streaming section as an accumulated failure (`component: "streaming_deadlines"`, no
   rollback, HTTP 503) instead of failing startup. Startup keeps its existing exit-on-invalid
   behavior. `GET /admin/config` reports the streaming activation mode as `applies on apply`
   with an accurate active/desired diff; the apply response's deadline status is the
   convergence receipt (`active == desired` on success).

## Consistency

The active policy is a `RwLock<Option<Arc<StreamingDeadlines>>>` on `ProxyState`. Each request
clones the `Arc` once at admission and keeps that snapshot for its whole lifetime, so an apply
never tears an in-flight resolution: a stream dispatched under the old policy keeps the old body
deadline to completion. Cross-request, readers can straddle a swap (e.g. two `/v1/models` calls
either side of an apply report different limits); that is eventual consistency by design, and
the apply response is the authoritative ordering point.

## Consequences

- The GLM incident shape is fixed without a per-model override: a 32K-token request scales to
  `max_body_ms` (810 s under the default ceiling) and passes a preflight header that the old
  raw default refused. The actual-lease check is unchanged and still refuses — never clamps —
  when the remaining reservation lifetime cannot cover body + headroom; near-ceiling estimates
  therefore require a fresh reservation, and a long setup can still cause a post-admission
  refusal. This is deliberate (refuse-never-clamp).
- Apply-time validation is moment-of-apply: revoking a provider later does not retroactively
  invalidate an active policy that references it; the next apply (or startup) is the checkpoint.
- Apply remains additive-only, like every config section: removing `streaming_deadlines` from
  the file never reverts the active policy (the `desired: null` diff in preview/status is the
  visible divergence). Reverting to built-in defaults is itself a declared policy — apply a
  section carrying the default values, or restart. Startup keeps the asymmetric behavior of
  seeding defaults when the section is absent.
- The admitted reservation TTL is clamped to the documented 900 s `RESERVATION_TTL_SECS`
  (`settlement/buffered.rs`), restoring the ceiling invariant the estimate's default cap
  (`ceiling − setup` with a 840 s ceiling) would otherwise have overflowed by the accounting
  wait — the body owner's remaining-lease check still guarantees body + settlement headroom
  fits inside the window.
- The body deadline remains an admission estimate and lifetime control, not a physical token
  bound; zero-usage terminal streams at an edge timeout remain possible for upstreams that
  stream nothing before the cap. Structured timeout classification and partial-usage accounting
  for such streams stay future work (see the incident handoff's asks 1, 3 and 4).
