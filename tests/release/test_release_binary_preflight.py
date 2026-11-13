"""Keep pre-tag platform evidence on the same unprivileged release build seam.

These guards validate workflow wiring, not native execution. Hosted jobs must
still build and smoke the candidate on each declared native platform.
"""

from __future__ import annotations

import copy
from fnmatch import fnmatchcase
from pathlib import Path
import re

import pytest
import yaml


ROOT = Path(__file__).resolve().parents[2]
GATE = "release-binary-preflight"
BUILD = (
    "cargo build --release --locked --features sentinelpass-ipc,policy-onnx "
    '--target "$TARGET" --bin sandhi-proxy --bin sandhi'
)


def workflow(name):
    return yaml.load(
        (ROOT / ".github/workflows" / name).read_text(), Loader=yaml.BaseLoader
    )


def check_preflight(ci, release):
    jobs = ci["jobs"]
    gate = jobs[GATE]
    assert GATE in jobs["ci-success"]["needs"], "platform failures must block the aggregate"
    assert gate["needs"] == "changes"
    assert gate["if"] == (
        "${{ always() && needs.changes.result == 'success' && "
        "needs.changes.outputs.release_binaries == 'true' }}"
    )
    assert gate["runs-on"] == "${{ matrix.os }}"
    assert gate["strategy"]["fail-fast"] == "false"
    expected = {
        ("ubuntu-24.04", "x86_64-unknown-linux-gnu"),
        ("macos-latest", "aarch64-apple-darwin"),
    }
    for job in (gate, release["jobs"]["binaries"]):
        cells = job["strategy"]["matrix"]["include"]
        assert len(cells) == 2
        assert {(cell["os"], cell["target"]) for cell in cells} == expected

    assert ci["permissions"] == {"contents": "read", "pull-requests": "read"}
    assert "environment" not in gate and "permissions" not in gate
    assert "secrets." not in str(gate) and "id-token" not in str(gate)
    assert "continue-on-error" not in gate
    assert gate["env"] == {"TARGET": "${{ matrix.target }}"}
    for step in gate["steps"]:
        assert "continue-on-error" not in step and "if" not in step
        if "uses" in step:
            assert re.fullmatch(r"[^@]+@[a-f0-9]{40}", step["uses"])
        if step.get("uses", "").startswith("actions/checkout@"):
            assert step["with"] == {"persist-credentials": "false"}
    toolchain = [
        step for step in gate["steps"]
        if step.get("uses", "").startswith("dtolnay/rust-toolchain@")
    ]
    assert len(toolchain) == 1
    assert toolchain[0]["with"]["targets"] == "${{ matrix.target }}"

    runs = [step["run"] for step in gate["steps"] if "run" in step]
    assert len(runs) == 1 and isinstance(runs[0], str)
    run = runs[0]
    build_lines = [line.strip() for line in run.splitlines() if line.strip().startswith("cargo build")]
    release_lines = [
        line.strip()
        for step in release["jobs"]["binaries"]["steps"]
        for line in step.get("run", "").splitlines()
        if line.strip().startswith("cargo build")
    ]
    assert build_lines == release_lines == [BUILD]
    identity = 'python3 scripts/check-release-version.py --version "$version"'
    smoke = (
        'python3 scripts/smoke-release-binaries.py --binary-dir '
        '"target/$TARGET/release" --expected-version "$version"'
    )
    assert identity in run and smoke in run
    assert run.index(identity) < run.index(BUILD) < run.index(smoke)
    assert not re.search(r"(?:\|\|\s*(?:true|:)|\b(?:cargo|npm)\s+publish|gh\s+release)", run)

    changes = jobs["changes"]
    assert changes["outputs"]["release_binaries"] == "${{ steps.filter.outputs.release_binaries }}"
    filter_step = next(
        step for step in changes["steps"]
        if step.get("uses", "").startswith("dorny/paths-filter@")
    )
    patterns = yaml.safe_load(filter_step["with"]["filters"])["release_binaries"]
    required = [
        "crates/sandhi-proxy/src/main.rs", "crates/sandhi-proxy/Cargo.toml",
        "crates/sandhi-providers/src/typed.rs", "Cargo.toml", "Cargo.lock",
        "scripts/smoke-release-binaries.py", "scripts/check-release-version.py",
        "tests/release/test_release_binary_smoke.py",
        "tests/release/test_release_binary_preflight.py",
        ".github/workflows/ci.yml", ".github/workflows/release.yml",
    ]
    assert all(any(fnmatchcase(path, pattern) for pattern in patterns) for path in required)
    assert not any(
        fnmatchcase(path, pattern)
        for path in ["README.md", "docs/operator/oidc-sso.md"] for pattern in patterns
    ), "docs-only changes must not rebuild both platform binaries"


def test_combined_release_binary_features_are_qualified_before_tagging():
    check_preflight(workflow("ci.yml"), workflow("release.yml"))


@pytest.mark.parametrize(
    "mutation",
    ["missing-gate", "missing-platform", "wrong-architecture", "private-runner",
     "missing-feature", "unlocked-ci", "unlocked-release", "missing-smoke",
     "ignored-build-failure", "privileged", "stored-secret", "mutable-action",
     "missing-release-trigger"],
)
def test_preflight_cannot_lose_a_platform_feature_or_failure_boundary(mutation):
    ci, release = copy.deepcopy(workflow("ci.yml")), copy.deepcopy(workflow("release.yml"))
    gate = ci["jobs"][GATE]
    build = next(step for step in gate["steps"] if "run" in step)
    if mutation == "missing-gate":
        ci["jobs"]["ci-success"]["needs"].remove(GATE)
    elif mutation == "missing-platform":
        gate["strategy"]["matrix"]["include"].pop()
    elif mutation in {"wrong-architecture", "private-runner"}:
        gate["strategy"]["matrix"]["include"][1][
            "target" if mutation == "wrong-architecture" else "os"
        ] = "x86_64-apple-darwin" if mutation == "wrong-architecture" else "public-overflow"
    elif mutation in {"missing-feature", "unlocked-ci"}:
        build["run"] = build["run"].replace(
            ",policy-onnx" if mutation == "missing-feature" else "--locked ", ""
        )
    elif mutation == "unlocked-release":
        step = next(step for step in release["jobs"]["binaries"]["steps"] if "cargo build" in step.get("run", ""))
        step["run"] = step["run"].replace("--locked ", "")
    elif mutation == "missing-smoke":
        build["run"] = "\n".join(line for line in build["run"].splitlines() if "smoke-release-binaries.py" not in line)
    elif mutation == "ignored-build-failure":
        build["continue-on-error"] = "true"
    elif mutation == "privileged":
        gate["permissions"] = {"contents": "write"}
    elif mutation == "stored-secret":
        gate["env"]["TOKEN"] = "${{ secrets.PUBLISH_TOKEN }}"
    elif mutation == "mutable-action":
        gate["steps"][0]["uses"] = "actions/checkout@main"
    elif mutation == "missing-release-trigger":
        step = next(step for step in ci["jobs"]["changes"]["steps"] if "filters" in step.get("with", {}))
        filters = yaml.safe_load(step["with"]["filters"])
        filters["release_binaries"].remove(".github/workflows/release.yml")
        step["with"]["filters"] = yaml.safe_dump(filters)
    with pytest.raises(AssertionError):
        check_preflight(ci, release)
