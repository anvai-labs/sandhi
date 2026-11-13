# Policy-worker verdict fixture and current documentation

Base `18077bc2c9c6c79e0e49e9ee3ac503681fb2c7f7` passed all native-platform jobs
and all 735 public SDK tests (three skips). Its Rust OpenTelemetry lane failed in
`python_worker_failure_and_findings_hold_zero_provider_calls`: normal policy denial
returned 503 rather than the expected 403. The aggregate failed; no merge or release
was performed. The prior native continuation case passed every executed matrix.

A controlled actual Python-worker fixture delays only the normal response by
350 ms. Under the original 200 ms gate it reproduces 503; under the existing
five-second semantic-test floor it returns 403. All seven modes have zero provider
POSTs and zero reservation, intent, physical-attempt and receipt rows. Redaction
assertions remain unchanged. This reproduces a failure mode; the exact original
CI timeout stage/cause is not established by the generic response.

The correction changes **only this positive semantic test case** to use the existing
`PolicyGate::with_deadline_floor`. All six unavailable/error modes retain their
original zero floor and 200 ms document deadline. Expected statuses, zero-dispatch
assertions, strict deadline/startup/saturation tests and every production timeout
remain unchanged. No runtime implementation or retry behavior changes.

The whole corrected target passes locally in both default and the failing
OpenTelemetry lane: seven passed and one existing pinned-ML fixture ignored in
each. The [artifact](artifacts/policy-worker-verdict-fixture-2026-11-13.json) pins
the controlled results and public source context. Full new exact-tip
public CI remains a required gate; the earlier failed run is retained.

The same checkpoint aligns current operator, recovery, metering, subscription,
README and internal architecture guidance with 0.17. Ordinary persistent storage,
one serving shard, original paired evidence/holds, explicit source fidelity and
owned proof work replace stale optional-store/multi-shard/fail-open/lossy/TTL-release
instructions. Ledger-free FFI, in-memory rate limits and offline historical shard
handling remain distinct. Historical ADR/review/release/phase records retain their
meaning behind clear labels. Retained legacy telemetry cannot enable removed paths
or establish source receipt health. Three unchanged curl credential placeholders
were replaced with shell environment references; the complete current guide now
passes the generic secret scan without suppression. The original owner/configuration
stays authoritative.

This is a source/test/documentation checkpoint, not proof of protected promotion,
package publication or a running deployment. The owner's conditional 0.17 release
still requires green checks, the protected promotion path and all-target verification.
