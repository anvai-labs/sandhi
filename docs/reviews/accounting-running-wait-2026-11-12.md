# Local terminal-settlement wait: worker running without publication

The corrected Responses source `eec5eaa` reproduced two failures in a full
`cargo test --workspace` run while the current-SDK suite ran in another process.
This run is not green. The
[sanitized artifact](artifacts/accounting-running-wait-2026-11-12.json)
retains the command, log hash, failed assertions and timing records.

`owned_buffered_back_leg_never_races_its_primary_settle` returned 502 on call 170
with automatic retry forbidden, one retained accounting job and a final usage
event containing ten input and two output tokens. Its printed upstream count of
170 includes earlier successful requests. The failure does not export this
request's paired reservation or receipt database join.

The Codex four-ingress test also observed zero settlement receipts where it
expected one. Its assertion does not print the failed cell's ingress, mode or
request ID. The adjacent second wait record cannot therefore be assigned to that
cell as a proven correlation.

Both unexpected timing records are for `terminal_settlement`: workers started
after 50 and 63 microseconds, then remained in `running` for about 2 seconds when
the unchanged HTTP wait expired. Neither had a return/publication timestamp;
readiness was pending. For the explicitly correlated back-leg failure,
`wait_elapsed_us=2001292`, `queue_us=63`, and `run_us=2001319`.

This narrows these occurrences to the execution interval after worker start.
It does not distinguish ledger work, known-busy retry/backoff, filesystem I/O or
thread descheduling. It does not establish the worker's later outcome or imply
that a zero debit or reservation release is safe. Four other queued admission
timeouts in the same log are deliberate late-admission recovery fixtures.

The failure remains a reliability/merge gate. Subsequent passing checks cannot
erase it. No deadline increase, inferred usage, replacement accounting owner or
inference replay was introduced. The next diagnostic step is bounded timing
inside the existing ledger execution path, including known-busy retry counts,
with request/transition correlation. No new transport or accounting authority is
needed. See the [operator timing guide](../operator/accounting-wait-diagnostics.md)
for the current marker meanings and limits.
