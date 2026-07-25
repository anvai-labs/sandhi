# ADR-0005: Enforcement correctness — reservation ceilings, a lease-based atomic ledger, and the observe/enforce split

Date: 2026-07-23

## Status

Proposed. Refines [ADR-0004](0004-two-plane-proxy-and-enforcement-boundary.md) D3 (the
"durable, atomic, shared ledger") and the reserve-then-reconcile mechanism assumed across
[TD-0003](../td/TD-0003-operator-surface-keys-budgets-attribution.md) and
[TD-0005](../td/TD-0005-declarative-policy-engine.md). Records the design that survived a
four-lens adversarial pressure-test (proxy data-path, distributed-systems ledger, policy/trust,
and agentic-workload). Does not touch the measure-vs-price boundary — enforcement is in neutral
tokens; still no dollars.

## Context — what the pressure-test falsified

Before writing enforcement code, the design was attacked from four independent lenses against the
shipped code (`crates/sandhi-core/src/budget.rs`, `crates/sandhi-proxy/src/lib.rs`). Three
mechanisms were falsified or shown to regress; the ADR-0004 *decisions* (two-plane, custody-is-
the-boundary, declarative policy) hold, but the mechanisms under them do not.

1. **The reserve-then-reconcile "hard cap" is soft — single node, no concurrency required.**
   `estimate_reservation` reserves `bytes/4 + max_output_tokens.unwrap_or(1)`; `reconcile`
   overwrites the reservation with the *actual* measured usage, unclamped. Budget 100, spent 99,
   one streaming request that omits `max_output_tokens` → reserved ≈ 1 → `check(99+0+1 ≤ 100)`
   passes → the stream emits 10 000 output tokens → `spent = 10 099`. The Tier-2 "authoritative /
   prevented" cap (ADR-0004 D2) is overshot 100× by a single request. Concurrency makes it worse
   (N requests each reserve their estimate, not their eventual actual), but concurrency is not
   required to break it. **Reserve-then-reconcile bounds overspend by `Σ(actualᵢ − estimateᵢ)`,
   not to zero — and the estimate is a lower bound.**

2. **The reservation is released to zero on stream interruption → a metering-evasion vector.**
   Usage is known only at the terminal frame; a client disconnect or upstream drop yields
   `completeness ≠ Final` → `finalize` takes the `release` branch and records **zero** billable
   tokens, though the upstream already generated (and will bill for) the output. Open a stream,
   read a lot, disconnect, repeat — free tokens below the meter. Long streams are the most
   expensive calls and the most likely to be cancelled (speculative agent branches).

3. **Durabilizing the current ledger *unchanged* is less correct than today's in-memory Mutex.**
   Today the ledger is one `HashMap` behind a `std::sync::Mutex`; within a process it is correct
   (reserve = check+increment is atomic; `Drop` guarantees release). Moving to a durable/shared
   store *reintroduces* three failures the Mutex was silently solving: (a) a durable `reserved`
   integer **leaks on crash** — no `Drop` runs, held tokens are never released, and the scope
   blocks forever on an empty budget (self-inflicted DoS); (b) delta-based `reconcile` is
   **non-idempotent** — at-least-once delivery (retry, failover replay) double-subtracts
   `reserved` and double-adds `spent`; (c) snapshot-then-write reserve is a **TOCTOU across
   replicas** — two replicas read `spent=90/100`, both admit, both write → N× overrun. Note this
   is exactly the shape TD-0005's *pure* `PolicyEngine` + `LedgerView` produces: decide on a
   snapshot, commit later.

Two more cross-cutting facts the lenses agreed on: the byte/4 estimate is wrong in both
directions (CJK ~10× under, base64 images / verbose tool schemas over), and "billable =
`tokens_in + tokens_out`" excludes the cache split, so once the ledger is durable its `spent`
diverges from the `usage_events` aggregate and audit becomes impossible.

## Decision

### D1. Reserve a ceiling, not an estimate; enforce it mid-stream; settle Partial on interruption

The reservation must be a **conservative upper bound** on what the call can spend, so admitting it
cannot overshoot a `Block` cap:

- Derive an effective **output ceiling** from the request's `max_output_tokens`, or synthesize the
  model's catalog max when absent. Reject pre-flight when `ceiling > limit − spent − reserved`
  for a `Block` scope (a call that *could* exceed the cap is never admitted).
- For `Block` scopes, enforce a **mid-stream cutoff**: accumulate output tokens from stream deltas
  and abort the upstream connection when cumulative crosses the reservation. Without a cutoff, a
  streaming `Block` cap is soft — state that explicitly wherever Tier-2 "prevented" is claimed.
- On stream interruption/disconnect, **settle `Partial`** from the accumulated delta count — never
  `release` to zero for a `Block`/Tier-2 scope.
- The input side of the estimate becomes **model-aware / family-calibrated** (fixed per-image
  token costs; a real or calibrated tokenizer), not `bytes/4`.

`PolicyDecision::Allow` therefore carries the **cap to enforce**, not merely an amount to record.

### D2. Reservations are TTL leases; settlement is idempotent and keyed by id; the atomic decrement runs inside the store

The three properties from Context #3 must land **together or not at all** — any one alone leaves
the durable ledger less correct than today:

- **Lease, not counter.** `reserve` returns a typed `Reservation { id, expires_at }`. A sweeper
  (or lazy reclaim at read time) reclaims any lease past a max-request TTL; long streams renew by
  heartbeat. A crash can no longer leak capacity. (Redis gives this via key TTL for free.)
- **Idempotent settle.** Settlement is `settle(reservation_id, actual)`, a state transition
  `reserved → settled` that is a no-op if already settled — safe under at-least-once. No bare
  delta arithmetic on a shared counter.
- **Atomic conditional reserve in the store.** The hard-cap admit is one atomic operation executed
  by the store — Redis Lua token-bucket, Postgres `UPDATE … WHERE spent+reserved+? ≤ limit
  RETURNING` (0 rows = denied), or SQLite `BEGIN IMMEDIATE` — never `SELECT`-in-proxy then
  `UPDATE`. The pure `PolicyEngine` may **decide eligibility**, but the budget **commit re-checks
  under the transaction and can still return `Denied`**.

### D3. Split observe from enforce — they have opposite consistency requirements

Metering and enforcement stop sharing `finalize`:

- **Enforce (synchronous, linearizable for `Block`).** The atomic reserve/settle above, on the
  request's critical path, keyed by an **idempotency key** so a client/framework retry does not
  double-charge. `Warn` scopes may use an eventually-consistent per-replica counter with async
  rollup (cheaper, no hot-path round-trip) — CRDT/PN-counters are acceptable for `Warn` only, as
  they can transiently exceed a cap.
- **Observe (asynchronous, at-least-once).** `UsageEvent` emission moves off the hot path into a
  buffered, at-least-once channel keyed by the idempotency/request id. A best-effort sink must
  **not** be the ledger of record for a hard cap; for `Block` scopes the settle write and the
  event write share one transaction.

### D4. One `billable()` definition, cache split preserved as dimensions

`sandhi-core` owns a single `billable()` used identically by reserve, settle, and the durable
aggregate. It keeps the cache split as distinct weighted terms (`fresh_input`, `cache_creation`,
`cache_read`, output, and reasoning) rather than flattening to `tokens_in + tokens_out` — still
neutral tokens, no dollars. This closes the budget-vs-event divergence and lets the downstream
pricer see the real cost basis. Reasoning tokens are made explicit (contract invariant: each
adapter either folds reasoning into `tokens_out` or the ledger adds it).

### D5. The ledger abstraction lives in `sandhi-core`; durability/atomicity/TTL live behind it

Two trait surfaces in `sandhi-core`:

- `LedgerView` — a read snapshot (`spent`/`reserved`/`limit` for a scope+window) that the pure
  `PolicyEngine` reads.
- the write side — `reserve(scope, ceiling) -> Reservation` (or `Denied`) and
  `settle(reservation_id, actual)` (idempotent), plus lease reclaim.

Atomicity, idempotency, TTL, and **calendar-aligned window anchors** (window boundaries from
`floor(now, UTC day/month)` with the **store as clock authority** — not a mutable per-scope start
that a limit-edit resets, not a fixed 30-day "month", not each replica's wall clock) live behind
the trait in `sandhi-store`. Impls to the same contract: in-memory (SDK/tests) → SQLite
(single-node) → Redis (HA). Note **single-node SQLite is durable but *not* shared** — only the
Redis/Postgres impl satisfies "hold across replicas"; ADR-0004 D3 conflated the two.

### D6. Availability: fail policy is per-tier; never block the async runtime on the ledger

Ledger-backend-unreachable is a **per-tier decision**: `Block`/Tier-2 fails **closed** (preserve
the cap) with a short-TTL local fallback cache so a blip degrades to slightly-stale enforcement
rather than a full outage (the Envoy RLS shadow/fail-open pattern); `Warn` fails **open**. Never
hold a `std::sync::Mutex` across an `.await` or a DB round-trip (today `reserve_budget` holds the
ledger Mutex across the call — that would block a tokio worker once the call is a network hop).

### D7. Honest trust claims and neutral identity

- Tier-1 "attested" → **"self-reported."** With the credential and transport in the client
  process there is no remote attestation; the reported number is adversary-controlled, and the
  only ground truth is downstream provider billing (outside this repo). "Detectable" is
  aspirational within Sandhi's boundary unless metering is routed through a server the client
  does not control.
- Add neutral identity to `RequestMetadataV1` + `UsageEvent`: `idempotency_key`, `run_id`,
  `step_id`, `parent_id`, W3C `traceparent`, and a `session_id` **derivable from standard
  signals** (OpenAI `user`, Anthropic `metadata.user_id`, or a stable prefix hash) so a drop-in
  SDK that cannot set a custom header still gets cache affinity and an agent's cost tree is
  reconstructable. All additive, versioned, inside measure-vs-price.

## Build order — do not durabilize first

The load-bearing sequencing insight: get the reservation semantics correct on the **in-memory**
ledger, *then* move behind the durable trait. Durabilizing the current model unchanged regresses
correctness (Context #3).

| Phase | Scope | Gate |
|---|---|---|
| **0** | Identity fields + `billable()` (D4, D7) + the `LedgerView`/`Reservation` **trait** (D5, in-memory impl unchanged) + security quick-wins (constant-time admin compare, dashboard auth, model charset validation). | Additive, no behavior change. |
| **1** | Ceiling reservation + mid-stream cutoff + Partial-on-disconnect + idempotent settle (D1, D2, D3) **on the in-memory ledger**. | Caps hold single-node. |
| **2** | Data plane (ADR-0004 D1 / TD-0006 raw forwarder). | Drop-in + cache promise true. |
| **3** | Durable/shared ledger behind the Phase-0 trait (D2, D5, D6) + token-bucket rate limits + async at-least-once sink. | Caps hold across restart + replicas. |
| **4** | Policy engine + trust (TD-0005 hardened). | Sits on a correct ledger. |

## Consequences

- **Positive:** the Tier-2 cap actually holds (single node in Phase 1, distributed in Phase 3);
  crash-safety, idempotency, and replica-consistency are designed in rather than retrofitted;
  metering latency leaves the hot path; budget and usage-event numbers reconcile; the ledger is
  swappable in-memory→SQLite→Redis behind one contract; agent workloads (fan-out, retries, long
  streams, cost trees) are first-class.
- **Cost:** a mid-stream cutoff means the proxy must count output deltas even on the transparent
  plane (cheap — a running integer, no full parse); the ceiling reservation can reject calls that
  *might* fit but whose worst case does not (correct for `Block`, tunable per policy); the durable
  atomic reserve adds a per-request store round-trip on the `Block` hot path (mitigated by
  Redis-Lua sub-ms + `Warn`-eventual + the fallback cache).
- **Boundary preserved:** ceilings, leases, cache-split dimensions, idempotency keys, and trace
  identity are all neutral units / metadata — no dollars, tiers, or SKUs.

## References

- [ADR-0004](0004-two-plane-proxy-and-enforcement-boundary.md) (two-plane + trust tiers; D3
  refined here)
- [TD-0005](../td/TD-0005-declarative-policy-engine.md), [TD-0006](../td/TD-0006-two-plane-proxy-transparent-metering.md)
- Prior art: Redis Lua token-bucket, Envoy ratelimit RLS (central decision + fail-open shadow),
  Stripe idempotency-keyed usage records, Apache Ranger PDP/PAP, OPA/Cedar (deny-by-default +
  decision logs).
