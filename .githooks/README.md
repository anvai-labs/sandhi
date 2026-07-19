# Git hooks

Activate once per clone:

```bash
git config core.hooksPath .githooks
```

| Hook | What it does | Bypass |
|---|---|---|
| `commit-msg` | Blocks AI-agent authorship attribution (runs `scripts/check_no_agent_attribution.py`). The human drives the code, not the agents. | `git commit --no-verify` |
| `pre-push` | Fast `rustfmt --check` (no compile) — the most common avoidable CI red. | `git push --no-verify` |

**Server-side enforcement (not bypassable):** `.github/workflows/ci.yml` re-runs the
attribution check on every push/PR, and branch protection on `develop` requires the
aggregate **`CI Success`** check to be green before merge.

Modeled on proximaDB's `.githooks`. proximaDB's additional *worktree mandate* (push only
from a `git worktree`) is intentionally **not** replicated here — it depends on repo-specific
`scripts/worktree.sh` tooling and is a workflow preference, not a governance rule.
