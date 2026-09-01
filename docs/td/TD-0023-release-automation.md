# TD-0023: Release automation — the crates.io chain, end to end

- **Status:** Implemented (2026-09-01, #74 + #188 + #191). This TD records the
  design of the automation that now exists, the dependency policy it enforces,
  and the manual residue that remains. Owns the "publish mechanics" layer of
  [RELEASING.md](../../RELEASING.md).
- **Relates to:** [ADR-0008](../adr/0008-inferflux-admission-and-session-affinity.md)
  (the InferFlux integration whose release-window blocker motivated this),
  [TD-0014](TD-0014-data-plane-resource-safety.md) (the review discipline that
  shaped it), anvai-labs/sentinelpass#74 (the upstream publish job),
  anvai-labs/homebrew-tap (the downstream formula bump).

## Why this exists

The v0.3.0 release was blocked by a dependency nobody could publish:
sandhi-store pinned `sentinelpass-protocol` via a **git tag**, and crates.io
rejects any manifest resolved from a git source at publish time. The protocol
crate existed (tag v0.8.0 in the sister repo); the *publish* of it was a
manual step in a different repository, unknown to the release flow — the
exact class of manual activity that fails precisely when it is most expensive.
The fix shipped as three coordinated changes, and this TD records the design
so the chain is a documented system rather than three PRs.

## The dependency policy (D1)

**A published Sandhi crate must not carry a git-source dependency — in any
form.** Not non-optional, not feature-gated: `cargo publish` verifies the
manifest and rejects git-source dependencies even behind optional features
(verified empirically; a scratch test suggesting otherwise was a false
green). Cross-repo contracts (e.g. `sentinelpass-protocol`) therefore:

1. get **published to crates.io** by their own repo's release flow (D2), and
2. are consumed as **`version = "X.Y"` pins**, and
3. when the contract needs to ship without its consumer's default build:
   gate behind a **non-default cargo feature** (as
   `sandhi-store`'s `sentinelpass-ipc` does — release binary builds enable
   the feature, so shipped binaries keep the capability; the crates artifact
   omits it).

## The automation chain (D2–D4)

**D2 — the contract crate publishes itself on release**
(anvai-labs/sentinelpass#74). A `crates-publish` job in sentinelpass's
release flow publishes `sentinelpass-protocol` on every `v*` **tag**, using
that repo's `CARGO_REGISTRY_TOKEN`. Review-hardened: it runs only on real
tag pushes (a branch named `v*` must not reach an irreversible registry via
workflow_dispatch); it gates on the **manifest version at the tag, not the
tag name** (13 of that repo's 17 historical tags had tag/manifest skew); it
**fails loudly** when crates.io answers unclearly (5xx/429) rather than
guessing; and a yanked version answers 200, so a yank is permanent — standard
crates.io semantics, documented rather than worked around.

**D3 — the consumer's crates job is idempotent** (#191). Each workspace
member checks crates.io before publishing and **skips if its version is
already there**, so a transient crates.io failure mid-chain no longer
permanently wedges the release (a bare rerun used to die on "crate already
published" for every member that landed before the hiccup).

**D4 — the consumer's pin follows automatically** (#191).
`update-protocol-pin` (scheduled daily 05:23 UTC + dispatchable) resolves the
latest published `sentinelpass-protocol` from crates.io, and when the
sandhi-store pin lags: bumps it via a committed helper script
(`.github/scripts/bump_protocol_pin.py`, which **fails loudly** when the pin
line moves — a silent no-op would strand the pin while claiming success),
**builds with the `sentinelpass-ipc` feature before opening the PR** (a
breaking protocol bump must not open a red PR silently), and opens the
pin-bump PR. Same-repo `GITHUB_TOKEN` — no cross-repo secrets.

## The full loop

```
sentinelpass tag v*  ──▶ crates-publish job ──▶ sentinelpass-protocol on crates.io
crates.io release    ──▶ update-protocol-pin ──▶ version-bump PR on sandhi (build-gated)
sandhi tag v*        ──▶ release.yml ──▶ crates (idempotent) + binaries + PyPI
sandhi release       ──▶ homebrew tap bump (scheduled) ──▶ brew users
```

## Manual residue (D5)

- **Token hygiene.** `CARGO_REGISTRY_TOKEN` (the owner's current login token)
  is set consistently on sandhi, proximaDB, and sentinelpass (2026-09-01).
  Revoking superseded tokens has **no API** — crates.io settings UI, owner
  action.
- **The npm leg** (`NPM_TOKEN`) was never configured; `@anvai-labs/sandhi` is
  intentionally unpublished. If npm distribution is ever wanted, the token is
  the only missing piece.
- **ProximaDB** publishes no crates (deb/rpm/msi channel) but holds the token
  for future parity.
- **Environment-gate approvals** (`owner-private-ci`) remain a deliberate
  manual control and are out of scope for automation.

## Acceptance (met, and how it was verified)

The v0.3.0 release ran the chain under fire: the publish dry-run **on the
release tree** caught the git-dependency blocker before tagging; the protocol
crate was published from its existing tag; the pin PR merged; the tagged
release published all four crates in order — verified at the registry:
crates.io `core/providers/store/proxy` all at 0.3.0, with store 0.3.0's
published manifest carrying `sentinelpass-protocol ^0.8 (optional)` and no
git source.

