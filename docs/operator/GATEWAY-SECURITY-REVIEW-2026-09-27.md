# Gateway consolidation review — 2026-09-27

Candidate only: no production activation, IdP mutation, credential reset or real
subscription inference is part of this review.

## Reviewer path

1. `crates/sandhi-proxy/src/auth.rs`, its negative tests, and store `vkeys.rs`:
   trusted subject/group resolution, bounded delegation, durable subject grants,
   shared ownership/budget scope, expiry and revocation.
2. `crates/sandhi-core/src/policy.rs` and `tests/policy.rs`: strict JSON/text
   extraction, verified selectors, bounded evaluators, local checks before remote
   text egress, and terminal decisions. Rust owns generated schema/facade output.
3. Proxy `policy.rs`, admission in `lib.rs`, store `policy.rs`: authorize before
   provider dispatch, deny on evaluation/mandatory receipt failure, content-free
   durable receipts. Subscription credential leases expire server-side.
4. `policy_workers.rs`, `policy_onnx.rs`, `policy_remote.rs` and their tests:
   artifact provenance, deadlines, finite capacity, cleanup and no remote replay.
5. `templates/python-evaluator`: one listener per server/container, named model
   URLs, bearer authentication/TLS/mTLS, killable children and immutable release
   preparation. Read `DEPLOYMENT.md` for network and cgroup boundaries.

## Review and checks

An independent companion-client review examined Sandhi identity, authorization,
delegation, policy extraction/selectors/admission and subscription leases. It found
no concrete authorization/policy bypass in those sections. The parent review
examined evaluator/service/container boundaries and matched the published claims
to the dated smoke evidence. Trusted native/Python extensions are not a hostile
code sandbox; finite deadlines are admission bounds rather than hard real-time
guarantees for every native operation.

Local consolidated checks: 799 Rust tests passed, 10 optional/environment tests
ignored; strict Clippy with the optional ONNX feature; 89.63% line coverage;
27 template tests; 19 Python lease/ML/export tests; schema/facade drift and
actionlint checks passed. The PR body records the tested immutable commit and
binding-workspace results. CI now compiles/tests the optional feature and checks
Python exports in addition to real TLS and hosted-Linux container smoke tests.

Previously recorded container smoke verified non-root execution, zero effective
capabilities, no-new-privileges, read-only root, noexec tmpfs, PID refusal, CPU
throttling, OOM killing, two model URLs, mTLS and zero default published ports on
Linux/arm64. Target deployment validation is still required; published networking
does not itself deny egress. These were synthetic local tests, not a production
load/quality claim.

## Merge versus rollout

One authoritative gateway remains required for budgets/rate limits. Alert delivery,
receipt-to-provider-outcome/model joins, retention/export and production detector
quality remain follow-ups. Durable subject keys require explicit revocation.
The companion Victor client now propagates terminal policy denials across runtime
recovery and graph execution. Buffered non-policy transport failures may still
trigger a new, separately admitted completion; there is no universal exactly-once
agent-runtime guarantee. This does not weaken the remote evaluator no-replay rule.

No production message-hub classify cutover is included. Deploy the matching
gateway and client artifacts only after credential renewal, actual provider
compatibility, privacy routing and latency acceptance. The synthetic detector
and local MLflow experiments establish an extension testbed, not production DLP.
