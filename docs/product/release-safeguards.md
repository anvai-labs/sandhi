# Release safeguards

Use these controls to publish the reviewed source and verify a complete set of
installable artifacts. A visible release page or a successful upload to one
registry does not establish a complete release.

For the supported target matrix, version preparation and commands, follow
[Releasing Sandhi](../../RELEASING.md). This page explains the trust boundaries
and validation evidence behind that process.

## Required gates

| Gate | Purpose | Required evidence |
| --- | --- | --- |
| Source authorization | Prevent publication from an unintended or changed source | Exact stable tag, protected-main ancestry and substantive successful CI for that exact main commit |
| Immutable build identity | Keep every target on the reviewed source | Source/control hashes and artifact-ID selection, revalidated before publishing |
| Least publisher authority | Keep registry credentials out of build jobs | Build and publisher job separation; narrow registry permissions and explicit environments |
| Artifact validation | Reject incomplete or inconsistent packages | Binary versions/smokes, wheel install/import coverage, crate metadata and npm tarball/dependency checks |
| Partial-publication recovery | Preserve already published immutable content | Matching archive bytes and package integrity; upload only explicitly missing artifacts |
| Final verification | Confirm every declared target | Read-only registry and release-asset verification; installation checks remain a distinct evidence category |

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

## Registry configuration

Configure each registry's intended authentication mechanism before publication.
The current workflow uses its configured crates upload credential and trusted
publishing for PyPI/npm. Each npm platform package needs its own matching binding;
a root-package binding does not authorize the platform packages. Confirm exact
repository, workflow filename, environment and permitted publication method using
registry settings rather than inferring them from existing package versions.

A repository-scoped secret can be requested by other same-repository workflows.
Restricting one publisher environment does not turn that secret into an
environment-scoped credential. Do not publish credential values, recovery material
or account-specific registration details as release evidence.

## Verify a published release

```sh
python3 scripts/verify-release.py vX.Y.Z --targets pypi,crates,npm,github --repo anvai-labs/sandhi
```

Only an explicit registry 404 identifies an absent version. Timeouts, rate limits,
authentication failures and server errors stop verification or upload. A package's
presence does not establish installability, provenance or the correctness of its
current trusted-publisher configuration.

The release suite exercises authorization substitutions, moved tags, wrong CI
provenance, missing artifacts, malformed metadata, packing, retry integrity and
binary smoke failures. Run it without registry writes:

```sh
python3 -m pytest tests/release -q
```

## Historical publication evidence

The safeguarded train published and verified all declared targets for
[v0.6.0](https://github.com/anvai-labs/sandhi/releases/tag/v0.6.0),
[v0.6.1](https://github.com/anvai-labs/sandhi/releases/tag/v0.6.1) and
[v0.7.0](https://github.com/anvai-labs/sandhi/releases/tag/v0.7.0).
These are historical outcomes, not guarantees that a future registry upload will
succeed. Release notes retain version-specific compatibility and acceptance limits;
publication does not certify a managed deployment.
