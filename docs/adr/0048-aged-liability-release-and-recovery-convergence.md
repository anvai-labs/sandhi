# ADR-0048: Aged liability release and recovery convergence

- **Status:** Source candidate for the next feature release.
- **Owner:** Durable evidence and recovery in `sandhi-store` (`ledger/evidence.rs`) and the
  `sandhi-proxy` maintenance sweep.
- **Supersedes:** The unbounded-hold consequence of ADR-0043's "unknown dispatch or unqualified
  usage keeps both liabilities" — for durable execution intents whose admission age exceeds the
  retention window. Everything else in ADR-0043/0044 stands.
- **Context incident:** A production edge served `GET /readyz` → 503
  `durable_accounting_recovery_unresolved` for over a week. The recovery cursor counts every
  non-terminal durable execution intent (`MayHaveDispatched`, `MissingObservation`,
  `UnresolvedObservation`, `Prepared`) with **no age limit**, and the acknowledged-settlement
  consumer (W05e) was never built — so one bad day of body-capped streams holds readiness down
  indefinitely.

## Decision

1. **Retention-window release (default 24 h, `SANDHI_LIABILITY_RELEASE_AFTER_SECS`, 0 = off).**
   The 60 s maintenance sweep (the same task as the reclaim sweep) releases the held budget
   liability of every durable execution intent admitted at or before `now − retention` that still
   lacks final source evidence (`Prepared`, `MayHaveDispatched`, `MissingObservation`,
   `UnresolvedObservation`).
2. **Logical state, not archival.** Released rows are stamped in place
   (`budget_execution_intent.liability_released_at`) rather than moved to an archive table.
   Same effect (queries exclude them; readiness stops counting them), fewer moving parts (no
   cross-table coordination with `checked_terminal_receipt` or the shard-widening guard), and it
   matches the house rule that unpaired evidence is retained forever. Evidence columns are never
   modified or deleted — "move, never delete" (TD-0024 D3) as a filter instead of a migration.
3. **Release is a zero-charge policy act, not a settlement.** The reservation closes
   (`settled = 1, actual = 0, settled_at`) in the same transaction as the stamp, which frees the
   in-flight capacity (`SUM(ceiling) WHERE settled = 0`). No `budget_settlement_outbox` row is
   written, `charged_tokens` and `spent` are untouched, and no usage is invented: the recorded
   outcome is "released unresolved after retention". If the upstream later proves usage, that
   spend is knowingly uncounted — accepted after a full retention window, per operator policy.
4. **Classification precedence.** `RecoveryState::ReleasedAfterRetention` short-circuits the
   recovery pager *before* the receipt machinery — a released reservation is settled without a
   receipt, which `checked_terminal_receipt` would reject as `LegacySettlement`. The stamping
   transaction re-verifies the class before stamping, so a race with live settlement resolves to
   the receipt, never to a release; rows that settled or carry a settleable charge
   (`ReadyToSettle`) are skipped untouched, and recovery settles those normally.
5. **Readiness convergence.** `RecoveryState::ReleasedAfterRetention` is terminal for the
   recovery cursor: `recover_inner` stops counting released rows as unresolved, so
   `/readyz` returns to 200 once every intent is terminal — settled, closed, or released —
   and a full sweep completes. Fresh unresolved intents still block readiness exactly as before.

## Consequences

- The existing reclaim guard (`NOT EXISTS budget_execution_intent`) still protects released
  reservations from deletion, so released rows keep their evidence anchors indefinitely.
- The settlement outbox's settled-but-unacknowledged rows are unchanged by this ADR: they never
  gated readiness. Their ack consumer remains W05e future work (TD-0024).
- A released row can never settle afterward (`settle_terminal_durable` would find the
  reservation closed); the stamp is a one-way, operator-visible policy boundary, logged at warn
  level on every non-zero sweep.
