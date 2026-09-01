#!/usr/bin/env python3
"""Rewrite the sentinelpass-protocol version pin in sandhi-store.

Usage: bump_protocol_pin.py <current> <latest>. Fails loudly when the pin
line is not found — a silent no-op here would strand the pin on an old
protocol version while claiming success.
"""
import pathlib
import re
import sys


def main() -> None:
    current, latest = sys.argv[1], sys.argv[2]
    manifest = pathlib.Path("crates/sandhi-store/Cargo.toml")
    text = manifest.read_text()
    # The pin is a git dependency: `tag = "vX.Y.Z"` inside the sentinelpass-protocol
    # dependency line (there is also a commented-out `path =` dev form at line 27 —
    # the regex anchors on the tag attribute so it can never match that comment).
    needle_re = re.compile(
        r'(sentinelpass-protocol = \{[^}]*?tag = ")'
        + re.escape(current)
        + r'(")'
    )
    match = needle_re.search(text)
    if match is None:
        expected = f'sentinelpass-protocol = {{ ... tag = "{current}" ... }}'
        sys.exit(
            f"pin line not found: expected {expected!r} in "
            "crates/sandhi-store/Cargo.toml — the pin format moved; "
            "update this script"
        )
    # Lambda replacement: a plain string replacement would re-interpret
    # backslashes in the version (re.escape is for PATTERNS, not replacements).
    manifest.write_text(
        needle_re.sub(lambda m: m.group(1) + latest + m.group(2), text, count=1)
    )
    print(f"pin: {current} -> {latest}")


if __name__ == "__main__":
    main()
