# TD-0024: Reservation retention and rollup — bounded ledger history

- **Status:** Draft (proposed). Owns **G31** in the [ADR-0006](../adr/0006-layer-boundary-and-protocol-scope.md)
  gap register. No phase is scheduled yet; this document exists so the next session touching the
  ledger's storage finds the design already reasoned.
- **Relates to:** [ADR-0005](../adr/0005-enforcement-correctness-reservation-ledger-observe-enforce-split.md)
  (D1 leases / D2 idempotent settle / D5 calendar windows — the invariants any retention scheme
  must preserve exactly), [TD-0016](TD-0016-enforcement-throughput-ceiling.md) (the throughput
  sibling; this is the *storage-side* ceiling), [TD-0021](TD-0021-co-design-seam-v2-proxy-path-contract.md)
  (G20 idempotent metering — shares a migration window, see D3),
  `crates/sandhi-store/src/ledger.rs`, the 2026-09-01 design audit (finding D1).

## Why this exists

Every admission inserts a `budget_reservation` row that is **never deleted**. The only deletes
are unsettled-and-expired (the lease reclaim). The per-request reads are two SUMs over the
scope's rows: settled `actual` within the window, and unsettled `ceiling`. For `Window::Total`
the settled SUM scans **every admission ever made to that scope, on every request**.

The covering index shipped with the design-audit hygiene PR removed the per-row table fetch
(`EXPLAIN QUERY PLAN` now reads `USING COVERING INDEX`), but the scan itself still grows linearly
with lifetime admissions — at 10 admissions/s that is ~864k rows per scope per day, in the
database file and in every reserve's index range. The design audit classified the growth as the
one HIGH data-structure finding the throughput TD does not already own: TD-0016 makes each
commit *cheap* (group commit, sharding); nothing makes the history *bounded*.

## Design

**D1 — hour-bucketed rollups, windows preserved by construction.** A
`budget_rollup(scope TEXT, hour_start INTEGER, actual INTEGER, PRIMARY KEY (scope, hour_start))`
table accumulates settled spend per whole UTC hour. Every window ADR-0005 D5 defines is a sum of
whole hours: `Daily` starts at UTC midnight, `Monthly` at the first of the month 00:00, `Total`
at 0 — so `spent(window) = rollup-sum(hours >= window_start) + live SUM(reservations settled in
the still-open hours)`. No window can ever disagree with the live rows it was folded from.

**D2 — sealing only complete hours, late settles adjust.** The existing reclaim sweep
(`reclaim_sweep_at` cadence) gains a sealing pass: settled rows whose hour is complete and older
than a grace period (TTL-sized, so a retried settle lands in the live table) are folded into the
rollup and deleted. A settle arriving for an already-sealed hour writes an adjustment row keyed
to that hour — idempotent settle-by-id (ADR-0005 D2) is unchanged; only its storage location
moves.

**D3 — share the migration window with G20.** `usage_events` has no primary key, which
forecloses the `UNIQUE(request_id)` upsert TD-0021 G20 will want. Both are store-schema changes
with the same operational shape (create-then-backfill-then-swap reads); doing them as one
migration PR halves the operator-facing churn. If G20 moves first, this TD follows its
conventions.

**D4 — non-goals, explicitly.** No change to lease semantics, ceiling math, the
observe/enforce split, or `synchronous=FULL` (rejected in TD-0016 D2 — durability is not the
bargaining chip). No backend selection or HA (TD-0007). The rollup is an internal storage
detail: no API, wire type, or dashboard surface changes.

## Phases

| Phase | Deliverable | Gate |
|---|---|---|
| P1 | Longevity bench: synthetic N-day history in `benches/ledger.rs`; measure reserve latency + DB size vs lifetime rows (before/after) | Numbers recorded here; no design lands before them (the TD-0015 rule) |
| P2 | Rollup table + sealing pass + window-equivalence tests (calendar boundaries, DST-free UTC arithmetic, late settle after seal, crash between fold and delete) | Every existing ledger test green unmodified; new equivalence tests pin `spent(window)` across seal boundaries |
| P3 | Operator surface: retention/env knobs documented (`SANDHI_RESERVATION_SEAL_AFTER_SECS`); dashboard shows rollup-vs-live provenance | Docs-only |
