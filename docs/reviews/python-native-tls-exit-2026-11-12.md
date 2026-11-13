# Unresolved native Python exit after a TLS rejection

The M5v full binding run recorded **146 passed, one failed**. In
`test_private_ca_preserves_chain_and_hostname_checks[True-127.0.0.1-transport_error]`,
the isolated child printed the expected `transport_error` for a hostname mismatch,
then exited with signal 11 (`returncode=-11`) rather than normal termination.
Stderr was empty; that run captured no native stack. The
[sanitized artifact](artifacts/python-native-tls-exit-2026-11-12.json) pins the
candidate/control outcomes and qualification limits.

This is failed native-process evidence. It does not establish that TLS verification
accepted the invalid hostname, or identify a cause in buffered accounting. The
reported rejection preceded process failure. No provider request, external key or
OS trust-store mutation was involved: the fixture uses a generated local CA,
loopback server and `synthetic-invalid` credential.

## Follow-up observations

- With `PYTHONFAULTHANDLER=1`, all three private-CA cases passed.
- The same candidate wheel and complete binding suite passed **147 tests** with
  fault tracing enabled. That later pass does not erase the earlier signal exit.
- Three probes per earlier/current wheel under GDB exited normally: six total.
- Twenty ordinary isolated probes per wheel with fault tracing also exited
  normally: forty total. No comparison run produced a crash or stack.

The earlier wheel is a control, not proof that it is immune. These observations
neither attribute the failure to the current delta nor establish that it is fixed.
Further occurrence-specific shutdown/native stack evidence is needed. Do not
ignore nonzero exits, introduce shutdown sleeps, or replace the expected TLS
failure merely to make the acceptance check green. The native-process reliability
gate remains open alongside the separately retained accounting waits.

## Final-wheel bounded debugger follow-up

A later independent investigation used the final buffered wheel, isolated module
path and a minimal constructed child environment. All **100 bounded GDB probes**
exited normally: 32 baseline, 32 Python debug-allocator, 32 glibc-perturbation and
four explicit loaded-extension path/hash checks. Each observed the expected TLS
rejection, return from `asyncio.run`, Python exit marker and native exit 0. No HTTP
request crossed the invalid-hostname boundary. At most three children ran at once;
no sleeps, forced collection, manual runtime shutdown or OS trust changes were
introduced.

The [sanitized probe artifact](artifacts/python-native-exit-debugger-probes-2026-11-12.json)
summarizes interpreter, harness and measured results. Stable temporary
native logs retain debugger events outside pytest rotation. No crash recurred, so
there is no occurrence-specific crash stack or core. Debugger, markers, environment
and module isolation alter timing relative to the failing full suite. These probes
establish non-reproduction only; the original signal exit remains unexplained and
does not receive a fix or reliability clearance from passing comparisons.
