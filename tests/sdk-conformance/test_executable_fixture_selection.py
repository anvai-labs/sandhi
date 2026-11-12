"""Executable fixtures must freeze this Cargo build, even beside a stale default target."""

import importlib
import json
from pathlib import Path
from types import SimpleNamespace

import pytest

import conftest
import source_provenance


@pytest.fixture
def configured_cargo(tmp_path, monkeypatch):
    repo = tmp_path / "source"
    repo.mkdir()
    configured = tmp_path / "different-target" / "triple" / "debug"
    files = {}
    for name in ("sandhi-proxy", "sandhi", "sdk_broker_transport"):
        suffix = (
            Path("examples") / name if name == "sdk_broker_transport" else Path(name)
        )
        fresh = configured / suffix
        fresh.parent.mkdir(parents=True, exist_ok=True)
        fresh.write_text("fresh-from-configured-target:" + name)
        stale = repo / "target" / "debug" / suffix
        stale.parent.mkdir(parents=True, exist_ok=True)
        stale.write_text("stale-default-target:" + name)
        files[name] = fresh
    state = {
        "head": "synthetic-head",
        "head_diff_sha256": "synthetic-diff",
        "input_files": [],
    }
    monkeypatch.setenv("CARGO_TARGET_DIR", str(tmp_path / "different-target"))
    monkeypatch.setattr(source_provenance, "source_state", lambda _repo: state)
    monkeypatch.setattr(
        source_provenance, "build_metadata", lambda _repo, command: {"command": command}
    )
    monkeypatch.setattr(conftest, "source_state", lambda _repo: state, raising=False)
    monkeypatch.setattr(
        conftest,
        "build_metadata",
        lambda _repo, command: {"command": command},
        raising=False,
    )
    commands = []
    output_mode = {"value": "normal"}

    def run(command, **kwargs):
        assert command[:2] == ["cargo", "build"]
        commands.append(command)
        names = (
            [command[command.index("--example") + 1]]
            if "--example" in command
            else (
                [command[command.index("--bin") + 1]]
                if "--bin" in command
                else ["sandhi-proxy", "sandhi"]
            )
        )
        features = (
            [command[command.index("--features") + 1]]
            if "--features" in command
            else []
        )
        messages = [
            {
                "reason": "compiler-artifact",
                "target": {
                    "name": name,
                    "kind": ["example" if "--example" in command else "bin"],
                },
                "executable": str(files[name]),
                "features": features,
                "profile": {"test": False, "opt_level": "0"},
                "fresh": True,
            }
            for name in names
        ]
        if output_mode["value"] == "missing_features":
            for message in messages:
                message["features"] = []
        if output_mode["value"] == "missing":
            messages = []
        elif output_mode["value"] == "ambiguous":
            messages *= 2
        return SimpleNamespace(
            stdout="\n".join(map(json.dumps, messages)), returncode=0
        )

    monkeypatch.setattr(source_provenance.subprocess, "run", run)
    return repo, files, commands, output_mode


@pytest.mark.parametrize(
    "fixture_name",
    [
        "backend_binaries",
        "broker_binary",
        "broker_plain_binary",
        "proxy_binary",
        "cli_binary",
    ],
)
def test_owned_fixture_freezes_compiler_artifacts_not_stale_default_target(
    configured_cargo, tmp_path_factory, monkeypatch, fixture_name
):
    repo, files, commands, _mode = configured_cargo
    module = (
        conftest
        if fixture_name in {"proxy_binary", "cli_binary"}
        else importlib.import_module(
            "test_backend_registry"
            if fixture_name == "backend_binaries"
            else "test_broker"
        )
    )
    monkeypatch.setattr(module, "REPO_ROOT", repo)
    result = getattr(module, fixture_name).__wrapped__(tmp_path_factory)
    directory = result if fixture_name == "backend_binaries" else result.parent
    names = (
        ["sandhi"]
        if fixture_name == "cli_binary"
        else (
            ["sandhi-proxy"]
            if fixture_name in {"proxy_binary", "broker_plain_binary"}
            else ["sandhi-proxy", "sandhi"]
        )
    )
    if fixture_name == "broker_binary":
        names.append("sdk_broker_transport")
    for name in names:
        frozen = directory / name
        assert frozen.read_text() == "fresh-from-configured-target:" + name
        files[name].write_text("another later Cargo feature build")
        assert frozen.read_text() == "fresh-from-configured-target:" + name
    assert all(
        "--message-format=json-render-diagnostics" in command for command in commands
    )


@pytest.mark.parametrize("mode", ["missing", "ambiguous"])
def test_owned_fixture_refuses_missing_or_ambiguous_artifacts_even_when_stale_files_exist(
    configured_cargo, tmp_path_factory, monkeypatch, mode
):
    repo, _files, _commands, output_mode = configured_cargo
    module = importlib.import_module("test_backend_registry")
    monkeypatch.setattr(module, "REPO_ROOT", repo)
    output_mode["value"] = mode
    with pytest.raises(AssertionError, match="exactly one"):
        module.backend_binaries.__wrapped__(tmp_path_factory)


def test_example_kind_is_selected_independently_of_same_named_binary(tmp_path):
    name = "sdk_broker_transport"
    records = [
        {
            "reason": "compiler-artifact",
            "target": {"name": name, "kind": [kind]},
            "executable": str(tmp_path / kind / name),
            "features": [],
        }
        for kind in ("bin", "example")
    ]
    output = "\n".join(map(json.dumps, records))
    assert source_provenance.binary_artifact(output, name, kind="example")[
        "executable"
    ] == str(tmp_path / "example" / name)
    assert source_provenance.binary_artifact(output, name)["executable"] == str(
        tmp_path / "bin" / name
    )


def test_feature_build_freezes_cli_proxy_and_example_with_separate_provenance(
    configured_cargo, tmp_path_factory, monkeypatch
):
    repo, _files, _commands, _mode = configured_cargo
    module = importlib.import_module("test_broker")
    monkeypatch.setattr(module, "REPO_ROOT", repo)
    native = module.broker_binary.__wrapped__(tmp_path_factory)
    plain = module.broker_plain_binary.__wrapped__(tmp_path_factory)
    feature_build = json.loads(
        (native.parent / "proxy-build-provenance.json").read_text()
    )
    example_build = json.loads(
        (native.parent / "example-build-provenance.json").read_text()
    )
    plain_build = json.loads((plain.parent / "provenance.json").read_text())
    assert set(feature_build["artifacts"]) == {"sandhi", "sandhi-proxy"}
    for artifact in feature_build["artifacts"].values():
        assert artifact["cargo_artifact"]["features"] == ["sentinelpass-ipc"]
    assert example_build["cargo_artifact"]["target_kind"] == "example"
    assert example_build["cargo_artifact"]["features"] == ["sentinelpass-ipc"]
    assert plain_build["cargo_artifact"]["features"] == []
    assert (
        plain_build["binary_snapshot"]
        != feature_build["artifacts"]["sandhi-proxy"]["binary_snapshot"]
    )


def test_required_native_feature_cannot_silently_use_plain_artifact(
    configured_cargo, tmp_path_factory, monkeypatch
):
    repo, _files, _commands, mode = configured_cargo
    module = importlib.import_module("test_broker")
    monkeypatch.setattr(module, "REPO_ROOT", repo)
    mode["value"] = "missing_features"
    with pytest.raises(AssertionError, match="missing required features"):
        module.broker_binary.__wrapped__(tmp_path_factory)


def test_snapshot_and_manifest_cannot_be_overwritten(configured_cargo, tmp_path):
    repo, _files, _commands, _mode = configured_cargo
    command = ["cargo", "build", "--locked", "--bin", "sandhi"]
    directory = tmp_path / "frozen"
    source_provenance.freeze_cargo_build(repo, command, [("sandhi", "bin")], directory)
    with pytest.raises(AssertionError, match="must not be overwritten"):
        source_provenance.freeze_cargo_build(
            repo, command, [("sandhi", "bin")], directory
        )


def test_real_cargo_bin_and_example_artifacts_freeze_outside_stale_default_target(
    tmp_path, monkeypatch
):
    import subprocess
    from test_source_provenance import source_repo

    (tmp_path / "real-source").mkdir()
    repo = source_repo.__wrapped__(tmp_path / "real-source")
    (repo / "Cargo.toml").write_text(
        '[package]\nname = "artifact-fixture"\nversion = "0.0.0"\nedition = "2021"\n'
        "[features]\nsentinelpass-ipc = []\n"
        '[[bin]]\nname = "fixture-proxy"\npath = "src/main.rs"\n'
        '[[bin]]\nname = "fixture-cli"\npath = "src/main.rs"\n'
        '[[example]]\nname = "fixture-broker"\npath = "src/main.rs"\n'
    )
    (repo / "Cargo.lock").write_text(
        'version = 4\n[[package]]\nname = "artifact-fixture"\nversion = "0.0.0"\n'
    )
    (repo / "src").mkdir()
    (repo / "src/main.rs").write_text(
        'fn main() { println!("{}", cfg!(feature = "sentinelpass-ipc")); }\n'
    )
    stale = repo / "target/debug/fixture-proxy"
    stale.parent.mkdir(parents=True)
    stale.write_bytes(b"stale-default-target")
    configured = tmp_path / "configured-target"
    monkeypatch.setenv("CARGO_TARGET_DIR", str(configured))
    snapshots = source_provenance.freeze_cargo_build(
        repo,
        [
            "cargo",
            "build",
            "--locked",
            "--offline",
            "--bins",
            "--example",
            "fixture-broker",
            "--features",
            "sentinelpass-ipc",
        ],
        [
            ("fixture-proxy", "bin"),
            ("fixture-cli", "bin"),
            ("fixture-broker", "example"),
        ],
        tmp_path / "real-frozen",
        required_features=("sentinelpass-ipc",),
    )
    manifest = json.loads((tmp_path / "real-frozen/provenance.json").read_text())
    for name, snapshot in snapshots.items():
        record = manifest["artifacts"][name]
        assert Path(record["binary_source"]).is_relative_to(configured)
        assert snapshot.read_bytes() != b"stale-default-target"
        assert "sentinelpass-ipc" in record["cargo_artifact"]["features"]
    assert (
        manifest["artifacts"]["fixture-broker"]["cargo_artifact"]["target_kind"]
        == "example"
    )
    assert (
        subprocess.run(
            [str(snapshots["fixture-cli"])], check=True, capture_output=True, text=True
        ).stdout.strip()
        == "true"
    )
