# Releasing Sandhi

A stable `vX.Y.Z` tag on protected `main` drives one **required** release train:
GitHub binaries, PyPI, all four Rust crates and all three npm packages. Missing credentials do
not remove a target from the contract. Prepare and commit the version before tagging; the tag
must match that reviewed source version.

The current safeguard implementation is tracked in
[release-safeguards.md](docs/product/release-safeguards.md). Implementation or green unit tests
alone do not close remote authority setup or authorize a release tag.

## What ships

| Target | Required artifacts |
|---|---|
| GitHub release | Linux x86_64 and macOS arm64 archives; each contains `sandhi-proxy` and `sandhi` |
| PyPI | `sandhi-gateway` wheels covering Linux x86_64/aarch64, macOS arm64 and Windows amd64 |
| crates.io | `sandhi-core`, `sandhi-providers`, `sandhi-store`, `sandhi-proxy`, all non-yanked |
| npm | `@anvailabs/sandhi`, `@anvailabs/sandhi-linux-x64-gnu`, `@anvailabs/sandhi-darwin-arm64`; root optional dependencies pin both platform packages exactly |

The committed `[workspace.package].version` in `Cargo.toml` is the software version authority.
The CLI, proxy `--version`, HTTP `/version.package_version`, Rust crates and binding manifests
must agree. Wire/chat contract versions are independent. CI runs
`python3 scripts/check-release-version.py`; new release tags must match the committed value.
Release jobs do not rewrite binary/wheel versions behind the source tag.

Prepare a version bump in an isolated worktree, before review and tagging:

```bash
python3 scripts/stage-crates-release.py --workspace "$PWD" --version X.Y.Z
python3 scripts/check-release-version.py --sync
python3 scripts/check-release-version.py --version X.Y.Z
```

Commit the manifests and all three Cargo lockfiles plus npm lockfile. Binary release smoke
checks both executable versions and HTTP identity against the approved release version.
Homebrew consumes those immutable release archives; a custom running gateway is a separate
installation and must be restarted from the verified build to adopt its version. Alignment of
source versions does not publish a release, rewrite historical tags, or upgrade a running service.

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

Recheck current branch protection and publishing permissions before promotion.
A prior release or administrative operation does not authorize bypassing required
reviews, failed checks or pending CI.

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
  loopback-only proxy health/readiness/start/stop drill. Both binaries implement `--help` and
  `--version` without loading service configuration or starting a listener.
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
  build jobs require the committed package version to match the authorized release tag.

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

The configured crates upload step reads `CARGO_REGISTRY_TOKEN`. Configure and
validate its intended registry permissions and expiry privately; a missing or
rejected credential stops publication. PyPI and npm use the workflow's trusted
publishing bindings rather than a substituted upload token.

A repository-scoped credential can be requested by other same-repository workflows.
Exposing it only in this workflow's upload step does not make it environment-scoped.
The retired manual publisher is not an alternative release entry point.

### Registry configuration checklist

- Verify the intended crates upload mechanism and its narrowly scoped credential.
- Verify that the PyPI project and **each** npm package authorize the exact
  repository, workflow filename and publisher environment. The npm binding must
  permit the publication method used by this workflow.
- Use the registry's actual configuration, not package presence or a previous
  successful upload, as evidence of the current binding.
- Keep credential values, recovery codes and private account details out of logs,
  issues and committed files. Record a registry rejection as a failure; do not
  silently omit its target or switch authentication mechanisms.

See [npm trusted publishers](https://docs.npmjs.com/trusted-publishers/),
[PyPI publisher setup](https://docs.pypi.org/trusted-publishers/adding-a-publisher/)
and the [release safeguards](docs/product/release-safeguards.md#registry-configuration).

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

Linux wheels use the manylinux2014 baseline. The aarch64 build runs natively on
`ubuntu-24.04-arm`; PR CI installs its release-mode abi3 wheel into fresh Python
3.11, 3.12 and 3.13 environments and runs the binding suite before `CI Success`.
The verifier requires a portable manylinux aarch64 artifact (a host-only `linux_aarch64`
or musllinux wheel does not satisfy this target). For historical releases predating
this target, pass `--pypi-platforms linux-x86_64,macos-arm64,windows-amd64` explicitly.
Adding this build does not backfill already-published package versions; downstream
consumers need a new Sandhi release and dependency-pin update.

Native-platform qualification is also part of ordinary, nonpublishing PR/push CI:

| Artifact | Public runner | Runtime qualification |
| --- | --- | --- |
| macOS ARM64 Python wheel | `macos-latest` | CPython 3.11, 3.12 and 3.13, one locked release build per interpreter |
| Windows x86_64 Python wheel | `windows-latest` | CPython 3.11, 3.12 and 3.13, one locked release build per interpreter |
| macOS ARM64 Node addon | `macos-latest` | Native ARM64 Node 24, locked release build and full binding suite |

Both new jobs are dependencies of `CI Success`. The Python job checks the actual runtime and Rust
compiler host before building, then requires `cp311-abi3` with native ARM64 macOS or `win_amd64`
tags in both the filename and wheel metadata. A new virtual environment outside the checkout
installs exactly that wheel, uses the pinned binding test requirements, and runs the full binding
suite with Python isolation enabled. The installed extension must match the built wheel's SHA-256
before and after the suite. The Node job checks the actual runtime/compiler architecture, confirms
that the generated loader opens the built local ARM64 addon, and runs the complete Node binding
suite. Reports and built Python wheels remain CI artifacts; these jobs have no publication step.
A runner label alone never establishes CPU architecture: if `macos-latest` supplies a different
host, qualification fails rather than silently testing an x86 build.

Offline workflow guards and Linux probe tests qualify the harness only. macOS/Windows acceptance
requires successful jobs on those actual public platforms for the reviewed source. This CI addition
does not diagnose or close the retained Python TLS child SIGSEGV or shutdown-deadline reliability
gates, and it does not grant release publication authority.

The verifier distinguishes `MISSING`, `INVALID` and `UNAVAILABLE`, retries within bounded limits,
checks declared wheel/platform coverage and npm dependencies, and streams GitHub archives to check
reported size and available SHA-256 digests. Registry metadata and archive checks are **not** an
end-user install/usability certification or a signature/provenance verification service.

For npm-only repair, dispatch `release.yml` from `main` with `npm_repair_tag=vX.Y.Z`. Both
platform artifacts remain present when preparing the root manifest, including on partial retry.
There is no unsafe legacy crates repair path and no prerelease `latest` publication support.
