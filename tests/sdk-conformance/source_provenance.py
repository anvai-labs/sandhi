"""Bounded source provenance for synthetic binary fixtures; not a hermetic build claim."""

from __future__ import annotations

import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess


def _git(repo: Path, *args: str) -> bytes:
    return subprocess.check_output(["git", "--no-optional-locks", *args], cwd=repo)


def source_state(repo: Path) -> dict:
    """Hash Git-visible source, including staged edits and nonignored untracked inputs."""
    head = _git(repo, "rev-parse", "HEAD").decode().strip()
    diff = _git(repo, "diff", "--no-ext-diff", "--no-textconv", "HEAD", "--binary")
    paths = _git(repo, "ls-files", "--cached", "--others", "--exclude-standard", "-z")
    inputs = []
    for name in sorted(set(paths.split(b"\0")) - {b""}):
        relative = os.fsdecode(name)
        path = repo / relative
        if path.is_symlink():
            item = {
                "path": relative,
                "kind": "symlink",
                "link_sha256": hashlib.sha256(
                    os.fsencode(os.readlink(path))
                ).hexdigest(),
            }
            if path.is_file():
                item["content_sha256"] = hashlib.sha256(path.read_bytes()).hexdigest()
            inputs.append(item)
        elif path.is_file():
            inputs.append(
                {
                    "path": relative,
                    "kind": "file",
                    "content_sha256": hashlib.sha256(path.read_bytes()).hexdigest(),
                }
            )
        elif not path.exists():
            inputs.append({"path": relative, "kind": "deleted"})
        else:
            raise AssertionError(
                f"source inventory cannot fingerprint non-file: {relative}"
            )
    assert (
        head == _git(repo, "rev-parse", "HEAD").decode().strip()
    ), "HEAD changed during source snapshot"
    return {
        "head": head,
        "head_diff_sha256": hashlib.sha256(diff).hexdigest(),
        "input_files": inputs,
    }


def assert_unchanged(before: dict, after: dict) -> None:
    assert before["head"] == after["head"], "HEAD changed during binary build"
    assert (
        before["head_diff_sha256"] == after["head_diff_sha256"]
    ), "tracked diff changed during binary build"
    assert (
        before["input_files"] == after["input_files"]
    ), "source input contents changed during binary build"


def build_metadata(repo: Path, command: list[str]) -> dict:
    """Record explicit invocation, tool versions and config hashes without config/secret values."""
    configs = []
    cargo_home = Path(os.environ.get("CARGO_HOME", str(Path.home() / ".cargo")))
    directories = [directory / ".cargo" for directory in (repo, *repo.parents)] + [
        cargo_home
    ]
    seen = set()
    for directory in directories:
        for name in ("config", "config.toml"):
            path = directory / name
            if path in seen or not path.is_file():
                continue
            seen.add(path)
            configs.append(
                {
                    "path": str(path),
                    "content_sha256": hashlib.sha256(path.read_bytes()).hexdigest(),
                }
            )
    safe_flags = {}
    for name, allowed in {
        "CARGO_NET_OFFLINE": {"true", "false", "1", "0"},
        "CARGO_PROFILE_DEV_DEBUG": {
            "0",
            "1",
            "2",
            "none",
            "limited",
            "full",
            "line-tables-only",
            "line-directives-only",
        },
        "CARGO_PROFILE_TEST_DEBUG": {
            "0",
            "1",
            "2",
            "none",
            "limited",
            "full",
            "line-tables-only",
            "line-directives-only",
        },
    }.items():
        value = os.environ.get(name)
        if value is not None:
            safe_flags[name] = value if value in allowed else "set; value omitted"
    jobs = os.environ.get("CARGO_BUILD_JOBS")
    if jobs is not None:
        safe_flags["CARGO_BUILD_JOBS"] = (
            jobs if jobs.isascii() and jobs.isdigit() else "set; value omitted"
        )
    extra_flag_names = [
        name
        for name in (
            "RUSTFLAGS",
            "CARGO_ENCODED_RUSTFLAGS",
            "CARGO_BUILD_RUSTFLAGS",
            "RUSTC",
            "RUSTC_WRAPPER",
            "RUSTC_WORKSPACE_WRAPPER",
            "CARGO_BUILD_TARGET",
            "RUSTUP_TOOLCHAIN",
            "CARGO_PROFILE_DEV_OPT_LEVEL",
            "CARGO_PROFILE_DEV_LTO",
            "CARGO_PROFILE_DEV_CODEGEN_UNITS",
            "CARGO_PROFILE_DEV_PANIC",
            "CARGO_INCREMENTAL",
        )
        if name in os.environ
    ]
    return {
        "command": command,
        "cargo_executable": shutil.which(command[0]),
        "cargo_version": subprocess.check_output(
            [command[0], "-V"], cwd=repo, text=True
        ).strip(),
        "rustc_version": subprocess.check_output(
            ["rustc", "-vV"], cwd=repo, text=True
        ).strip(),
        "selected_flags": safe_flags,
        "additional_flag_env_names_with_values_omitted": extra_flag_names,
        "cargo_config_files": configs,
        "scope": "Git-visible tracked and nonignored untracked files; selected Cargo configuration hashes",
        "limits": [
            "Not a hermetic compiler input read-set or proof of a reproducible build.",
            "Git-ignored inputs, external dependency/generated inputs and directory symlink contents are not inventoried.",
            "Arbitrary environment/config values are never recorded; extra flag values may be omitted.",
        ],
    }


def inventory_digest(state: dict) -> str:
    return hashlib.sha256(
        json.dumps(state, sort_keys=True, separators=(",", ":")).encode()
    ).hexdigest()


def binary_artifact(output: str, name: str) -> dict:
    """Select the executable Cargo actually built, including configured target layouts."""
    matches = []
    for line in output.splitlines():
        if not line.startswith("{"):
            continue
        try:
            message = json.loads(line)
        except json.JSONDecodeError:
            continue
        if not isinstance(message, dict):
            continue
        target = message.get("target", {})
        if (
            message.get("reason") == "compiler-artifact"
            and target.get("name") == name
            and target.get("kind") == ["bin"]
            and isinstance(message.get("executable"), str)
        ):
            path = Path(message["executable"])
            assert path.is_absolute(), "Cargo executable path must be absolute"
            profile = message.get("profile", {})
            matches.append(
                {
                    "executable": str(path),
                    "features": message.get("features", []),
                    "fresh": message.get("fresh"),
                    "profile": {
                        key: profile.get(key)
                        for key in (
                            "opt_level",
                            "debuginfo",
                            "debug_assertions",
                            "overflow_checks",
                            "test",
                        )
                    },
                }
            )
    assert len(matches) == 1, "Cargo must report exactly one matching binary executable"
    return matches[0]
