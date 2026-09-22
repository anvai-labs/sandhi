"""Offline staging tests use copies only; never mutate the source manifests."""

import importlib.util
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tomllib

import pytest


ROOT = Path(__file__).resolve().parents[2]
SPEC = importlib.util.spec_from_file_location("stage_crates", ROOT / "scripts/stage-crates-release.py")
stager = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(stager)


@pytest.fixture
def workspace(tmp_path):
    for relative in stager.MANIFESTS:
        target = tmp_path / relative
        target.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(ROOT / relative, target)
    shutil.copyfile(ROOT / "Cargo.lock", tmp_path / "Cargo.lock")
    # Fixed old-version fixture keeps mutation cases independent of future release bumps.
    stager.stage(tmp_path, "0.3.0")
    (tmp_path / "untouched.bin").write_bytes(bytes(range(256)))
    return tmp_path


def snapshot(workspace):
    return {path.relative_to(workspace): path.read_bytes()
            for path in workspace.rglob("*") if path.is_file()}


def alter(workspace, relative, old, new):
    path = workspace / relative
    text = path.read_text()
    assert old in text
    path.write_text(text.replace(old, new))


def reject_without_mutation(workspace, version="0.6.0"):
    before = snapshot(workspace)
    with pytest.raises((stager.Denied, OSError)):
        stager.stage(workspace, version)
    assert snapshot(workspace) == before


def test_real_manifests_stage_exact_versions_and_preserve_other_bytes(workspace):
    before = snapshot(workspace)
    old = tomllib.loads(before[Path("Cargo.toml")].decode())["workspace"]["package"]["version"]
    assert stager.stage(workspace, "0.6.0") == 4
    after = snapshot(workspace)
    assert set(before) == set(after)
    for relative, original in before.items():
        expected = original
        if str(relative) == "Cargo.toml" or str(relative) in {
                f"crates/{name}/Cargo.toml" for name in stager.CRATES if stager.EDGES[name]}:
            expected = expected.replace(f'version = "{old}"'.encode(), b'version = "0.6.0"')
        assert after[relative] == expected
    assert stager.stage(workspace, "0.6.0") == 0
    assert snapshot(workspace) == after


@pytest.mark.parametrize("version", ["", "v0.6.0", "0.6", "0.6.0-rc.1", "0.6.0+build", "00.6.0",
                                     "0.06.0", "0.6.00", "0.6.0\n", "../0.6.0", "0.0.0",
                                     "18446744073709551616.0.0", "9" * 10000, None, 6])
def test_invalid_version_has_no_writes(workspace, version):
    reject_without_mutation(workspace, version)


@pytest.mark.parametrize("old,new", [
    ('"crates/sandhi-store",', ''),
    ('"crates/sandhi-store",', '"crates/*",'),
    ('"crates/sandhi-store",', '"../sandhi-store",'),
    ('resolver = "2"', 'resolver = "3"'),
    ('[workspace.package]', '[workspace.package]\npublish = false\n[workspace.dependencies]\nx = "1"'),
    ('[workspace]', '[workspace]\nexclude = ["crates/sandhi-store"]'),
    ('[workspace]', '[workspace]\ndefault-members = ["crates/sandhi-core"]'),
])
def test_changed_workspace_topology_denied(workspace, old, new):
    alter(workspace, "Cargo.toml", old, new)
    reject_without_mutation(workspace)


@pytest.mark.parametrize("old,new", [
    ('name = "sandhi-proxy"', 'name = "impostor"'),
    ('version.workspace = true', 'version = "0.3.0"'),
    ('version.workspace = true', 'version.workspace = 1'),
    ('version.workspace = true', 'version.workspace = true\nworkspace = "../../../elsewhere"'),
    ('path = "../sandhi-core"', 'path = "../../elsewhere"'),
    ('path = "../sandhi-core", ', ''),
    ('path = "../sandhi-core"', 'path = "../sandhi-core", git = "https://example.invalid/repo"'),
    ('path = "../sandhi-core"', 'path = "../sandhi-core", registry = "other"'),
    ('path = "../sandhi-core"', 'path = "../sandhi-core", package = "impostor"'),
    ('path = "../sandhi-core", version = "0.3.0"', 'path = "../sandhi-core"'),
    ('path = "../sandhi-core", version = "0.3.0"', 'path = "../sandhi-core", version = "^0.3.0"'),
    ('sandhi-core = { path = "../sandhi-core", version = "0.3.0" }', ''),
    ('sandhi-core = {', 'renamed-core = { package = "sandhi-core",'),
    ('[dev-dependencies]', '[dev-dependencies]\nsandhi-core = "0.3.0"'),
    ('[dependencies]', '[dependencies]\nother = { path = "../../other", version = "1" }'),
    ('[dependencies]', '[dependencies]\nalias = { package = "sandhi-core", version = "0.3.0" }'),
    ('[dependencies]', '[patch.crates-io]\nx = { git = "https://example.invalid/x" }\n[dependencies]'),
])
def test_last_manifest_fault_denied_before_any_write(workspace, old, new):
    alter(workspace, "crates/sandhi-proxy/Cargo.toml", old, new)
    reject_without_mutation(workspace)


@pytest.mark.parametrize("replacement", [
    "sandhi-core = { path = '../sandhi-core', version = '0.3.0' }",
    '"sandhi-core" = { path = "../sandhi-core", version = "0.3.0" }',
    '[dependencies.sandhi-core]\npath = "../sandhi-core"\nversion = "0.3.0"\n[dependencies.placeholder]',
])
def test_unsupported_syntax_fails_closed(workspace, replacement):
    alter(workspace, "crates/sandhi-providers/Cargo.toml",
          'sandhi-core = { path = "../sandhi-core", version = "0.3.0" }', replacement)
    reject_without_mutation(workspace)


def test_git_dependency_comments_and_crlf_preserved(workspace):
    path = workspace / "crates/sandhi-core/Cargo.toml"
    path.write_bytes(path.read_bytes() + b'\nexternal = { git = "https://example.invalid/a", rev = "deadbeef" } # unchanged\n')
    for relative in stager.MANIFESTS:
        path = workspace / relative
        path.write_bytes(path.read_bytes().replace(b"\n", b"\r\n"))
    before = (workspace / "crates/sandhi-core/Cargo.toml").read_bytes()
    assert stager.stage(workspace, "0.6.0") == 4
    assert (workspace / "crates/sandhi-core/Cargo.toml").read_bytes() == before
    assert b'\r\nversion = "0.6.0"\r\n' in (workspace / "Cargo.toml").read_bytes()


def test_version_lookalike_in_string_denied_without_semantic_change(workspace):
    # A line-oriented editor must not mistake TOML multiline text for a field.
    alter(workspace, "crates/sandhi-store/Cargo.toml", '[dependencies]',
          '[package.metadata]\nnote = \'\'\'\nsandhi-core = { path = "../sandhi-core", version = "0.3.0" }\n\'\'\'\n[dependencies]')
    reject_without_mutation(workspace)


@pytest.mark.parametrize("fault", ["file-symlink", "directory-symlink", "hardlink", "oversize", "invalid", "missing"])
def test_unsafe_or_invalid_manifest_denied_without_writes(workspace, tmp_path, fault):
    path = workspace / "crates/sandhi-proxy/Cargo.toml"
    if fault == "file-symlink":
        target = workspace / "saved.toml"
        path.rename(target)
        path.symlink_to(target)
    elif fault == "directory-symlink":
        target = workspace / "saved-dir"
        path.parent.rename(target)
        path.parent.symlink_to(target, target_is_directory=True)
    elif fault == "hardlink":
        os.link(path, workspace / "saved.toml")
    elif fault == "oversize":
        path.write_bytes(b"#" * (stager.MAX_MANIFEST_BYTES + 1))
    elif fault == "invalid":
        path.write_bytes(b"\xffinvalid TOML")
    else:
        path.unlink()
    reject_without_mutation(workspace)


def test_symlinked_workspace_denied(workspace):
    link = workspace / "workspace-link"
    link.symlink_to(workspace, target_is_directory=True)
    before = snapshot(workspace)
    with pytest.raises(stager.Denied):
        stager.stage(link, "0.6.0")
    assert snapshot(workspace) == before


def test_cli_offline(workspace):
    result = subprocess.run([sys.executable, str(ROOT / "scripts/stage-crates-release.py"),
                             "--workspace", str(workspace), "--version", "0.6.0"],
                            capture_output=True, text=True, timeout=15, check=True)
    assert "changed 4 manifests; Cargo.lock unchanged" in result.stdout


def test_no_subprocess_or_network_used(workspace, monkeypatch):
    import socket
    def denied(*args, **kwargs):
        pytest.fail("staging attempted subprocess or network IO")
    monkeypatch.setattr(subprocess, "Popen", denied)
    monkeypatch.setattr(socket, "socket", denied)
    assert stager.stage(workspace, "0.6.0") == 4

# Version consistency is the same packaging concern as staging, so keep its drift
# regressions in this existing owner rather than cloning release workflow tests.
IDENTITY_SPEC = importlib.util.spec_from_file_location("release_identity", ROOT / "scripts/check-release-version.py")
identity = importlib.util.module_from_spec(IDENTITY_SPEC)
IDENTITY_SPEC.loader.exec_module(identity)


def test_committed_package_versions_are_consistent():
    assert identity.check(ROOT) == tomllib.loads((ROOT / "Cargo.toml").read_text())["workspace"]["package"]["version"]


@pytest.mark.parametrize("relative", ["bindings/python/Cargo.toml", "bindings/node/Cargo.toml",
    "bindings/node/package.json", "bindings/node/package-lock.json", "Cargo.lock",
    "bindings/python/Cargo.lock", "bindings/node/Cargo.lock"])
def test_binding_and_lock_drift_is_detected_and_sync_preserves_external_versions(tmp_path, relative):
    import json
    files = (*stager.MANIFESTS, "Cargo.lock", "bindings/python/Cargo.toml", "bindings/node/Cargo.toml",
             "bindings/node/package.json", "bindings/node/package-lock.json", "bindings/python/Cargo.lock", "bindings/node/Cargo.lock")
    for name in files:
        target = tmp_path / name
        target.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(ROOT / name, target)
    version = identity.check(tmp_path)
    path = tmp_path / relative
    raw = path.read_text()
    old = f'"version": "{version}"' if relative.endswith('.json') else f'version = "{version}"'
    if relative.endswith('Cargo.lock'):
        # Third-party packages can legitimately share our version. Corrupt the
        # named first-party package, not whichever version happens to come first.
        old = f'name = "sandhi-core"\nversion = "{version}"'
    assert old in raw
    path.write_text(raw.replace(old, old.replace(version, "9.9.9"), 1))
    before = snapshot(tmp_path)
    with pytest.raises(ValueError, match="drift"):
        identity.check(tmp_path)
    assert snapshot(tmp_path) == before
    with pytest.raises(ValueError, match="tag disagrees"):
        identity.check(tmp_path, "8.8.8", sync=True)
    assert snapshot(tmp_path) == before
    identity.check(tmp_path, sync=True)
    assert identity.check(tmp_path) == version
    assert path.read_text() == raw
