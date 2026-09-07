# Safeguarded milestone release

Status: safeguard implementation integrated with green post-merge CI; remote ref controls
verified. Registry credential closure (SG07) and final release execution (SG08) remain open.
Tracker: [TD-0026](../td/TD-0026-gateway-product-evolution.md); publish mechanics:
[TD-0023](../td/TD-0023-release-automation.md).

## Outcome and authority

A milestone must publish the reviewed, CI-verified source and a complete, installable set of
declared artifacts without granting build jobs unnecessary publishing authority. A failed or
partial publication must be visible, not reported as a complete release.

UA01–UA05 accept the engineering release scope and its limits. The user authorized
closing release-safeguard gaps, including implementation, tests and plan updates, and has
subsequently requested trusted publishing of a newly built release while retaining existing
packages. Confirm the exact version and target set before execution; reviewed promotion and
green exact-main CI remain prerequisites. Production is not authorized. Do not reopen
accepted product limits to expand M1.

## Tracked work

| ID | Gap and deliverable | Verification / closure condition | State |
|---|---|---|---|
| SG01 | Inventory release trust and artifact gaps; preserve authority boundaries | Source/config evidence, explicit scope and owner dependencies | Complete: initial inventory below |
| SG02 | Reject unauthorized refs/events and ambiguous source identity before build/publish | Stable exact tag syntax; main ancestry; exact successful main CI; immutable commit across jobs; moved/deleted-tag denial; negative tests | Complete: integrated by PR #237, pre/post-merge CI green; publication not performed |
| SG03 | Verify every expected registry/platform artifact, not just a version entry | Missing/yanked/invalid/unavailable differentiated; exact npm dependencies/platforms; binary and wheel coverage; deterministic negative tests | Implemented and locally verified; registry reads are not installation evidence |
| SG04 | Validate prepared npm packages and packed contents before publication | Exact target set, binaries, loader/types and dependencies; no source/nested-binary leakage; safe partial retry | Implemented and locally verified; full release matrix still unexecuted |
| SG05 | Integrate safe release checks into ordinary public CI | Unit/negative tests and workflow contracts green; independent adversarial review; focused develop PR and post-merge CI | Complete: [PR #237](https://github.com/anvai-labs/sandhi/pull/237) merged; exact-head and post-merge CI green; 207 hosted safeguard tests passed |
| SG06 | Restrict release authority outside editable workflow code | Read-back evidence for approved tag/environment controls; preserve current protections; identify repository-secret exposure | Complete for ref controls: two active tag rulesets and four restricted environments independently verified; credential closure remains SG07 |
| SG07 | Confirm registry-side authority without test-publishing | Trusted-publisher bindings and registry token scope verified by authorized owner; artifact presence alone insufficient | Partial: owner supplied root npm publisher configuration; both platform packages, current PyPI binding and crates replacement/revocation remain open |
| SG08 | Final milestone promotion and release | Explicit version/target approval; fresh cumulative review/CI; main post-merge CI; publish/verify/back-sync; P01–P03 still open | Not started; final execution gate |

## Initial findings

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
The current workflow still requires the scoped token. A tokenless migration is an alternative
requiring a reviewed workflow change and per-crate registry configuration; it has not been
implemented or accepted as a replacement for the current checklist.

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
an npm-only scope has been surfaced for confirmation; no new tag or publishing run exists.

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
`cargo publish --no-verify`, short-lived credentials and post-job revocation. No crates workflow
migration, registry binding change or legacy token revocation has been performed.

Local validation: `python3 -m pytest tests/release -q` passed **220 tests, zero skips**.
The sandbox initially blocked npm child-process execution (`EPERM`); the complete offline
suite passed with approved subprocess access. This includes the actual pinned generator and
synthetic-header packaging fixtures, not native ABI execution or registry publication.

These findings describe the pre-change workflow. The local implementation below addresses its
code paths; SG06/SG07 are still required to close external authority gaps.

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
- New `CRATES_RELEASE_TOKEN` is intentionally required from the `crates-io` environment. No
  fallback to the legacy repository secret; no missing-credential success. Owners must configure
  and scope it, then revoke/remove the old token. Neither token contents nor validity were read.

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

SG07 is not complete: the `crates-io` environment currently has no secrets; the legacy repository
`CARGO_REGISTRY_TOKEN` remains. The owner must configure a least-privilege `CRATES_RELEASE_TOKEN`
only in that environment and revoke/remove the legacy token, then confirm registry-side trusted
publishers for PyPI and all three npm packages. Secret values were neither read nor changed.
