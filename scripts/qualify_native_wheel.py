#!/usr/bin/env python3
"""Nonpublishing native-wheel qualification; run with an isolated Python (-I)."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import re
import shutil
import subprocess
import sys
import zipfile

TARGETS = {
    "aarch64-apple-darwin": ("darwin", {"arm64", "aarch64"}, r"macosx_\d+_\d+_arm64"),
    "x86_64-pc-windows-msvc": ("win32", {"amd64", "x86_64"}, r"win_amd64"),
}


def check_host(target: str, expected_python: str) -> dict:
    system, machines, _ = TARGETS[target]
    machine = platform.machine().lower()
    if sys.platform != system or machine not in machines:
        raise ValueError(f"wrong runtime host: {sys.platform}/{machine}, expected {target}")
    if (
        expected_python not in {"3.11", "3.12", "3.13"}
        or f"{sys.version_info.major}.{sys.version_info.minor}" != expected_python
    ):
        raise ValueError("qualification requires CPython 3.11, 3.12 or 3.13")
    if sys.maxsize <= 2**32:
        raise ValueError("qualification requires a 64-bit interpreter")
    if platform.python_implementation() != "CPython":
        raise ValueError("qualification requires CPython")
    rust = subprocess.check_output(["rustc", "-vV"], text=True)
    hosts = [line.removeprefix("host: ") for line in rust.splitlines() if line.startswith("host: ")]
    if hosts != [target]:
        raise ValueError(f"Rust compiler host {hosts!r} does not match runtime target {target}")
    return {
        "platform": sys.platform,
        "machine": machine,
        "python": platform.python_version(),
        "target": target,
        "rustc": rust.strip(),
    }


def inspect_wheel(path: Path, target: str) -> dict:
    """Require exact native OS/CPU/abi3 tags, including internal wheel metadata."""
    _, _, platform_pattern = TARGETS[target]
    pattern = rf"sandhi_gateway-[^-]+-cp311-abi3-({platform_pattern})\.whl"
    match = re.fullmatch(pattern, path.name)
    if not match:
        raise ValueError(f"wrong wheel tags for {target}: {path.name}")
    tag = f"cp311-abi3-{match[1]}"
    with zipfile.ZipFile(path) as archive:
        metadata = [n for n in archive.namelist() if n.endswith(".dist-info/WHEEL")]
        if len(metadata) != 1:
            raise ValueError("wheel requires one WHEEL metadata record")
        wheel_metadata = archive.read(metadata[0]).decode()
        tags = [line[5:] for line in wheel_metadata.splitlines() if line.startswith("Tag: ")]
        if tags != [tag] or "Root-Is-Purelib: false" not in wheel_metadata:
            raise ValueError("internal wheel tags/native classification disagree with filename")
        suffix = ".pyd" if target.endswith("msvc") else ".so"
        extensions = [
            n for n in archive.namelist() if n.startswith("sandhi_gateway/") and n.endswith(suffix)
        ]
        if len(extensions) != 1:
            raise ValueError("wheel requires exactly one Sandhi native extension")
        extension = extensions[0]
        return {
            "wheel": path.name,
            "wheel_sha256": hashlib.sha256(path.read_bytes()).hexdigest(),
            "tag": tag,
            "extension": extension,
            "extension_sha256": hashlib.sha256(archive.read(extension)).hexdigest(),
        }


# The child is the new venv's isolated interpreter, never the bootstrap/site Python.
ORIGIN_PROBE = r"""
import hashlib, importlib.machinery, importlib.metadata, json, pathlib, sys, sysconfig
import sandhi_gateway
import sandhi_gateway.sandhi_gateway as native
prefix = pathlib.Path(sys.prefix).resolve()
assert sys.prefix != sys.base_prefix, "wheel test escaped the venv"
assert not __import__('site').ENABLE_USER_SITE, "user site enabled"
paths = [pathlib.Path(sysconfig.get_path(k)).resolve() for k in ('purelib', 'platlib')]
for module in (sandhi_gateway, native):
    origin = pathlib.Path(module.__file__).resolve()
    assert any(origin.is_relative_to(p) for p in paths), f"foreign wheel import: {origin}"
    assert origin.is_relative_to(prefix), f"global wheel import: {origin}"
assert isinstance(native.__loader__, importlib.machinery.ExtensionFileLoader), "native extension not loaded"
actual = hashlib.sha256(pathlib.Path(native.__file__).read_bytes()).hexdigest()
assert actual == sys.argv[1], "installed native extension differs from built wheel"
dist = importlib.metadata.distribution('sandhi-gateway')
assert pathlib.Path(dist.locate_file('')).resolve().is_relative_to(prefix)
print(json.dumps({'python': sys.version, 'prefix': str(prefix), 'module': sandhi_gateway.__file__, 'extension': native.__file__, 'extension_sha256': actual, 'version': dist.version}))
"""


def isolated_python(venv: Path) -> list[str]:
    executable = venv / ("Scripts/python.exe" if sys.platform == "win32" else "bin/python")
    return [str(executable), "-I"]


def test_wheel(
    wheel_dir: Path, venv: Path, target: str, report_path: Path, expected_python: str
) -> None:
    root = Path(__file__).resolve().parents[1]
    host = check_host(target, expected_python)
    wheels = sorted(wheel_dir.resolve().glob("*.whl"))
    if len(wheels) != 1:
        raise ValueError(f"expected exactly one built wheel, found {len(wheels)}")
    wheel = wheels[0]
    info = inspect_wheel(wheel, target)
    venv = venv.resolve()
    if venv.exists():
        raise ValueError("qualification venv must be new, not an existing/global environment")
    requirements = root / "bindings/python/requirements-test.txt"
    tests = root / "bindings/python/tests"
    report = {
        "schema_version": 1,
        "host": host,
        **info,
        "requirements_sha256": hashlib.sha256(requirements.read_bytes()).hexdigest(),
        "result": "started",
    }
    report_path = report_path.resolve()
    report_path.parent.mkdir(parents=True, exist_ok=True)
    report_path.write_text(json.dumps(report, indent=2) + "\n")
    subprocess.run([sys.executable, "-I", "-m", "venv", str(venv)], check=True)
    python = isolated_python(venv)
    env = {k: v for k, v in os.environ.items() if k.upper() not in {"PYTHONPATH", "PYTHONHOME"}}
    # The TLS tests require OpenSSL's -addext option. Use the hosted image's
    # OpenSSL 3, rather than macOS's older system LibreSSL. No trust-store edits.
    candidates = [
        Path("/opt/homebrew/opt/openssl@3/bin/openssl"),
        Path("C:/Program Files/Git/mingw64/bin/openssl.exe"),
    ]
    openssl = next((p for p in candidates if p.is_file()), None)
    if openssl:
        env["PATH"] = str(openssl.parent) + os.pathsep + env.get("PATH", "")
    if not shutil.which("openssl", path=env.get("PATH")):
        raise ValueError("full TLS binding tests require OpenSSL on the hosted runner")
    env["PYTHONNOUSERSITE"] = "1"
    subprocess.run(
        python
        + [
            "-m",
            "pip",
            "--isolated",
            "install",
            "--no-index",
            "--no-deps",
            "--only-binary=:all:",
            str(wheel),
        ],
        cwd=venv,
        env=env,
        check=True,
    )
    subprocess.run(
        python + ["-m", "pip", "--isolated", "install", "-r", str(requirements)],
        cwd=venv,
        env=env,
        check=True,
    )
    origin = subprocess.check_output(
        python + ["-c", ORIGIN_PROBE, info["extension_sha256"]], cwd=venv, env=env, text=True
    )
    report["installed"] = json.loads(origin)
    report["test_command"] = python + ["-m", "pytest", str(tests), "-q"]
    report_path.write_text(json.dumps(report, indent=2) + "\n")
    subprocess.run(report["test_command"], cwd=venv, env=env, check=True)
    # Recheck bytes after the full suite, including its subprocess TLS probes.
    subprocess.check_output(
        python + ["-c", ORIGIN_PROBE, info["extension_sha256"]], cwd=venv, env=env, text=True
    )
    report["result"] = "passed"
    report_path.write_text(json.dumps(report, indent=2) + "\n")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--target", required=True, choices=TARGETS)
    parser.add_argument("--check-host", action="store_true")
    parser.add_argument("--expected-python", required=True, choices=["3.11", "3.12", "3.13"])
    parser.add_argument("--wheel-dir", type=Path, default=Path("dist"))
    parser.add_argument("--venv", type=Path)
    parser.add_argument("--report", type=Path, default=Path("native-wheel-report.json"))
    args = parser.parse_args()
    if args.check_host:
        print(json.dumps(check_host(args.target, args.expected_python), indent=2))
    else:
        if args.venv is None:
            parser.error("--venv must be a new directory outside the checkout")
        root = Path(__file__).resolve().parents[1]
        if args.venv.resolve().is_relative_to(root):
            parser.error("--venv must be outside the source checkout")
        test_wheel(args.wheel_dir, args.venv, args.target, args.report, args.expected_python)


if __name__ == "__main__":
    main()
