# Admission store diagnostics

The [M5t failed workspace run](accounting-running-wait-2026-11-12.md#subsequent-admission-occurrence)
placed a spontaneous two-second admission wait inside the store's reserve call.
This increment adds bounded process-local markers inside that existing operation,
from shard acquisition through context validation, transaction start, reservations,
row insertion and commit. It adds no SQL, durable rows, locks, dispatch authority,
retry, deadline or durability changes.

The [operator guide](../operator/accounting-wait-diagnostics.md) defines the labels
and their limits. A label records entry; it cannot prove a writer lock, fsync,
successful commit or cause of a failure. Individual atomic reads may span a
transition. Uninstrumented operations stay `unobserved`, and other owned-work
boundaries clear the prior admission marker.

## Evidence

- A real SQLite writer-lock fixture observed `begin_transaction`, no early
  reservation and a pending original call. Releasing the writer produced the same
  correlated admission, two reservations, no receipt and a Prepared dispatch fence.
- A real context-insert trigger failure rolled back every admission row.
- The owned proxy fixture produced one sanitized timeout log with the actual store
  stage, retained its original ticket, cleared the marker after return and closed
  the same pair before dispatch with no charge. A separate focused pass checks
  the review-requested stale-stage assertion.
- The full workspace passed **1,223 tests**, with six opt-in ignores across
  52 suites. Default and ONNX/SentinelPass all-target Clippy, formatting and strict
  MkDocs passed. The [artifact](artifacts/admission-store-diagnostics-2026-11-12.json)
  records the test-only assertion timing distinction and measured results.

The spontaneous stall did **not** recur in this run. These controlled fixtures
qualify diagnostic behavior, not the cause of the previous admission or terminal
settlement failures. Both retained reliability occurrences remain merge gates.
No inference was replayed to repair accounting and no deadline was widened.

For a future unexpected back-leg fixture failure, the test now retains only its
synthetic temporary database directory and prints its path with the failed request
ID. This permits a later request-specific reservation/receipt join after the
original worker returns. It does not copy production state or make the last
printed usage event authoritative for another request. This passing run produced
no retained failure database.
