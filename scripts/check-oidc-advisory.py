#!/usr/bin/env python3
"""Fail closed when the reviewed RSA advisory non-applicability assumptions drift.

This binds a human-readable operation review to dependency/source identity. It is
not a call-graph proof, and source patterns alone do not establish non-applicability.
"""
import datetime as dt
import hashlib
import json
from pathlib import Path
import subprocess
import tomllib

ROOT = Path(__file__).resolve().parents[1]
RECORD = "docs/security/oidc-rsa-advisory.json"


def check_graph(metadata, expected_present):
    packages = {p["id"]: p for p in metadata["packages"]}
    nodes = {n["id"]: n for n in metadata["resolve"]["nodes"]}
    reachable, pending, parents = set(), list(metadata["workspace_members"]), {}
    while pending:
        current = pending.pop()
        if current in reachable:
            continue
        reachable.add(current)
        for dep in nodes[current]["deps"]:
            if any(k["kind"] != "dev" for k in dep["dep_kinds"]):
                parents.setdefault(dep["pkg"], set()).add(current)
                pending.append(dep["pkg"])
    rsa = [p for p in reachable if packages[p]["name"] == "rsa"]
    if not expected_present:
        if rsa:
            raise ValueError("RSA appeared in a binding production graph")
        return
    if len(rsa) != 1:
        raise ValueError("reviewed RSA dependency route changed")
    consumers = parents.get(rsa[0], set())
    if len(consumers) != 1:
        raise ValueError("additional production RSA consumer")
    oidc = packages[next(iter(consumers))]
    if oidc["name"] != "openidconnect" or oidc["version"] != "4.0.1":
        raise ValueError("production RSA consumer changed")
    oidc_parents = parents.get(oidc["id"], set())
    if len(oidc_parents) != 1 or packages[next(iter(oidc_parents))]["name"] != "sandhi-proxy":
        raise ValueError("additional production OIDC consumer")


def check_record(root, record, today=None):
    today = today or dt.datetime.now(dt.timezone.utc).date()
    if not dt.date.fromisoformat(record["reviewed_on"]) <= today < dt.date.fromisoformat(record["expires_on"]):
        raise ValueError("OIDC RSA advisory assessment expired or future-dated")
    locked = tomllib.loads((root / "Cargo.lock").read_text())["package"]
    for name, expected in record["packages"].items():
        found = [p for p in locked if p["name"] == name]
        if len(found) != 1 or any(found[0].get(k) != v for k, v in expected.items()):
            raise ValueError(f"reviewed {name} version/checksum changed")
    adapter = root / record["adapter"]
    if hashlib.sha256(adapter.read_bytes()).hexdigest() != record["adapter_sha256"]:
        raise ValueError("OIDC adapter changed: reassess private-key operation reachability")
    # Catch ordinary new use sites; the pinned adapter and independent operation
    # review are the actual basis, not this supplemental source-pattern check.
    allowed = {record["adapter"], "crates/sandhi-proxy/src/auth/tests.rs"}
    for source in (root / "crates").rglob("*.rs"):
        text = source.read_text()
        if ("openidconnect" in text or "rsa::" in text) and source.relative_to(root).as_posix() not in allowed:
            raise ValueError(f"new RSA/OIDC use site requires review: {source.relative_to(root)}")


def main():
    record = json.loads((ROOT / RECORD).read_text())
    check_record(ROOT, record)
    for manifest, expected_present in [("Cargo.toml", True),
            ("bindings/python/Cargo.toml", False), ("bindings/node/Cargo.toml", False)]:
        result = subprocess.run(["cargo", "metadata", "--locked", "--all-features",
            "--format-version", "1", "--manifest-path", str(ROOT / manifest)],
            check=True, capture_output=True, text=True)
        check_graph(json.loads(result.stdout), expected_present)
    print("RUSTSEC-2023-0071 non-applicability assumptions match the reviewed record")


if __name__ == "__main__":
    main()
