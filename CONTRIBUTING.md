# Contributing to Sandhi

Apache-2.0. See [ADR-0001](docs/adr/0001-sandhi-architecture-and-wire-contract.md) for the
architecture and the [usage-event wire contract](schemas/usage-event.v1.schema.json).

## Setup

```bash
git config core.hooksPath .githooks   # activate the commit-msg + pre-push hooks
cargo test --workspace
```

## Rules (modeled on the anvai-labs family: victor, proximaDB)

- **No third-party AI-agent authorship attribution** in commit/PR text — no
  `Co-Authored-By: Claude/Codex/Copilot/…`, no "Generated with …" tagline, no robot emoji, no
  agent model signature, no bot co-author. Enforced by the `commit-msg` hook **and** CI
  (server-side, not bypassable). Mentions of `CLAUDE.md`/`AGENTS.md` or the Anthropic/OpenAI
  APIs are fine.
  **Exception — our own agent:** [victor](https://github.com/anvai-labs) is first-party
  tooling we credit deliberately, so `Generated-by: victor-code-ai` and a `victor-code-ai`
  co-author trailer are allowed (see `ALLOWED_PATTERNS` in
  `scripts/check_no_agent_attribution.py`).
- `cargo fmt` and `cargo clippy --all-targets -- -D warnings` must pass — CI gates on both.
- **Test-driven:** line coverage must stay **≥ 75%** (`cargo llvm-cov --workspace
  --fail-under-lines 75`, a CI gate). New behavior lands with tests.
- Decisions go in `docs/adr/NNNN-slug.md` (heading `# ADR-NNNN: …`); larger executable designs go
  in `docs/td/TD-NNNN-*.md`. Update the [documentation status index](docs/README.md) in the same PR
  that changes a TD's lifecycle or transfers its remaining scope.
- **Branch workflow:** open PRs against `develop`. Require the aggregate **`CI Success`** check
  and an approving PR review before merge; do not use an administrator bypass. `main` is the
  release trunk. Live settings are authoritative: the 2026-09-05 checkpoint found
  `enforce_admins=false` on `develop`, contrary to older prose; this was not changed.
- **CI execution:** normal Sandhi PR CI uses GitHub-hosted `ubuntu-latest` with repository
  variable `OWNER_PRIVATE_CI_ENABLED=false` (2026-09-05 owner decision). The legacy private route
  remains in the workflow but is disabled. Do not approve or re-enable it to unblock standard
  checks. The separate overflow-runner diagnostic is not part of normal PR verification.

## Community & release notes

- Notable changes per release live in [CHANGELOG.md](CHANGELOG.md) (Keep a Changelog);
  see [RELEASING.md](RELEASING.md) for the cut-a-release flow.
- Security issues go through [SECURITY.md](SECURITY.md) — **do not** open a public issue; the
  proxy holds real upstream keys server-side.
- Everyone agrees to the [Code of Conduct](CODE_OF_CONDUCT.md).
