# Development PR train after 0.17.0

This ledger records the disposition and next gate for every PR open during the
2026-11-13 release audit. Unique, aligned changes proceed through `develop` and a
later reviewed promotion. They do not silently alter the frozen 0.17.0 source.
Update the row when a candidate, review, CI, merge or release milestone changes;
link the actual evidence before marking it complete.

## Release boundary

The reviewed 0.17.0 main source is
[`d841fcaaa89410a02ec1ae2413a2f18e869777b3`](https://github.com/anvai-labs/sandhi/commit/d841fcaaa89410a02ec1ae2413a2f18e869777b3),
promoted through [PR #451](https://github.com/anvai-labs/sandhi/pull/451).
Its source tree matches the independently reviewed candidate. The owner authorized
the admin merge after green PR/develop CI and clean adversarial review.

Exact-main [CI run 43376120112](https://github.com/anvai-labs/sandhi/actions/runs/43376120112)
passed on unchanged-source attempt 2. The first attempt failed the isolated
container OOM assertion without logging its individual observed values. The same
strict probe passed on the retry; no assertion was weakened and no cause is
claimed. The complete latest attempt and executed release safeguards passed before
the immutable `v0.17.0` tag was created. The [release workflow](https://github.com/anvai-labs/sandhi/actions/runs/43381244838)
failed before publication: both binary smoke fixtures omitted the now-required
durable store. GitHub release creation and every registry publisher were skipped.
The tag stays immutable; a reviewed 0.17.1 correction is being prepared. A separate
current-profile stream-finality defect was reproduced during follow-up review.
Both corrections passed independent review and all 21 public PR checks, then
merged through [corrective PR #452](https://github.com/anvai-labs/sandhi/pull/452)
and [main promotion #454](https://github.com/anvai-labs/sandhi/pull/454).
Exact-main CI for `10caae8` is now the gate before a new 0.17.1 tag. Publication is complete only after
GitHub, PyPI, crates.io and npm artifacts all verify. Consumer adoption and production deployment have their
own gates; see the [release checklist](../releases/v0.17.0.md).

## Follow-up status

| PR | State and owner lane | Evidence and next gate |
| --- | --- | --- |
| [#395](https://github.com/anvai-labs/sandhi/pull/395) | **Closed: superseded**; dependency audit | Its complete protocol 0.19.1 pin/checksum update is duplicated in #448, while #447 tracks the later source-identical protocol candidate. No unique implementation discarded. |
| [#410](https://github.com/anvai-labs/sandhi/pull/410) | **Merged; post-merge CI passed**; maintenance | Exact head `c6907009` passed independent review and hosted checks. Normal merge `eec61506`; [post-merge CI](https://github.com/anvai-labs/sandhi/actions/runs/43381316579) passed. Immutable Actions pin updates only; not included in tagged 0.17.0. |
| [#396](https://github.com/anvai-labs/sandhi/pull/396) | **Merged; post-merge CI passed**; documentation maintenance | History-preserving head `d1f0f6a` passed exact independent review and hosted checks, then merged as `bc919774`. Strict MkDocs, three documentation and ten brand tests passed. [Post-merge CI](https://github.com/anvai-labs/sandhi/actions/runs/43382440829) passed. |
| [#447](https://github.com/anvai-labs/sandhi/pull/447) | **Reviewed candidate; hosted CI pending**; protocol maintenance | Minimal 0.20.0 protocol pin/checksum update. Published protocol source files and dependency declarations match 0.19.0/0.19.1 apart from release metadata. 717 proxy/store feature tests and Clippy passed; exact independent review is clean. Current-base history-preserving head `f89d2174` still needs public CI and normal merge. |
| [#362](https://github.com/anvai-labs/sandhi/pull/362) | **Reviewed source candidate**; provider contracts | Preserve useful vendor envelope admission through explicit backend profiles. The old catalog-inferred selection conflicts with current authority boundaries. Unsupported envelopes currently remain unqualified; no invented counts demonstrated. Explicit replacement `7c77526` passed scoped independent review; combine with the corrected common source fence before integration CI. |
| [#363](https://github.com/anvai-labs/sandhi/pull/363) | **Refreshed candidate; review pending**; catalog | Candidate `0e9bb72` refreshes primary provider facts and passed 12 catalog tests; catalog data must not enable routes or enlarge admission budgets. |
| [#364](https://github.com/anvai-labs/sandhi/pull/364) | **Qualified and independently reviewed candidate**; typed contract | Its proposed typed `output.reasoning` field is unique. Existing validated reasoning extensions already prevent buffered display loss. Candidate `da84a95` uses additive minor 14 and regenerated schemas/facades; core, codec, four-family/Codex and fresh binding checks passed. Integration CI remains required. |
| [#448](https://github.com/anvai-labs/sandhi/pull/448) | **Split candidates under qualification**; dependencies/security | Unique grouped updates remain. ORT rc15 breaks release-feature API compilation; OIDC 4.1 needs a fresh scoped security assessment. Required CI fails. The compatible serde_json/Tokio/libc split is [PR #455](https://github.com/anvai-labs/sandhi/pull/455), with 1,303 Rust tests, Clippy and three advisory checks passed. Separate ORT `815fa3c` preserves existing ownership behavior; OIDC `68a5769` removes the patched bigint ignore and retains bounded RSA review. Independent review/integration CI still gate those changes. |

Stale auto-merge was disabled on #362, #363 and #364 while their unique work is
reworked. No PR was closed solely because of age. The
[full initial audit](https://github.com/anvai-labs/sandhi/pull/451#issuecomment-6846017883)
records all eight dispositions.

## Corrective release and documentation queue

| Item | State / owner lane | Next gate |
| --- | --- | --- |
| Release smoke fixture | **Reviewed fix**; release tooling | Private durable ledger and empty credential store; actual binary smoke passes. Add the actual smoke to ordinary CI so this startup contract is tested before tags. Integrate reviewed `ed5b0fea` into the correction. |
| Current Chat stream finality | **Backport qualification**; accounting | Standard and terminal-choice totals followed by new output must remain partial/unresolved. Preserve harmless identical-total idempotence, both budget holds and one physical send. Independent exact review and full CI required. |
| 0.17.1 correction | **Preparing**; release coordination | Consistent package versions, both corrections, reviewed promotion, exact-main CI, immutable new tag and all-target verification. Keep failed 0.17.0 tag; coordinate Victor's exact binding pin and published-wheel qualification. |
| Generic OIDC guide | **In progress**; documentation | Lead with provider-neutral requirements; Kanidm is one example. Verified `https://id.anvaiops.dev` discovery is available, but required introspection metadata is absent. Use it as a discovery example with an explicit compatibility gap, not a working integration claim; change no auth or service configuration. |

## Three-project identity co-design

The user requested a demonstrated bridge, not a provider-branded placeholder.
These are local source-candidate milestones; no live IDM deployment or published
SDK compatibility is inferred from them.

| Milestone | Verified state | Remaining gate |
| --- | --- | --- |
| IDM online resource-token purpose | Protocol source independently reviewed; actual HTTPS/PostgreSQL login, consent, PKCE, issuance receipt, introspection, rollback, cap/race and revocation fixtures passed. Legacy console/UserInfo purposes remain separate. | Public CI and reviewed merge; live service unchanged. |
| Supported operator registration | Existing owner review/passkey/apply path configures resource purpose and consumer permissions. Exact-artifact local acceptance and separate source review passed, including tenant/version guards, removal/regrant and fresh-grant recovery. | Include this supported path in the final combined demonstration and deployment runbook. |
| Sandhi resource audience | Optional `access_token_audience` preserves omitted `client_id` behavior while keeping browser/Basic identity separate. Fresh aligned build, 23 auth tests, corrected observer/raw tests and scoped independent review passed. | Combined post-release source and public CI; published 0.17.1 does not acquire this feature by implication. |
| Victor broker/provider path | Existing private token-file owner retained; 24 credential tests passed. Initial local run proved calls and denials, but review rejected mutable-client-state token provenance and READY-only tool success as insufficient oracles. | Corrected actual-wire credential/request-ID capture and matching successful tool-result transcript have passed the new aligned run; final evidence review remains separate. |
| Combined acceptance | Owned local IDM, Sandhi and real Victor Agent execute against a synthetic loopback model backend. Corrected run includes actual wire/receipt joins, tool execution and six denial controls. Earlier limited proofs are preserved. | Independent evidence acceptance; repeat against the final combined source and qualify the published binding before consumer merge. |
| Live development service | Public discovery at `https://id.anvaiops.dev` was checked; required introspection metadata is absent on the current deployment. | Reviewed service rollout, migration/rollback plan and separate live acceptance. No live configuration or credential writes performed. |

Focused commits and their reviews are preserved in a combined post-release train
so shared schema, auth and lockfile changes receive one current integration
qualification. Original PRs close as completed/superseded only after their unique
content is actually integrated.

## Completion evidence

Each active row needs an exact source commit and reviewed base, independent clean
review, appropriate local qualification, successful required hosted CI, develop
merge commit and post-merge CI. Record the later main promotion and published
version separately. A prepared patch, a green older head or an open replacement
PR is not a merged or released change. All CI uses public hosted runners.
