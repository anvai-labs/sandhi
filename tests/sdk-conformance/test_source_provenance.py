"""Adversarial provenance checks using disposable local repositories, without inference."""

import json
from pathlib import Path
import subprocess

import pytest

from source_provenance import (
    assert_unchanged,
    binary_artifact,
    build_metadata,
    source_state,
)


def git(repo: Path, *args: str) -> bytes:
    return subprocess.check_output(
        [
            "git",
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "user.name=Synthetic Test",
            "-c",
            "user.email=synthetic@example.invalid",
            *args,
        ],
        cwd=repo,
    )


@pytest.fixture
def source_repo(tmp_path):
    git(tmp_path, "init", "-q")
    (tmp_path / ".gitignore").write_text("target/\nnode_modules/\n")
    (tmp_path / "Cargo.toml").write_text("# synthetic source\n")
    git(tmp_path, "add", ".")
    git(tmp_path, "commit", "-qm", "synthetic baseline")
    return tmp_path


def test_staged_source_change_cannot_disappear_from_build_provenance(source_repo):
    before = source_state(source_repo)
    (source_repo / "Cargo.toml").write_text("# staged source change\n")
    git(source_repo, "add", "Cargo.toml")
    assert (
        git(source_repo, "diff", "--binary") == b""
    ), "old unstaged-only probe misses this edit"
    after = source_state(source_repo)
    assert before["head_diff_sha256"] != after["head_diff_sha256"]
    with pytest.raises(AssertionError, match="tracked diff changed"):
        assert_unchanged(before, after)


def test_head_advance_with_identical_source_is_rejected(source_repo):
    before = source_state(source_repo)
    git(source_repo, "commit", "--allow-empty", "-qm", "synthetic head advance")
    after = source_state(source_repo)
    assert before["input_files"] == after["input_files"]
    assert before["head_diff_sha256"] == after["head_diff_sha256"]
    with pytest.raises(AssertionError, match="HEAD changed"):
        assert_unchanged(before, after)


def test_untracked_rust_cargo_and_ui_inputs_are_hashed_while_ignored_caches_are_excluded(
    source_repo,
):
    paths = ["src/new.rs", "crates/new/Cargo.toml", "dashboard/app.js"]
    for relative in paths:
        path = source_repo / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text("synthetic input")
    before = source_state(source_repo)
    inventory = {item["path"]: item for item in before["input_files"]}
    assert all(inventory[name]["content_sha256"] for name in paths)
    for relative in ["target/generated.rs", "node_modules/cached.js"]:
        path = source_repo / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text("ignored cache")
    assert_unchanged(before, source_state(source_repo))
    (source_repo / "src/new.rs").write_text("changed untracked input")
    after = source_state(source_repo)
    assert before["head_diff_sha256"] == after["head_diff_sha256"]
    with pytest.raises(AssertionError, match="source input contents changed"):
        assert_unchanged(before, after)


def test_config_contents_are_hashed_and_arbitrary_environment_values_are_omitted(
    source_repo, monkeypatch
):
    config = source_repo / ".cargo/config.toml"
    config.parent.mkdir()
    config.write_text("# synthetic configuration content that must not be exported\n")
    monkeypatch.setenv("CARGO_HOME", str(source_repo / "private-cargo-home"))
    monkeypatch.setenv("CARGO_PROFILE_DEV_DEBUG", "0")
    monkeypatch.setenv("RUSTFLAGS", "synthetic-sensitive-flag-content")
    monkeypatch.setattr(
        subprocess, "check_output", lambda *args, **kwargs: "synthetic compiler version"
    )
    result = build_metadata(source_repo, ["cargo", "build", "--locked"])
    serialized = json.dumps(result)
    assert "synthetic-sensitive-flag-content" not in serialized
    assert "synthetic configuration content" not in serialized
    assert result["selected_flags"]["CARGO_PROFILE_DEV_DEBUG"] == "0"
    assert "RUSTFLAGS" in result["additional_flag_env_names_with_values_omitted"]
    assert next(
        item for item in result["cargo_config_files"] if item["path"] == str(config)
    )["content_sha256"]
    assert result["command"] == ["cargo", "build", "--locked"]


def test_binary_selection_uses_cargo_artifact_instead_of_default_target_guess(tmp_path):
    executable = str(
        tmp_path / "configured-target" / "native-triple" / "debug" / "sandhi-proxy"
    )
    artifact = {
        "reason": "compiler-artifact",
        "target": {"name": "sandhi-proxy", "kind": ["bin"]},
        "executable": executable,
        "fresh": True,
        "features": ["default"],
        "profile": {"test": False, "debuginfo": 0},
    }
    output = "non-JSON build tool output\n{not-json}\n" + json.dumps(artifact) + "\n"
    selected = binary_artifact(output, "sandhi-proxy")
    assert selected["executable"] == executable
    assert selected["fresh"] is True
    assert selected["profile"]["debuginfo"] == 0


@pytest.mark.parametrize("case", ["missing", "library", "other_binary", "duplicate"])
def test_missing_or_ambiguous_binary_artifact_never_falls_back_to_stale_file(
    tmp_path, case
):
    artifact = {
        "reason": "compiler-artifact",
        "target": {"name": "sandhi-proxy", "kind": ["bin"]},
        "executable": str(tmp_path / "sandhi-proxy"),
    }
    if case == "library":
        artifact["target"]["kind"] = ["lib"]
    elif case == "other_binary":
        artifact["target"]["name"] = "other"
    records = [] if case == "missing" else [artifact]
    if case == "duplicate":
        records.append(artifact)
    with pytest.raises(AssertionError, match="exactly one"):
        binary_artifact("\n".join(map(json.dumps, records)), "sandhi-proxy")
