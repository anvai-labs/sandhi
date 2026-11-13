# M1 acceptance boundaries

This historical milestone accepted an engineering baseline while keeping usability,
live broker integration and deployment recovery as separate qualification gates.
The [v0.6.0 release](https://github.com/anvai-labs/sandhi/releases/tag/v0.6.0)
was published and artifact-verified; that outcome does not certify production use.

## Acceptance decision record

| Area | Accepted engineering boundary | Separate deployment evidence |
| --- | --- | --- |
| Usability | Automated API/browser checks | Observed operator completing tasks without coaching |
| Budgets | Estimated neutral-token admission with original usage settlement | Any stricter token, monetary or workload-specific guarantee |
| Broker integration | Synthetic Sandhi boundary and isolated component tests | Actual platform/version interoperability and grant lifecycle |
| Recovery | Operator-controlled quarantine and reconciliation | Named recovery owner and current policy/revocation source |
| Performance | Measured single-node synthetic baseline | Workload-specific latency, capacity and recovery objectives |
| Publication | Reviewed source and every declared target verified | Managed installation, provider acceptance and operational readiness |

These limits constrain claims; they do not grant deployment authority or change
review and release gates. For the maintained publication procedure, use
[release safeguards](release-safeguards.md) and [Releasing Sandhi](../../RELEASING.md).

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

## Deferred observed-user session — P01, required before production

The reviewer should attempt each task before reading the expected result. Record assistance
needed rather than quietly coaching every step into a pass. Use synthetic credentials only.

| Task given to the reviewer | Ask afterward | Pass condition / finding to record |
|---|---|---|
| Register an existing reference, mint a scoped key, make a request and find its usage/run | Who does this usage belong to, and which key is usable? | Correct attribution and safe key handling; record confusion or missing navigation |
| Apply a budget that blocks the next request, then explain a recovery action | Was an upstream call made? Can earlier/in-flight usage exceed the admission estimate? | Correct denial/estimate explanation; no suggestion of a strict monetary bound |
| Recover a locked or missing/denied reference | Should you unlock, correct the reference, request a grant or paste a secret? | Correct distinct action; no fallback/unlock performed by the gateway |
| Review a restored snapshot with a later revocation and possible forced exit | Can traffic resume solely because readiness is green? | No; identify current policy authority, quarantine and uncertain consumption |

Record: reviewer/role, tested build, date, task outcomes, assistance, confusion/blockers, explicit acceptance/hold scope. Do not include tokens, keys or vault contents.
Usability, broker interoperability and recovery ownership require their own deployment acceptance.
A screenshot review alone is
an inspection, not an observed unassisted usability session; label it accordingly.
