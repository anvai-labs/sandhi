# M1 acceptance decisions and evidence

Status: additional automated journey evidence passed locally; **no user decision or acceptance recorded**.
Tracker: [TD-0026](../td/TD-0026-gateway-product-evolution.md), C01f; prior
[workload and integration evidence](m1-acceptance.md) remains valid.

The question is not whether an automated test can impersonate an accepting user. It is which
claims the engineering evidence supports, which operating limits the owner accepts, and which
usability questions still need observation. Recommendations below are proposals, not approvals.
No choice silently changes the existing M1 gate, opens a promotion PR, publishes a package or deploys.

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

**Recommendation:** retain the existing gate, but make it a focused review by one named
developer/operator of the four journeys below. This is sufficient only for the defined M1
checkpoint; the proposed five-user/ten-minute onboarding study stays unvalidated.

**Rationale/evidence:** tests can prove an authorized operation worked and a denial had no
dispatch. They cannot prove that a person found the operation or understood what to do next.
The new stitched checks remove avoidable manual regression work, leaving only that judgment.

**Alternative requiring your explicit decision:** promote an engineering checkpoint on automated
evidence and defer observed usability acceptance to before production use. That revises the
current gate; it must be recorded as a revision, not as a completed user study or walkthrough.

**Decision to record:** retain the walkthrough gate and name the reviewer, or explicitly revise
its timing/scope. Generic permission to continue or merge does not select either option.

### UA02 — Are estimated neutral-token budgets acceptable for this checkpoint?

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

**Human decision:** accept that certification boundary, or require live certification before
main promotion. The latter needs an approved disposable daemon/build and test-vault setup;
personal vault access, unlock and grant changes are not authorized by this packet.

### UA04 — Who owns restored access policy and uncertain consumption?

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

**Recommendation:** accept a measured single-node engineering baseline, with no production
latency/capacity/RTO promise. Define deployment workload, thresholds, duration and hardware before
using a benchmark as a production acceptance gate.

**Rationale/evidence:** the 36-phase [workload artifact](evidence/m1-workload-2026-09-06.json)
retains per-phase distributions, scheduling lag and sampled CPU/RSS/FD observations. It has no
warmup, uses a synthetic local provider and a short accumulating history, and runs on a shared
host. No production SLO or leak-freedom conclusion follows. Readiness is lifecycle-only; queue
emptiness is not a commit receipt; shutdown exit 124 means cleanup was not proven complete.

**Human decision:** accept that scope for M1, or supply the target workload/SLO/recovery objective
that must pass first. A broader request changes the validation scope and must be planned explicitly.

### UA06 — What action should acceptance authorize?

**Recommendation:** once the selected gate is satisfied, authorize only the reviewed
`develop` → `main` promotion with fresh exact-head review/CI and post-merge verification.

**Rationale/evidence:** the cumulative review also includes the already-integrated protocol
0.8.1 update and npm bootstrap documentation. The latter documents publishing; it does not
authorize publishing. M1 is not all later accounting/fleet/broker work.

**Decision to record:** accept that promotion scope, identify any excluded change, or hold.
Tags, package publication and production rollout remain separate actions. Earlier merge bypass
authorization covers only a missing approving review after clean review/green CI, never checks.

## Minimal observed-user session (if UA01 is retained)

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
All decision states are **pending** until the owner supplies them. A screenshot review alone is
an inspection, not an observed unassisted usability session; label it accordingly.
