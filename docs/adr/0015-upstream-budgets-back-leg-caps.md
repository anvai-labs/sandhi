# ADR-0015: Upstream budgets — back-leg caps

Date: 2026-11-08

## Status

**Accepted for implementation.** Implemented with ledger/flow unit tests,
operator round-trip coverage, declarative-config parsing, and end-to-end
proxy tests (including regression tests for the lease-release paths surfaced
in review).

Relates to [ADR-0005](0005-enforcement-correctness-reservation-ledger-observe-enforce-split.md)
(the lease ledger — D1 reserve/settle, D2 rollback, D4 `billable()`, D6
fail-open/closed), [ADR-0004](0004-two-plane-proxy-and-enforcement-boundary.md)
(two-plane enforcement), and [TD-0003](../td/TD-0003-operator-surface-keys-budgets-attribution.md)
(the operator budget surface it reuses).

## Problem

The budget chain enforced only **front-leg** scopes — `vk:{id}` or `group:{g}`
derived from the virtual key, or an operator-set custom scope — bounding
user→gateway spend. Nothing bounded the **back leg**: how much a single
provider (e.g. a costly self-hosted InferFlux cluster or one OpenAI org) may
be charged in aggregate by this gateway over its window. Front-leg caps do not
compose into that answer: many unconstrained keys can each stay under their
own cap while the provider's total sails past anything an operator intended.

## D1. Scope convention — `upstream:{provider}`, per-provider only

The back-leg scope string is `upstream:{provider-slug}` — e.g.
`upstream:inferflux`, `upstream:openai` — where the slug is the resolved
provider's `slug()`, the same value carried in every `UsageEvent` and in the
catalog (`upstream_ref` on a virtual key names the registered upstream; the
slug is what the event records). **Per-provider only**: never per-model, never
hierarchical (owner decision — a model dimension would fork every cap into a
cross-product no operator asked for, and nesting front legs under back legs
would double-charge every call). The string is plain ledger data: nothing in
the reserve/settle path special-cases it.

The cap aggregates by **adapter slug**, not by registered upstream or by
organization: multiple registered upstreams that share a slug (two mounts of
the same provider family, or two keys to the same org) share one cap, because
enforcement happens against the resolved adapter, not against a credential.

One consequence of genericity needs a guard: because `vk.budget_scope` is a
free-form operator string, a virtual key pointed at an `upstream:`-prefixed
scope would hold two leases on the same scope per call and settle the same
actual into it twice, exhausting the cap at half its configured limit. The
prefix is therefore **reserved and enforced at every minting surface**:
`POST /admin/keys/share` rejects it with a 400; the client-credentials
registry loader makes such an entry fatal (startup exits); and OIDC config
validation rejects it on any inference grant at startup — config-authored
grant scopes flow into both the live OIDC-derived key (`grant_groups`) and
the durable subject-key mint (`MintRequest`), so request-time guarding would
be too late. (The subject-key re-delegation path derives the live key's scope
as `group:`/`vk:`, but the durable record still carries the config scope.)
The declarative `vkeys` section and group profiles never carry a budget scope
at all, so there is nothing to guard on those surfaces.

## D2. Dual-lease admission — same ceiling, front denial checked first

At admission the gateway now holds **two leases** for one call, both with the
same conservative ceiling (`reservation_ceiling`, unchanged):

1. the existing front-scope lease (`group:`/`vk:`/custom); then
2. `upstream:{slug}` — reserved only after the front reserve **and** the
   front-denial check, so a front denial short-circuits without ever taking a
   back lease.

The back reserve runs while the front-scope `PendingAdmission` still owns its
lease: a cancellation of the handler future at the back reserve's await point
(client disconnect, drain) rolls the front lease back through
`PendingAdmission`'s Drop — a bare `Admission` is never held across an await.

An upstream `Admission::Denied` (exhausted `Block` back-leg cap) returns the
dialect-shaped `429 budget exhausted` identical in shape to the front denial,
logs `reservation denied: upstream budget exhausted`, and records the denial
metric **labelled by the upstream scope's policy — never by scope** (the
TD-0011 D2 invariant: scopes like `vk:<id>` are unbounded cardinally). The
still-owned front lease releases through `PendingAdmission`'s Drop. Gateway
draining between the two reserves likewise releases the front lease through
that Drop and returns the draining error.

From its creation to the `RequestAccounting` hand-off the back lease lives in
an RAII `UpstreamHold` (mirroring `PendingAdmission`'s D2 rollback: blocking,
settle 0, under the ledger lock) so that any early return in between releases
it exactly once. On the legacy path the hold exists only briefly — after the
back reserve, the front denial has already been checked and the
deadline-infeasibility and cutoff guards have already run, so the remaining
stretch to the accounting construction has no early returns; the hold is the
invariant, not a busy guard. The buffered/tracked path is where the hold does
real work: `settlement::buffered` receives the already-admitted back lease as
parameters and wraps it in the hold **before any of its own guards**, so its
refusals — the tracked-requirements 400, the transport-deadline guard,
capacity, denied admission, authorization failure, cutoff — cannot leak a
lease: ineligible traffic must not hold `reserved()` against a Block upstream
cap until TTL reclaim. Every pre-dispatch refusal there settles it to 0.

An `upstream:*` scope under `Warn` on a ledger backend error admits
**unmetered** (D6 fail-open), logged and counted like the front path.

## D3. Settlement — same actual, finalize-time

The back lease settles with the **same** cache-inclusive `billable()` actual
(D4) as the primary lease, at finalize — one call, one quantity, charged to
both scopes. No second overshoot metric observation: overshoot is a property
of the call and is counted once, on the primary scope.

Threshold alerts are evaluated for the upstream scope too, against its own
configured limit, reusing the same helper — **on the legacy settlement path
only**. Tracked/buffered mode fires no alerts for either scope (a pre-existing
gap, unchanged by this ADR; the primary scope did not alert there either).

In the tracked/buffered path the primary lease settles canonically through
the settlement-jobs machinery, which carries only the primary scope; the back
lease therefore settles **synchronously** in `finalize_owned` with the same
actual (settle is idempotent by lease id, ADR-0005 D2). Unfinalized calls
(settle-on-drop) settle both leases the same way through the existing drop
path.

## D4. Crash semantics — TTL reclaim, no spend recorded

The back lease does **not** participate in the buffered recovery registry or
terminal-evidence machinery. A crash mid-call leaves the upstream lease held
until TTL reclaim releases it **without recording spend** — accepted, exactly
mirroring the existing D2 reclaim semantics for front leases. The exposure
window is the reservation TTL, same as today's front leg.

## D5. Cost — a second lease per call, even when no cap is set

The back reserve is **unconditional**: every generation call takes an
`upstream:{slug}` lease whether or not the operator configured a cap on that
scope (a scope with no limit admits freely, but the lease is still taken and
settled). In durable mode this adds one reserve INSERT + one settle per
request plus the alert-time `spent` read — roughly doubling ledger write
traffic per generation call. Enforcement semantics are unchanged; an operator
who does not configure `upstream:*` caps pays only the extra writes. Reuse of
a ledger connection pool keeps this bounded; the unconditional reserve was
chosen over conditional reserving so the scope's `reserved()` reflects
in-flight load uniformly and the code carries no second admission mode.

## D6. Everything else reuses the generic scope machinery

`upstream:*` is ordinary scope data, so with no new code: the operator API
(`POST /admin/budget`, `GET /admin/budget`, `GET /admin/budget/usage?scope=`),
the declarative config `budgets` section, restart rehydration
(`rehydrate_budgets`), the durable budgets table, the alerts registry, and
the dashboard budgets card all accept and round-trip back-leg scopes. The
dashboard only gained a `back leg` badge on rows whose scope starts with
`upstream:` so operators can distinguish front-leg from back-leg caps at a
glance.

Non-goals: per-model back caps, cap hierarchy, a shared/HA ledger backend
(TD-0007), and any change to what a `Warn` scope does on success.
