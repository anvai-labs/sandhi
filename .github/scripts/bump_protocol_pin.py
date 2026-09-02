#!/usr/bin/env python3
"""Rewrite the sentinelpass-protocol version pin in sandhi-store.

Usage: bump_protocol_pin.py <current> <latest>. Both arguments are accepted
with or without a leading "v", and whichever pin shape the manifest carries is
rewritten in place: the crates.io ``version = "X.Y.Z"`` form (develop/main,
post-#188) or the historical git ``tag = "vX.Y.Z"`` form. Fails loudly when
the pin line is not found — a silent no-op here would strand the pin on an old
protocol version while claiming success.
"""
import pathlib
import re
import sys

MANIFEST = pathlib.Path("crates/sandhi-store/Cargo.toml")

# (attribute, prefix) — the replacement keeps the manifest's existing shape.
FORMS = (
    ("version", ""),
    ("tag", "v"),
)


def bare(v: str) -> str:
    return v.removeprefix("v")


def main() -> None:
    current, latest = bare(sys.argv[1]), bare(sys.argv[2])
    text = MANIFEST.read_text()
    for attr, prefix in FORMS:
        # The needle anchors on `sentinelpass-protocol = {` + the attribute so
        # it can never match the commented-out `path =` dev form or the
        # `dep:sentinelpass-protocol` entries in the [features] table.
        needle = re.compile(
            r"(sentinelpass-protocol = \{[^}]*?" + attr + ' = ")'
            + re.escape(prefix + current)
            + r'(")'
        )
        if needle.search(text) is None:
            continue
        # Lambda replacement: a plain string replacement would re-interpret
        # backslashes in the version (re.escape is for PATTERNS, not replacements).
        MANIFEST.write_text(
            needle.sub(lambda m: m.group(1) + prefix + latest + m.group(2), text, count=1)
        )
        print(f"pin ({attr} form): {prefix}{current} -> {prefix}{latest}")
        return
    expected = " or ".join(
        f'sentinelpass-protocol = {{ ... {attr} = "{prefix}{current}" ... }}'
        for attr, prefix in FORMS
    )
    sys.exit(
        f"pin line not found: expected {expected} in "
        "crates/sandhi-store/Cargo.toml — the pin format moved; "
        "update this script"
    )


if __name__ == "__main__":
    main()
