# Git hooks

Activate once per clone:

```bash
git config core.hooksPath .githooks
```

| Hook | What it does | Bypass |
|---|---|---|
| `commit-msg` | Blocks **third-party** AI-agent authorship attribution (runs `scripts/check_no_agent_attribution.py`). Our own agent is the exception — `victor-code-ai` trailers are allowed. | `git commit --no-verify` |
| `pre-push` | Requires a local independent adversarial-review record for each pushed commit, then runs `rustfmt --check` (no compile). Requires Python 3. | `git push --no-verify` |

Before pushing, have a separate reviewer or reviewer agent inspect the final commit
against its review base. Resolve findings, commit the fixes, and have the resulting
commit reviewed again. Record only an explicit clean result from that reviewer:

```bash
python3 scripts/adversarial_review.py record \
  --commit <reviewed-commit-sha> --base origin/develop \
  --reviewer '<independent-reviewer-or-session>' \
  --summary '<review scope, findings, and disposition>' --independent-review
python3 scripts/adversarial_review.py check --commit <reviewed-commit-sha>
git push
```

Use the actual reviewed ancestor as `--base`. A named base must still resolve to
the same commit when pushing; fetch before review so the local remote-tracking ref
is current. If that base advances, obtain a fresh review. A literal base SHA pins
the reviewed range and does not assert freshness against the remote branch.
The record binds the commit SHA, tree SHA, base ref/SHA, reviewer, summary, and time.
Any commit change, including an amend or rebase, requires a new review record.
Records live under the Git common directory's `sandhi/adversarial-reviews/`, outside
tracked files, and are shared by this clone's worktrees.

The hook reads Git's actual pushed object IDs from stdin, including non-HEAD refs
and every ref in a multi-ref push. Annotated tags are checked against their peeled
commit; tags to non-commit objects are rejected. Ref deletions need no review.
This belongs in pre-push: pre-commit cannot bind a record to the final commit SHA.

This is a local process reminder, not authenticated independent approval or a
security boundary. Unsigned JSON can be edited and local hooks can be bypassed.
Do not generate a record to approve your own work or treat the recording command
as a reviewer. Independent approval before merge and green CI remain separate
requirements; this record neither configures nor substitutes for GitHub reviews
or branch protection. CI runs the gate's regression tests, not these local records.

**Server-side enforcement (not bypassable):** `.github/workflows/ci.yml` re-runs the
attribution check on every push/PR, and branch protection on `develop` requires the
aggregate **`CI Success`** check to be green before merge.

Modeled on proximaDB's `.githooks`. proximaDB's additional *worktree mandate* (push only
from a `git worktree`) is intentionally **not** replicated here — it depends on repo-specific
`scripts/worktree.sh` tooling and is a workflow preference, not a governance rule.
