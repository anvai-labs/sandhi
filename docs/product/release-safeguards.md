# Safeguarded milestone release

Status: implementation in progress, authorized by the user on 2026-09-07 UTC.
Tracker: [TD-0026](../td/TD-0026-gateway-product-evolution.md); publish mechanics:
[TD-0023](../td/TD-0023-release-automation.md).

## Outcome and authority

A milestone must publish the reviewed, CI-verified source and a complete, installable set of
declared artifacts without granting build jobs unnecessary publishing authority. A failed or
partial publication must be visible, not reported as a complete release.

UA01–UA05 accept the engineering release scope and its limits. The user has now authorized
closing release-safeguard gaps, including implementation, tests and plan updates. This is not
yet a command to create a release tag, publish packages or deploy production. Final release
execution under UA06 remains explicit. Do not reopen accepted product limits to expand M1.

## Tracked work

| ID | Gap and deliverable | Verification / closure condition | State |
|---|---|---|---|
| SG01 | Inventory release trust and artifact gaps; preserve authority boundaries | Source/config evidence, explicit scope and owner dependencies | Complete: initial inventory below |
| SG02 | Reject unauthorized refs/events and ambiguous source identity before build/publish | Stable exact tag syntax; main ancestry; exact successful main CI; immutable commit across jobs; moved/deleted-tag denial; negative tests | Implemented and locally verified; remote CI pending |
| SG03 | Verify every expected registry/platform artifact, not just a version entry | Missing/yanked/invalid/unavailable differentiated; exact npm dependencies/platforms; binary and wheel coverage; deterministic negative tests | Implemented and locally verified; registry reads are not installation evidence |
| SG04 | Validate prepared npm packages and packed contents before publication | Exact target set, binaries, loader/types and dependencies; no source/nested-binary leakage; safe partial retry | Implemented and locally verified; full release matrix still unexecuted |
| SG05 | Integrate safe release checks into ordinary public CI | Unit/negative tests and workflow contracts green; independent adversarial review; focused develop PR and post-merge CI | In progress: hosted JSON-depth failure fixed; 207 local tests, no skips, clean review; [PR #237](https://github.com/anvai-labs/sandhi/pull/237) awaiting updated CI |
| SG06 | Restrict release authority outside editable workflow code | Read-back evidence for approved tag/environment controls; preserve current protections; identify repository-secret exposure | Complete for ref controls: two active tag rulesets and four restricted environments independently verified; credential closure remains SG07 |
| SG07 | Confirm registry-side authority without test-publishing | Trusted-publisher bindings and registry token scope verified by authorized owner; artifact presence alone insufficient | Pending: owner/account-side confirmation may be required |
| SG08 | Final milestone promotion and release | Explicit version/target approval; fresh cumulative review/CI; main post-merge CI; publish/verify/back-sync; P01–P03 still open | Not started; final execution gate |

## Initial findings

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
  passed 53 verifier/workflow tests and the actual required generator fixture. Updated remote
  CI and post-merge CI are still required; these local results do not replace them.

Read-back [evidence](evidence/release-controls-2026-09-07.json) records:

| Control | Applied configuration |
|---|---|
| [Tag creation rule 22437305](https://github.com/anvai-labs/sandhi/rules/22437305) | Active for `refs/tags/v*`; only repository administrators may bypass creation restriction |
| [Tag immutability rule 22437308](https://github.com/anvai-labs/sandhi/rules/22437308) | Active for the same tags; update/deletion prohibited, no bypass actors |
| `npm` environment | Only `v*` tags and the `main` branch (npm repair) |
| `pypi`, `crates-io`, `github-release` environments | Only `v*` tags |
| Main/develop protection | Full API protection JSON independently compared with pre-change snapshots: unchanged |
| CI routing | Public runners retained; `OWNER_PRIVATE_CI_ENABLED=false` |

These are ref restrictions, **not** required human environment-review rules. No test tag or
publishing job was created to prove denial. An administrator can still edit repository settings;
historical workflow authority is not fully closed by these controls alone.

SG07 is not complete: the `crates-io` environment currently has no secrets; the legacy repository
`CARGO_REGISTRY_TOKEN` remains. The owner must configure a least-privilege `CRATES_RELEASE_TOKEN`
only in that environment and revoke/remove the legacy token, then confirm registry-side trusted
publishers for PyPI and all three npm packages. Secret values were neither read nor changed.
