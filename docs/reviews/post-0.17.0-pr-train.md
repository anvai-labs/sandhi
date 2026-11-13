# Development PR train after 0.17.0

This ledger records the disposition and next gate for every PR open during the
2026-11-13 release audit. Unique, aligned changes proceed through `develop` and a
later reviewed promotion. They do not silently alter the frozen 0.17.0 source.
Update the row when a candidate, review, CI, merge or release milestone changes;
link the actual evidence before marking it complete.

## Current 0.18 access and publication checkpoint

The owner selected a credential-free local starter, with optional token and OIDC
profiles. The authenticated public development integration is qualified in
dependency order before the Sandhi → official SDK → Victor release train.

| Gate | Current evidence | Remaining |
| --- | --- | --- |
| Starter and durable execution | Baseline: 1,322 Rust and 61 startup/recovery tests passed. Independent review reproduced a missing inference browser-origin fence. The corrected artifact passed 31 starter HTTP cases and an independent 51-case attack/legitimate-client matrix, with zero model dispatch for hostile requests. Exact review and all 24 executed public checks passed through [#459](https://github.com/anvai-labs/sandhi/pull/459) | Complete; retained in reviewed main promotion #461 |
| Victor Agent interoperability | Corrected gateway and reviewed Victor source passed 16 actual tool-loop cases plus three Anvai resource-token session cases with the optimized IDM and official SDK 0.17.1 | Official SDK 0.18 adoption after publication |
| Dashboard and documentation | Three starter browser cases, 12 script security regressions and actual Anvai browser SSO passed. Admin/operator/viewer/unassigned roles, TLS/cookies/CSRF/logout and replay refusals qualified; actual local screenshots included. Reviewed source is promoted through #461 | This public-issuer evidence update needs its own docs review and CI; live dashboard SSO remains separate |
| IDM source | Reviewed source includes introspection, explicit consumer registration and schema 70; the optimized deployment artifact was qualified | Retain artifact-bound deployment evidence |
| Public development IDM | Owner/passkey-approved distinct clients and the actual public-issuer PKCE/UserInfo/resource-token → Victor → protected local Sandhi flow passed: two synthetic attempts, two 14-token settlements and actual wire/SQL/issuer-receipt joins | Live gateway browser SSO and additional live lifecycle controls remain unqualified; generic Python-UA 403 is a separate compatibility observation |
| Deployment tooling | Narrow replacement path reviewed; nine guards and synthetic 67→70/catalog/backup checks passed. Actual-host artifact/configuration preflight and private backup integrity passed before migration. Private operations PR #257 and exact post-merge CI passed | Preserve artifact-bound deployment evidence; future changes need their own acceptance |
| One-shot public PKCE publisher | Reviewed example merged through private PR #258. Ten local adversarial guards passed; real public-issuer native PKCE/UserInfo/resource-token/private-file access now passed with owner-approved registrations | Consent-label presentation correction tracked separately; no refresh or automatic renewal claim |
| Publication | Sandhi 0.17.1 remains published; 0.18 and Victor 0.29 remain candidates | Sandhi release/artifact verification, then Victor official-SDK qualification and release |

The development upgrade uses a private backup kept on the existing host and
compares existing data and custody hashes there. No live identity database is
exported to local test fixtures. Full rollback rehearsal was waived for this
development endpoint; failures remain closed while corrected forward.

An earlier default-Python-user-agent 403 was initially overstated as an actual
Sandhi transport blocker. Direct tests of the clients' existing declared user
agents and the actual gateway corrected that conclusion. No Cloudflare policy
was changed. The optional configuration tool's existing credential could not
inspect zone settings; its GET-only 403 made no mutation. Genuine owner/passkey
approval subsequently completed the distinct live registrations used by the
[public-issuer Agent acceptance](#public-issuer-agent-acceptance).

Checkpoint [PR #459](https://github.com/anvai-labs/sandhi/pull/459) passed all
24 executed public checks, including both Linux/macOS release-feature binary
preflights, and merged as `0af02a79`. The merge tree matches the reviewed
candidate. The documentation follow-up records corrected public-profile
evidence. The public-issuer Agent run is now qualified within the scope below;
final release verification and package publication remain open.

[Promotion #461](https://github.com/anvai-labs/sandhi/pull/461) merged the reviewed
source to main as
[`b6aa6b72052ced1c4376c3652edaefb61b182a1c`](https://github.com/anvai-labs/sandhi/commit/b6aa6b72052ced1c4376c3652edaefb61b182a1c).
The exact-main independent review is clean.
[Exact-main public CI](https://github.com/anvai-labs/sandhi/actions/runs/43463768724)
is running at this checkpoint. Its success gates release tagging; publication is
complete only after artifact verification.

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
passed on unchanged-source attempt 2. The only underlying first-attempt failure
was external PyPI DNS resolution before the macOS Python 3.11 tests; only that
job and its dependent aggregate were rerun. The later docs-only develop CI also
passed. Local Rust, fresh binding, ONNX and documentation checks passed. This source merge does not change the published 0.17.1 feature set.

## Follow-up status

| PR | State and owner lane | Evidence and next gate |
| --- | --- | --- |
| [#395](https://github.com/anvai-labs/sandhi/pull/395) | **Closed: superseded**; dependency audit | Its complete protocol 0.19.1 pin/checksum update is duplicated in #448, while #447 tracks the later source-identical protocol candidate. No unique implementation discarded. |
| [#410](https://github.com/anvai-labs/sandhi/pull/410) | **Merged; post-merge CI passed**; maintenance | Exact head `c6907009` passed independent review and hosted checks. Normal merge `eec61506`; [post-merge CI](https://github.com/anvai-labs/sandhi/actions/runs/43381316579) passed. Immutable Actions pin updates only; not included in tagged 0.17.0. |
| [#396](https://github.com/anvai-labs/sandhi/pull/396) | **Merged; post-merge CI passed**; documentation maintenance | History-preserving head `d1f0f6a` passed exact independent review and hosted checks, then merged as `bc919774`. Strict MkDocs, three documentation and ten brand tests passed. [Post-merge CI](https://github.com/anvai-labs/sandhi/actions/runs/43382440829) passed. |
| [#447](https://github.com/anvai-labs/sandhi/pull/447) | **Merged through #456**; protocol maintenance | Minimal 0.20.0 protocol pin/checksum update. Published protocol source files and dependency declarations match 0.19.0/0.19.1 apart from release metadata. 717 proxy/store feature tests and Clippy passed; exact independent review is clean. Independently reviewed `f89d2174` is included in merged PR #456; combined and exact post-merge CI passed. |
| [#362](https://github.com/anvai-labs/sandhi/pull/362) | **Closed: replacement merged in #456**; provider contracts | Preserve useful vendor envelope admission through explicit backend profiles. The old catalog-inferred selection conflicts with current authority boundaries. Unsupported envelopes currently remain unqualified; no invented counts demonstrated. Explicit replacement `7c77526` passed scoped independent review and is combined with the corrected common source fence in the reviewed integration; full integration and post-merge CI passed. |
| [#363](https://github.com/anvai-labs/sandhi/pull/363) | **Closed: replacement merged in #456**; catalog | Candidate `0e9bb72` refreshes primary provider facts and passed catalog and combined-source review; catalog data must not enable routes or enlarge admission budgets. |
| [#364](https://github.com/anvai-labs/sandhi/pull/364) | **Closed: replacement merged in #456**; typed contract | Its proposed typed `output.reasoning` field is unique. Existing validated reasoning extensions already prevent buffered display loss. Candidate `da84a95` uses additive minor 14 and regenerated schemas/facades; core, codec, four-family/Codex and fresh binding checks passed. Full integration and post-merge CI passed. |
| [#448](https://github.com/anvai-labs/sandhi/pull/448) | **Closed: qualified split replacements merged in #456**; dependencies/security | All unique grouped updates have reviewed replacements. The original grouped head failed release-feature API compilation with ORT rc15. The split adaptation and OIDC 4.1 scoped reassessment are now included in the reviewed integration. The compatible serde_json/Tokio/libc split is [PR #455](https://github.com/anvai-labs/sandhi/pull/455), with 1,303 Rust tests, Clippy and three advisory checks passed. Separate ORT `815fa3c` preserves existing ownership behavior; OIDC `68a5769` removes the patched bigint ignore and retains bounded RSA review. Combined independent review and local qualification passed; full integration and post-merge CI passed. |

Stale auto-merge was disabled on #362, #363 and #364 while their unique work was
reworked. Those older heads are now closed after their replacements merged. No PR was closed solely because of age. The
[full initial audit](https://github.com/anvai-labs/sandhi/pull/451#issuecomment-6846017883)
records all eight dispositions.

## Corrective release and documentation queue

| Item | State / owner lane | Next gate |
| --- | --- | --- |
| Release smoke fixture | **Merged to corrective main**; release tooling | Private durable ledger and empty credential store; actual binary smoke passes. The actual smoke also runs in ordinary CI so this startup contract is tested before tags. Reviewed `ed5b0fea` is integrated; the actual-binary smoke passed public PR and promotion CI. Published 0.17.1 includes this correction; exact-main and all-target verification passed. |
| Current Chat stream finality | **Merged to corrective main**; accounting | Standard and terminal-choice totals followed by new output must remain partial/unresolved. Preserve harmless identical-total idempotence, both budget holds and one physical send. Reviewed backport `60316e1` and full PR/promotion CI passed. Published 0.17.1 includes this correction; exact-main and all-target verification passed. |
| 0.17.1 correction | **Published; all four targets verified**; release coordination | Exact reviewed main `10caae8`, immutable tag, successful release attempt 2 and independent final verifier are complete. Failed 0.17.0 tag remains immutable. Victor's exact binding pin, published-wheel qualification, reviewed consumer merge and all five post-merge workflows passed; its package publication remains separate. |
| Generic OIDC guide | **Merged through #457**, with later reviewed updates; documentation | Provider-neutral requirements, the [verified public-issuer Agent flow](../operator/oidc-sso.md#public-development-issuer-verified-agent-flow) and [local browser/lifecycle fixture](../operator/oidc-sso.md#local-co-design-victor-sandhi-and-anvaiops-development-idm) distinguish source, deployment and acceptance scope. The earlier missing discovery metadata was resolved by the reviewed IDM development upgrade. |

## Three-project identity co-design

The [co-design walkthrough](../operator/oidc-sso.md#local-co-design-victor-sandhi-and-anvaiops-development-idm)
is the common usage example: separate Victor origin and Sandhi consumer clients,
supported owner/passkey registration and two actual Agent/tool-loop calls joined
to two 14-token receipts. The original local fixture separately qualified browser
roles, rotation and refusal controls. The later public-issuer native flow used
real approved registrations, the official published SDK 0.17.1, reviewed Victor
source and the protected Sandhi 0.18 source candidate. Earlier SDK 0.17.0 evidence
remains historical; no official SDK 0.18 acceptance is implied.

| Milestone | Verified state | Remaining gate |
| --- | --- | --- |
| IDM online resource-token purpose | Independent source review and local HTTPS/PostgreSQL login, PKCE, receipt, introspection, atomic issuance rollback, cap/race and revocation controls passed. The reviewed optimized development artifact/schema 70 is deployed with existing data/custody preserved. | Broader live expiry, revocation and ACL-removal controls remain separate from the positive public run. |
| Supported operator registration | Genuine owner review/passkey/apply created a public native Victor origin, distinct confidential Sandhi consumer and explicit resource/consumer permissions on the public development issuer. | The consumer has no browser assignment; live gateway browser SSO remains unqualified. |
| Sandhi resource audience | Optional `access_token_audience` preserves omitted `client_id` behavior while keeping browser/Basic identity separate. Fresh aligned build, 23 auth tests, corrected observer/raw tests and scoped independent review passed. | Combined independent review, local checks, public integration CI and exact post-merge CI passed; the feature is merged to develop. This feature is absent from published 0.17.1. |
| Victor broker/provider path | Existing private token-file owner retained; 24 credential tests passed. Initial local run proved calls and denials, but review rejected mutable-client-state token provenance and READY-only tool success as insufficient oracles. | Corrected actual-wire credential/request-ID joins and matching successful tool-result transcript passed independent evidence review. Published-wheel qualification passed 2,593 provider/configuration/consumer tests and eight released-gateway Agent loops. [Victor PR #1771](https://github.com/anvai-labs/victor/pull/1771) merged as `17a8b197`; all five exact post-merge workflows passed, including [ci-fast](https://github.com/anvai-labs/victor/actions/runs/43402272287). Victor remains unpublished. |
| Combined acceptance | Local fixture acceptance passed three tests with official SDK 0.17.1, actual tools, joined receipts and six refusal controls. The later real public-issuer native flow independently passed two successful Agent calls with the same 14-token synthetic source split. | Sandhi 0.18 and Victor remain unpublished; official SDK 0.18 adoption and additional live controls retain their own gates. |
| Live development service | `https://id.anvaiops.dev` now advertises qualified Basic introspection; actual Sandhi/broker transport, protected gateway startup and the owner-approved native Agent flow passed. No Cloudflare policy change was required. | Dashboard browser SSO, HTTPS gateway serving, renewal and paid-provider acceptance were not exercised by this run. |

### Public issuer Agent acceptance

Verified on **2026-11-13 UTC**. One 300-second `online_gateway` token with
`gateway.access` was published to a private `0600` file and reused for two calls.
The public origin had no client secret; Sandhi authenticated introspection as its
separate confidential consumer. The audience `https://localhost:43168` identifies
the resource; the protected test gateway itself used loopback HTTP.

| Qualified component | Source/version | Artifact SHA-256 |
| --- | --- | --- |
| Deployed Anvai Identity | Runtime source `cde3469274077e9b706e11e0abc848afa1ffd12f`; example merge `63f72c28eac25a93d34689347264ca5ca859da28` | `ef963615eaf1b32c9acd4bb10159f7b6085a1b86e53262040b2082312ca03129` |
| Protected Sandhi | Unreleased 0.18 source candidate | `e8931f18e0f44756f4aada842fce10a78ca3c5ffea842af9dd3e348a59d365e3` |
| Victor | Reviewed source `268dfeedee27ed34d0dfa30b0ddaca710d51c780` | Source identity; no published 0.29 artifact claim |
| Official Python SDK | Published `sandhi-gateway==0.17.1` | Native module `55a22940c7acc4920c7f4abc5695b7337f2718a0b383b86783304a64896d9d84` |

The two physical model attempts each settled **14 tokens**, totaling **28**.
Actual wire bearer hashes and returned request IDs joined to durable execution
records and the live issuer's matching issuance receipt; the read-tool transcript
matched the owned test file. Only artifact/source identities are published here;
owner/client identifiers and token/request correlation values remain private.

This verifies native Agent access through Sandhi's optional OIDC profile with
synthetic model usage. The publisher used OAuth code/S256, authoritative HTTPS
OpenID UserInfo and the advertised custom resource-access grant; no broker
ID-token verification or general OIDC certification is claimed.
No new live denial/revocation controls, automatic renewal, live gateway browser
SSO, HTTPS gateway serving, paid-provider calls or cache-savings measurement are
claimed. The earlier local controls retain their separate provenance. Sandhi's
local administrator binding and fixture-only grant were explicit, not inferred
from the IAM scope. The consent scope-label display correction is tracked
separately from the verified registration and token semantics.

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
