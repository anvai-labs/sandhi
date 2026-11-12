# Accounting wait diagnostics

Tracked requests keep their existing bounded accounting waits. On a timeout,
Sandhi logs `tracked accounting wait unresolved` with the gateway `request_id`,
the accounting transition and process-local worker timings. Join the request ID
to `x-sandhi-request-id` and any available durable attempt/receipt evidence.

The transition is `admission`, `dispatch_authorization`, `pre_dispatch_close` or
`terminal_settlement`. Each transition has its own timing record and epoch-bound
ticket. The same request may have more than one transition.

```mermaid
flowchart LR
    Submit[Submitted] -->|queue_us| Start[Blocking worker starts]
    Start -->|run_us| Return[Operation returns or worker interrupts]
    Return -->|publication_us| Publish[Result and notification published]
    Publish --> Waiter[Existing ticket waiter]
```

| Field | Meaning |
| --- | --- |
| `job_stage` | `queued`, `running`, `publishing` or `published` at observation |
| `elapsed_us` | Wall time since submitting this transition |
| `queue_us` | Time until the blocking callback starts, or time observed waiting; an unstarted interrupted callback ends this interval |
| `run_us` | Callback wall time, including ledger operations and existing known-busy retry/backoff; absent before callback start |
| `publication_us` | Time after callback return/interruption through registry publication and notification; absent before return |
| `published_at_us` | Completed publication offset from submission, when observed |
| `wait_started_us` | This HTTP wait's start offset from submission |
| `wait_elapsed_us` | Wall time since this HTTP wait started |
| `wait_ms` | Configured accounting wait budget |
| `interrupted` | The existing worker guard published uncertainty after interruption; storage may already have committed |
| `readiness` | Nonblocking registry observation: `pending`, `available`, `stale_or_collected`, `registry_busy` or `registry_poisoned` |

The timings use a monotonic clock with microsecond precision. They describe wall
time, including scheduler delays. A record is local to its ticket and worker; no
global per-request timing map, background task or durable row is introduced.
The readiness probe uses a try-lock, so logging can report registry contention
without waiting for that registry lock.

An illustrative running-operation timeout could contain:

```text
request_id=req_example transition=admission reason=wait_timeout
wait_ms=2000 wait_started_us=Some(50) wait_elapsed_us=2005000
job_stage=running elapsed_us=2005050 queue_us=60 run_us=Some(2004990)
publication_us=None published_at_us=None interrupted=false readiness=pending
```

This example is synthetic. `Option` fields use the log formatter's `Some(...)` /
`None` notation. Actual text/JSON formatting follows the configured subscriber.

## Interpreting a failure

A long queued interval points investigation toward blocking-pool or thread
scheduling. A long running interval needs correlation with ledger contention,
existing backoff, SQLite and storage timings. A long publication interval needs
registry/notification investigation. Publication and wait-start offsets help
place observed completion relative to the configured wait. The fields are
separate observations: publication can race with timeout handling and a readiness
probe. A `published` / `available` snapshot alone cannot establish a lost wakeup.

Durable physical attempts and settlement receipts remain authoritative. Missing
timings, interrupted work or an absent immediate database row never prove that
inference did not happen. Liability, retry policy and dispatch authorization
continue to follow the existing fences. Diagnostics do not change deadlines,
retry accounting ambiguity, replay inference or release reservations.

## Qualification and open reliability work

Controlled tests occupy the only blocking thread, gate a real durable admission,
and hold the result registry after operation return. They distinguish queue,
execution and publication, verify a correlated warning while the registry is
busy, and retain exactly one admission/result. Queued cancellation retains its
existing interrupted evidence with no callback execution.

The [public admission-timeout failure](../reviews/public-ci-admission-timeout-2026-11-12.md)
and earlier local admission/dispatch waits remain unresolved. The instrumentation
is intended to capture the missing phase evidence during a failure. Passing
stress runs without a timeout do not identify its cause or close that gate.
