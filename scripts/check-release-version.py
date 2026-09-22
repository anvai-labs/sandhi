#!/usr/bin/env python3
"""Check all shipped package identities against the committed workspace version.

--sync updates binding manifests and first-party lock entries for a reviewed release
preparation; it never changes the authoritative Cargo.toml version or external packages.
No release job uses --sync. Protocol/schema versions are deliberately independent.
"""
import argparse
import json
from pathlib import Path
import re
import sys
import tomllib

CRATES = {"sandhi-core", "sandhi-providers", "sandhi-store", "sandhi-proxy"}
BINDINGS = {"bindings/python": "sandhi-gateway-py", "bindings/node": "sandhi-node"}


def stable_version(value):
    if not isinstance(value, str) or not re.fullmatch(r"(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)", value):
        raise ValueError("expected stable package version X.Y.Z")
    return value


def check(root, expected=None, sync=False):
    root = Path(root)
    version = stable_version(tomllib.loads((root / "Cargo.toml").read_text())["workspace"]["package"]["version"])
    if expected is not None and stable_version(expected) != version:
        raise ValueError("release tag disagrees with committed workspace version")
    proposals = {}
    for name in CRATES:
        path = root / "crates" / name / "Cargo.toml"
        doc = tomllib.loads(path.read_text())
        if doc["package"]["name"] != name or doc["package"]["version"] != {"workspace": True}:
            raise ValueError("first-party crates must inherit workspace version")
        for dependency, detail in doc.get("dependencies", {}).items():
            if dependency in CRATES and detail.get("version") != version:
                raise ValueError("first-party dependency drift; run stage-crates-release.py first")
    for directory, name in BINDINGS.items():
        path = root / directory / "Cargo.toml"
        raw = path.read_text()
        doc = tomllib.loads(raw)
        if doc["package"]["name"] != name:
            raise ValueError("unexpected binding package")
        old = doc["package"]["version"]
        updated, count = re.subn(r'(?m)^version = "' + re.escape(old) + r'"$', f'version = "{version}"', raw)
        if count != 1:
            raise ValueError("ambiguous binding version")
        if raw != updated: proposals[path] = updated
    for relative in ("bindings/node/package.json", "bindings/node/package-lock.json"):
        path = root / relative
        doc = json.loads(path.read_text())
        if doc.get("name") != "@anvailabs/sandhi": raise ValueError("unexpected npm package")
        changed = doc.get("version") != version
        doc["version"] = version
        if "packages" in doc:
            changed |= doc["packages"][""].get("version") != version
            doc["packages"][""]["version"] = version
        if changed: proposals[path] = json.dumps(doc, indent=2, ensure_ascii=True) + "\n"
    for relative in ("Cargo.lock", "bindings/python/Cargo.lock", "bindings/node/Cargo.lock"):
        path = root / relative
        raw = path.read_text()
        packages = tomllib.loads(raw)["package"]
        owned = CRATES | set(BINDINGS.values())
        seen = set()
        for package in packages:
            if package["name"] not in owned: continue
            name = package["name"]
            if name in seen or "source" in package: raise ValueError("ambiguous first-party lock package")
            seen.add(name)
            pattern = r'(\[\[package\]\]\nname = "' + re.escape(name) + r'"\nversion = ")[^"]+("\n)'
            updated, count = re.subn(pattern, lambda m: m[1] + version + m[2], raw)
            if count != 1: raise ValueError("unsupported first-party lock entry")
            raw = updated
        required = CRATES if relative == "Cargo.lock" else {"sandhi-core", "sandhi-providers", BINDINGS[relative.rsplit("/", 1)[0]]}
        if not required <= seen: raise ValueError("missing first-party lock entry")
        if path.read_text() != raw: proposals[path] = raw
    if proposals and not sync:
        raise ValueError("package version drift: " + ", ".join(str(p.relative_to(root)) for p in proposals))
    # All documents validate before any write. Run in an isolated worktree without concurrent edits.
    for path, content in proposals.items(): path.write_text(content)
    return version


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--workspace", type=Path, default=Path("."))
    parser.add_argument("--version")
    parser.add_argument("--sync", action="store_true")
    args = parser.parse_args(argv)
    try:
        version = check(args.workspace, args.version, args.sync)
    except (OSError, ValueError, KeyError, TypeError) as error:
        print(f"FAIL: {error}", file=sys.stderr)
        return 1
    print(f"All package identities match {version}")
    return 0

if __name__ == "__main__":
    sys.exit(main())
