#!/usr/bin/env python3
"""Stage only the four first-party crate versions, without executing build code.

This deliberately supports Sandhi's current topology, not arbitrary Cargo manifests.
All inputs and proposed TOML documents are validated before any write. Other bytes
are retained exactly. Run on an isolated checkout, without concurrent writers.
Cargo.lock is intentionally untouched: subsequent Cargo commands without --locked
refresh workspace package versions. Compile verification belongs in an unprivileged
job; the publisher uses cargo publish --no-verify, not a build helper installation.
"""

import argparse
import copy
import os
from pathlib import Path
import re
import stat
import sys
import tomllib


MAX_MANIFEST_BYTES = 1024 * 1024
CRATES = ("sandhi-core", "sandhi-providers", "sandhi-store", "sandhi-proxy")
EDGES = {
    "sandhi-core": (),
    "sandhi-providers": ("sandhi-core",),
    "sandhi-store": ("sandhi-core",),
    "sandhi-proxy": ("sandhi-core", "sandhi-providers", "sandhi-store"),
}
MANIFESTS = ("Cargo.toml", *(f"crates/{name}/Cargo.toml" for name in CRATES))


class Denied(ValueError):
    pass


def require(condition, message):
    if not condition:
        raise Denied(message)


def stable_version(value):
    require(isinstance(value, str) and len(value) <= 62
            and re.fullmatch(r"(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)", value),
            "version must be a canonical stable X.Y.Z")
    require(all(int(part) <= 2**64 - 1 for part in value.split(".")), "version component overflow")
    require(value != "0.0.0", "development placeholder cannot be published")
    return value


def parse(raw):
    try:
        return tomllib.loads(raw.decode("utf-8"))
    except (UnicodeError, tomllib.TOMLDecodeError, RecursionError) as error:
        raise Denied("invalid or excessively nested manifest") from error


def check_dependencies(document, expected, old_version):
    seen = set()

    def visit(table, location=()):
        if not isinstance(table, dict):
            return
        for key, value in table.items():
            here = (*location, key)
            if key in ("dependencies", "dev-dependencies", "build-dependencies"):
                require(isinstance(value, dict), "unsupported dependency table")
                for name, dependency in value.items():
                    target = dependency.get("package", name) if isinstance(dependency, dict) else name
                    local = isinstance(dependency, dict) and "path" in dependency
                    if name not in CRATES and target not in CRATES and not local:
                        continue  # Preserve external registry/git dependencies byte-for-byte.
                    require(here == ("dependencies",) and name in expected and target == name,
                            "unexpected local dependency or crate alias")
                    require(isinstance(dependency, dict)
                            and dependency.get("path") == "../" + name
                            and dependency.get("version") == old_version
                            and not ({"git", "registry", "workspace", "package"} & dependency.keys()),
                            "local dependency must use the expected path and current version")
                    seen.add(name)
            if isinstance(value, dict):
                visit(value, here)
            elif isinstance(value, list):
                for item in value:
                    if isinstance(item, dict):
                        visit(item, (*here, "[]"))

    visit(document)
    require(seen == set(expected), "missing required first-party dependency")


def validate(documents):
    root = documents["Cargo.toml"]
    require(set(root) == {"workspace"}, "unsupported root manifest topology")
    workspace = root.get("workspace", {})
    require(isinstance(workspace, dict) and set(workspace) == {"resolver", "members", "package"}
            and workspace.get("resolver") == "2"
            and workspace.get("members") == [f"crates/{name}" for name in CRATES],
            "workspace must contain exactly the four expected members in canonical order")
    package = workspace.get("package")
    require(isinstance(package, dict), "missing workspace package")
    old_version = stable_version(package.get("version"))
    check_dependencies(root, (), old_version)
    for name in CRATES:
        document = documents[f"crates/{name}/Cargo.toml"]
        package = document.get("package", {})
        require(isinstance(package, dict) and package.get("name") == name
                and isinstance(package.get("version"), dict)
                and set(package["version"]) == {"workspace"}
                and package["version"]["workspace"] is True
                and "workspace" not in package
                and not ({"workspace", "patch", "replace"} & document.keys()),
                "crate must inherit the expected workspace version without overrides")
        check_dependencies(document, EDGES[name], old_version)
    return old_version


def replace_version(line, old_version, version):
    pattern = r'(?<![\w-])version[ \t]*=[ \t]*"' + re.escape(old_version) + r'"'
    matches = list(re.finditer(pattern, line))
    require(len(matches) == 1, "unsupported or ambiguous version assignment syntax")
    match = matches[0]
    start = match.end() - len(old_version) - 1
    return line[:start] + version + line[start + len(old_version):]


def propose(raw, old_version, version, dependencies=None):
    text = raw.decode("utf-8")
    if dependencies is None:
        pattern = r"(?m)^\[workspace\.package\][ \t]*(?:#[^\r\n]*)?\r?\n(?P<body>.*?)(?=^\[|\Z)"
        matches = list(re.finditer(pattern, text, re.DOTALL))
        require(len(matches) == 1, "unsupported workspace.package section syntax")
        section = matches[0]
        body = section.group("body")
        lines = list(re.finditer(r'(?m)^version[ \t]*=[^\r\n]*', body))
        require(len(lines) == 1, "unsupported workspace version syntax")
        line = lines[0]
        start, end = section.start("body") + line.start(), section.start("body") + line.end()
        text = text[:start] + replace_version(line.group(), old_version, version) + text[end:]
    else:
        for name in dependencies:
            pattern = r"(?m)^" + re.escape(name) + r"[ \t]*=[ \t]*\{[^\r\n]*\}[ \t]*(?:#[^\r\n]*)?\r?$"
            matches = list(re.finditer(pattern, text))
            require(len(matches) == 1, "local dependencies must use single-line inline tables")
            match = matches[0]
            text = text[:match.start()] + replace_version(match.group(), old_version, version) + text[match.end():]
    return text.encode("utf-8")


def stage(workspace, version):
    stable_version(version)
    workspace = Path(workspace).absolute()
    # Reject symlinked ancestors, not only the leaf Cargo.toml. No path supplied
    # by a manifest is ever used for IO; the file allowlist is fixed above.
    require(".." not in workspace.parts, "workspace path must not contain parent traversal")
    for directory in (*reversed(workspace.parents), workspace):
        require(not directory.is_symlink() and directory.is_dir(), "unsafe workspace directory")
    streams = {}
    try:
        originals = {}
        for relative in MANIFESTS:
            path = workspace / relative
            for directory in path.relative_to(workspace).parents:
                require(not (workspace / directory).is_symlink()
                        and (workspace / directory).is_dir(), "unsafe manifest directory")
            info = path.lstat()
            require(stat.S_ISREG(info.st_mode) and info.st_nlink == 1
                    and 0 < info.st_size <= MAX_MANIFEST_BYTES, "unsafe manifest file")
            descriptor = os.open(path, os.O_RDWR | os.O_NOFOLLOW | os.O_NONBLOCK)
            stream = os.fdopen(descriptor, "r+b")
            streams[relative] = stream
            opened = os.fstat(stream.fileno())
            require((opened.st_dev, opened.st_ino) == (info.st_dev, info.st_ino), "manifest changed during open")
            raw = stream.read(MAX_MANIFEST_BYTES + 1)
            require(0 < len(raw) <= MAX_MANIFEST_BYTES, "manifest exceeds size limit")
            originals[relative] = raw
        documents = {name: parse(raw) for name, raw in originals.items()}
        old_version = validate(documents)
        updated = {"Cargo.toml": propose(originals["Cargo.toml"], old_version, version)}
        expected = copy.deepcopy(documents)
        expected["Cargo.toml"]["workspace"]["package"]["version"] = version
        for name in CRATES:
            relative = f"crates/{name}/Cargo.toml"
            updated[relative] = propose(originals[relative], old_version, version, EDGES[name])
            for dependency in EDGES[name]:
                expected[relative]["dependencies"][dependency]["version"] = version
        parsed = {name: parse(raw) for name, raw in updated.items()}
        require(parsed == expected, "staging changed fields other than intended versions")
        require(validate(parsed) == version, "staged topology failed validation")
        for relative, stream in streams.items():
            stream.seek(0)
            require(stream.read(MAX_MANIFEST_BYTES + 1) == originals[relative], "manifest changed during staging")
        changed = 0
        for relative, stream in streams.items():
            if updated[relative] == originals[relative]:
                continue
            stream.seek(0)
            stream.write(updated[relative])
            stream.truncate()
            stream.flush()
            changed += 1
        return changed
    finally:
        for stream in streams.values():
            stream.close()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--workspace", type=Path, required=True)
    parser.add_argument("--version", required=True)
    args = parser.parse_args()
    try:
        changed = stage(args.workspace, args.version)
    except (Denied, OSError, RecursionError) as error:
        print(f"Crate staging denied: {error}", file=sys.stderr)
        return 1
    print(f"Staged {args.version}; changed {changed} manifests; Cargo.lock unchanged")
    return 0


if __name__ == "__main__":
    sys.exit(main())
