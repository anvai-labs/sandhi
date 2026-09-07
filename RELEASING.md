# Releasing Sandhi

A stable `vX.Y.Z` tag on protected `main` drives one **required** release train:
GitHub binaries, PyPI, all four Rust crates and all three npm packages. Missing credentials do
not remove a target from the contract. Versions are staged from the tag, not committed by hand.

The current safeguard implementation is tracked in
[release-safeguards.md](docs/product/release-safeguards.md). Implementation or green unit tests
alone do not close remote authority setup or authorize a release tag.

## What ships

| Target | Required artifacts |
|---|---|
| GitHub release | Linux x86_64 and macOS arm64 archives; each contains `sandhi-proxy` and `sandhi` |
| PyPI | `sandhi-gateway` wheels covering Linux x86_64, macOS arm64 and Windows amd64 |
| crates.io | `sandhi-core`, `sandhi-providers`, `sandhi-store`, `sandhi-proxy`, all non-yanked |
| npm | `@anvailabs/sandhi`, `@anvailabs/sandhi-linux-x64-gnu`, `@anvailabs/sandhi-darwin-arm64`; root optional dependencies pin both platform packages exactly |

Bindings remain separate Cargo workspaces. Published crate manifests must use registry versions,
not git-source dependencies; see [TD-0023](docs/td/TD-0023-release-automation.md).

## Promotion and release gates

1. Merge focused, reviewed changes into `develop` after green CI; verify post-merge CI.
2. Close the engineering acceptance and safeguard tracker, including remote publisher controls.
   Hands-on usability and production integration/recovery gates remain separate, explicitly open
   prerequisites to production—not falsely claimed as completed release tests.
3. Open a conventional-title `chore:` promotion PR from `develop` to `main`. Review the cumulative
   diff; merge only after clean review and green CI. Verify the exact main merge commit's push CI.
4. Obtain explicit version/target execution approval, then create an immutable stable tag on that
   verified commit. Never move/delete an existing release tag to repair publication.
5. Monitor every publisher and the final artifact verifier. A partly published release is incomplete
   even if one registry or the GitHub release page is visible.
6. Verify post-release evidence and back-sync main into develop through the protected PR flow.

Read-only preflight on 2026-09-07 found main requiring strict `CI Success` and one approving
review, but **not** linear history or administrator enforcement. Do not assume those stronger
settings or weaken existing protection. Recheck actual settings before promotion. Prior specific
authority to bypass an unavailable human approval does not authorize bypassing failed/pending CI.

## Source authorization

The read-only `authorize` job rejects anything except an exact stable tag push or an npm-only
repair dispatched from `refs/heads/main`. It resolves annotated tags to a commit, checks
protected-main ancestry, and requires successful **exact-source main push CI** from the canonical
CI workflow (including executed `CI Success` and `Release safeguards` jobs).

An unrelated check with the same name, PR CI, skipped mirror, older successful run hiding a newer
failure, or a changed CI attempt cannot authorize publication. Jobs use immutable source/control
SHAs. The proof artifact is selected by artifact ID, independently bound to authorization outputs,
and rechecked immediately before writes. A tag change/deletion or changed CI proof stops publication.
There remains a small check-to-write race; remote immutable-tag controls are required.

Legacy tags whose main CI predates the safeguards job are intentionally **not** repairable through
this workflow. Do not weaken the gate or rerun an old workflow as a workaround. Plan an explicitly
reviewed new version if immutable old package metadata is wrong.

## Build and publisher boundaries

Build jobs have read-only GitHub permissions, no publishing environment, and no registry token.
Actions are pinned by full commit SHA and checkout credentials are not persisted.

- Binary builds enable `sentinelpass-ipc`; smoke checks use the operator CLI plus an isolated,
  loopback-only proxy health/readiness/start/stop drill. The proxy does not implement `--help`.
- Wheels are installed/imported on each build host before upload.
- Native npm addons are loaded on their build hosts. An unprivileged packaging job requires both
  architectures, complete loaders/types, exact manifests/dependencies, and allowed packed files.
  It rejects lifecycle hooks before packing and checks tarball digests. The privileged job receives
  only the checked bundle, never installs build dependencies, and publishes explicit tarballs.
  All three package manifests must identify `anvai-labs/sandhi` as their repository for
  trusted publishing; a binding on the root package does not authorize platform packages.
- A separate unprivileged job checks the staged Rust workspace. Both it and the crates publisher
  use the same first-party, standard-library-only version staging helper from the control SHA.
  No third-party dependency compilation or build-helper installation runs in the crates publishing job.
  The existing repository token is exposed only to the upload step. Publication uses
  `--no-verify` to avoid compiling dependency build scripts while holding registry authority.
  This is **not** a packaged-crate installation test; registry resolution/package checks still
  occur at publish time. The helper leaves the lockfile unchanged; Cargo refreshes local package
  entries during checking/publication (publication does not use `--locked`). Other unprivileged
  build jobs continue to use pinned cargo-edit for version staging.

Serializing each release tag avoids overlapping publication runs, without canceling an active
release. All builds must pass before the GitHub release is created or any registry is written
on a full release (npm-only repair checks only its own builds); cross-registry publication is
not transactional and must not be described as atomic.

## Required owner-side authority setup

Ref restrictions were applied and independently verified on 2026-09-07:
[read-back evidence](docs/product/evidence/release-controls-2026-09-07.json).
The owner selected reuse of the existing crates token, with trusted publishing only for PyPI
and npm. SG07 tracks that decision and remaining registry evidence; ref settings alone do not
establish credential validity or registry-side bindings.

| Publisher | Required authority |
|---|---|
| GitHub assets | `github-release` environment restricted to permitted release tags; job-scoped `contents: write` |
| PyPI | Trusted publisher for repo `anvai-labs/sandhi`, workflow `release.yml`, environment `pypi`; environment allows release tags |
| npm | Trusted publisher on **each of the three packages**, same repo/workflow, environment `npm`; allow release tags and branch `main` for repair |
| crates.io | Existing repository secret `CARGO_REGISTRY_TOKEN`, supplied only to the upload step; `crates-io` environment allows release tags; no OIDC permission |

Protect `v*` tag creation and prohibit updates/deletions. Tag and branch deployment rules are
distinct: allowing only branch `main` would block legitimate tag-triggered publishing.

Do **not** revoke, delete or rotate `CARGO_REGISTRY_TOKEN` as part of this release: the owner
explicitly confirmed it is retained and should be reused. Neither a new `CRATES_RELEASE_TOKEN`
nor crates.io trusted-publisher bindings are required by the selected mechanism. Token contents
must not appear in chat, logs or committed files. Metadata confirms presence, not validity/scope;
an authentication failure must stop publication, not skip crates successfully.

The repository-scoped credential remains potentially accessible to other same-repository
workflows that request it. Supplying it only to this workflow's upload step and restricting the
`crates-io` environment do **not** make the secret environment-scoped. This retained exposure
is part of the owner's existing-token choice; do not claim it was eliminated. The retired manual
publisher remains disabled. Public package presence cannot establish trusted-publisher bindings.

### Owner confirmation for SG07

Owner evidence received on 2026-09-07 confirms the reported trusted-publisher settings for
the root `@anvailabs/sandhi` package, including direct `npm publish` permission. This does
not yet confirm either platform package or the current PyPI settings. See the
[SG07 evidence](docs/product/release-safeguards.md#owner-publisher-evidence-2026-09-07).

Do not send token values, recovery codes or credentials in chat. Confirm only the following
non-secret facts after checking the registry account settings:

- **Received:** the owner states the existing crates token has not been revoked and should
  be reused. GitHub read-back confirms the repository secret name `CARGO_REGISTRY_TOKEN`.
  Its value, permissions and expiry were not read; actual authorization is checked by publication.
- The PyPI project and **each** npm package authorize `anvai-labs/sandhi`, workflow filename
  `release.yml`, and their exact environment (`pypi` or `npm`). npm must permit direct
  `npm publish`, not only staged publication. Review the registry-side forms, not package presence:
  [npm trusted publishers](https://docs.npmjs.com/trusted-publishers/) and
  [PyPI publisher setup](https://docs.pypi.org/trusted-publishers/adding-a-publisher/).

After owner confirmation, read back only GitHub secret names/scope and record the registry
configuration evidence. Do not publish a throwaway version to test credentials. Until SG07 is
closed, retain the release hold; do not silently omit crates or any other target.

The old manual `publish-crates.yml` is a fail-only stub and workflow ID `317193810` is now
`disabled_manually` in GitHub. This does **not** revoke stored credentials or neutralize every
historical workflow path: external environment/ref controls and credential rotation remain
necessary. See GitHub's
[environment restrictions](https://docs.github.com/en/actions/reference/workflows-and-actions/deployments-and-environments)
and [immutable action pinning guidance](https://docs.github.com/en/actions/reference/security/secure-use).

## Partial publication and verification

Only an explicit registry 404 authorizes a missing-version upload; timeouts, rate limits, authentication
failures and server errors stop the job. Crates already present still undergo final non-yanked
verification. PyPI skips already-uploaded files. Existing binary archives must match locally built
bytes; existing npm versions must match the checked tarball integrity before being skipped. A rebuilt
archive that differs is a blocker, not permission to overwrite. Prefer retrying the original immutable
artifact bundle after diagnosing a partial failure.

The final verifier explicitly requires all targets on a tag push, or npm only on an authorized
repair. A missing secret never turns a publisher into a successful optional skip.

```bash
python3 scripts/verify-release.py vX.Y.Z --targets pypi,crates,npm,github --repo anvai-labs/sandhi
python3 scripts/verify-release.py vX.Y.Z --targets npm
(cd bindings/node && npm ci --ignore-scripts --no-audit --no-fund)
python3 -m pytest tests/release -q
```

The verifier distinguishes `MISSING`, `INVALID` and `UNAVAILABLE`, retries within bounded limits,
checks declared wheel/platform coverage and npm dependencies, and streams GitHub archives to check
reported size and available SHA-256 digests. Registry metadata and archive checks are **not** an
end-user install/usability certification or a signature/provenance verification service.

For npm-only repair, dispatch `release.yml` from `main` with `npm_repair_tag=vX.Y.Z`. Both
platform artifacts remain present when preparing the root manifest, including on partial retry.
There is no unsafe legacy crates repair path and no prerelease `latest` publication support.
