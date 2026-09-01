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
    needle = f'sentinelpass-protocol = {{ version = "{current}"'
    if needle not in text:
        sys.exit(
            f"pin line not found: expected {needle!r} in "
            "crates/sandhi-store/Cargo.toml — the pin format moved; "
            "update this script"
        )
    manifest.write_text(text.replace(needle, needle.replace(current, latest, 1), 1))
    print(f"pin: {current} -> {latest}")


if __name__ == "__main__":
    main()
