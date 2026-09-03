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
```

- `develop` — active development; protected (requires `CI Success`).
- `main` — release trunk; protected **more strictly** (require `CI Success`, up-to-date branch,
  linear history, no force-push/deletion, admins included).
- **Cut a release:** open a PR `develop → main`, merge once green, then
  `git tag vX.Y.Z && git push origin vX.Y.Z`. The `release` workflow does the rest.

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
trusted publisher is scoped to its own job.

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
