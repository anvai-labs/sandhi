#!/usr/bin/env python3
"""Verify a built Python wheel carries Sandhi's committed release identity."""

from __future__ import annotations

import argparse
from email.parser import Parser
import hashlib
from pathlib import Path
import re
import sys
import tomllib
import zipfile


TARGET_PLATFORM_PATTERNS = {
    "x86_64-unknown-linux-gnu": r"(?:manylinux[0-9_]+|linux)_x86_64",
    "aarch64-unknown-linux-gnu": r"manylinux[0-9_]+_aarch64",
    "aarch64-apple-darwin": r"macosx_[0-9]+_[0-9]+_arm64",
    "x86_64-pc-windows-msvc": r"win_amd64",
}


def workspace_version(root: Path) -> str:
    document = tomllib.loads((root / "Cargo.toml").read_text(encoding="utf-8"))
    version = document["workspace"]["package"]["version"]
    if not isinstance(version, str) or not re.fullmatch(
        r"(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)", version
    ):
        raise ValueError("workspace version must be stable X.Y.Z")
    return version


def check_wheel(
    wheel: Path, expected_version: str, target: str, manylinux: str = "auto"
) -> dict[str, str]:
    identity = re.fullmatch(
        rf"sandhi_gateway-{re.escape(expected_version)}-(cp311-abi3-.+)\.whl",
        wheel.name,
    )
    if identity is None:
        raise ValueError(f"wheel filename identity/tag mismatch: {wheel.name}")
    filename_tag = identity.group(1)
    if not re.fullmatch(r"cp311-abi3-[A-Za-z0-9_]+(?:\.[A-Za-z0-9_]+)*", filename_tag):
        raise ValueError(f"wheel filename contains a malformed tag: {wheel.name}")
    primary_tag, *alias_platform_tags = filename_tag.split(".")
    interpreter_abi = "-".join(primary_tag.split("-", 2)[:2])
    expected_tags = [
        primary_tag,
        *(f"{interpreter_abi}-{platform_tag}" for platform_tag in alias_platform_tags),
    ]
    if len(expected_tags) != len(set(expected_tags)):
        raise ValueError("wheel filename contains duplicate tags")
    target_pattern = TARGET_PLATFORM_PATTERNS.get(target)
    if target_pattern is None:
        raise ValueError(f"unsupported release wheel target: {target}")
    advertised_platforms = {tag.split("-", 2)[-1] for tag in expected_tags}
    if target in {"x86_64-unknown-linux-gnu", "aarch64-unknown-linux-gnu"}:
        architecture = target.split("-", 1)[0]
        if manylinux != "2014":
            raise ValueError(f"unsupported Linux wheel policy: manylinux={manylinux!r}")
        required_platforms = {
            f"manylinux2014_{architecture}",
            f"manylinux_2_17_{architecture}",
        }
        if advertised_platforms != required_platforms:
            raise ValueError(
                f"wheel platforms {sorted(advertised_platforms)!r} do not exactly match "
                f"the configured manylinux2014 policy for {target}"
            )
    else:
        if manylinux != "auto":
            raise ValueError(f"manylinux policy is not valid for {target}")
        if len(expected_tags) != 1:
            raise ValueError(f"{target} wheel must advertise exactly one deployment tag")
        platform_tag = next(iter(advertised_platforms))
        if re.fullmatch(target_pattern, platform_tag) is None:
            raise ValueError(
                f"wheel platform {platform_tag!r} is incompatible with release target {target}"
            )

    with zipfile.ZipFile(wheel) as archive:
        metadata_names = [
            name
            for name in archive.namelist()
            if name == f"sandhi_gateway-{expected_version}.dist-info/METADATA"
        ]
        wheel_names = [
            name
            for name in archive.namelist()
            if name == f"sandhi_gateway-{expected_version}.dist-info/WHEEL"
        ]
        if len(metadata_names) != 1 or len(wheel_names) != 1:
            raise ValueError("wheel dist-info identity is missing or ambiguous")
        metadata = Parser().parsestr(archive.read(metadata_names[0]).decode("utf-8"))
        wheel_metadata = Parser().parsestr(archive.read(wheel_names[0]).decode("utf-8"))

    if metadata.get("Name", "") != "sandhi-gateway":
        raise ValueError(f"wheel metadata name mismatch: {metadata.get('Name')!r}")
    if metadata.get("Version", "") != expected_version:
        raise ValueError(f"wheel metadata version mismatch: {metadata.get('Version')!r}")
    if wheel_metadata.get("Root-Is-Purelib", "") != "false":
        raise ValueError("Sandhi wheel must be platform-native, not pure Python")
    if wheel_metadata.get("Generator", "") != "maturin (1.16.0)":
        raise ValueError(
            f"wheel generator mismatch: {wheel_metadata.get('Generator')!r}; expected Maturin 1.16.0"
        )
    tags = wheel_metadata.get_all("Tag", [])
    if len(tags) != len(set(tags)) or set(tags) != set(expected_tags):
        raise ValueError("internal wheel tags disagree with the wheel filename")

    return {
        "wheel": wheel.name,
        "name": metadata["Name"],
        "version": metadata["Version"],
        "generator": wheel_metadata["Generator"],
        "sha256": hashlib.sha256(wheel.read_bytes()).hexdigest(),
    }


def main(argv: list[str] | None = None) -> int:
    root = Path(__file__).resolve().parents[1]
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--wheel-dir", type=Path, required=True)
    parser.add_argument("--version")
    parser.add_argument("--target", required=True)
    parser.add_argument("--manylinux", default="auto")
    args = parser.parse_args(argv)
    try:
        expected = args.version or workspace_version(root)
        if args.version and not re.fullmatch(
            r"(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)", expected
        ):
            raise ValueError("expected wheel version must be stable X.Y.Z")
        wheels = sorted(args.wheel_dir.resolve().glob("*.whl"))
        if len(wheels) != 1:
            raise ValueError(f"expected exactly one built wheel, found {len(wheels)}")
        result = check_wheel(wheels[0], expected, args.target, args.manylinux)
    except (OSError, KeyError, TypeError, ValueError, zipfile.BadZipFile) as error:
        print(f"FAIL: {error}", file=sys.stderr)
        return 1
    print(
        "wheel identity verified: {name} {version} ({generator}) {sha256}".format(**result)
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
