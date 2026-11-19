from __future__ import annotations

import importlib.util
from pathlib import Path
import zipfile

import pytest
import yaml


ROOT = Path(__file__).resolve().parents[2]
SPEC = importlib.util.spec_from_file_location(
    "wheel_metadata_guard", ROOT / "scripts" / "check_wheel_metadata.py"
)
guard = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(guard)


def make_wheel(
    path: Path,
    *,
    version="0.22.0",
    name="sandhi-gateway",
    generator="maturin (1.16.0)",
    purelib="false",
    internal_tag=None,
):
    platform_tag = "manylinux_2_34_x86_64"
    wheel = path / f"sandhi_gateway-{version}-cp311-abi3-manylinux_2_34_x86_64.whl"
    with zipfile.ZipFile(wheel, "w") as archive:
        archive.writestr(
            f"sandhi_gateway-{version}.dist-info/METADATA",
            f"Metadata-Version: 2.4\nName: {name}\nVersion: {version}\n",
        )
        archive.writestr(
            f"sandhi_gateway-{version}.dist-info/WHEEL",
            "Wheel-Version: 1.0\n"
            f"Generator: {generator}\n"
            f"Root-Is-Purelib: {purelib}\n"
            f"Tag: {internal_tag or 'cp311-abi3-' + platform_tag}\n",
        )
    return wheel


def test_accepts_exact_release_identity(tmp_path):
    wheel = make_wheel(tmp_path)
    result = guard.check_wheel(wheel, "0.22.0")
    assert result["name"] == "sandhi-gateway"
    assert result["version"] == "0.22.0"
    assert result["generator"] == "maturin (1.16.0)"


@pytest.mark.parametrize(
    "kwargs",
    [
        {"version": "0.0.0"},
        {"name": "sandhi"},
        {"generator": "maturin (1.12.4)"},
        {"purelib": "true"},
        {"internal_tag": "manylinux_2_34_x86_64"},
    ],
)
def test_rejects_stale_or_wrong_wheel_identity(tmp_path, kwargs):
    wheel = make_wheel(tmp_path, **kwargs)
    with pytest.raises(ValueError, match="mismatch|native|tags"):
        guard.check_wheel(wheel, "0.22.0")


def test_release_pypi_build_invokes_the_guard():
    release = yaml.safe_load((ROOT / ".github/workflows/release.yml").read_text())
    release_commands = [
        step.get("run", "")
        for step in release["jobs"]["pypi-build"]["steps"]
    ]
    assert 'python3 scripts/check_wheel_metadata.py --wheel-dir dist --version "$RELEASE_VERSION"' in release_commands
