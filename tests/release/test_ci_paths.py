"""Push comparisons must use event SHAs, not the mutable default branch.

The wiring assertion binds the git regression to the pinned workflow inputs. These
offline tests do not execute the remote action or claim hosted CI acceptance.
"""

from pathlib import Path
import subprocess

import pytest
import yaml


ROOT = Path(__file__).resolve().parents[2]
BASE = "${{ github.event_name == 'push' && github.event.before || '' }}"
REF = "${{ github.event_name == 'push' && github.sha || '' }}"


def test_push_comparison_uses_immutable_event_bounds_without_changing_pr_api_route():
    workflow = yaml.load(
        (ROOT / ".github/workflows/ci.yml").read_text(), Loader=yaml.BaseLoader
    )
    steps = workflow["jobs"]["changes"]["steps"]
    filters = [s for s in steps if s.get("uses", "").startswith("dorny/paths-filter@")]
    assert len(filters) == 1
    options = filters[0]["with"]
    assert options.get("base") == BASE
    assert options.get("ref") == REF
    # Keep the default GitHub token: the pinned action uses PR changed-files API
    # for both PR event types. Empty non-push inputs do not override that route.
    assert "token" not in options
    for patterns in yaml.safe_load(options["filters"]).values():
        assert ".github/workflows/ci.yml" in patterns


@pytest.mark.parametrize("default_branch", ["develop", "main"])
def test_promotion_and_docs_push_compare_before_to_exact_after(tmp_path, default_branch):
    def git(*args):
        return subprocess.check_output(
            ["git", "-C", str(tmp_path), *args], text=True, stderr=subprocess.PIPE
        ).strip()

    git("init", "-q", "-b", "main")
    git("config", "user.name", "CI fixture")
    git("config", "user.email", "ci-fixture@example.invalid")
    git("config", "commit.gpgsign", "false")
    (tmp_path / "README.md").write_text("baseline\n")
    git("add", "README.md")
    git("commit", "-qm", "baseline")
    before = git("rev-parse", "HEAD")
    git("switch", "-qc", "develop")
    (tmp_path / "crates").mkdir()
    (tmp_path / "crates/runtime.rs").write_text("// promoted change\n")
    git("add", "crates/runtime.rs")
    git("commit", "-qm", "runtime change")
    git("switch", "-q", "main")
    git("merge", "--no-ff", "-qm", "promotion", "develop")
    after = git("rev-parse", "HEAD")
    # Original failure: default develop already has the same promoted tree.
    if default_branch == "develop":
        assert git("diff", "--name-only", "develop...main") == ""
    # The corrected event-SHA comparison does not consult default_branch.
    assert git("diff", "--name-only", before, after) == "crates/runtime.rs"
    (tmp_path / "README.md").write_text("documentation only\n")
    git("add", "README.md")
    git("commit", "-qm", "docs only")
    docs_after = git("rev-parse", "HEAD")
    assert git("diff", "--name-only", after, docs_after) == "README.md"
    # A later branch move cannot alter the pinned promotion comparison.
    assert git("diff", "--name-only", before, after) == "crates/runtime.rs"
