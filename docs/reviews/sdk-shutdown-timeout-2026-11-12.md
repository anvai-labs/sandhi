# SDK shutdown timeout with retained unknown usage

The M5v vendor SDK/dashboard/CPU-origin run finished **563 passed, one failed,
three skipped**. The failed cell was
`test_tracked_binary_correlation_and_shutdown[streaming-unresolved-tls]`.
Its provider deliberately omitted usage. The TLS client observed the expected
stream failure, but after SIGTERM the proxy did not exit within the test's existing
six-second wait. The expected unresolved-cleanup exit was 124. This run is not a
green SDK qualification. The failure traceback shows that the process had returned
124 by traceback rendering/teardown; it does not establish a permanent hang or
capture the later exit timestamp.

The [sanitized artifact](artifacts/sdk-shutdown-timeout-2026-11-12.json) pins the
full log and observed stderr hash. Six identical copied binaries belong to separate
backend fixtures; the shutdown test launched the session fixture's shared target
binary, whose occurrence-time executable hash was not captured. The
original pytest temporary stderr/database directory was no longer present during
the later audit; the sanitized join and chronology were exported before cleanup.
Request
`req_1794506155941_0` joins to its original execution, physical attempt, terminal
observation and both reservations:

| Evidence | Observed result |
| --- | --- |
| Physical source | Attempt ordinal 1, HTTP 200, source terminal `success` |
| Canonical usage | `unavailable`, attempts 1, frozen charge absent |
| Primary reservation | Ceiling 34, actual placeholder 0, unsettled |
| Upstream reservation | Ceiling 34, actual placeholder 0, unsettled |
| Settlement receipts | None |
| Request usage rows | One |

Those zeros are unavailable/unsettled placeholders, not measured zero usage or
released liability. The test asserted one upstream call before SIGTERM; its final
post-shutdown assertion was not reached. The source's success record does not make
its absent counters qualified or prove successful client delivery.

The log records shutdown cutoff with about five seconds remaining, connection
draining about 1.25 seconds later, then incomplete tracked cleanup with one retained
job. It does not identify the subsequent blocked phase or provide a native stack.
Do not assign the timeout to SQL, runtime destruction, exit handlers or scheduling
without further evidence, or conflate it with earlier accounting waits.

The binary's watchdog currently calls `std::process::exit`. The
[Rust standard-library contract](https://doc.rust-lang.org/std/process/fn.exit.html)
states that this runs registered exit handlers and serializes concurrent Rust exit
calls. That identifies a hard-stop contract risk to test separately; no stack ties
it to this occurrence. The current timeout remains a reliability/merge gate. No
wait increase, inferred charge, replacement accounting owner or inference replay
was introduced.
