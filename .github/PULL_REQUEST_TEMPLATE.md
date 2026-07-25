<!-- Pull request template — Sandhi -->

## Summary

<!-- What does this change do, and why? One or two paragraphs. -->

## Change type

- [ ] `feat` — new capability
- [ ] `fix` — bug fix
- [ ] `perf` — performance improvement
- [ ] `refactor` — no behavior change
- [ ] `docs` — documentation only (CI skips the code jobs)
- [ ] `ci` / `build` — tooling, CI, release
- [ ] `test` — tests only
- [ ] `chore` — misc

## Checklist

- [ ] **No third-party AI-agent authorship attribution** in commits or PR text —
      no `Co-Authored-By: Claude/Codex/Copilot/…`, no "Generated with …", no robot
      emoji, no agent model signature, no bot co-author. Our own agent is the
      exception: `victor-code-ai` trailers are allowed. (Enforced by the
      `commit-msg` hook **and** server-side CI — not bypassable.)
- [ ] `cargo fmt --all --check` passes.
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` passes.
- [ ] `cargo test --workspace` passes; new behavior ships with tests.
- [ ] Line coverage stays **≥ 75%**
      (`cargo llvm-cov --workspace --fail-under-lines 75`).
- [ ] Decisions are recorded in `docs/adr/NNNN-slug.md` where applicable.
- [ ] If I touched a schema or contract type, I regenerated the codegen output
      and committed it (`scripts/gen-*.sh`, `scripts/gen-binding-contract-facades.py`).
- [ ] **Nothing emits dollars, tiers, or SKU names** — Sandhi measures neutral
      units only.

## Related

<!-- Issues, ADRs, TDs, or prior PRs this builds on. -->
