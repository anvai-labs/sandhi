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
    # for pull_request events. Empty non-push inputs do not override that route.
    assert "token" not in options
    for patterns in yaml.safe_load(options["filters"]).values():
        assert ".github/workflows/ci.yml" in patterns


def test_rust_jobs_and_local_development_use_the_same_pinned_toolchain():
    for name in ("ci.yml", "release.yml"):
        workflow = yaml.load(
            (ROOT / ".github/workflows" / name).read_text(), Loader=yaml.BaseLoader
        )
        toolchains = [
            step["with"]["toolchain"]
            for job in workflow["jobs"].values()
            for step in job.get("steps", [])
            if step.get("uses", "").startswith("dtolnay/rust-toolchain@")
        ]
        assert toolchains and set(toolchains) == {"1.100.0"}, name
    assert (ROOT / "rust-toolchain.toml").read_text() == (
        '[toolchain]\nchannel = "1.100.0"\n'
    )


def test_maturin_builds_use_the_same_pinned_release():
    expected = "1.16.0"
    workflows = {}
    for name in ("ci.yml", "release.yml"):
        workflows[name] = yaml.load(
            (ROOT / ".github/workflows" / name).read_text(), Loader=yaml.BaseLoader
        )
        actions = [
            step["with"].get("maturin-version")
            for job in workflows[name]["jobs"].values()
            for step in job.get("steps", [])
            if step.get("uses", "").startswith("PyO3/maturin-action@")
        ]
        assert actions and set(actions) == {expected}, name
    installs = [
        line.strip()
        for workflow in workflows.values()
        for job in workflow["jobs"].values()
        for step in job.get("steps", [])
        for line in step.get("run", "").splitlines()
        if "pip install" in line and "maturin" in line
    ]
    assert installs == ["pip install maturin==1.16.0"]
    assert (
        f"maturin=={expected}"
        in (ROOT / "bindings/python/requirements-test.txt").read_text()
    )
    assert (
        f'requires = ["maturin=={expected}"]'
        in (ROOT / "bindings/python/pyproject.toml").read_text()
    )


def test_toolchain_changes_recompile_every_native_job():
    workflow = yaml.load(
        (ROOT / ".github/workflows/ci.yml").read_text(), Loader=yaml.BaseLoader
    )
    filters = yaml.safe_load(
        next(
            step
            for step in workflow["jobs"]["changes"]["steps"]
            if step.get("id") == "filter"
        )["with"]["filters"]
    )
    required = {
        "release_binaries",
        "rust",
        "python",
        "node",
        "schema",
        "sdk_conformance",
        "codegen",
    }
    assert required <= set(filters)
    assert all("rust-toolchain.toml" in filters[name] for name in required)


@pytest.mark.parametrize("default_branch", ["develop", "main"])
def test_promotion_and_docs_push_compare_before_to_exact_after(
    tmp_path, default_branch
):
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
