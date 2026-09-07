# M1 acceptance decisions and evidence

Status: **UA01–UA05 accepted** for this release; hands-on usability acceptance deferred until
before production. UA06 remains pending; no observed-user or live-integration pass is recorded.
Tracker: [TD-0026](../td/TD-0026-gateway-product-evolution.md), C01f; prior
[workload and integration evidence](m1-acceptance.md) remains valid.

The question is not whether an automated test can impersonate an accepting user. It is which
claims the engineering evidence supports, which operating limits the owner accepts, and which
usability questions still need observation. Recommendations below are proposals unless explicitly
accepted in the decision record.
No choice silently changes the existing M1 gate, opens a promotion PR, publishes a package or deploys.

## Acceptance decision record

Authority: explicit user instruction in this working session, 2026-09-07 UTC. Evidence baseline:
`develop` at `cf469bcbd06679109d0803b3ad47f66a77e4ac51`, including C01f and its green post-merge CI.

| Decision | State | Recorded outcome |
|---|---|---|
| UA01 — acceptance method | Accepted | Use automated engineering evidence for this release; explicitly defer hands-on usability acceptance until before production |
| UA02 — budget guarantees | Accepted | Accept the existing estimate-based token budgets for this release; no strict final-token or monetary-cap guarantee |
| UA03 — broker assurance | Accepted | Accept current synthetic Sandhi boundary and isolated broker-component evidence for this release; require live interoperability and grant-lifecycle validation before production use of that integration |
| UA04 — recovery responsibility | Accepted | Accept operator-controlled quarantine and reconciliation for this release; assign the recovery owner, authoritative policy source and cutover/rollback rules before deployment |
| UA05 — performance scope | Accepted | Accept a measured single-node engineering baseline without production throughput, latency, recovery-time or fleet-capacity promises |
| UA06 — promotion/release scope | Pending | Next decision: authorize the safeguarded v0.6.0 unified milestone release versus promotion only with publication deferred |

This is an explicit revision of the acceptance timing for this release, not completion of the
original observed-user gate. The automated evidence is accepted as the release acceptance method;
the operating-limit decisions UA02–UA05 are accepted. Promotion/publication scope (UA06) remains
to be confirmed. Existing review/CI requirements are unchanged. M1 release acceptance is not
yet closed as a whole.

Deferred gate **P01 — hands-on usability before production** remains open. Before production use,
assign an accepting operator and record the tested build, all four journey outcomes, assistance,
confusion and any blocking corrections using the session below. Operator/results are unassigned
and not performed; publication or a green build does not satisfy P01. The proposed five-user
study remains separately unvalidated.

Deferred gate **P02 — live broker integration before production use of that integration** remains
open under UA03. Validate Sandhi with the actual broker in an isolated setup using disposable
credentials; record the component versions, platform, interoperability and grant-lifecycle
outcomes. Current synthetic boundary tests and isolated broker tests do not satisfy this gate.
No live compatibility, grant-lifecycle, desktop or browser-to-broker connector certification is
claimed. UA03 selects when validation is required; it does not authorize access to a personal
vault, production grants or production deployment.

Deferred gate **P03 — recovery ownership and procedure before deployment** remains open under
UA04. Before production deployment, name the recovery owner and authoritative post-snapshot
policy/revocation source, and record cutover and rollback rules. For each restore, keep traffic
isolated until the operator verifies integrity, accounting, current access policy and revocations;
if current policy cannot be established, remain quarantined. Record uncertain consumption rather
than inferring zero loss from readiness. Owner/source/rules are not yet assigned. Accepting this
operating model does not perform a restore, approve traffic cutover, promise automatic fleet
recovery or establish lossless accounting.

## What automation can settle

The prior integrated baseline passed 4,608 synthetic requests, exactly reconciled 3,072 gateway
events and 546,600 neutral tokens, and verified denial and shutdown outcomes. Hosted SDK tests
passed 233 cases; two optional sibling-browser checks passed locally. Recovery drills include
fixed single-file/two-shard state, real SIGKILL leases and post-snapshot revocation reconciliation.
See the linked acceptance record for exact source/build identities and pre/post-merge CI.

The additional slice connects previously separate checks into complete operator journeys:

The clean source-pinned full suite passed **251 tests with zero skips** in 211.29 seconds,
including all three SDKs and both optional AgentBrowser integrations. The linked packet retains
nine masked PNG/JSON pairs and final JUnit; 18 additional pinned broker-source tests passed.
Remote integration status is tracked separately in C01f, not inferred from local results.

| Journey | Automated question | Evidence from this slice |
|---|---|---|
| First useful request | Does the exact key minted in the served browser authorize a synthetic call, with correct subject/group/session/run/step in persistent evidence and displayed totals? | Browser/API/SQL assertions and masked synthetic screenshots |
| Budget intervention | Does a cap committed through the browser deny a real request before dispatch, without changing usage/leases, and permit recovery after an explicit cap change? | Cap/spend values, 429/no-dispatch deltas, recovery result |
| Broker failure/recovery | Does the browser distinguish locked, missing and denied references, show safe recovery copy, and register only the exact authorized reference without a secret write? | Canonical visible copy, HTTP outcome and synthetic broker operation assertions |
| Restored accounting | Do AgentBrowser's accessible attribution/budget rows match restored API/SQL evidence, not merely the model/scope labels? | Exact scoped row comparisons and overview card DOM-value/label checks; card visibility is not established by this API |

Screenshots are synthetic, masked review aids, not proof of general secret redaction. JSON
journey evidence is not a substitute for final pytest/JUnit results, including fixture teardown.
None of this measures a new user's comprehension, confidence, unassisted completion time or
screen-reader experience. Chromium automation is not a cross-browser accessibility certification.

The stitched first-request journey exposed an implementation defect: `/admin/usage/run/{id}`
returns a `run` envelope, while the dashboard read its fields at the top level. The fix consumes
the real envelope, checks the requested run identity and rejects missing, negative or unsafe
counts. Browser regressions cover a valid persisted run, escaped hostile step labels, a missing
run and ten malformed-response cases. These checks establish rendering behavior, not whether
the run view is easy to discover or its own/subtree distinction is understandable.

The [masked screenshots and assertion records](evidence/m1-decisions-2026-09-07/README.md)
make the owner review concrete. In particular, recovery captures show a new success confirmation
while the earlier four-second error toast remains visible; denied-reference copy states the
denial without spelling out grant-request steps. Decide through the focused review whether
that feedback is clear enough for this checkpoint or needs a UX correction before acceptance.

## Decisions for the owner

### UA01 — What acceptance standard should gate main promotion?

**Recorded decision (2026-09-07 UTC):** the user selected automated engineering evidence for
this release and explicitly deferred hands-on usability acceptance until before production (P01).

**Original recommendation (not selected):** retain the existing gate, but make it a focused review by one named
developer/operator of the four journeys below. This is sufficient only for the defined M1
checkpoint; the proposed five-user/ten-minute onboarding study stays unvalidated.

**Rationale/evidence:** tests can prove an authorized operation worked and a denial had no
dispatch. They cannot prove that a person found the operation or understood what to do next.
The new stitched checks remove avoidable manual regression work, leaving only that judgment.

**Selected alternative:** promote an engineering checkpoint on automated
evidence and defer observed usability acceptance to before production use. That revises the
current gate; it must be recorded as a revision, not as a completed user study or walkthrough.

The explicit user decision above selects this alternative. UA01 alone does not approve other
choices or establish observed-user results; UA02 and UA03 were subsequently accepted separately.

### UA02 — Are estimated neutral-token budgets acceptable for this checkpoint?

**Recorded decision (2026-09-07 UTC):** the user explicitly accepted the existing estimate-based
token budgets for this release. This accepts the disclosed admission/settlement contract; it
does not add a strict mode, change configured policies or extend guarantees to monetary caps.

**Recommendation:** accept the disclosed estimate-based admission contract for M1, with `Block`
for enforced rejection and `Warn` only where over-budget admission is intentional. Do not describe
either as a strict measured-total or monetary cap.

**Rationale/evidence:** [metering guarantees](metering-and-budget-guarantees.md) and the concurrent
under-reservation regression demonstrate that multiple admitted calls can settle above their
reservations. The workload reconciles actual categories rather than clamping them to estimates.
Browser denial automation establishes the committed-policy/no-dispatch path, not a universal bound.

**Human verification:** after seeing the warning and a denial, can the reviewer explain what is
blocked, what can still overshoot, and which action is appropriate? If the actual use case needs
a strict total bound, hold that use case for the model/request-bound work; no strict mode exists
to enable today. The neutral-measurement boundary remains the existing architecture decision.

### UA03 — What broker assurance is required before promotion versus deployment?

**Recorded decision (2026-09-07 UTC):** the user selected the recommendation: accept the current
synthetic Sandhi boundary tests and 18 isolated SentinelPass contract tests for this release,
with live interoperability and grant-lifecycle validation required before production use of
the integration (P02). The live-validation-before-release alternative was not selected.

**Recommendation:** use exact-reference onboarding through the bounded native read-grant path;
accept synthetic protocol evidence for the explicitly scoped engineering checkpoint, while
requiring live compatibility/grant-lifecycle certification before claiming production integration.

**Rationale/evidence:** [broker tests and contract](broker-integration-contract.md) cover
locked/missing/denied/timeouts, no automatic unlock or plaintext fallback, and separate local
disablement versus broker/provider revocation. Synthetic grants do not exercise a real daemon.
The sibling SentinelPass checkout is absent, but an isolated public-source checkout at
`00d1e7de09e3d360954240be7588dd7c73ac317e` passed 18 broker-side IPC/grant tests. This adds
real Unix-socket source-contract coverage, not Sandhi interoperability certification: that
workspace is 0.8.2 while Sandhi embeds protocol 0.8.1. AgentBrowser's in-memory secret registry
is not a SentinelPass connector. See the [probe record](evidence/m1-decisions-2026-09-07/README.md).

**Selected boundary:** current evidence is sufficient for this release's broker-assurance
decision, not for production integration certification. P02 remains open; personal vault access,
unlock and grant changes are not authorized by this packet.

### UA04 — Who owns restored access policy and uncertain consumption?

**Recorded decision (2026-09-07 UTC):** the user selected the recommendation: operator-controlled
recovery with quarantine and explicit reconciliation is acceptable for this release. A recovery
owner, authoritative policy source and cutover/rollback rules must be established before
deployment (P03). The alternative of requiring stronger automated recovery guarantees before
release was not selected.

**Recommendation:** accept manual quarantine and explicit reconciliation for the single-node
checkpoint. Name an operator, an authoritative post-snapshot revocation/policy source, and a
traffic-cutover/rollback rule before any deployment recovery.

**Rationale/evidence:** [recovery drills](../operator/recovery-drill.md) prove committed rows and
held leases survive the tested failures. An old snapshot can resurrect a later-revoked key.
Readiness is not integrity, credential health or permission to restore traffic. Forced exits
and best-effort observations do not establish complete physical-attempt liability or RPO=0.

**Human verification:** ask the reviewer what happens when the current revocation record is
unavailable. The safe answer is to keep quarantine, not infer permission from a ready process.
If automatic fleet recovery or complete uncertain-consumption reconstruction is mandatory,
keep that deployment blocked on W05/W12 rather than relabeling these tests as those guarantees.

### UA05 — What operational/performance promise is being accepted?

**Recorded decision (2026-09-07 UTC):** the user selected the recommendation: release as a measured
single-node engineering baseline, without production throughput, latency, recovery-time or fleet
capacity guarantees. The alternative of requiring deployment-specific performance targets and
validation before release was not selected. Before making a later performance promise, define
the target workload, hardware, thresholds and test duration and validate those claims; the
existing synthetic observations are not that validation.

**Recommendation:** accept a measured single-node engineering baseline, with no production
latency/capacity/RTO promise. Define deployment workload, thresholds, duration and hardware before
using a benchmark as a production acceptance gate.

**Rationale/evidence:** the 36-phase [workload artifact](evidence/m1-workload-2026-09-06.json)
retains per-phase distributions, scheduling lag and sampled CPU/RSS/FD observations. It has no
warmup, uses a synthetic local provider and a short accumulating history, and runs on a shared
host. No production SLO or leak-freedom conclusion follows. Readiness is lifecycle-only; queue
emptiness is not a commit receipt; shutdown exit 124 means cleanup was not proven complete.

**Selected boundary:** accept the stated scope for this release. No target workload, production
SLO or recovery-time objective was supplied or certified. A broader performance promise changes
the validation scope and must be planned explicitly.

### UA06 — What action should acceptance authorize?

**Recommendation now proposed for explicit approval:** release **v0.6.0** as the M1 engineering
milestone with the accepted limitations. Integrate the acceptance record through a focused
`develop` PR; validate publisher configuration and harden the publishing entry points using
narrowly scoped release-tag protection and publisher-environment restrictions. Preserve required
CI and existing branch protections. Then promote `develop` → `main` after clean cumulative
review and fresh exact-head CI, verify post-merge CI, tag the verified commit, publish the planned
GitHub binaries, PyPI, four Rust crates and npm/platform packages, verify actual artifacts, and
back-sync `main` into `develop`. Production deployment is excluded. Stop for owner action if
registry-side authorization cannot be established; do not silently omit a failed target.

**Alternative:** authorize promotion only under the same review/CI gates, with tagging and all
package publication deferred to a separate decision. This preserves the engineering checkpoint
without activating publishers yet.

**Rationale/evidence:** the cumulative review also includes the already-integrated protocol
0.8.1 update and npm bootstrap documentation. The latter documents publishing; it does not
authorize publishing. M1 is not all later accounting/fleet/broker work.

**Decision to record:** the approved promotion/publication scope, version, targets and narrowly
scoped publishing-protection work, or any exclusions/hold. None of these new publishing actions
is approved by accepting UA01–UA05 alone. Earlier merge bypass authorization covers only a
missing approving review after clean review/green CI, never checks. Production rollout remains
separate and subject to P01–P03 and the applicable deployment validations.

### Release-readiness preflight — 2026-09-07 UTC

Read-only checks continued while collecting acceptance decisions. These results are not a tag,
publication approval or proof that the next OIDC publish will succeed.

| Check | Observed evidence | Remaining action |
|---|---|---|
| Integrated engineering baseline | `develop` at `cf469bc`; [post-merge CI](https://github.com/anvai-labs/sandhi/actions/runs/34080462032) passed | Fresh full-diff review and CI still required for promotion |
| Last registry release | `python3 scripts/verify-release.py v0.5.1` exited 0: PyPI, all four crates and the main npm package present | Verify the next actual version after publication; presence is not installability certification |
| Native/platform artifacts | GitHub lists Linux x64 and macOS arm64 v0.5.1 archives; both corresponding npm platform packages report version 0.5.1 | Test newly built artifacts for the next release |
| Historical npm failure | [Repair run 33740352186](https://github.com/anvai-labs/sandhi/actions/runs/33740352186) failed with E404 on the Linux platform package; that package is now present | Do not mistake the old failure for current absence, or current presence for proven OIDC configuration |
| Publisher configuration | GitHub metadata lists `CARGO_REGISTRY_TOKEN`; `npm` and `pypi` environments exist | Token validity and registry-side trusted-publisher bindings were not verified; secret values were not read |
| Publishing protections | `npm`/`pypi` environment API reports no deployment branch policy and no protection rules; repository rulesets query returns an empty list | Review tag/dispatch authorization and approve appropriate publishing restrictions before the tag; no settings changed and no claim of a complete organization-policy audit |

UA06 should settle the release version/targets and the publishing-protection work as well as
promotion scope. Recommendation: retain the planned unified release targets, validate publisher
configuration and harden the publishing entry points before tagging. An explicitly narrower
release scope would require documented workflow/verification changes, not a silently skipped
publisher. Do not rerun an old publish workflow merely to test credentials.

## Deferred observed-user session — P01, required before production

The reviewer should attempt each task before reading the expected result. Record assistance
needed rather than quietly coaching every step into a pass. Use synthetic credentials only.

| Task given to the reviewer | Ask afterward | Pass condition / finding to record |
|---|---|---|
| Register an existing reference, mint a scoped key, make a request and find its usage/run | Who does this usage belong to, and which key is usable? | Correct attribution and safe key handling; record confusion or missing navigation |
| Apply a budget that blocks the next request, then explain a recovery action | Was an upstream call made? Can earlier/in-flight usage exceed the admission estimate? | Correct denial/estimate explanation; no suggestion of a strict monetary bound |
| Recover a locked or missing/denied reference | Should you unlock, correct the reference, request a grant or paste a secret? | Correct distinct action; no fallback/unlock performed by the gateway |
| Review a restored snapshot with a later revocation and possible forced exit | Can traffic resume solely because readiness is green? | No; identify current policy authority, quarantine and uncertain consumption |

Record: reviewer/role, tested build, date, task outcomes, assistance, confusion/blockers, chosen
UA01–UA06 outcomes and explicit accept/hold scope. Do not include tokens, keys or vault contents.
UA01–UA05 are accepted; UA06 and pre-production gates P01–P03 remain **pending**.
A screenshot review alone is
an inspection, not an observed unassisted usability session; label it accordingly.
