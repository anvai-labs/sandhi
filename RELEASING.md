# Releasing Sandhi

**Unified version:** one tag `vX.Y.Z` drives one release train — `sandhi-proxy` binaries and the
PyPI wheel (`sandhi-gateway`), plus crates.io and npm when those publishers are configured. The
four Rust crates include `sandhi-core`, `sandhi-providers`, `sandhi-store`, and `sandhi-proxy`.
All release versions are derived from the tag; do not hand-edit package versions.

## Monorepo layout (what ships where)

| Path | Package | Ships to |
|---|---|---|
| `crates/sandhi-core` | `sandhi-core` (Rust lib — the SDK/metering primitives) | crates.io |
| `crates/sandhi-providers` | `sandhi-providers` (Rust lib — transport + resilience) | crates.io |
| `crates/sandhi-store` | `sandhi-store` (Rust lib — durable SQLite sink + aggregates) | crates.io |
| `crates/sandhi-proxy` | `sandhi-proxy` (Rust lib + **server** binary + dashboard) | crates.io + GitHub Release binaries |
| `bindings/python` | `sandhi-gateway` (PyO3 wheel) | PyPI |
| `bindings/node` | `@anvailabs/sandhi` (napi addon) | npm (Trusted Publishing) |

Each binding is its **own** Cargo workspace, so Rust / Python / TypeScript changes are isolated
and fmt/clippy/build independently (see `.github/workflows/ci.yml`).

## Crates.io publish mechanics

The `release.yml` crates job publishes the four workspace crates in
dependency order (core → providers → store → proxy) with skip-if-published
guards (idempotent after a partial failure), and `cargo set-version` derives
every version from the tag. **Published manifests must not carry git-source
dependencies** — cross-repo contracts are consumed as crates.io version pins
(see [TD-0023](docs/td/TD-0023-release-automation.md) for the full chain,
including the automated `sentinelpass-protocol` publish and pin-bump loop).

## Branch flow

```
feature branch → PR → develop  (CI Success gate)
develop        → PR → main     (stricter gate: strict + linear history)
main           → tag vX.Y.Z    → release.yml publishes configured targets
main           → merge back into develop (the post-promote back-sync)
```

- `develop` — active development; protected (requires `CI Success`).
- `main` — release trunk; protected **more strictly** (require `CI Success`, up-to-date branch,
  linear history, no force-push/deletion, admins included).
- **Cut a release:** open a PR `develop → main`, merge once green, then
  `git tag vX.Y.Z && git push origin vX.Y.Z`. The `release` workflow does the rest.
- **Title the promotion PR with a conventional prefix** (`chore:` — the `lint-title` gate rejects
  `release:`; found live during the v0.5.0 cut).
- **After every promotion merge, sync main back into develop** (`git checkout develop && git merge
  origin/main && git push`). The promotion merge commit exists only on main; without the back-sync
  the next `develop → main` PR is permanently `BEHIND` on main's up-to-date-branch requirement.
  This exact trap stalled the v0.5.0 promotion — v0.4.0's promotion (#199) had never been synced
  back.

## One-time publisher setup (maintainer)

The release workflow is present, but publishing needs configuration you own. Binaries need no
setup; PyPI and npm use **no stored secret at all** — both publish via OIDC Trusted Publishing and
require the trusted-publisher configuration below (they fail without it). crates.io skips
explicitly when its secret is absent:

| Target | Setup |
|---|---|
| **GitHub Release binaries** | none — uses the built-in `GITHUB_TOKEN`. Works immediately. |
| **PyPI** (`sandhi-gateway`) | Configure a **Trusted Publisher** (OIDC) on PyPI: project → Publishing → add GitHub publisher (repo `anvai-labs/sandhi`, workflow `release.yml`, environment `pypi`). No token stored. |
| **crates.io** | Add repo secret `CARGO_REGISTRY_TOKEN` (a crates.io API token). Publishes `core → providers → store → proxy` in order. |
| **npm** (`@anvailabs/sandhi`) | Configure a **Trusted Publisher** on npm: package settings → Trusted Publishers → add (repo `anvai-labs/sandhi`, workflow `release.yml`, environment `npm`). No token stored. Note the scope: **`anvailabs`** — the npm org we own; `anvai-labs` was not available on npm, so the package name intentionally differs from the GitHub org. |

Also create GitHub **Environments** named `pypi` and `npm` (Settings → Environments) so each
trusted publisher is scoped to its own job; pin `npm`'s deployment branch policy to `main`
(see the hardening note under *Notes*).

## After the tag: verification is part of the release

The crates.io publish step `exit 0`s when its credential is absent, so a job can report
**success while shipping nothing**. The `verify` job therefore checks the *registries*, not the
job results, and fails the run if an expected target is missing.

Run it by hand any time:

```bash
python3 scripts/verify-release.py vX.Y.Z
```

Two hard-won details are baked in: crates.io **rejects requests without a `User-Agent`** and returns
an error object that reads exactly like "not published", and PyPI's JSON API lags an upload by up to
a minute — so the script sends a UA and retries before concluding anything is absent. Both produced
wrong conclusions before this existed.

## Notes

- Internal crate deps carry a `version` (e.g. `sandhi-core = { path = "…", version = "0.0.0" }`)
  so `cargo publish` resolves them from crates.io for external users; `cargo set-version` rewrites
  them to the tag version at release.
- Treat any new registry or platform as a separately verified release target. Iterate on
  `release.yml` through the same PR flow.
- **npm mechanics** (the v0.5.1 lesson): the per-platform package dirs (`bindings/node/npm/`)
  are **gitignored by design** and are generated at publish time by `napi create-npm-dir` —
  `napi artifacts` writes into them and fails with a bare ENOENT when they are missing. Two
  CLI-2.x behaviors shape the workflow around it: `napi prepublish` publishes **only the
  platform packages** (a platform whose binary is missing is *warned and skipped*, silently —
  the workflow therefore fails the run if no platform binary arrived before publishing),
  and the **main** package needs an explicit root `npm publish --access public` afterwards.
  Scoped packages default to *restricted*; `publishConfig.access = "public"` in the root
  `package.json` propagates into every generated platform package (verified against 2.18.4).
  The napi `triples` config must list **exactly** the targets the `npm-build` matrix builds:
  a configured-but-unbuilt triple yields `optionalDependencies` pointing at packages that
  never exist (and warn-skips at publish); a matrix leg without a triple fails loudly at
  `napi artifacts`. Add a matrix leg and a config triple together.
- **npm repair** (same lesson): a tag whose npm leg failed while crates/PyPI landed (a rerun
  would re-publish those and hard-fail) is repaired without burning a version: run
  `release.yml` → **Run workflow** on `main` with `npm_repair_tag = vX.Y.Z`. Only the npm jobs
  run — same OIDC Trusted-Publishing environment, the tag's own tree — everything else is
  event-gated to tag pushes. The publish is **idempotent**: a package (main or platform)
  already on the registry is skipped rather than E403-ing, so a rerun after a partial
  publish completes the remainder. Afterwards verify with
  `python3 scripts/verify-release.py vX.Y.Z`. Caveat: the repair accepts any existing tag and
  `napi prepublish` never sets a dist-tag, so a prerelease tag (`vX.Y.Z-rc1`) publishes as
  `latest` — don't cut `-rc` tags, or extend the publish with `--tag next` first.
- **npm hardening (maintainer, GitHub settings)**: pin the `npm` environment's deployment
  branch policy to `main`, and add tag protection to `refs/tags/v*`. Without that, any
  write-access actor can dispatch the repair from an arbitrary branch (or push a tag whose
  tree carries a doctored `release.yml`) and run arbitrary code under the npm OIDC identity —
  the same power a tag push already grants, but the policies close both doors.
