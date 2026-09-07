"""No registry writes: command runner and registry are always replaced by fixtures."""
import hashlib
import base64
import importlib.util
import json
from pathlib import Path
import sys

import pytest

SCRIPTS = Path(__file__).parents[2] / "scripts"
sys.path.insert(0, str(SCRIPTS))
SPEC = importlib.util.spec_from_file_location("publish_npm", SCRIPTS / "publish-npm-release.py")
publisher = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(publisher)


@pytest.fixture
def packet(tmp_path, monkeypatch):
    data = {"validated": True, "version": "0.6.0", "packages": []}
    for name in publisher.NAMES:
        path = tmp_path / (name.removeprefix("@").replace("/", "-") + "-0.6.0.tgz")
        path.write_bytes(b"checked package fixture")
        data["packages"].append(dict(name=name, version="0.6.0", tarball="/build/" + path.name,
            sha256=hashlib.sha256(path.read_bytes()).hexdigest(),
            integrity="sha512-" + base64.b64encode(hashlib.sha512(path.read_bytes()).digest()).decode("ascii")))
    (tmp_path / "manifest.json").write_text(json.dumps(data))
    monkeypatch.setattr(publisher.guard, "verify_proof_file", lambda *args: {"version": "0.6.0"})
    return tmp_path, data


def test_publish_checked_tarballs_only(packet, monkeypatch):
    path, _ = packet
    calls = []
    monkeypatch.setenv("GH_TOKEN", "must-not-reach-npm")
    assert publisher.publish(path, "0.6.0", Path("proof"), query=lambda *a: None,
        run=lambda args, **kw: calls.append((args, kw))) == 3
    assert len(calls) == 3
    for args, kwargs in calls:
        assert args[:2] == ["npm", "publish"] and args[2].endswith(".tgz")
        assert "--ignore-scripts" in args and kwargs["check"] and kwargs["timeout"] == 180
        assert "GH_TOKEN" not in kwargs["env"]


@pytest.mark.parametrize("fault", ["missing", "changed", "extra", "symlink", "version", "name", "order", "unchecked"])
def test_invalid_bundle_denied_before_any_query_or_write(packet, fault):
    path, data = packet
    first = path / Path(data["packages"][0]["tarball"]).name
    if fault == "missing": first.unlink()
    if fault == "changed": first.write_bytes(b"changed")
    if fault == "extra": (path / "extra.tgz").write_bytes(b"extra")
    if fault == "symlink":
        first.unlink()
        first.symlink_to(path / "manifest.json")
    if fault == "version": data["packages"][0]["version"] = "0.7.0"
    if fault == "name": data["packages"][0]["name"] = "@other/name"
    if fault == "order": data["packages"].reverse()
    if fault == "unchecked": data["validated"] = False
    (path / "manifest.json").write_text(json.dumps(data))
    def forbidden(*a, **kw): raise AssertionError("external call occurred")
    with pytest.raises(publisher.guard.Denied):
        publisher.publish(path, "0.6.0", Path("proof"), query=forbidden, run=forbidden)


def test_partial_retry_preserves_all_manifests(packet):
    path, data = packet
    original = (path / "manifest.json").read_bytes()
    first = data["packages"][0]
    def query(name, version):
        return {"name": name, "version": version, "dist": {"integrity": first["integrity"]}} if name == first["name"] else None
    calls = []
    assert publisher.publish(path, "0.6.0", Path("proof"), query=query,
        run=lambda args, **kw: calls.append(args)) == 2
    assert (path / "manifest.json").read_bytes() == original


def test_existing_root_mismatch_denied_before_platform_publication(packet):
    path, _ = packet
    def query(name, version):
        return {"name": name, "version": version, "dist": {"integrity": "different"}} if name == publisher.NAMES[-1] else None
    with pytest.raises(publisher.guard.Denied):
        publisher.publish(path, "0.6.0", Path("proof"), query=query,
            run=lambda *a, **kw: pytest.fail("must preflight all before writing"))


def test_authority_rechecked_before_each_write(packet, monkeypatch):
    path, _ = packet
    calls = []
    monkeypatch.setattr(publisher.guard, "verify_proof_file", lambda *a: calls.append("check") or {"version": "0.6.0"})
    publisher.publish(path, "0.6.0", Path("proof"), query=lambda *a: None,
        run=lambda *a, **kw: calls.append("write"))
    assert calls == ["check", "check", "write", "check", "write", "check", "write"]


def test_unverified_integrity_cannot_skip_different_existing_artifact(packet):
    path, data = packet
    data["packages"][0]["integrity"] = "sha512-" + "A" * 86 + "=="
    (path / "manifest.json").write_text(json.dumps(data))
    with pytest.raises(publisher.guard.Denied): publisher.bundle(path, "0.6.0")


def test_bundle_version_must_match_authorization(packet, monkeypatch):
    path, _ = packet
    monkeypatch.setattr(publisher.guard, "verify_proof_file", lambda *a: {"version": "0.7.0"})
    with pytest.raises(publisher.guard.Denied):
        publisher.publish(path, "0.6.0", Path("proof"),
            query=lambda *a: pytest.fail("version mismatch must precede registry lookup"))
