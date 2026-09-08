# Safeguarded milestone release

Status: v0.6.0 is fully published and back-synced. W05b is integrated on `develop`; the owner
explicitly selected `v0.6.1` as a documented compatibility exception despite source-breaking
public Rust API additions. Release preparation is in progress. Reuse the existing crates token;
trusted publishing applies only to PyPI/npm. Promotion review, exact-source CI, complete
publication verification and back-sync remain mandatory. Production is not authorized.
See the [v0.6.1 checkpoint](#v061-compatibility-exception-execution-checkpoint-2026-09-08-utc)
and the [current registry decision](#owner-correction-reuse-the-crates-token-2026-09-07).
Tracker: [TD-0026](../td/TD-0026-gateway-product-evolution.md); publish mechanics:
[TD-0023](../td/TD-0023-release-automation.md).

## Outcome and authority

A milestone must publish the reviewed, CI-verified source and a complete, installable set of
declared artifacts without granting build jobs unnecessary publishing authority. A failed or
partial publication must be visible, not reported as a complete release.

UA01–UA05 accept the engineering release scope and its limits. The user authorized
closing release-safeguard gaps, including implementation, tests and plan updates, and has
subsequently requested trusted publishing of a newly built release while retaining existing
packages. After the proposed v0.6.0 all-target release was presented, the owner directed continuing
the plan; execute that milestone with the existing crates token and PyPI/npm trusted publishing.
The owner authorized actual publication to validate registry authorization, without a throwaway
version or another registry-settings confirmation cycle. Reviewed promotion and green exact-main
CI remain prerequisites; registry rejection remains a release failure. Production is not authorized. Do not reopen
accepted product limits to expand M1.

## Tracked work

| ID | Gap and deliverable | Verification / closure condition | State |
|---|---|---|---|
| SG01 | Inventory release trust and artifact gaps; preserve authority boundaries | Source/config evidence, explicit scope and owner dependencies | Complete: initial inventory below |
| SG02 | Reject unauthorized refs/events and ambiguous source identity before build/publish | Stable exact tag syntax; main ancestry; exact successful main CI; immutable commit across jobs; moved/deleted-tag denial; negative tests | Complete: integrated by PR #237, pre/post-merge CI green; publication not performed |
| SG03 | Verify every expected registry/platform artifact, not just a version entry | Missing/yanked/invalid/unavailable differentiated; exact npm dependencies/platforms; binary and wheel coverage; deterministic negative tests | Implemented and locally verified; registry reads are not installation evidence |
| SG04 | Validate prepared npm packages and packed contents before publication | Exact target set, binaries, loader/types and dependencies; no source/nested-binary leakage; safe partial retry | Complete: build/pack validation passed in initial and npm-only repair runs; successful retry published a complete checked package set |
| SG05 | Integrate safe release checks into ordinary public CI | Unit/negative tests and workflow contracts green; independent adversarial review; focused develop PR and post-merge CI | Complete: [PR #237](https://github.com/anvai-labs/sandhi/pull/237) merged; exact-head and post-merge CI green; 207 hosted safeguard tests passed |
| SG06 | Restrict release authority outside editable workflow code | Read-back evidence for approved tag/environment controls; preserve current protections; identify repository-secret exposure | Complete for ref controls: two active tag rulesets and four restricted environments independently verified; credential closure remains SG07 |
| SG07 | Validate the owner-selected registry authority | Owner-authorized existing crates token; PyPI/npm trusted bindings; artifact presence alone insufficient | Complete: crates token reuse, PyPI OIDC and npm OIDC all succeeded in actual publication; npm packages carry signed GitHub Actions provenance |
| SG08 | Final milestone promotion and release | Version/target authority; fresh cumulative review/CI; main post-merge CI; publish/verify/back-sync; P01–P03 still open | Complete: initial [release run](https://github.com/anvai-labs/sandhi/actions/runs/34173783838) built every target and published GitHub/PyPI/crates; npm-only [repair](https://github.com/anvai-labs/sandhi/actions/runs/34188380114) published/verified npm; [PR #244](https://github.com/anvai-labs/sandhi/pull/244) back-sync and exact-develop [CI](https://github.com/anvai-labs/sandhi/actions/runs/34217140784) passed |

## v0.6.1 compatibility-exception execution checkpoint (2026-09-08 UTC)

The owner overrode the normal version recommendation and selected `v0.6.1`. This is an explicit
release-label decision, not a claim that W05b is source-compatible with v0.6.0. The changelog must
retain the migration warning. W05b remains opt-in and non-authoritative; no proxy wiring,
persistence, settlement, pricing, export, UI or enforcement behavior is claimed. P01–P03 remain
open and continue to block production authorization.

- [x] Integrate W05b through PR #246; exact-head CI `34229915141` and post-merge CI
  `34230690137` passed, and the fresh adversarial review was clean.
- [x] Prepare the compatibility-exception record and pass local gates: 284 release-safeguard
  tests, 618 workspace tests, strict all-feature/all-target Clippy, formatting and diff checks.
- [ ] Merge this focused release-preparation record into `develop` after clean review and green
  exact-head CI; verify post-merge `develop` CI.
- [ ] Open and adversarially review the cumulative `develop` to `main` promotion; merge only
  after required CI is green, then verify exact-main push CI.
- [ ] Confirm `v0.6.1` is absent from the tag, GitHub release and every target registry; create
  the immutable tag only at the verified `main` commit.
- [ ] Monitor all builds and publishers: two GitHub archives, three PyPI wheel platforms, four
  crates and the root plus two platform npm packages.
- [ ] Run the independent all-target verifier and record artifact identities and any partial or
  repaired publication without moving the tag.
- [ ] Back-sync immutable release evidence from `main` to `develop` through a reviewed, green PR.

## Current execution checkpoint (2026-09-07)

- [x] Registry-mechanism correction [PR #242](https://github.com/anvai-labs/sandhi/pull/242)
  merged as `c528ad443d34de721cedde83982d741dd64414ec`; exact develop
  [push CI](https://github.com/anvai-labs/sandhi/actions/runs/34161667180) passed.
- [x] Fresh bounded cumulative accounting/recovery and operator/security reviews found no
  release-blocking issue on that exact candidate. Full
  [promotion CI](https://github.com/anvai-labs/sandhi/actions/runs/34163305520) passed,
  including Rust, bindings, coverage, SDK conformance, security and release safeguards.
- [x] [PR #243](https://github.com/anvai-labs/sandhi/pull/243) merged as
  `9d40f01b871c1bb00975ceca55c7279e2b3f3dde`, with a tree identical to the reviewed candidate.
  Only the unavailable human approval was bypassed under existing owner authority; no
  protection was changed and no pending or failing check was bypassed.
- [x] Exact-main [push CI](https://github.com/anvai-labs/sandhi/actions/runs/34163982562),
  attempt 1, passed with executed, successful `Release safeguards` and `CI Success` jobs.
- [x] Create immutable `v0.6.0` at that verified main commit. On 2026-09-08 UTC, fresh
  protected-main ancestry, canonical CI and tag-absence checks passed; active tag immutability
  rules still denied updates/deletions with no bypass actors. The tag resolves to `9d40f01`.
- [x] Finish all release builds and smokes. The tag-triggered
  [release run](https://github.com/anvai-labs/sandhi/actions/runs/34173783838) passed source
  authorization, both binary builds/smokes, all three wheel build/install/import jobs,
  both native npm build/load jobs, staged crates-check and npm-package validation.
- [x] Publish and verify both GitHub archives, all three PyPI wheel platforms and all four crates.
- [x] Publish and verify the root plus both platform npm packages. After the initial and first
  repair attempts failed `ENEEDAUTH`, the owner configured all three per-package trusted
  publishers. Npm-only run `34188380114` then published all three packages with signed provenance;
  its hosted verifier and an independent all-target verifier passed.
- [x] Record actual outcomes and back-sync main into develop through
  [PR #244](https://github.com/anvai-labs/sandhi/pull/244), with fresh exact-head review/CI.

Pre-tag registry checks returned HTTP 404 for every required package's `0.6.0` version;
the GitHub tag and release were absent. These are version-conflict checks, not failed uploads
or evidence of registry authorization. No existing artifact was modified. P01–P03 remain
pre-production gates; accepted engineering evidence and estimate-based budgets are unchanged.

### Publication outcome (2026-09-08 UTC)

| Target | Actual evidence | State |
|---|---|---|
| GitHub | [v0.6.0](https://github.com/anvai-labs/sandhi/releases/tag/v0.6.0); both archives verified for size and SHA-256 | Published and verified |
| PyPI | `sandhi-gateway==0.6.0`; all three non-yanked wheel platforms verified; fresh Linux wheel install/import and installed-version check passed in a disposable environment | Published through trusted publishing |
| crates.io | `sandhi-core`, `sandhi-providers`, `sandhi-store`, `sandhi-proxy` 0.6.0 verified non-yanked | Published with the existing token; no token changed |
| npm | Initial job `101901932706` and first repair `34181729319` failed `ENEEDAUTH`; after owner configuration, [repair run 34188380114](https://github.com/anvai-labs/sandhi/actions/runs/34188380114) published root, Linux x64 and macOS arm64 packages with signed provenance | Published through trusted publishing and verified |
| Aggregate | Successful repair hosted verifier plus independent `verify-release.py v0.6.0 --targets pypi,crates,npm,github --attempts 1` | All explicitly expected targets verified |

The Linux archive SHA-256 is
`f9d1461346d3006c89078a9f2beae38145e43adc0ca199d37bddcd0f51590556`.
Its downloaded build artifact (`10036625056`) contains only executable `sandhi-proxy` and
`sandhi`; isolated CLI help, health/readiness and zero-exit SIGTERM checks passed after an
approved rerun outside the socket-restricted sandbox. The GitHub release asset has the same
digest. These checks use no providers, credentials or durable store and do not establish
production behavior. The macOS archive SHA-256 is
`81c8baaab5f5654922412c4ac42bae3db7068bdbd067b488dbdc8a1fb5b6bf9b`.

The initial npm attempt used Node **24.20.0**, npm **11.19.0**, a public GitHub runner and `id-token: write`;
the publishing helper retains the OIDC variables while withholding `GH_TOKEN` from npm.
Package-repository identity and complete checked tarballs passed validation. These facts rule
out an unsupported CLI version or a missing repository-field build defect, but `ENEEDAUTH`
does not identify the exact registry/OIDC exchange cause. Npm's matching CLI implementation
falls back to that generic error when OIDC does not yield credentials.

The owner subsequently configured each package to authorize owner `anvai-labs`, repository
`sandhi`, workflow `release.yml`, environment `npm`, and direct `npm publish`, following
[npm's setup guidance](https://docs.npmjs.com/trusted-publishers/). The successful repair reused
the immutable tag and safeguarded npm-only path; it did not move the tag, substitute an npm token,
or rebuild/overwrite the already published GitHub/PyPI/crates artifacts.

## Initial findings

### Owner correction: reuse the crates token (2026-09-07)

The owner clarified that the existing crates.io token is **not revoked and should be reused**;
trusted publishers are used only for PyPI and npm. This supersedes earlier requirements below
to migrate crates to OIDC, create an environment replacement token, or revoke the existing token.
Those dated sections remain historical evidence, not current release instructions.

- Restore `secrets.CARGO_REGISTRY_TOKEN` only in the crates upload-step environment. Its
  repository-level presence was read back; value, validity, permissions and expiry were not read.
- Remove the crates OIDC action and `id-token: write`; leave PyPI/npm OIDC unchanged.
- Retain first-party stdlib staging, unprivileged compilation, proof checks before uploads,
  `cargo publish --no-verify`, all-build gates, immutable source/action pins and environment rules.
- Do not revoke/remove/rotate the token or require four crates trusted-publisher bindings.
  The repository-scoped secret remains available to other workflows that request it; this
  exposure is retained under the owner's choice, not falsely described as environment isolation.
- The owner confirms the intended PyPI/npm mechanism; prior root npm settings and historical
  PyPI upload evidence remain recorded. This does not independently read back every current
  per-package binding. Failures during authorized publication remain failures, not optional skips.

No secret value, registry settings, existing package, tag or main branch was changed by this
correction. Reviewed/green integration and main promotion still precede v0.6.0 publication.

Local correction validation: **284 release tests passed, zero skips**; independent workflow
review passed **33 tests** with no blocking findings. Actionlint 1.7.12 and `git diff --check`
pass. These checks validate workflow wiring, not the reusable token's registry permissions.

### Owner publisher evidence (2026-09-07)

The owner supplied the trusted-publisher settings for `@anvailabs/sandhi`: repository
`anvai-labs/sandhi`, workflow `release.yml`, environment `npm`, with both `npm publish`
and `npm stage publish` permissions. This confirms the reported root-package configuration,
not an authenticated read-back or a successful new publication. The two platform packages
require their own confirmation; the root setting does not cover them automatically.

Read-only GitHub checks also found the `pypi-publish` job and its upload step successful in
[the v0.5.1 run](https://github.com/anvai-labs/sandhi/actions/runs/33708378080).
That is historical execution evidence, not confirmation of today's registry settings.
The latest npm repair [run](https://github.com/anvai-labs/sandhi/actions/runs/33740352186)
failed in its publishing step; the job conclusion alone does not identify the cause.

The owner requested a publishing attempt. Preflight found protected `main` still at
`72ced4bdcbf479ac5aeda6d513674bb91803ecda`, while the safeguard implementation is on
`develop` at `cd0a87f011d29c87a99ae81de8bd43d57a21d604`. The `crates-io` environment
secret list remains empty. No workflow was dispatched, tag created, package published,
or token revoked during this check. Continue through SG07 and reviewed promotion before
publishing an explicitly selected new version; do not rerun the legacy workflow as a probe.

Crates.io now also supports [OIDC trusted publishing](https://blog.rust-lang.org/2025/07/11/crates-io-development-update-2025-07/).
At this checkpoint the workflow still required a scoped token. The subsequently authorized
tokenless migration and per-crate registry prerequisites are tracked below.

### Platform-package removal preflight (2026-09-07)

The owner requested removing the two platform packages on suspicion of prior token-based
publication. Read-only npm registry metadata shows both `@anvailabs/sandhi-linux-x64-gnu`
and `@anvailabs/sandhi-darwin-arm64` contain versions `0.5.0` and `0.5.1`, with `latest`
pointing to `0.5.1`. Neither version advertises `dist.attestations` or repository metadata.
Absent attestations do not establish the authentication method or prove compromise.

The existing root `@anvailabs/sandhi@0.5.0` explicitly pins both platform packages as optional
dependencies. Unpublishing would remove those native dependency targets. Under the
[npm unpublish policy](https://docs.npmjs.com/policies/unpublish/), deleted package/version
pairs cannot be reused; entirely removing a package also imposes a 24-hour publishing hold.
These packages are older than 72 hours and have a published dependent, so the documented
self-service unpublish criteria are not met.

No unpublish or deprecation was attempted. A bounded npm owner-session check returned
`E401 Unauthorized`; registry governance changes require an authenticated owner session.
The recommendation is to retain existing artifacts, configure each platform package's trusted
publisher, and publish a reviewed new version through the safeguarded release train. After
verification, restrict traditional token publishing and revoke obsolete automation tokens
following [npm's migration guidance](https://docs.npmjs.com/trusted-publishers/).
Removal or deprecation must not be recorded as complete, or substituted for publisher setup.

### New-build trusted publishing follow-up (2026-09-07)

The owner withdrew the removal request and requested trusted publishing of a new build.
Retain all existing npm packages and versions. The planned v0.6.0 all-target release versus
an npm-only scope was surfaced; the owner subsequently directed continuing the plan toward
v0.6.0 across all targets. No new tag or publishing run exists.

The current npm workflow already uses OIDC. The pinned NAPI generator copies the source
repository into platform manifests. Packaging now additionally rejects missing or mismatched
repository identity on all three manifests, before preparation or packing, with positive and
negative regression tests. This is preventive release validation, not a finding that the new
generator omitted the repository. Registry-side bindings still require owner confirmation or
authorized release execution; local npm login is not needed for GitHub's OIDC publication.

Independent review identified an additional requirement if crates publishing migrates to OIDC:
`id-token: write` applies to the entire job. Merely adding authentication after the existing
`cargo install cargo-edit` step would expose OIDC authority to dependency build scripts.
Remove third-party compilation from that job before granting OIDC, using reviewed first-party
version staging or immutable outputs from an unprivileged staging job. Preserve proof checks,
`cargo publish --no-verify`, short-lived credentials and post-job revocation. Registry bindings
and legacy token revocation remain account-side prerequisites, not consequences of a code change.

Local validation: `python3 -m pytest tests/release -q` passed **220 tests, zero skips**.
The sandbox initially blocked npm child-process execution (`EPERM`); the complete offline
suite passed with approved subprocess access. This includes the actual pinned generator and
synthetic-header packaging fixtures, not native ABI execution or registry publication.

### Crates OIDC migration in progress (2026-09-07)

Historical implementation checkpoint; integration subsequently completed as recorded below.

- [PR #239](https://github.com/anvai-labs/sandhi/pull/239), head
  `af7d36bc252e993ad19dcee8a1d563b2b0f1ba6c`, has clean independent review and **220 local
  passing release tests**. Hosted CI remained queued at this checkpoint; no merge bypassed it.
- The crates job now requests OIDC using official action `v1.0.5`, pinned to
  `c6f97d42243bad5fab37ca0427f495c86d5b1a18`. Only the upload step receives the temporary
  token; the action's post step revokes it. No stored-token fallback is permitted.
- A first-party stdlib version-staging helper replaces dependency/tool compilation throughout
  the privileged crates job; the unprivileged check job uses the same helper. Proof checks
  precede credential exchange and every upload. All-build gating and `--no-verify` remain.
- Owner setup changes from creating `CRATES_RELEASE_TOKEN` to four per-crate trusted-publisher
  bindings. Do not treat GitHub authentication as a crates.io owner session, and do not claim
  legacy token revocation from deleting a secret or migrating a workflow.
- This migration is not yet integrated or remotely validated. Registry bindings and revocation,
  cumulative main promotion/review/CI, the actual build/publish matrix and final artifact
  verification remain open. Existing artifacts and production acceptance gates remain unchanged.

Migration validation: the full offline release suite passed **284 tests, zero skips**;
independent staging/workflow review passed **87 tests** with no blocking findings. Pinned
Actionlint 1.7.12 and `git diff --check` pass. A disposable copy checked with
`cargo metadata --offline --no-deps` contains exactly four `0.6.0` packages and five internal
dependency requirements at `^0.6.0`. Real source manifests and lockfiles were not staged.
This is metadata/staging evidence, not a release-mode compilation or publishing test.

### Trusted-publishing integration checkpoint (2026-09-07)

Historical checkpoint: the later owner correction above supersedes the crates OIDC/revocation
requirements, without undoing the recorded test and integration evidence.

| Change | Reviewed head | Develop merge | Executed pre/post-merge CI |
|---|---|---|---|
| [PR #239](https://github.com/anvai-labs/sandhi/pull/239): npm repository identity | `af7d36bc252e993ad19dcee8a1d563b2b0f1ba6c` | `4ba75d172ebd995c0044039165dda5d9856ed271` | [34147426407](https://github.com/anvai-labs/sandhi/actions/runs/34147426407), [34148138831](https://github.com/anvai-labs/sandhi/actions/runs/34148138831): success |
| [PR #240](https://github.com/anvai-labs/sandhi/pull/240): crates OIDC and stdlib staging | `b272f1b0b869f781d4948ed712e3b5507042caeb` | `76316690f2f27268a1da7cf3c633219e06396be5` | [34148093698](https://github.com/anvai-labs/sandhi/actions/runs/34148093698), [34148361660](https://github.com/anvai-labs/sandhi/actions/runs/34148361660): success |

Both heads received clean independent review before merge. Only the previously authorized
missing-human-approval admin bypass was used; no failed/pending CI was bypassed, and branch
protection/ref controls were not relaxed. Hosted migration CI reported **284 passed, zero skips**
and passed pinned Actionlint. Post-merge `Release safeguards` and `CI Success` executed and
succeeded on public GitHub runners; `OWNER_PRIVATE_CI_ENABLED` remains `false`.

The develop merge tree at `76316690` is byte-identical to reviewed migration head `b272f1b0`. Additional
bounded cumulative reviews against main `72ced4bd` found no substantiated new blockers:

- Accounting/reservation/recovery/shutdown review: **134 focused Rust tests passed**, including
  98 core tests; accepted estimated-token and single-node guarantees are unchanged.
- Operator/admin/dashboard/vault/error-boundary review: **89 browser/management/recovery
  tests passed, zero skips**, plus **3 Rust lifecycle tests**. The initial restricted invocation
  hit socket `EPERM`; its approved rerun passed. This is synthetic evidence, not live integration.
- Fresh synthetic acceptance journeys: **5 passed** via
  `timeout 180s python3 -m pytest tests/sdk-conformance/test_acceptance_decisions.py -q` with
  approved browser/localhost access. An earlier restricted invocation stalled without results
  and was terminated before the bounded rerun. No hands-on usability acceptance is claimed.

These scoped reviews prepare promotion; they do not replace cumulative promotion-PR CI or
exact-main post-merge CI. Main has not been promoted and no release tag, registry upload,
package removal/deprecation, registry publisher edit or credential revocation was performed.

Remaining owner gate: confirm the two platform npm bindings, current PyPI binding, and all four
crates.io bindings using [the owner checklist](../../RELEASING.md#owner-confirmation-for-sg07).
Confirm legacy token revocation at crates.io separately from removal of the GitHub repository
secret after the replacement path is established. No new long-lived crates secret is required.
Then complete reviewed main promotion, exact-main CI, the v0.6.0 build/publish matrix and final
artifact verification. P01–P03 production gates remain explicitly open.

These initial findings describe the pre-change workflow. The implementation below addresses its
code paths; SG06 ref controls are now complete and SG07 registry authority closure remains open.

The current release workflow triggers on broad `v*.*.*` tags and accepts an npm repair from any
dispatch ref. Its shell pattern is not strict SemVer. Builds/publishers resolve tag names
independently, and there is no main-ancestry or exact main-CI gate. Top-level `contents: write`
also reaches jobs that only need read access, and release actions use mutable refs.

The verifier conflates unavailable registries with absent packages, checks only the main npm
version, and does not establish complete binary/wheel/platform coverage. npm packaging requires
only one native binary; deletion of already-published package directories before manifest
generation risks incomplete dependency metadata. The immutable previously published v0.5.1
main npm manifest lacks the current expected optional-dependency structure; a passing legacy
presence check does not certify that package's installation behavior.

Read-only GitHub preflight reports unrestricted `npm`/`pypi` environments and no repository
rulesets. A repository-level crates token is not protected merely by adding an environment to
one workflow job: another same-repository workflow can still request that secret. Do not claim
environment isolation until the registry credential is scoped accordingly by its owner.

## Design boundaries

Use a read-only authorization job; validate tag/event/source before outputs are consumed. Check
out immutable commit SHAs, not mutable repair input in publisher jobs. A stable release must
have successful CI from the canonical CI workflow's main push for that exact source commit;
an unrelated check named `CI Success`, a PR run, skipped mirror or an older successful attempt
must not stand in for that evidence. Revalidate the tag at publication time.

Privilege belongs to publishing jobs, not builders. Pin actions by full commit SHA, disable
persisted checkout credentials and serialize publications for the same tag without canceling
an in-progress release. Environment/tag controls must guard the entry point even if someone
changes workflow code. Do not relax branch protections or infer OIDC correctness from package
presence. Stable releases only; prerelease distribution requires separate explicit support.

GitHub documents [immutable action pinning](https://docs.github.com/en/actions/reference/security/secure-use)
and [environment branch/tag restrictions](https://docs.github.com/en/actions/reference/workflows-and-actions/deployments-and-environments).
Those are complementary controls, not a substitute for exact source/artifact verification.

No old release is republished merely to test permissions. Use disposable fixtures for regression
tests; package preflight may pack/build locally but must not invoke a registry write.

## Implementation evidence (2026-09-07 UTC)

- `python3 -m pytest tests/release -q`: **202 passed**, including real offline npm packing,
  registry transport failures, proof substitutions, workflow-negative mutations and binary-smoke
  failure paths. The local sandbox denied npm child execution, so the same offline suite ran
  with approved subprocess access; no registry writes occurred.
- The actual pinned NAPI 2.18.4 manifest generator passed the prepare/three-package pack
  integration fixture. Its native headers are synthetic; this is packaging compatibility
  evidence, not a native ABI or cross-platform installation test.
- Independent scoped adversarial review is clean after fixing the legacy crates dispatch/input
  route, proxy `--help` hang, unchecked npm SHA-512 retry identity and proof-version binding.
- Actionlint 1.7.12 (official release archive SHA-256 verified) passes for the release, retired
  publisher and CI workflows. The same pinned syntax check is included in ordinary CI.
- Existing local debug binaries passed CLI help, exact loopback `/healthz` and `/readyz`
  responses, no-store readiness and zero-exit SIGTERM. This is not a freshly built release
  archive, a version check, provider traffic, persistence test or cross-platform pass.
- Ordinary CI now runs `Release safeguards` without a path filter and includes it in `CI Success`.
  The unified workflow pins source/control SHAs and action commits, selects the proof/packed npm
  bundle by artifact ID, separates builds from publishing, and retires the manual crates bypass.
- The initial implementation required `CRATES_RELEASE_TOKEN` from the `crates-io` environment,
  with no legacy fallback or missing-credential success. The OIDC migration above supersedes
  creating that token; legacy token revocation remains necessary. No token values were read.

The full new cross-platform release matrix and actual registry publication have **not** run.
Local tests and previous M1 engineering evidence cannot substitute for those execution results.
Remote CI, registry credential closure and final release approval remain explicitly open. No
tag, package upload or production deployment has been performed. The authorized remote ref
restrictions have now been applied and read back as recorded below.

## SG05 follow-up and SG06 closure (2026-09-07 UTC)

The user continued the plan after the recommended tag/environment restrictions were presented;
the scoped remote operations were approved. No branch protection was relaxed.

- [CI run 34095880588](https://github.com/anvai-labs/sandhi/actions/runs/34095880588) completed
  with one failure: Python 3.12 accepted 2,000-deep JSON that the local interpreter rejected.
  The verifier now enforces a 64-container limit before decoding, independently of interpreter
  recursion behavior. Escaped strings and alternate encodings have explicit negative coverage.
- That run also skipped the optional NAPI generator fixture because its tooling was absent.
  CI now installs the lockfile-pinned CLI without lifecycle scripts; the generator test is required,
  not skippable. All other substantive jobs in the original run passed; its aggregate failed,
  so no merge was attempted.
- Updated local suite: **207 passed, zero skips**; Actionlint clean. Independent delta review
  passed 53 verifier/workflow tests and the actual required generator fixture. The updated
  remote and post-merge CI subsequently passed as recorded below.

### Integration evidence

[PR #237](https://github.com/anvai-labs/sandhi/pull/237) merged into `develop` as
`f790ca71879ba8d87d83cbd3aed2e5e351d9f3ad` after independent clean review of exact head
`80692e23efd390f420c7d96aed6c1c57ba34f4b2` and successful
[CI 34107985444](https://github.com/anvai-labs/sandhi/actions/runs/34107985444).
That hosted safeguard job reported **207 passed, no skips**. The tested base remained
`cf469bcbd06679109d0803b3ad47f66a77e4ac51` at merge.

[Post-merge CI 34131251842](https://github.com/anvai-labs/sandhi/actions/runs/34131251842)
passed on the exact merge commit, including executed `Release safeguards` and `CI Success`.
All substantive CI jobs passed on public runners. The owner's existing administrative bypass
authorization was used only for the unavailable human approving review; no failed/pending check
was bypassed and no branch protection was changed. This closes integration, not SG07 or a release.

### Applied ref restrictions

Read-back [evidence](evidence/release-controls-2026-09-07.json) records:

| Control | Applied configuration |
|---|---|
| [Tag creation rule 22437305](https://github.com/anvai-labs/sandhi/rules/22437305) | Active for `refs/tags/v*`; only repository administrators may bypass creation restriction |
| [Tag immutability rule 22437308](https://github.com/anvai-labs/sandhi/rules/22437308) | Active for the same tags; update/deletion prohibited, no bypass actors |
| `npm` environment | Only `v*` tags and the `main` branch (npm repair) |
| `pypi`, `crates-io`, `github-release` environments | Only `v*` tags |
| Main/develop protection | Full API protection JSON independently compared with pre-change snapshots: unchanged |
| CI routing | Public runners retained; `OWNER_PRIVATE_CI_ENABLED=false` |

The retired manual crates workflow (`317193810`) was subsequently disabled through GitHub and
read back as `disabled_manually`; its only recorded run was already complete, and no runs were
deleted or canceled. The code stub and service-level disable complement each other, but neither
revokes the legacy repository token or closes all historical credential access paths.

These are ref restrictions, **not** required human environment-review rules. No test tag or
publishing job was created to prove denial. An administrator can still edit repository settings;
historical workflow authority is not fully closed by these controls alone.

At this checkpoint SG07 was incomplete: the `crates-io` environment had no secrets and the legacy
repository `CARGO_REGISTRY_TOKEN` remained. The later OIDC migration supersedes creating a new
environment token; the owner must confirm per-crate trusted publishers and revoke/remove the
legacy token, plus confirm current PyPI and platform npm bindings. Secret values were neither
read nor changed.
