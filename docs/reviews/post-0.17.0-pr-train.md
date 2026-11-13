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
The tag stays immutable. A separate current-profile stream-finality defect was reproduced during follow-up review.
Both corrections passed independent review and all 21 public PR checks, then
merged through [corrective PR #452](https://github.com/anvai-labs/sandhi/pull/452)
and [main promotion #454](https://github.com/anvai-labs/sandhi/pull/454).
The reviewed corrective main
[`10caae8a4cc2b8a226fe3157a9ac1f45cca09ec6`](https://github.com/anvai-labs/sandhi/commit/10caae8a4cc2b8a226fe3157a9ac1f45cca09ec6)
passed exact-main CI and is published as immutable
[v0.17.1](https://github.com/anvai-labs/sandhi/releases/tag/v0.17.1).
[Release workflow 43393858026](https://github.com/anvai-labs/sandhi/actions/runs/43393858026)
passed on attempt 2 after an npm propagation verifier retry, with no republish.
Independent final verification passed GitHub archives/checksums, PyPI wheel
coverage/non-yanked metadata, all four crates and npm main/platform metadata.
Fresh published GitHub binary smoke and 88/88 installed npm conformance tests
also passed. Consumer adoption and deployment remain separate; see the
[published release and upgrade guide](../releases/v0.17.1.md).

The combined **0.18.0 source train remains unreleased**. Its independently
reviewed [integration PR #456](https://github.com/anvai-labs/sandhi/pull/456)
merged to `develop` as
[`f6396731`](https://github.com/anvai-labs/sandhi/commit/f63967311d4e99307f6e68a9204d319dd9daf037)
after [full public CI](https://github.com/anvai-labs/sandhi/actions/runs/43395768465)
passed. The merge tree matches the reviewed candidate. Exact post-merge
[develop CI](https://github.com/anvai-labs/sandhi/actions/runs/43399122047)
is a separate gate. Local Rust, fresh binding, ONNX and documentation checks
also passed. This source merge does not change the published 0.17.1 feature set.

## Follow-up status

| PR | State and owner lane | Evidence and next gate |
| --- | --- | --- |
| [#395](https://github.com/anvai-labs/sandhi/pull/395) | **Closed: superseded**; dependency audit | Its complete protocol 0.19.1 pin/checksum update is duplicated in #448, while #447 tracks the later source-identical protocol candidate. No unique implementation discarded. |
| [#410](https://github.com/anvai-labs/sandhi/pull/410) | **Merged; post-merge CI passed**; maintenance | Exact head `c6907009` passed independent review and hosted checks. Normal merge `eec61506`; [post-merge CI](https://github.com/anvai-labs/sandhi/actions/runs/43381316579) passed. Immutable Actions pin updates only; not included in tagged 0.17.0. |
| [#396](https://github.com/anvai-labs/sandhi/pull/396) | **Merged; post-merge CI passed**; documentation maintenance | History-preserving head `d1f0f6a` passed exact independent review and hosted checks, then merged as `bc919774`. Strict MkDocs, three documentation and ten brand tests passed. [Post-merge CI](https://github.com/anvai-labs/sandhi/actions/runs/43382440829) passed. |
| [#447](https://github.com/anvai-labs/sandhi/pull/447) | **Merged through #456**; protocol maintenance | Minimal 0.20.0 protocol pin/checksum update. Published protocol source files and dependency declarations match 0.19.0/0.19.1 apart from release metadata. 717 proxy/store feature tests and Clippy passed; exact independent review is clean. Independently reviewed `f89d2174` is included in merged PR #456; combined CI passed and exact post-merge CI remains. |
| [#362](https://github.com/anvai-labs/sandhi/pull/362) | **Closed: replacement merged in #456**; provider contracts | Preserve useful vendor envelope admission through explicit backend profiles. The old catalog-inferred selection conflicts with current authority boundaries. Unsupported envelopes currently remain unqualified; no invented counts demonstrated. Explicit replacement `7c77526` passed scoped independent review and is combined with the corrected common source fence in the reviewed integration; full integration CI passed; post-merge CI remains. |
| [#363](https://github.com/anvai-labs/sandhi/pull/363) | **Closed: replacement merged in #456**; catalog | Candidate `0e9bb72` refreshes primary provider facts and passed catalog and combined-source review; catalog data must not enable routes or enlarge admission budgets. |
| [#364](https://github.com/anvai-labs/sandhi/pull/364) | **Closed: replacement merged in #456**; typed contract | Its proposed typed `output.reasoning` field is unique. Existing validated reasoning extensions already prevent buffered display loss. Candidate `da84a95` uses additive minor 14 and regenerated schemas/facades; core, codec, four-family/Codex and fresh binding checks passed. Full integration CI passed; post-merge CI remains. |
| [#448](https://github.com/anvai-labs/sandhi/pull/448) | **Closed: qualified split replacements merged in #456**; dependencies/security | All unique grouped updates have reviewed replacements. The original grouped head failed release-feature API compilation with ORT rc15. The split adaptation and OIDC 4.1 scoped reassessment are now included in the reviewed integration. The compatible serde_json/Tokio/libc split is [PR #455](https://github.com/anvai-labs/sandhi/pull/455), with 1,303 Rust tests, Clippy and three advisory checks passed. Separate ORT `815fa3c` preserves existing ownership behavior; OIDC `68a5769` removes the patched bigint ignore and retains bounded RSA review. Combined independent review and local qualification passed; full integration CI passed; post-merge CI remains. |

Stale auto-merge was disabled on #362, #363 and #364 while their unique work was
reworked. Those older heads are now closed after their replacements merged. No PR was closed solely because of age. The
[full initial audit](https://github.com/anvai-labs/sandhi/pull/451#issuecomment-6846017883)
records all eight dispositions.

## Corrective release and documentation queue

| Item | State / owner lane | Next gate |
| --- | --- | --- |
| Release smoke fixture | **Merged to corrective main**; release tooling | Private durable ledger and empty credential store; actual binary smoke passes. The actual smoke also runs in ordinary CI so this startup contract is tested before tags. Reviewed `ed5b0fea` is integrated; the actual-binary smoke passed public PR and promotion CI. Published 0.17.1 includes this correction; exact-main and all-target verification passed. |
| Current Chat stream finality | **Merged to corrective main**; accounting | Standard and terminal-choice totals followed by new output must remain partial/unresolved. Preserve harmless identical-total idempotence, both budget holds and one physical send. Reviewed backport `60316e1` and full PR/promotion CI passed. Published 0.17.1 includes this correction; exact-main and all-target verification passed. |
| 0.17.1 correction | **Published; all four targets verified**; release coordination | Exact reviewed main `10caae8`, immutable tag, successful release attempt 2 and independent final verifier are complete. Failed 0.17.0 tag remains immutable. Victor's exact binding pin and published-wheel qualification passed; reviewed consumer adoption remains separate. |
| Generic OIDC guide | **Prepared for review**; documentation | Provider-neutral requirements, Kanidm example and [one local co-design walkthrough](../operator/oidc-sso.md#local-co-design-victor-sandhi-and-anvaiops-development-idm) separate demonstrated behavior from the public development host's missing introspection metadata. Documentation review/CI remain; no live service configuration changed. |

## Three-project identity co-design

The [co-design walkthrough](../operator/oidc-sso.md#local-co-design-victor-sandhi-and-anvaiops-development-idm)
is the common usage example: separate Victor origin and Sandhi consumer clients,
supported owner/passkey registration, real local issuance/introspection/revocation,
and two actual Agent/tool-loop calls joined to two 14-token receipts. These are
local source-candidate milestones. Final acceptance passed with the official
published SDK 0.17.1, the reviewed combined Sandhi 0.18 candidate and the IDM
candidate aligned to its actual remote baseline. All three acceptance tests passed;
no live IDM deployment is inferred. Earlier SDK 0.17.0 evidence remains historical.

| Milestone | Verified state | Remaining gate |
| --- | --- | --- |
| IDM online resource-token purpose | Protocol source independently reviewed; actual HTTPS/PostgreSQL login, consent, PKCE, issuance receipt, introspection, rollback, cap/race and revocation fixtures passed. Legacy console/UserInfo purposes remain separate. | Remote-base alignment, fresh qualification and independent review passed; the private PR is open with hosted CI running. Inherited dependency advisories require reviewed remediation. Reviewed merge remains; live service unchanged. |
| Supported operator registration | Existing owner review/passkey/apply path configures resource purpose and consumer permissions. Exact-artifact local acceptance and separate source review passed, including tenant/version guards, removal/regrant and fresh-grant recovery. | Supported owner/passkey registration passed the final remote-aligned demonstration. Hosted CI and reviewed merge remain; deployment requires its own qualification. |
| Sandhi resource audience | Optional `access_token_audience` preserves omitted `client_id` behavior while keeping browser/Basic identity separate. Fresh aligned build, 23 auth tests, corrected observer/raw tests and scoped independent review passed. | Combined post-release independent review, local checks and public integration CI passed; the feature is merged to develop and exact post-merge CI remains. This feature is absent from published 0.17.1. |
| Victor broker/provider path | Existing private token-file owner retained; 24 credential tests passed. Initial local run proved calls and denials, but review rejected mutable-client-state token provenance and READY-only tool success as insufficient oracles. | Corrected actual-wire credential/request-ID joins and matching successful tool-result transcript passed independent evidence review. Published-wheel qualification passed 2,593 provider/configuration/consumer tests and eight released-gateway Agent loops; reviewed adoption remains separate. |
| Combined acceptance | Owned local IDM, Sandhi and real Victor Agent execute against a synthetic loopback model backend. Corrected run includes actual wire/receipt joins, tool execution and six denial controls. Earlier limited proofs are preserved. | Independent evidence review passed for the local candidate run. Final-source repeat passed three tests with official SDK 0.17.1, combined Sandhi 0.18 and remote-aligned IDM candidates. Independent final-source review passed; hosted CI gates consumer merge. |
| Live development service | Public discovery at `https://id.anvaiops.dev` was checked; required introspection metadata is absent on the current deployment. | Reviewed service rollout, migration/rollback plan and separate live acceptance. No live configuration or credential writes performed. |

Focused commits and their reviews are preserved in a combined post-release train
so shared schema, auth and lockfile changes receive one current integration
qualification. Original #362, #363, #364 and #448 closed only after their
replacement commits were verified as ancestors of merged develop. History-preserving
#447, #453 and #455 also became merged through the train.

## Completion evidence

Each active row needs an exact source commit and reviewed base, independent clean
review, appropriate local qualification, successful required hosted CI, develop
merge commit and post-merge CI. Record the later main promotion and published
version separately. A prepared patch, a green older head or an open replacement
PR is not a merged or released change. All CI uses public hosted runners.
