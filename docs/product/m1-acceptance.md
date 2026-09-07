# M1 workload and operator acceptance

Status: automated engineering evidence accepted as this release's acceptance method (UA01);
hands-on usability deferred until before production. Remaining release decisions are pending.
Owner/tracker: [TD-0026](../td/TD-0026-gateway-product-evolution.md), C01e/W06d.

The follow-up [decision packet](m1-acceptance-decisions.md) separates further automatable journey
checks from owner choices, with evidence, rationale, recommendations and an observed-user script.
The explicit user decision recorded in UA01 on 2026-09-07 UTC revises the timing of the hands-on
gate for this release; it does not claim an observed-user pass or close the other decisions.

M1 is a trustworthy **single-node baseline**, not the complete gateway roadmap. W01–W04,
drain-aware readiness, best-effort operational visibility and isolated recovery are its inputs.
The workload evidence below complements those tests. It cannot establish usability on behalf
of an operator or authorize a production deployment.

## Automated workload contract

The real proxy talks only to a separate synthetic HTTP/1.1 provider process over loopback.
A separate driver sends four gateway lanes: transparent Gemini and translated OpenAI ingress,
each unary and paced SSE. Direct-provider unary/SSE runs use the same provider family and
corpus. They are a comparison baseline, not a measurement of a previous gateway revision or
a justified subtraction of provider time from gateway latency.

The local reference profile has 32 scoped synthetic tenants, three repetitions, 128 requests
per phase, concurrency 8 for closed-loop traffic, and a fixed 50 arrivals/second profile with
a bounded pending set. The three usage variants independently exercise fresh/cache input
and zero, smaller-than-output, and larger-than-output separate reasoning counts. Request size
has a deterministic 10:1 skew. SSE has 16 content frames with configured 2 ms spacing.

For each gateway phase, acceptance requires:

- Every offered request completes successfully; timeouts, bad response content/categories and
  generator overflow invalidate the run. Fixed arrivals retain scheduling lag and scheduled-to-
  completion latency instead of silently becoming a closed-loop workload.
- Exactly one persisted event per request/step, correct subject/group/run binding, exact usage
  categories, and independently matching enforcement spend per tenant and run totals.
- No remaining unsettled lease, no observational queue backlog/drop at the sampled checkpoint,
  and the expected forwarding-plane metric and upstream request deltas. SQL/API reconciliation,
  not empty queues alone, proves the tested accounting.
- Untimed unauthorized and exhausted-budget checks do not dispatch or create observations;
  normal proxy shutdown exits 0.

Reports record latency/TTFB/first-content distributions, scheduled arrival lag, offered/completed
rates, CPU deltas, sampled RSS/FD peaks, repeat variance and platform/config/binary identity.
CPU resolution and resource sampling limitations are explicit. No latency SLO, improvement,
sustained capacity, memory-allocation result, long-history behavior or full TD-0015 completion
is inferred. There is no warmup; driver connections restart between phases while provider pool
and ledger history accumulate. Shared-host contention and small-sample tails can be noisy.

The small pytest profile is an accounting/corpus CI gate, not a performance regression threshold.

```bash
cargo build -p sandhi-proxy --bin sandhi-proxy
python -m pytest tests/sdk-conformance/test_workload_acceptance.py -q
python tests/sdk-conformance/workload_acceptance.py \
  --binary target/debug/sandhi-proxy \
  --output target/m1-workload.json \
  --summary-output target/m1-workload-summary.json
```

Use the full report for per-request evidence and retain its exact binary digest. A caller-supplied
revision is not cryptographic build provenance; the artifact labels it accordingly. Never run
this harness with real credentials, arbitrary remote provider endpoints or production storage.
The compact summary keeps distributions, independent accounting, resources, configuration and
the canonical full-report digest while omitting individual request samples. It is not a substitute
for retaining the full report. Distinct output paths are required. Optimized Python (`-O` or
`PYTHONOPTIMIZE`) is refused so it cannot strip the acceptance assertions.

Independent review corrected missing streaming terminal validation, unexpected ledger-scope
charges and optimized-Python false passes. Negative tests also cover malformed successful HTTP
payloads, generator overload, missing metrics and evidence-output aliases.

### Recorded baseline — 2026-09-06

The [compact evidence](evidence/m1-workload-2026-09-06.json) records a passing run against
integrated W06c `develop` revision `8ae84019d7dd5919ea5d242a31c67befa6ff0a4c`. The locally observed
build command was `cargo build --locked -p sandhi-proxy --bin sandhi-proxy` (unoptimized dev,
default features), using Rust 1.98.0. The harness itself does not verify that build provenance;
it records the executable SHA-256 and marks the supplied revision unverified. Harness revision
`dfdca27736591e3b862c85448232a62a39523c71` was clean when the run began.

| Observation | Result |
|---|---|
| Lanes/profiles/repeats | Six lanes × two profiles × three repeats = 36 phases |
| Completed synthetic requests | 4,608 of 4,608; no generator rejection or request error |
| Gateway accounting | 3,072 events across 32 tenants; 546,600 neutral charged tokens |
| Final checkpoints | No unsettled leases; queue/drop assertions passed; denied calls did not dispatch |
| Shutdown and elapsed run | Exit 0; 66.28 seconds including setup/checks/cleanup |
| Combined local regression suite | 227 passed, one unavailable Google SDK skipped; includes real AgentBrowser smoke |
| Hosted PR regression suite | 233 passed, two optional sibling-browser tests skipped; no sibling checkout in hosted CI |

The summary retains every phase distribution and resource observation. Raw per-request evidence
was retained locally as `/tmp/sandhi-w06d-final.json` (canonical digest in the summary) and independently
reviewed; it is not a repository-hosted artifact. Reproduce with the command above to retain a new
full report in a durable evidence store. Raw SQL/metric checkpoints are asserted by the reviewed
harness, not copied into the report. These are observations of one small synthetic profile,
not a measured production RTO, leak-freedom proof or throughput commitment. Earlier exploratory
runs are not substituted for this integrated baseline.

### Integration evidence

| Slice | Reviewed head and merge | Latest-head CI | Post-merge CI |
|---|---|---|---|
| W06c recovery | [PR #233](https://github.com/anvai-labs/sandhi/pull/233): `7440dab` → `8ae8401` | [34058455291](https://github.com/anvai-labs/sandhi/actions/runs/34058455291), passed | [34059776510](https://github.com/anvai-labs/sandhi/actions/runs/34059776510), passed |
| W06d automation | [PR #234](https://github.com/anvai-labs/sandhi/pull/234): `948e7af` → `324ba87` | [34060286384](https://github.com/anvai-labs/sandhi/actions/runs/34060286384), passed | [34060985111](https://github.com/anvai-labs/sandhi/actions/runs/34060985111), passed |

Both merges followed clean independent review and green latest-head CI on public hosted runners.
Only the missing approving review was bypassed under explicit user authorization; no failed or
pending check was bypassed and no protection was changed. W06d merged after W06c post-merge CI
passed. Private-route mirrors were skipped and are not used as validation evidence.

## Actual-user review — deferred until before production (P01)

The original gate required this review before M1 promotion. On 2026-09-07 UTC, the user explicitly
accepted automated engineering evidence for this release and deferred hands-on usability
acceptance until before production. See [UA01 and P01](m1-acceptance-decisions.md).

This gate is distinct from permission to merge a green PR. The accepting developer/operator
must review the normal and failure journeys and report their observed result. A scripted browser
test or a prepared walkthrough is not evidence of an actual user's experience.

| Journey | Review task | Evidence/result to record |
|---|---|---|
| First useful request | Identify the configured provider reference, create a scoped virtual key, send a synthetic request and locate its attributed usage | Operator identity/role, build, observed outcome, confusion or missing next action; optional observed time |
| Budget denial | Set a committed scope budget, trigger a denial, explain its next action and compare UI/API state | Denial understood without confusing estimated admission with a strict monetary cap; no upstream dispatch for the denied call |
| Locked broker | Attempt reference registration with a locked synthetic broker, distinguish locked/denied/missing, recover the exact authorized reference | No secret exposure or fallback, correct recovery action, explicit difference between metadata and secret authority |
| Recovery and limits | Review the W06c quarantine/revocation procedure and known forced-exit uncertainty | Accept/reject the single-node operating limits; identify any blocker before traffic cutover |

Record findings and corrections before signing off. The proposed five-user/ten-minute onboarding
target in the vision document remains unvalidated unless that study is actually performed; one
operator's review must not be reported as that study.

Accepting operator: **pending assignment before production**. Observed user results: **not performed**.
UA01: **accepted timing revision**. UA02: **existing estimated token budgets accepted**.
UA03: **current component/synthetic broker evidence accepted for this release**; live interoperability
and grant-lifecycle validation required before production use of the integration (P02).
UA04: **operator-controlled recovery accepted for this release**; recovery owner, authoritative
policy source and cutover/rollback rules remain required before deployment (P03).
UA05: **measured single-node baseline accepted without production performance promises**.
Remaining release decision UA06: **pending**;
M1 release acceptance is not yet complete as a whole.

## Promotion gate

W06c and W06d land through focused `develop` PRs with clean adversarial review and green latest-head
and post-merge CI. Only after the recorded M1 release acceptance decisions close under the
explicit UA01 timing revision may a `develop` → `main` PR
promote this checkpoint. Review the full promotion diff and verify the resulting `main` CI.
No tag, package publication, production rollout or later roadmap guarantee is implied.

The prospective promotion review compared `main` at `72ced4b` with acceptance head `948e7af` and
found no new release blocker in the cumulative diff/contract review, building on the completed
per-slice adversarial reviews. It checked accounting propagation and disclosed migrations,
configured-store/readiness compatibility, and intact CI/security gates. The promotion also
carries the already-integrated protocol 0.8.1 update (PR #227) and npm bootstrap documentation
(PR #226). This was not an exhaustive new audit of every line, production certification, or
permission to skip actual-user acceptance. UA01 subsequently deferred that acceptance until
before production; it remains unperformed. Recheck the exact promotion head and CI when the
remaining release decisions close; no `main` promotion PR has been opened for this checkpoint.
