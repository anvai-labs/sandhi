# TD-0026: Gateway product evolution and delivery plan

Status: In progress — review and planning complete; M1 implementation underway.
Date: 2026-09-04
Baseline: `ed1781e` (Sandhi); source review, not a release certification.

## Mandate

Work backward from a coherent product vision to user journeys, requirements, architecture,
contracts, implementation slices, and executable acceptance gates. Cover developer and operator
UX, operational tracking, security, usage and cost controls, reliability, and ecosystem integration.
Co-design credential lifecycle and shared capabilities with SentinelPass and AgentBrowser. Keep proposed behavior
distinct from shipped behavior and preserve ownership of existing ADRs and TDs.

This is the persistent activity tracker. Product implementation was authorized on 2026-09-04;
marking a planning task complete does not mark its implementation complete.

## Activity tracker

| ID | Activity | State | Evidence / next action |
|---|---|---|---|
| A01 | Inventory vision, designs, contracts, implementation, and tests | Complete | Evidence register F01–F16 links findings to source, impact and acceptance |
| A02 | Research gateway capability and guarantee expectations | Complete | Primary documentation inspected on 2026-09-04; translate findings into independent product requirements |
| A03 | Review SentinelPass integration and reciprocal opportunities | Complete | Adapter plus public broker source at `00d1e7de` inspected; SP0–SP4 proposed; live daemon validation remains an implementation gate |
| A04 | Define working-backward vision, journeys, and success measures | Complete | J01–J06 and R01–R13 drafted; self-hosted-first and existing pricing boundary used as stated defaults, not user sign-off |
| A05 | Produce evidence register and target architecture | Complete | Confirmed behavior, gaps and validation hypotheses distinguished; component/contract/failure ownership defined |
| A06 | Sequence implementation with dependencies and release gates | Complete | W01–W14 below map requirements, findings, owners, dependencies and acceptance |
| A07 | Validate baseline and review documents | Complete | Workspace and IPC-feature tests passed; whitespace/content checks and 68 local documentation links passed across five files |
| A08 | Extend co-design to browser execution and joint smoke coverage | Complete | AgentBrowser inspected and rebuilt at `dec28b388`; AB01 real-engine synthetic smoke passed; AB02–AB05 broker/evidence/REST contracts remain proposed |

## Tracking rules

- States: Pending, In progress, Blocked, Complete, Deferred. Record the concrete reason for a block.
- Each delivery slice records owner role/repository, dependencies, acceptance evidence, and status.
- Update this file and the documentation index together when lifecycle status changes.
- Update owning TDs when implementation lands; this umbrella does not supersede accepted ADRs.
- Estimates describe effort ranges after dependencies clear, not committed delivery dates.
- Cross-repository designs are proposals until both contract implementations and compatibility
  checks exist. Never treat a published message type as proof of daemon authorization behavior.
- A slice marked Complete below is implemented and locally verified, not necessarily merged or
  released. Track remote CI, integration and publication separately in the checkpoint table.

## Integration and release checkpoints

| Checkpoint | Scope | Local verification | Remote CI / review | Integrated | Released |
|---|---|---|---|---|---|
| C01 | W01–W04 and inactive W05a storage foundation; branch `feat/gateway-trust-checkpoint` | Passed; clean scoped re-review, 103 SDK/browser tests, 37 upgraded Python tests, 94.12% binding coverage | [PR #230](https://github.com/anvai-labs/sandhi/pull/230) merged with explicit owner-authorized review bypass; pre- and [post-merge CI](https://github.com/anvai-labs/sandhi/actions/runs/34015573003) green | Complete: `8f56b91` on `develop` | No |
| C01b | W06b best-effort buffer visibility; branch `feat/operational-buffer-visibility`, updated from C01 | Passed: 105 SDK/browser tests, native workspace tests, 87.18% coverage; current-base adversarial re-review clean | [PR #231](https://github.com/anvai-labs/sandhi/pull/231) merged as `f777b89671c4b52854cb6c527dd03fa01a7ada52`; latest-head CI `34029737562` and post-merge CI `34030218505` passed; authorized admin review bypass only | Complete | No |
| C01c | W06a drain-aware readiness; branch `feat/drain-aware-readiness` | Passed: native workspace, OTLP proxy tests, 87.76% native coverage, 113 SDK/browser tests; independent review finding fixed and re-reviewed clean | [PR #232](https://github.com/anvai-labs/sandhi/pull/232) merged as `31151d9`; latest-head CI `34042875545` and post-merge CI `34048329909` passed; authorized missing-review bypass only | Complete | No |
| C01d | W06c recovery drills; branch `test/recovery-drills` | Passed: 181 SDK/browser tests (one unavailable SDK skipped), default/native/OTLP tests and clippy, 87.77% native coverage; adversarial findings fixed and independently re-reviewed clean | [PR #233](https://github.com/anvai-labs/sandhi/pull/233) merged as `8ae8401`; latest-head CI `34058455291` and post-merge CI `34059776510` passed on public hosted runners; authorized missing-review bypass only | Complete | No |
| C01e | W06d workload/operator acceptance; branch `test/m1-acceptance` | Automation passed: 227 local SDK/browser tests (one unavailable SDK skip), 233 hosted SDK tests (two optional sibling-browser skips), 36-phase integrated workload and clean independent review; [evidence and actual-user gate](../product/m1-acceptance.md). Accepting operator/results still pending | [PR #234](https://github.com/anvai-labs/sandhi/pull/234) merged as `324ba87`; latest-head CI `34060286384` and post-merge CI `34060985111` passed on public hosted runners; authorized missing-review bypass only | Automation verified and integrated; human acceptance pending | No |
| C01f | Operator decision evidence and run-view correction; branch `test/operator-decision-evidence` | 251 SDK/browser tests, zero skips; Rust tests/clippy/fmt passed; nine masked journey pairs plus JUnit; 18 pinned broker-component tests | [PR #236](https://github.com/anvai-labs/sandhi/pull/236) is the live record for exact-head review, CI and post-merge verification | Follow PR merge state; source-pinned local evidence remains immutable | No; UA01–UA05 accepted, UA06 pending |
| C01g | Release safeguards and accepted engineering scope; branch `docs/m1-release-acceptance` | 207 safeguard tests passed, zero skips; isolated local binary smoke passed; independent review clean; SG06 remote ref controls verified; [SG01–SG08 tracker](../product/release-safeguards.md) | [PR #237](https://github.com/anvai-labs/sandhi/pull/237) open; hosted JSON-depth failure fixed, updated public CI pending | Pending | No; SG07 credential closure and final execution gate open |
| C02 | M1 engineering-release acceptance under UA01–UA05, then `develop` → `main`; hands-on UX deferred to P01 before production | UA01–UA05 accepted; SG06 ref controls applied; UA06 final execution and SG07 credential gate pending | Pending promotion PR and post-merge CI | Pending | No; tagging/publishing is a separate action |

C01f follow-up: `test/operator-decision-evidence` closes the automatable links between browser
onboarding, real requests, budget denials, broker recovery and restored numeric evidence. Source
`2b4788b` passed 251 SDK/browser tests with zero skips in a clean isolated Python environment;
nine masked PNG/JSON pairs and final JUnit are in the [decision packet](../product/m1-acceptance-decisions.md).
Eighteen isolated pinned broker-source tests add component evidence, not live interoperability.
The exposed dashboard run-envelope mismatch is corrected with malformed/unsafe-response and
escaped-label regressions. Independent source re-review is clean after closing false-pass gaps;
remote review/CI, merge and post-merge verification are recorded in [PR #236](https://github.com/anvai-labs/sandhi/pull/236).
The PR's live state is authoritative for integration, separate from this source checkpoint.
On 2026-09-07 UTC the user explicitly accepted automated engineering evidence for this release
and deferred hands-on usability acceptance until before production (UA01/P01). This revises C02's
acceptance timing, not the evidence: no observed-user pass exists. The user subsequently accepted
the existing estimate-based token budgets (UA02) and current component/synthetic broker evidence
(UA03) for this release. Live interoperability and grant-lifecycle validation remain required
before production use of the integration (P02). The user also accepted operator-controlled
recovery (UA04); recovery ownership, current policy authority and cutover/rollback rules remain
required before deployment (P03). The user accepted the measured single-node baseline without
production performance promises (UA05). Promotion/publication scope (UA06) remains pending.

C01 checkpoints completed work now instead of waiting for W05–W14. Do not claim M1 complete
or promote C02 until initial W06 evidence and the remaining M1 release acceptance decisions exist
under UA01. P01 still gates production. Preserve required reviews,
environment approvals and branch protections; no CI bypass or automatic publication is implied.

## Decision log

| ID | Working decision | State |
|---|---|---|
| D01 | Lead with self-hosted teams and provide an explicit path to fleet operation | Provisional; preference requested |
| D02 | Preserve neutral measurement in Sandhi; price and reconcile money downstream | Existing ADR-0001 boundary; preference requested on any expansion |
| D03 | SentinelPass owns provider-secret custody and grant lifecycle; Sandhi owns AI request admission and usage | Proposed; audit both sides before finalizing |
| D04 | Fix trust and operational correctness before expanding protocol breadth | Proposed sequencing |

## Review outputs

- [Checkpoint adversarial review](../reviews/checkpoint-adversarial-review-2026-09-05.md):
  confirmed regressions, corrections, re-review evidence and explicit remaining gates.
- [Evidence register and codebase review](../reviews/gateway-review-2026-09-04.md): F01–F16,
  baseline validation and capability disposition.
- [Vision, journeys, requirements and architecture](../product/gateway-vision-and-requirements.md):
  J01–J06, R01–R13, component seams, accounting and security invariants, proposed success measures.
- [SentinelPass co-design](../upstream/sentinelpass-gateway-codesign.md): reciprocal capabilities
  S01–S06, existing trust boundary, proposed contracts, failure behavior and SP0–SP4 delivery.
- [Browser, gateway and vault co-design](../upstream/browser-gateway-vault-codesign.md):
  component boundaries, secret resolution, correlated evidence and AB01–AB05 joint delivery.

## Delivery tracker

Implementation states are tracked per slice below. Effort ranges are rough engineer effort after dependencies
clear, excluding external release lead time. Owners below are responsible roles/repositories;
individual assignment and capacity are not assumed. Every slice needs a reviewable change,
verification evidence and an operator-facing release note before its status becomes Complete.

| ID | Delivery / traceability | Owner | Depends on | Estimated effort | Acceptance / evidence to attach | State |
|---|---|---|---|---|---|---|
| W01 | Truthful authenticated dashboard; R01, F01/F06/F07 | Sandhi proxy/UI maintainer | Baseline | 3–5 days | Embedded assets, shared auth, explicit data states, script-safe actions and fallible reads; 18 browser/fault cases plus AB01 smoke pass; workspace coverage 87.54%, native IPC-feature 172 tests pass; operator guide/changelog updated | Complete |
| W02 | Committed management writes; R02 foundation, F02 | Sandhi operator/store maintainer | Baseline | 3–5 days | Complete: failed commits preserve live metadata; strict budget input; explicit config/inline-alert partial outcomes; 20 real HTTP/SQLite/restart/concurrency/UI/CLI regressions pass plus signed-cap store test; release/operator notes updated | Complete |
| W03 | Metering semantics and guarantee correction; R03, F03/F04 | Sandhi core/providers maintainer | Baseline | 1–2 weeks | Explicit reasoning inclusion and minor-7 schemas; legacy totals preserved; parser/plane/store/ledger/binding corpus and 24 end-to-end cases pass; [estimated-reservation capability matrix](../product/metering-and-budget-guarantees.md) and concurrent overshoot regression | Complete |
| W04 | Safe broker integration and onboarding; R07/R13, F08/F09, SP0/SP1 | Sandhi store/proxy + SentinelPass protocol owners | Baseline | 1–2 weeks | Native real-handler reproduction fixed; bounded runtime/queue, explicit capabilities, read-only API/CLI/browser registration and 16 broker scenarios pass; [integration contract](../product/broker-integration-contract.md). Live daemon certification remains SP4; generations/revocation cutoff remain W09 | Complete |
| W05 | Attempt accounting and durable evidence; R03/R05/R11, F05/F06 | Sandhi core/store + downstream consumer owner | W02/W03; accounting-contract review | 2–4 weeks | W05a complete: atomic settlement receipts, bounded/fenced delivery claims and 14 focused tests; [substep contract](../product/attempt-accounting-and-evidence.md). W05b–e retain physical-attempt capture, proxy integration, recovery, export and consumer review gates. A settlement receipt is not an upstream attempt | In progress |
| W06 | Operational readiness and recovery; R05/R12, F12/F13 | Sandhi operations/proxy maintainer | Baseline; W05 for authoritative backlog | 1–2 weeks | W06a/b/c and W06d automation verified and integrated. UA01 accepts engineering evidence for this release; remaining release decisions pending. Hands-on usability deferred until before production, not performed. See TD-0020 and checkpoint gates | In progress |
| W07 | Scoped management, audit and egress security; R06/R10, F07/F11/F15 | Sandhi security/API + control-plane owner | W01/W02; identity-boundary review | 2–4 weeks | Cross-scope CRUD/query denial, mutation audit, bounded/redacted egress and webhook delivery, auth/session threat model, failure-injection evidence | Pending |
| W08 | Declarative policy and hierarchical usage controls; R02/R04/R08, F10/F16 | Sandhi core/store + policy consumer owner | W02/W03/W05/W07 | 2–4 weeks | Reconcile TD-0005; durable revision/conflict preconditions, shadow/effective policy, signed freshness, all-scope/window atomic reservation, rate/token/concurrency tests, explainable denial | Pending |
| W09 | Credential generations and revocation; R07, F09, SP2 | Sandhi credential manager + SentinelPass owners | W04/W05/W07; broker contract release | 2–3 weeks | Bounded new-dispatch cutoff, generation switch/drain, restart and missed-event recovery, broker outage/expiry matrix | Pending |
| W10 | Operator investigation and cost evidence; R01/R05/R11, J02/J03/J04/J06 | Sandhi UI/API + pricing consumer owner | W05/W06/W07; W08 for full policy view | 2–3 weeks | Paginated/filterable timeline, reserved/uncertain/final quantities, data freshness; downstream price-version reconciliation; complete user drills | Pending |
| W11 | Governed routes and bounded guardrails; R09/R10, F14/F15 | Sandhi providers/proxy maintainer | W03/W05/W07/W08; measured workload | 2–4 weeks | Concurrent half-open breaker, eligible routes, retry accounting, affinity/residency restrictions, bounded stream/content inspection and false-positive evaluation | Pending |
| W12 | Fleet and long-lived storage guarantees; R04/R12, F10/F13 | Sandhi store/operations maintainer | W05/W06/W08; TD-0007 backend decision | 3–6 weeks | Two-replica admission races, partition/failover behavior, clock/TTL recovery; bounded history and rollup equivalence; restore and upgrade drills | Pending |
| W13 | Reciprocal posture and lifecycle release; R07/R13, S03/S04/S05, SP3/SP4 | Sandhi + SentinelPass owners | W07/W09/W10; scoped metadata ADR | 2–4 weeks | Authorized metadata projection, usage freshness, zero cross-domain reuse inference, old/new protocol and Unix/Windows matrix, joint runbook | Pending |
| W14 | Optional monetary admission and protocol breadth | Sandhi + relevant consumer owners | W05/W08/W10; named demand; relevant ADR gates | Spike 2–5 days each; implementation estimated after decision | Monetary reservation/compensation design; separate modality/MCP/duplex/H2 admission evidence; no scope admitted by checklist alone | Deferred |

Critical dependency chain for governed operation:

```mermaid
flowchart LR
    W02[Committed writes] --> W05[Durable attempt evidence]
    W03[Correct metering] --> W05
    W01[Trustworthy dashboard] --> W07[Scoped management and audit]
    W05 --> W08[Policy and hierarchical admission]
    W07 --> W08
    W04[Safe broker boundary] --> W09[Credential lifecycle]
    W05 --> W09
    W07 --> W09
    W08 --> W12[Fleet guarantees]
    W09 --> W13[Shared posture and release]
    W10[Investigation and cost evidence] --> W13
```

The table is authoritative for dependencies; the diagram shows the major chains only. W01–W04
and initial W06 measurements can proceed independently with adequate staffing. Within a shared
worktree or small team, land them sequentially to avoid overlapping proxy/core edits. This plan
does not assume parallel agents or staffing that has not been assigned.

## Milestones and review checkpoints

### Authorized M1 completion sequence (2026-09-06)

Timing amendment, 2026-09-07 UTC: UA01 explicitly replaces the pre-promotion hands-on requirement
below for this release with accepted automated engineering evidence. Hands-on acceptance is
deferred, not completed, and remains required before production (P01). Other release decisions,
fresh review/CI and publication controls are unchanged. The original sequence is retained here
for audit context.

1. W06c/C01d: stop disposable writers, snapshot/validate a complete fixed-topology SQLite set,
   restore into a new quarantined directory, prove committed state and continued accounting;
   exercise WAL, forced-exit leases, unavailable broker and post-snapshot revocation reconciliation.
   Add a recovery runbook; no live-backup, topology-migration or durable-attempt claim.
2. W06d/C01e: record a reproducible synthetic workload baseline and failure outcomes; exercise
   operator journeys with API/browser tooling and obtain an actual-user first-request, budget
   and locked-broker walkthrough. Record the accepting operator and result, not an inferred sign-off.
3. Land each focused PR into `develop` only after clean adversarial review and green latest-head
   CI; verify post-merge CI. Existing authorization permits missing-review admin bypass only,
   never failed/pending checks or protection changes.
4. Once M1 acceptance evidence exists, open `develop` → `main` C02 promotion PR, review its full
   diff and verify CI before merge; verify the resulting `main` push. Tagging/publishing remains
   separate and is not part of this promotion request.

W06c and W06d automation integration and post-merge verification are complete. The user selected
UA01's engineering-evidence method for this release and explicitly deferred hands-on acceptance
until before production. The [decision record](../product/m1-acceptance-decisions.md) tracks that
approval, the subsequent acceptance of existing estimated token budgets (UA02) and current
broker evidence (UA03), operator-controlled recovery (UA04) and measured single-node performance
scope (UA05), remaining UA06 and deferred P01–P03. Next: confirm promotion/publication scope,
the proposed v0.6.0 targets and publishing safeguards. Do not tag or publish based on UA01–UA05.
The user subsequently authorized release-safeguard gap closure. Execute and track SG01–SG07
in the [release-safeguard plan](../product/release-safeguards.md); SG08/final publication remains
an explicit gate. Independent implementation/review work may proceed without claiming release
completion or reopening UA01–UA05.
Do not repeatedly request the now-deferred walkthrough as a release prerequisite. Do not mark
it performed or authorize production. Later live broker/fleet guarantees remain open.

Release-readiness preflight (2026-09-07 UTC) continued without publishing: the existing verifier
confirms v0.5.1 on PyPI, all four crates and the main npm package; both npm platform packages and
two GitHub binary archives are present. Historical npm E404 is not current artifact absence.
Registry-side trusted-publisher configuration remains unverified; environment/tag publishing
protections need review before the next tag. Evidence and the remaining UA06 choice are in the
[decision packet](../product/m1-acceptance-decisions.md). No publisher settings were changed.

### Milestone definitions

1. **M0 — review package:** A01–A07 complete, provisional decisions explicit, findings reproducible
   from cited code and future validation clearly separated. Complete; M1 is now underway.
2. **M1 — trustworthy single-node baseline:** W01–W04 plus initial W06. A configured dashboard is
   usable, control updates are truthful, metering semantics are pinned and broker calls are safe.
   Original scope includes actual-user journey review; UA01 defers that review until before
   production for this engineering release, without claiming usability validation.
3. **M2 — accountable team gateway:** W05–W10. Scoped authority, durable attempt evidence,
   hierarchical policy, incident investigation and credential lifecycle work together. Monetary
   reporting remains downstream. Complete operator/security/finance scenario reviews.
4. **M3 — resilient governed deployment:** W11–W13 as needed by deployment demand. Publish tested
   routing, fleet limits, storage longevity, recovery and credential-posture guarantees. No fleet
   claim before shared-ledger and distributed failure gates pass.
5. **M4 — admitted expansions:** selected W14 spikes, each with a named adopter and separate scope
   decision. Defer unproven breadth instead of making M1–M3 depend on it.

At each checkpoint, collect the observed user result, latency/resource cost, denied/failed-path
behavior and unresolved tradeoffs. A prototype walkthrough is not a usability result; a green
unit suite is not a distributed correctness proof. Record evidence and update this tracker.

## Existing design ownership and required reconciliation

| Existing work | Relationship to this plan |
|---|---|
| ADR-0001/0004/0005 | Preserve neutral measurement, two forwarding planes, custody enforcement and truthful settlement; propose amendments explicitly if guarantees or event semantics change |
| TD-0003/0009/0011/0013/0021/0022 (complete) | Keep shipped status; W01–W05 are follow-up correctness/product work, not a claim the earlier release never shipped |
| TD-0004/0005 | Own catalog/policy; reconcile stale examples and SDK authority claims before W08 |
| TD-0007/0012 | Own shared ledger/limiter evolution; W08/W12 specify hierarchical and fleet acceptance |
| TD-0014/0015/0016/0020 | Own resource safety, measurement, throughput and readiness; W06/W12 coordinate rather than create duplicate implementations |
| TD-0017/0018/0019 | Own TLS lifecycle, duplex exploration and untrusted codec hardening; maintain protocol admission gates |
| TD-0024 | Own retention/rollups; update old idempotency assumptions and ensure late settlement/recovery is compatible with W05 |
| TD-0025 | Own family registry and funnel refactoring; use differential evidence from W03/W05 before changing structure; separate behavior fixes from moves |
| TD-0023 | Own release automation; extend existing feature/contract gates for new broker contracts without reopening completed release scope |

## Verification and release plan

- Behavioral regressions first for confirmed defects. Browser tests must run the served UI;
  hand-authenticated API tests alone missed F01. Fault tests must force actual storage/IPC failure.
- Run workspace tests and the native IPC feature suite for affected changes. Run real-SDK
  conformance, schema/codegen drift and Python/Node contract tests when the relevant boundaries
  change. Preserve repository formatting, clippy and coverage requirements.
- Add property/model tests for multi-scope reservation, retries, cancel, unknown usage, expiry,
  late settlement, outbox replay and rollup. Use process crashes as well as injected errors.
- Capture baseline/after performance using a fixed mock-provider workload: complete and streaming,
  transparent and translation, token/cache/reasoning variants, many tenants and long history.
  Report hardware, dataset, concurrency, offered load, error ratio and sample variance.
- Use feature flags for new policy/routing behavior; shadow decisions before enforcement. Do not
  shadow a known correctness fix by silently retaining wrong measurements.
- Version public contracts; regenerate rather than hand-edit schemas. Validate old readers and
  historical data semantics. Back up before migration; test restore and continued accounting.
- Canary rollout needs a named operator, observed readiness/export lag and a rollback rule.
  Restoring an old binary must not discard new ledger/attempt data or resurrect revoked credentials.
- Release notes distinguish fixed defects, new capabilities, migration actions and limitations.
  Update this plan, owning TDs, the index and operator guide with actual verification evidence.

## Open decisions and dependencies

| ID | Decision/dependency | Default / resolution point |
|---|---|---|
| Q01 | Self-hosted teams versus enterprise-first priority | Self-hosted first; user preference requested; adjust ordering before implementation staffing |
| Q02 | Pricing ownership | Preserve ADR-0001; monetary reporting/admission integrated through a connected authority; no money in Sandhi core |
| Q03 | Strict budget support by provider/model/category | W03 resolved current eligibility: none; bytes/4 is not a proven input bound. Current admission is estimated; no strict mode exists. Future strict policy must reject unsupported combinations and prove all charged dimensions first |
| Q04 | Physical-attempt versus existing logical event semantics | Add a separate versioned record; confirm downstream reconciliation with W05 contract review |
| Q05 | Shared backend and cross-scope atomicity | TD-0007 experiment before W12; no assumption that independent SQLite shards provide fleet coordination |
| Q06 | Broker auto-lock, credential lease lifetime and emergency cancellation | SP2 security/operator review; proposed 60-second revalidation target is unmeasured |
| Q07 | External credential posture grants | SentinelPass's required grant-class ADR before SP3; no full-vault or reuse-graph exposure |
| Q08 | SentinelPass local/working-branch context | Public pinned source reviewed; inspect actual sibling branch when available before modifying it |
| Q09 | Staffing and calendar commitment | Assign maintainers after M0 review; effort ranges are not a release schedule |

## Activity log

- 2026-09-04: Inspected Sandhi `ed1781e`, reconciled selected prose with actual code, and recorded
  16 findings. Researched primary gateway documentation to refine independent capability and
  guarantee requirements. Added the SentinelPass co-design to the mandate on user request.
- 2026-09-04: Located public SentinelPass source after the sibling path was absent; reviewed its
  security architecture, exact grants, daemon enforcement and credential registry boundary at
  `00d1e7de`. No external changes or communications performed.
- 2026-09-04: Workspace baseline passed after online dependency resolution; native IPC feature
  suite passed (172 tests). No source, generated schema or dependency files changed.
- 2026-09-04: Planning package complete. Checked five changed/new documents for whitespace,
  final newlines, local links and excluded product references; all checks passed. M1–M4 remain
  unimplemented; product defaults and cross-repository contracts remain proposed.
- 2026-09-04: W01 completed after implementation authorization. Workspace tests/coverage passed
  (`cargo llvm-cov --workspace --fail-under-lines 75 --ignore-filename-regex 'src/generated/'
  --summary-only`, 87.54% lines); workspace all-target clippy and formatting passed. Proxy native
  IPC-feature suite passed all 172 tests. SDK/dashboard suite with AgentBrowser enabled passed
  33 tests; Gemini SDK module skipped because `google-genai` is absent locally (CI installs it).
  Inspected synthetic dashboard screenshot and mobile overflow regression. No real vault tested.
- 2026-09-04: Added three-way co-design and AB01–AB05 tracker. Repaired missing AgentBrowser
  workspace links using frozen/offline install and rebuilt packages; no tracked sibling changes.
  Real service/Chromium smoke passed using synthetic secret refs and exact-origin test egress.
  W02 committed management writes is the next Sandhi slice; W04 and AB02/AB03 define the next
  joint contract review, not an already-shipped live credential integration.
- 2026-09-05: W02 complete. `ProxyLedger::set_budget` now returns a fallible result; the operator
  holds ledger/metadata locks through commit and publication. Invalid scope/window/policy,
  oversized signed-SQLite caps and invalid alert thresholds fail before writes. Config failures
  and inline-alert partial commits have explicit non-success reports; UI preserves newly minted
  keys from partial config and CLI preserves partial budget results with a nonzero exit.
- 2026-09-05: Verification: 20 new management regressions passed (real SQLite rejection triggers,
  killed/restarted server, 32 concurrent budget writers, failed dedup reads, partial config and
  browser/CLI feedback). Full SDK/dashboard/management/AgentBrowser run: 53 passed, 1 Gemini SDK
  module skipped (`google-genai` unavailable locally; CI installs it). Workspace coverage gate
  passed at 87.30% lines; native IPC-feature suite passed 172 tests; all-target workspace clippy,
  formatting and JavaScript syntax checks passed. Signed-cap overflow store regression passed.
- 2026-09-05: W02 is commit-truthfulness groundwork, not all of R02: config apply remains
  non-transactional, direct inline-alert retries can duplicate rules, concurrent writes are
  last-writer-wins, and revision-conflict handling remains W08. No real vault or sibling source
  changed. Next slice W03: pin separate versus included reasoning counts, correct the accounting
  corpus and unsupported strict-cap claims; then W04's bounded broker runtime and grant tests.
- 2026-09-05: W03 complete. Added explicit `reasoning_included` to parser, typed usage, event and
  binding contracts; minor 7 and generated event schema pin compatibility. Gemini's separate
  reasoning counts now contribute below/equal/above candidate output. Proxy events retain the
  reasoning count and marker; SQL/group/run totals match settlement. Existing null-marker rows
  keep legacy arithmetic; no historical data or ledger rewrite was performed.
- 2026-09-05: W03 streaming assertions exposed Gemini completion before final usage: translated
  Responses clients received zero usage despite correct internal settlement. Gemini now defers
  Finish until successful EOF, after final usage. Chunk-boundary tests assert terminal ordering;
  a transport-failure test prohibits a premature success. Destination usage preserves output and
  cache conventions; all 24 streamed/complete cross-plane reasoning scenarios pass.
- 2026-09-05: W03 guarantee correction: README/dashboard/operator guide and ADR-0005/TD-0013
  now distinguish estimated admission from a strict total-token cap. Multiple in-flight
  underestimated calls can jointly overshoot; settlement retains actual measurement. The
  [capability and migration contract](../product/metering-and-budget-guarantees.md) lists missing
  proofs, legacy limitations and reader/validator upgrade ordering. No provider/model is
  certified for strict total-token enforcement, and no selectable strict mode is implied.
- 2026-09-05: Final W03 verification: `cargo llvm-cov --workspace --fail-under-lines 75
  --ignore-filename-regex 'src/generated/' --summary-only` passed at **87.42% lines**;
  `cargo clippy --workspace --all-targets -- -D warnings` and formatting passed.
  `cargo test -p sandhi-proxy -p sandhi-store --features sentinelpass-ipc --quiet` passed
  **233 tests**. Full SDK/dashboard suite with `SANDHI_AGENTBROWSER_ROOT` set passed **77 tests**,
  including actual AgentBrowser service/Chromium smoke; one Gemini SDK module remains skipped
  because `google-genai` is absent locally. Freshly rebuilt Python binding passed **29 tests**
  and Node binding **24**. Schema/facade regeneration was byte-reproducible; all nine shipped
  schemas passed metaschema validation. Tests used synthetic upstreams and disposable stores;
  no real provider/vault or sibling source was changed.
- 2026-09-05: Next implementation slice is **W04**, not yet started: reproduce native broker
  handler/runtime failure against a fake daemon, then implement bounded runtime-safe execution,
  explicit read/write backend capabilities and grants, and failure/onboarding tests. Align the
  synthetic broker/browser scenarios with SP0/SP1 and AB02/AB03. Live SentinelPass and joint
  pinned-checkout CI remain separate acceptance gates; W05 still owns durable attempt evidence.
- 2026-09-05: W04 complete in Sandhi. The original native `POST /admin/keys` disconnected without
  a response in the disposable-daemon regression. Replaced caller-owned Tokio runtime entry/drop
  with a dedicated worker, one active IPC operation plus 16 queued calls and queue-inclusive
  deadlines. Credential mutations now use bounded single-writer blocking offload, retaining the
  permit through persistence/publication even when the caller disconnects. Timed-out writes are
  explicitly uncertain and are never automatically retried.
- 2026-09-05: W04 onboarding: added `/admin/keys/reference`, `sandhi keys reference` and dashboard
  read-only reference mode; no supplied secret, broker write, unlock or provider verification call.
  Local backend capabilities are separate from authorization. Missing native feature/token or
  unknown backend is unavailable, not an implicit keyring/CLI fallback. Locked/denied/missing,
  unsupported, busy and timeout states are distinct; arbitrary broker errors are redacted.
  New references reject case/separator/wildcard/trailing-dot normalization collisions.
- 2026-09-05: W04 local offboarding now commits revocation before secret cleanup, does not send
  unsupported broker deletes, and reports cleanup independently from broker/provider revocation.
  Failed local commit cannot delete the secret. Startup now rehydrates each exact provider label
  and emits safe recovery-state warnings instead of silently omitting failed lookups. Updated
  runbook, TD-0003, review follow-up, changelog and two-way/three-way co-design boundaries.
- 2026-09-05: W04 verification: **93 passed, 1 skipped** in the full SDK/dashboard suite with
  `SANDHI_AGENTBROWSER_ROOT` configured. This includes **16 new broker scenarios** against native
  and non-IPC binaries: grants, canonical references, locked/missing/rejected states, hangs,
  overlapping writes, redaction, inventory/metadata faults, unsupported deletion, explicit CLI
  capabilities and API/CLI/browser read-only onboarding. The skipped module requires the locally
  absent `google-genai` dependency. Actual AgentBrowser service/Chromium smoke still passes.
  `cargo test --workspace --quiet` passed; the native-feature coverage run also passed all tests:
  `cargo llvm-cov --workspace --features sentinelpass-ipc --fail-under-lines 75
  --ignore-filename-regex 'src/generated/' --summary-only` reported **86.99% line coverage**.
  All-target clippy passed with and without `sentinelpass-ipc`; formatting, JavaScript syntax,
  generated facade checks and diff whitespace checks passed. No core/binding contract changed.
- 2026-09-05: W04 limits are explicit: no real vault or sibling source changed; no joint sign-off
  or Windows/live-daemon certification. Legacy CLI startup I/O and keyring operations have no
  adapter deadline. Broker save/SQLite metadata are not a distributed transaction. Cached secret
  generations, live grant invalidation and bounded new-dispatch cutoff remain W09/SP2/AB03.
  W01–W04 do not complete M1 until initial W06 operational readiness/recovery is verified.
- 2026-09-05: Next slice **W05** is pending: design the separate physical-attempt/evidence contract
  without changing existing logical-event meaning; pin retry/idempotency reconciliation and
  unknown liabilities; then implement atomic settlement/outbox and crash/replay/export tests.
  Downstream consumer review remains its contract gate; no external mutation is implied.
- 2026-09-05: W05 started and split into W05a–e in the
  [accounting/evidence design](../product/attempt-accounting-and-evidence.md). W05a is complete
  in the working tree: a separate store API commits settlement and immutable receipt in one
  `BEGIN IMMEDIATE` transaction, rejects conflicting charges/wrong scopes/missing and legacy
  leases, and returns the original receipt on exact replay. Existing proxy settlement and
  logical event behavior are unchanged; no physical-attempt schema or exporter is released.
- 2026-09-05: W05a delivery primitives claim bounded batches with expiring random fencing tokens,
  reject stale acknowledgements and retain acknowledged receipt tombstones. Sharded settlement
  routes by scope; delivery uses explicit validated local shard indices. Legacy widening refuses
  any evidence-bearing source before creating target files or changing source rows. General
  topology migration, backlog limits and retention remain W05e/TD-0024. Reconciled TD-0024's
  obsolete assumption that logical dedup had not shipped.
- 2026-09-05: W05a verification: **14 focused tests passed**, including a child-process helper;
  receipt insert/update rejection and whole-claim-batch rollback, independent SQLite writer/
  claimant races, reopen/ack/replay, integer bounds, shard identity collisions and migration
  refusal. Abrupt process exit before/after commit is covered; the pre-commit child stages SQL
  directly, while injection tests exercise the production API. No power-loss, fsync/disk-full,
  upstream dispatch or receiver crash certification is claimed.
- 2026-09-05: Full `cargo test --workspace --quiet` passed. Native-feature coverage run passed:
  `cargo llvm-cov --workspace --features sentinelpass-ipc --fail-under-lines 75
  --ignore-filename-regex 'src/generated/' --summary-only` reported **87.04% line coverage**
  (**90.61%** for the new receipt module). All-target clippy passed with/without the native
  feature; formatting, generated binding facade and whitespace checks passed. No public
  core/schema/binding contract changed in W05a. Browser suites were not rerun for this store-only
  slice; W04's AgentBrowser/broker evidence remains the last browser verification.
- 2026-09-05: Next slice **W05b**: capture real transport dispatch/terminal attempts, including
  retries, transparent/translation planes, pre-dispatch failures, cancellation and incomplete
  measurement. Draft the neutral attempt contract for downstream review before external
  release; then W05c connects attempts/leases/receipts under explicit failure policy. W05d owns
  unknown liability and late amendments; W05e owns export, retention and consumer acknowledgement.
  SentinelPass SP2 and AgentBrowser AB04 correlation remain co-design gates, not new authority
  or evidence of completion. W05 and M2 remain in progress; M1 still requires initial W06.
- 2026-09-05: C01 checkpoint authorized. Created `feat/gateway-trust-checkpoint` from unchanged
  `origin/develop` (`ed1781e`); overlapping W01–W04 runtime/tests are one commit, W05a storage
  another, with separate CI and documentation commits. Fresh full SDK/browser rerun with
  `SANDHI_AGENTBROWSER_ROOT=/home/vsingh/code/agentbrowser python -m pytest tests/sdk-conformance/ -q`
  passed **93 tests, 1 skipped** (Google SDK unavailable locally); remote CI installs that SDK.
  Regenerated chat schemas successfully. Existing 87.04% workspace coverage and clippy evidence
  above applies to the unchanged implementation. PR/remote CI and merge remain pending, not
  inferred from local success. Workflow edits retain the human private-runner approval gate.
- 2026-09-05: C01 committed and pushed; [PR #230](https://github.com/anvai-labs/sandhi/pull/230)
  targets `develop`. The protected-base CI run entered `owner-private-ci` approval waiting;
  the ordinary PR mirror is intentionally skipped and is not verification evidence. Live
  branch protection requires `CI Success` and one approving review (`REVIEW_REQUIRED`).
  Integration is blocked on those external approvals; no merge or release occurred. Live
  `enforce_admins` is false, contrary to older contributor prose; no administrator bypass or
  protection change was used. After approval, resume real CI diagnostics/fixes, merge only
  with the required checks/review satisfied, and verify post-merge CI before closing C01.
- 2026-09-05: Owner explicitly authorized disabling Sandhi private CI routing. Set and read back
  repository variable `OWNER_PRIVATE_CI_ENABLED=false`; normal PR jobs now select GitHub-hosted
  `ubuntu-latest`. A new documentation commit triggers a fresh `pull_request` run rather than
  relying on the previous skipped mirror or approving private execution. No environment, runner
  group, branch protection or required review was removed. The standalone self-hosted overflow
  diagnostic remains separate and is not part of normal PR CI. Parallel read-only agents audit
  the hosted route and scope initial W06 while C01 CI runs; no M1/main promotion is implied.
- 2026-09-05: C01 hosted cutover verified against live run
  [34005979889](https://github.com/anvai-labs/sandhi/actions/runs/34005979889): regular PR jobs
  have `ubuntu-latest` labels and execute on GitHub Actions runners, not the private pool.
  Routing, attribution, title, Node binding, Rust and security checks passed at this checkpoint;
  remaining checks were queued and the PR still required an approving review. The old private
  run was cancelled automatically on the new push. Do not interpret its skipped mirror as CI success.
- 2026-09-05: Parallel W06b implementation and review completed in an isolated worktree/branch,
  preserving PR #230's reviewed scope. `BufferedSink` and `BufferedAlertStore` now expose
  sender-free snapshots of logical capacity, accepted-but-not-started items, executing callbacks
  and dropped items. Admission and close share the bookkeeping lock; callbacks never hold it.
  Panic cleanup counts queued uninvoked callbacks as abandoned, not the uncertain result of the
  panicking callback. No snapshot or completed callback certifies durable persistence.
- 2026-09-05: W06b metrics are attached in the real binary and served through existing `/metrics`
  auth with fixed usage/alerts labels. Unconfigured is explicit; no fabricated zero backlog.
  Independent review caught and corrected incomplete drop HELP text and interleaved metric
  families; a new ordering regression pins contiguous Prometheus exposition. Full workspace
  tests passed; native-feature coverage was **87.26%**. All-target clippy passed with/without
  native IPC; formatting, facade and whitespace checks passed. The complete SDK/browser/broker
  suite with AgentBrowser passed **95 tests, 1 skipped** (Google SDK unavailable locally),
  including two new real-binary buffer capacity/auth/unconfigured checks. No schema changed.
- 2026-09-05: W06 remains in progress. Next operational slice W06a must make readiness reachable
  over real sockets during bounded quiesce, prohibit new dispatch after cutoff and define listener
  plus writer shutdown deadlines; a router-only flag is insufficient. W06c backup/restore and
  W06d workload/operator-user acceptance remain open, as do W05b–e's authoritative evidence
  gates. W06b is Prometheus-only; storage-write failures, ring evictions, worker health, age and
  OTLP parity remain explicit observability follow-ups. C01b awaits its own PR after C01 lands.
- 2026-09-05: Owner authorized merge after clean adversarial review and green CI. Parallel
  security/accounting reviews identified scheme-alias and Python parser reasoning regressions,
  a misleading metadata-fault fixture, and lost config reconciliation details. Fixed all four;
  re-review found no remaining checkpoint blocker. Full SDK/browser tests with real optional
  AgentBrowser: **103 passed, 1 skipped** (Google SDK absent locally); Python bindings **37
  passed**. Workspace/native IPC tests and clippy with/without IPC pass; native workspace line
  coverage **86.96%** exceeds 75%; formatting, generated facade and whitespace checks pass.
  See the linked checkpoint review for exact commands and limitations. Hosted run
  [34005979889](https://github.com/anvai-labs/sandhi/actions/runs/34005979889) passed all checks
  at `97e4195`, before these fixes; fresh CI on the review-fix HEAD remains mandatory. Live
  GitHub state still requires one approving PR review, with no reviews recorded. Chat approval
  authorizes the merge operation but does not replace that protected-branch gate; no bypass.
  Separately reviewed W06b work remains on `feat/operational-buffer-visibility` (`800753d`),
  not in C01. Next: fresh C01 CI and eligible review, merge and post-merge CI, then its separate
  W06b PR; W06a readiness/drain, W06c recovery and W06d acceptance still gate M1/main promotion.
- 2026-09-06: Hosted [34010886202](https://github.com/anvai-labs/sandhi/actions/runs/34010886202)
  passed all required jobs for review-fix `628fb26`. Push-time dependency alerts revealed that
  advisory CI excluded independent bindings. Source review exempts the old PyO3 version from
  the high-severity iterator advisory and found no calls to the other two affected APIs, but
  the gate gap and old dependency still warranted correction: upgrade PyO3/async bridge to
  patched 0.29 releases, retain explicit GIL requirement, declare the existing Rust 1.88 locked
  build floor, and audit all three workspaces/all features on binding and policy changes.
  All three local advisory scans pass without ignores; independent re-review is clean. Fresh
  CI for this additional hardening and an eligible GitHub approval remain mandatory.
- 2026-09-06: Runtime/CI hardening `7faa9f3` passed every actual required job and `CI Success`
  in hosted [34014212504](https://github.com/anvai-labs/sandhi/actions/runs/34014212504).
  Verified all executing jobs use `ubuntu-latest`; private authorization skips as intended.
  Post-upgrade Python tests passed locally and in CI (**37**); instrumented wheel coverage
  passed both locally and in CI at **94.12%**. All three advisory checks passed without ignores.
  Final source/workflow re-review has no confirmed blocker. GitHub still reports
  `REVIEW_REQUIRED`, `reviews: []`, `mergeStateStatus: BLOCKED`; C01 integration is blocked
  only on an eligible approving GitHub review, subject to green latest-head checks. No merge,
  bypass, release or main promotion occurred. Next authorized action: after that review,
  recheck head/checks, merge normally into `develop`, then verify post-merge CI before W06b PR.
- 2026-09-06: Owner subsequently explicitly authorized administrator bypass of the missing
  approving review, conditional on clean adversarial review and green CI. Rechecked exact
  head `4e90cdb` and all required checks, then merged PR #230 into `develop` as `8f56b91`.
  The merged tree matches the reviewed head; branch protections were not changed. Post-merge
  [34015573003](https://github.com/anvai-labs/sandhi/actions/runs/34015573003) passed every
  required validation and the aggregate gate. C01 integration is complete, not a release.
- 2026-09-06: Resumed C01b by merging current `develop` into the published W06b branch without
  rewriting history. Kept both checkpoint corrections and W06b implementation; resolved the
  tracker-only conflict by retaining both evidence histories and current integration state.
  Next: full current-base regression, independent adversarial re-review, separate PR and CI.
  W06a probe-reachable shutdown remains the next implementation slice after this checkpoint;
  W06c/W06d and M1/main promotion are not completed by buffer metrics.
- 2026-09-06: C01b current-base verification passed: full native-feature workspace suite,
  all-target clippy with/without native IPC, fmt, generated binding facade and diff checks.
  Native-feature workspace line coverage is **87.18%**; full SDK/browser/broker tests with
  optional real AgentBrowser are **105 passed, 1 skipped** (Google SDK absent locally).
  Coverage initially hit sandbox socket restrictions; the permitted loopback rerun passed.
  Independent adversarial re-review found no blocker and verified runtime/test files unchanged
  from the prior reviewed W06b implementation. This is local evidence; remote CI is separate.
- 2026-09-06: Opened [PR #231](https://github.com/anvai-labs/sandhi/pull/231) for C01b.
  Merge remains conditional on clean scoped review and green latest-head CI; verify the
  resulting `develop` push before closing integration. The PR records live CI/merge evidence;
  this source snapshot does not predeclare a successful merge. Next implementation is W06a,
  whose TD-0020 execution gates now cover dispatch-authorization races, blocking settlement,
  runtime/telemetry cleanup and saturated probe admission, not just a router readiness flag.
- 2026-09-06: C01b integrated through PR #231; reviewed head
  `995f105375a8ba1c7c78f988b42cab382b6cffc5`, merge `f777b89671c4b52854cb6c527dd03fa01a7ada52`,
  latest-head CI `34029737562` and post-merge CI `34030218505` passed. Only the authorized
  missing-review admin bypass was used; no protection settings changed or main promotion occurred.
- 2026-09-06: Started C01c/W06a on `feat/drain-aware-readiness`. Implementing a shared cutoff,
  same-port bounded quiesce, queued/upload admission rejection, owned reservation rollback,
  admin mutation gates and one binary shutdown deadline including blocking cleanup. Local
  compilation and focused watchdog/admin tests pass; network tests, cancellation regression,
  independent adversarial review and full regression remain open. Probe reachability remains
  subject to existing connection/per-IP limits; no dedicated probe listener is implied.
  W06c recovery and W06d workload/user acceptance still gate M1/main promotion.
- 2026-09-06: W06a local acceptance passed: native-feature workspace suite (proxy library 101
  tests), OTLP-feature proxy suite, default and combined native/OTLP strict clippy, formatting,
  facade drift and advisories for all three Rust workspaces. Native coverage is **87.76%**.
  Full SDK/dashboard/broker tests with real AgentBrowser are **113 passed, 1 skipped** (Google
  SDK absent locally), including eight real-process shutdown cases: HTTP/TLS fresh/keep-alive
  probes, held SSE, queued/slow-body cutoff, connection-cap shedding, hung SSE and locked SQLite.
  Focused tests cover detached reservation cancellation, all admin mutation handlers, config
  partial application, metrics authorization and watchdog/runtime teardown. Independent review
  found a trailing OTLP span-drop guard race; moving the operation guard last fixed it, and
  re-review is clean. Socket/advisory checks required permitted sandbox reruns. C01c is ready
  for a separate `develop` PR; remote CI and integration are not predeclared. Next implementation
  after C01c is W06c: disposable backup/restore and incident recovery drills, followed by W06d
  workload/operator acceptance. No main promotion or release is authorized by local verification.
- 2026-09-06: Opened [PR #232](https://github.com/anvai-labs/sandhi/pull/232) for C01c.
  Public-runner routing remains enabled (`OWNER_PRIVATE_CI_ENABLED=false`). Merge remains
  conditional on clean review and green latest-head CI; the PR records live CI/merge evidence.
  This source snapshot does not predeclare integration. W06c remains the next implementation slice.
- 2026-09-06: C01c integrated as `31151d9`; pre-merge CI `34042875545` and post-merge CI
  `34048329909` passed. User authorized W06c and W06d focused `develop` PRs followed by M1
  promotion only after clean review/green CI and acceptance gates. Started `test/recovery-drills`;
  actual accepting operator requested separately, not inferred from merge authorization.
- 2026-09-06: Recovery review reproduced a P1 startup bypass: an incompatible reservation schema
  left a persisted zero hard cap intact, but configured ledger-open failure selected a new memory
  ledger; readiness was 200 and a synthetic request dispatched successfully. C01d now includes
  fail-closed configured-storage initialization and real startup regression tests. Runbook
  preflight alone is insufficient. This correction does not add automatic shard completeness,
  topology migration, or runtime broker revocation guarantees.
- 2026-09-06: C01d local gates passed: 181 SDK/browser tests, one unavailable Google SDK skip;
  default/native workspace and OTLP proxy tests and strict all-target clippy; native coverage
  87.77%. The final combined suite includes real AgentBrowser restore smoke. Snapshot review
  findings (nested restore mutation and publication-size mismatch) and the P1 configured-store
  fallback were corrected; independent startup re-review passed 17 cases including URI rejection.
  The socket-blocked SDK attempt was rerun with permission. Initial fixtures demonstrate exact
  committed-state restoration and conservative held leases, not production RTO/RPO or complete
  consumption reconstruction. Ready for the focused C01d PR; W06d actual-user acceptance remains
  pending and cannot be inferred from merge authorization.
- 2026-09-06: Opened [PR #233](https://github.com/anvai-labs/sandhi/pull/233) for C01d,
  implementation `2c55ded`. Public runners remain selected. Existing authorization permits
  bypassing only a missing approving review after clean adversarial review and green latest-head
  CI; post-merge CI must also pass. Live evidence is recorded on the PR, not predeclared here.
- 2026-09-06: C01d merged as `8ae8401` after clean independent review and latest-head CI
  `34058455291` passed on public runners; only the missing approving review was bypassed under
  existing authorization. Post-merge push CI `34059776510` is pending. The superseded old-head
  run was canceled to release its concurrency group; no required current-head check was bypassed.
- 2026-09-06: C01e automation is locally ready. Combined SDK/browser suite passed 227 tests with
  one unavailable Google SDK skip. A fresh default-feature dev build from integrated `8ae8401`
  passed 36 workload phases: 4,608 requests, 3,072 gateway events across 32 tenants, 546,600 neutral
  tokens, zero unsettled leases and clean denied paths/shutdown, in 66.28 seconds. The
  [acceptance record](../product/m1-acceptance.md) links compact evidence and distinguishes local
  full-artifact retention, build provenance, resource limitations and the pending actual-user gate.
  Independent review fixed stream-terminal, unexpected-ledger-scope and optimized-Python false
  passes, then independently recomputed final artifact totals/statistics/digests with no blockers.
  W06d automation may land separately from human sign-off, but W06d/M1 remain incomplete until
  an actual accepting operator reviews the required journeys. No main promotion is authorized
  by synthetic workload success alone.
- 2026-09-06: Completed C01d integration verification: post-merge CI `34059776510` passed at
  `8ae8401`. C01e automation then merged through PR #234 as `324ba87` after reviewed head
  `948e7af` passed CI `34060286384`; post-merge CI `34060985111` also passed. Hosted SDK runs
  passed 233 tests with two optional sibling-browser skips; local combined runs exercised those
  browser checks. All executed CI jobs used public hosted runners. Both merges bypassed only
  the missing approving review under existing authorization, never failed/pending checks.
- 2026-09-06: Cumulative prospective promotion review (`main` `72ced4b` → candidate `948e7af`)
  found no new release blocker on top of completed slice reviews. It includes the already-integrated
  protocol 0.8.1 and npm bootstrap documentation updates; it is not production certification.
  W06d/M1 remain incomplete because accepting operator and observed journey results have not been
  supplied. C02/main promotion is deliberately unopened. Next: obtain the recorded actual-user
  outcomes, address any findings, then recheck the promotion head/review/CI and resulting main push.
