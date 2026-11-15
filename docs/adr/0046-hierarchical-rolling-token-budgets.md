# ADR-0046: Hierarchical rolling token budgets

- **Status:** Source candidate for the next feature release.
- **Owner:** Durable admission and budget metadata in `sandhi-store` and `sandhi-proxy`.
- **Supersedes:** The calendar-window part of ADR-0005 D5. Reservation atomicity, idempotent
  settlement, TTL liability and fail-closed behavior remain unchanged.

## Decision

A durable budget is identified by `(scope, window)`, not scope alone. A scope may independently
carry any combination of `hourly`, `daily`, `weekly`, `monthly` and `total` neutral-token caps.
The singular `limit_tokens` plus `window` operator shape remains compatible and addresses one row
of that hierarchy.

All fixed windows are trailing intervals, measured from the authoritative store clock:

| Window | Enforcement interval |
| --- | --- |
| Hourly | `(now - 1 hour, now]` |
| Daily | `(now - 24 hours, now]` |
| Weekly | `(now - 7 days, now]` |
| Monthly | `(now - 30 days, now]` |
| Total | All settled history |

There is no calendar reset event and no mutable reset counter. Admission at `now` sums rows by
their immutable settlement time; consumption leaves the applicable interval continuously as time
advances. A `monthly` cap means a fixed trailing thirty days, not a calendar month.

Admission checks every configured hard window in one SQLite immediate transaction. Each window
uses its own settled sum plus the scope's shared in-flight reservation ceilings. Any breaching
`block` window refuses before a reservation is inserted; `warn` windows do not refuse. Unknown
paired usage remains held and does not disappear at a window boundary. The existing single
reservation/settlement receipt continues to charge all applicable front and upstream scopes once.

The legacy one-row-per-scope `budget_limit` table is migrated in place to a composite
`(scope, window)` primary key and existing rows are preserved. Updating one window does not delete
another window; declarative apply remains additive. Operator metadata groups all rows by scope,
while list/usage responses expose the full `windows` array and retain the shortest configured
window in the legacy top-level fields for older clients.

Alert thresholds are evaluated for every configured window with that window's limit and spend. A
scope's existing threshold rule can therefore fire on its hourly utilization even when its monthly
utilization remains low.

## Qualification boundary

The release gate must prove all of the following:

- a legacy one-window database migrates without losing its cap, policy or spend;
- multiple windows for one scope persist, restart and list in deterministic hierarchy order;
- hourly, daily, weekly and monthly sums exclude progressively older immutable settlements;
- capacity returns one hour after hourly consumption without a UTC-day boundary;
- a longer window remains binding after a shorter window rolls;
- concurrent reservations cannot oversubscribe any hard window;
- a `warn` hierarchy admits over its cap while still recording spend and threshold evidence;
- singular API/config/CLI requests continue to behave exactly as before.

This remains a single-node SQLite enforcement contract. Multi-replica admission, calendar-month
semantics, monetary budgets and strict total-token guarantees are non-goals.
