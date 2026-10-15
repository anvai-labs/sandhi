#!/usr/bin/env python3
"""Read or update the SentinelPass protocol pin without changing its shape.

Usage: bump_protocol_pin.py --current | <current> <latest>.
Supported inline dependency shapes are version="X.Y.Z", version="=X.Y.Z"
and the historical tag="vX.Y.Z". Only the live dependencies entry may change.
"""
import copy
import pathlib
import re
import sys
import tomllib

MANIFEST = pathlib.Path("crates/sandhi-store/Cargo.toml")
FORMS = (("version", "="), ("version", ""), ("tag", "v"))


def bare(value: str) -> str:
    version = value.removeprefix("=").removeprefix("v")
    if not re.fullmatch(r"(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)", version):
        raise ValueError("expected a stable protocol version")
    return version


def read_pin(text: str) -> tuple[dict, str, str, str]:
    data = tomllib.loads(text)
    dependency = data["dependencies"]["sentinelpass-protocol"]
    attributes = [key for key in ("version", "tag") if key in dependency]
    if len(attributes) != 1:
        raise ValueError("expected exactly one protocol version or tag")
    attribute = attributes[0]
    raw = dependency[attribute]
    version = bare(raw)
    for attr, prefix in FORMS:
        if attr == attribute and raw == prefix + version:
            return data, attribute, prefix, version
    raise ValueError("unsupported protocol pin shape")


def main() -> None:
    text = MANIFEST.read_text()
    data, attribute, prefix, installed = read_pin(text)
    if sys.argv[1:] == ["--current"]:
        print(installed)
        return
    if len(sys.argv) != 3:
        sys.exit("usage: bump_protocol_pin.py --current | <current> <latest>")
    current, latest = bare(sys.argv[1]), bare(sys.argv[2])
    if current != installed:
        sys.exit("current protocol version differs from the live manifest")
    if current == latest:
        print(f"pin already at {current}")
        return
    expected = copy.deepcopy(data)
    expected["dependencies"]["sentinelpass-protocol"][attribute] = prefix + latest
    # Ignore commented assignments. A matching dev/target dependency is not the
    # live entry: parsed candidate equality below rejects edits to other sections.
    needle = re.compile(
        r'(?m)^([ \t]*sentinelpass-protocol\s*=\s*\{[^}]*?\b'
        + attribute + r'\s*=\s*")' + re.escape(prefix + current) + r'(")'
    )
    candidates = []
    for match in needle.finditer(text):
        candidate = (text[:match.start()] + match.group(1) + prefix + latest
                     + match.group(2) + text[match.end():])
        if tomllib.loads(candidate) == expected:
            candidates.append(candidate)
    if len(candidates) != 1:
        sys.exit("could not uniquely update the live protocol pin; manifest unchanged")
    MANIFEST.write_text(candidates[0])
    print(f"pin ({attribute} form): {prefix}{current} -> {prefix}{latest}")


if __name__ == "__main__":
    main()
