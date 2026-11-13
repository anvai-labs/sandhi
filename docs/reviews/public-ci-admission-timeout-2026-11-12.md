# Public CI admission-wait failure

*Request identifiers are aliases; recorded measurements are unchanged.*

The SDK checkpoint `45b2ae1` passed public SDK/dashboard conformance, coverage,
packaging and documentation. Its [public Rust job](https://github.com/anvai-labs/sandhi/actions/runs/43208267099/job/131148979877)
failed during the SentinelPass feature run. Twelve jobs succeeded; Rust and the
aggregate gate failed. The checkpoint is not green for merge.

The test `tracked_typed_streams_join_all_ingress_formats_to_source_usage_and_paired_receipts`
received HTTP 502 instead of 200 in `messages_implicit/gemini/malformed`. The log
immediately preceding the assertion reports tracked admission `wait_timeout` for
`example-request-1`. The original suffix was a gateway request counter, not an inference
number. The public log does not establish an origin-call count or a durable
attempt/receipt join for the failed cell.

The [sanitized structural artifact](artifacts/public-ci-admission-timeout-2026-11-12.json)
retains the public source/job context, observed failure and public
runner labels. All fourteen jobs used GitHub Actions runners with
`ubuntu-24.04`/`ubuntu-24.04-arm`. This identifies the tested platform boundary.

This matches the failure shape of the earlier local admission-wait failure; it
does not prove a common cause. The retained local dispatch-authorization failure
is also unresolved. No blind CI retry, deadline increase or liability release was
used to hide the failure. Needed evidence remains bounded request-correlated
queue, worker and publication timings during a failed run. A later passing run
alone cannot close this gate.
