# Contributing to Sandhi

Apache-2.0. See [ADR-0001](docs/adr/0001-sandhi-architecture-and-wire-contract.md) for the
architecture and the [usage-event wire contract](schemas/usage-event.v1.schema.json).

## Setup

```bash
git config core.hooksPath .githooks   # activate the commit-msg + pre-push hooks
cargo test --workspace
```

## Rules (modeled on the anvai-labs family: victor, proximaDB)

- **No AI-agent authorship attribution** in commit/PR text — the human drives the code, not
  the agents. No `Co-Authored-By: Claude/Codex/…`, no "Generated with …" tagline, no robot
  emoji, no agent model signature. Enforced by the `commit-msg` hook **and** CI (server-side,
  not bypassable). Mentions of `CLAUDE.md`/`AGENTS.md` or the Anthropic/OpenAI APIs are fine.
- `cargo fmt` and `cargo clippy --all-targets -- -D warnings` must pass — CI gates on both.
- Decisions go in `docs/adr/NNNN-slug.md` (heading `# ADR-NNNN: …`).
- **Branch workflow:** open PRs against `develop`. `develop` is protected — the aggregate
  **`CI Success`** check must be green (`enforce_admins` on; no force-push or deletion). `main`
  is the release trunk.
