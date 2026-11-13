# Public CI remote-policy verdict failure

The recovery checkpoint `21005ab` passed twelve public CI jobs, including vendor
SDK/dashboard conformance, coverage and packaging. Its
[Rust job](https://github.com/anvai-labs/sandhi/actions/runs/43221583750)
failed in `cargo test --workspace --features policy-onnx`: the first healthy
evaluator case of `remote_findings_and_failures_hold_zero_provider_calls`
returned 503 instead of 403. The aggregate gate failed. Documentation passed.
The [structural artifact](artifacts/public-ci-policy-verdict-2026-11-12.json)
records all fourteen actual GitHub-hosted Ubuntu runner allocations. No private
or overflow runner was used.

The failed assertion preceded response-body, evaluator-call, provider-call and
receipt assertions. The public log therefore does not establish those counts.
This differs from the retained accounting admission-wait failure; neither its
cause nor this policy failure's exact phase is proven by this log.

The fixture used a 150 ms remote-evaluation cap inside a 200 ms outer gate budget.
The gate also includes blocking-worker scheduling and the mandatory durable audit
commit. A deadline or audit failure correctly denies inference with 503. An
independent controlled probe of the published source returned 403 for an
immediate valid blocking score and 503 for the same score delayed 180 ms, with
one evaluator call and zero provider calls in both controlled cases. This shows
a plausible failure path, not the cause of the public failure.

The corrected semantic fixture deliberately delays its healthy reply 250 ms and
sets explicit bounded test budgets: 2 seconds for the remote and 5 seconds for
the outer gate through the existing deadline-floor seam. The delayed fixture
fails with the original caps, then passes with those semantic budgets. It still
requires exactly 403 for a blocking score, exactly 503 for an unavailable
evaluator, one evaluator call, zero provider calls, a receipt header and one
durable audit row. Bounded failure diagnostics contain status, policy code,
elapsed time, receipt presence, counts and response length; no prompt or response
body is logged. The zero-provider assertion runs before the verdict assertion.

Separate tight deadline, saturation, expired-request and cooldown tests retain
their original budgets. Production policy deadlines and fail-closed behavior
are unchanged. All eleven runnable remote-policy tests pass with the ONNX
feature; the external Flask deployment case remains opt-in. This corrects the
test's coupling of semantic verdicts to latency; it does not resolve the open
spontaneous accounting-wait diagnosis or substitute for the next public CI run.
