#!/usr/bin/env python3
"""Publish only the immutable, checked npm artifact bundle; never build or run hooks.

This command DOES publish. Unit tests inject transports and command runners; do not run it
against a real version merely to test permissions. Registry errors are not absence.
"""

import argparse
import base64
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import urllib.error
import urllib.parse
import urllib.request

import release_guard as guard

NAMES = ["@anvailabs/sandhi-linux-x64-gnu", "@anvailabs/sandhi-darwin-arm64", "@anvailabs/sandhi"]


def bundle(directory, version):
    guard.stable_tag("v" + version)
    guard.require(version != "0.0.0", "development placeholder cannot be published")
    directory = directory.resolve()
    manifest = directory / "manifest.json"
    guard.require(manifest.is_file() and not manifest.is_symlink() and manifest.stat().st_size < 65536,
                  "unsafe bundle manifest")
    data = json.loads(manifest.read_text())
    guard.require(data.get("validated") is True and data.get("version") == version, "unchecked bundle")
    packages = data.get("packages")
    guard.require(isinstance(packages, list) and len(packages) == 3
                  and [p.get("name") for p in packages] == NAMES, "incomplete or reordered bundle")
    paths = []
    for package in packages:
        name = package["name"]
        filename = name.removeprefix("@").replace("/", "-") + "-" + version + ".tgz"
        guard.require(package.get("version") == version
                      and Path(package.get("tarball", "")).name == filename, "bundle identity mismatch")
        path = directory / filename
        guard.require(path.is_file() and not path.is_symlink() and 0 < path.stat().st_size <= 134217728,
                      "unsafe package archive")
        with path.open("rb") as stream:
            digest = hashlib.file_digest(stream, "sha256").hexdigest()
        guard.require(re.fullmatch(r"[a-f0-9]{64}", package.get("sha256", ""))
                      and digest == package["sha256"], "package digest mismatch")
        guard.require(re.fullmatch(r"sha512-[A-Za-z0-9+/]{86}==", package.get("integrity", "")),
                      "invalid package integrity")
        with path.open("rb") as stream:
            integrity = "sha512-" + base64.b64encode(hashlib.file_digest(stream, "sha512").digest()).decode("ascii")
        guard.require(integrity == package["integrity"], "package integrity mismatch")
        paths.append(path)
    guard.require({p.name for p in directory.iterdir()} == {"manifest.json", *(p.name for p in paths)},
                  "unexpected bundle entries")
    return list(zip(packages, paths))


def registry_version(name, version):
    # No credential sent, and no cross-host redirects followed.
    url = "https://registry.npmjs.org/" + urllib.parse.quote(name, safe="") + "/" + version
    request = urllib.request.Request(url, headers={"User-Agent": "sandhi-release", "Accept": "application/json"})
    try:
        with urllib.request.build_opener(guard.NoRedirect()).open(request, timeout=20) as response:
            guard.require(response.status == 200, "unexpected registry response")
            raw = response.read(2_000_001)
            guard.require(len(raw) <= 2_000_000, "registry metadata exceeds bound")
            data = json.loads(raw)
            guard.require(isinstance(data, dict), "invalid registry metadata")
            return data
    except urllib.error.HTTPError as error:
        status = error.code
        error.close()
        if status == 404:
            return None
        raise guard.Denied("registry unavailable; refusing publication") from None


def publish(directory, version, proof, *, query=registry_version, run=subprocess.run, api=None):
    packages = bundle(directory, version)  # Validate ALL artifacts before any external write.
    api = api or guard.API(os.environ.get("GH_TOKEN", ""))
    authorization = guard.verify_proof_file(api, proof)
    guard.require(authorization["version"] == version, "bundle/authorization version mismatch")
    pending = []
    for package, path in packages:
        existing = query(package["name"], version)
        if existing is not None:
            guard.require(existing.get("name") == package["name"] and existing.get("version") == version
                          and existing.get("dist", {}).get("integrity") == package["integrity"],
                          "existing immutable npm artifact differs; do not overwrite")
        else:
            pending.append(path)
    # Do not leak the read-only GitHub API token into npm's environment.
    npm_env = {key: value for key, value in os.environ.items() if key != "GH_TOKEN"}
    npm_env["npm_config_ignore_scripts"] = "true"
    for path in pending:
        guard.verify_proof_file(api, proof)
        run(["npm", "publish", str(path), "--ignore-scripts", "--access", "public", "--tag", "latest",
             "--registry", "https://registry.npmjs.org"], check=True, timeout=180, env=npm_env)
    return len(pending)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--package-dir", type=Path, required=True)
    parser.add_argument("--version", required=True)
    parser.add_argument("--proof", type=Path, required=True)
    args = parser.parse_args()
    try:
        count = publish(args.package_dir, args.version, args.proof)
        print(f"npm publication completed ({count} uploads); registry verification remains required")
        return 0
    except (ValueError, TypeError, AttributeError, OSError, subprocess.SubprocessError):
        print("npm publication failed: invalid bundle, authority, registry state or upload", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
