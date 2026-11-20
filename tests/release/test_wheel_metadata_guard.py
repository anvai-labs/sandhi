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
    filename_tag="manylinux2014_x86_64.manylinux_2_17_x86_64",
    internal_tags=None,
):
    wheel = path / f"sandhi_gateway-{version}-cp311-abi3-{filename_tag}.whl"
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
            + "".join(
                f"Tag: {tag}\n"
                for tag in (
                    internal_tags
                    or (
                        (internal_tag,)
                        if internal_tag
                        else tuple(f"cp311-abi3-{tag}" for tag in filename_tag.split("."))
                    )
                )
            ),
        )
    return wheel


def test_accepts_exact_release_identity(tmp_path):
    wheel = make_wheel(tmp_path)
    result = guard.check_wheel(
        wheel, "0.22.0", target="x86_64-unknown-linux-gnu", manylinux="2014"
    )
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
        guard.check_wheel(
        wheel, "0.22.0", target="x86_64-unknown-linux-gnu", manylinux="2014"
    )


def test_accepts_composite_manylinux_filename_aliases(tmp_path):
    wheel = make_wheel(
        tmp_path,
        filename_tag="manylinux2014_x86_64.manylinux_2_17_x86_64",
        internal_tags=(
            "cp311-abi3-manylinux2014_x86_64",
            "cp311-abi3-manylinux_2_17_x86_64",
        ),
    )
    result = guard.check_wheel(
        wheel, "0.22.0", target="x86_64-unknown-linux-gnu", manylinux="2014"
    )
    assert result["version"] == "0.22.0"


def test_rejects_empty_composite_manylinux_filename_aliases(tmp_path):
    wheel = make_wheel(
        tmp_path,
        filename_tag="manylinux2014_x86_64..manylinux_2_17_x86_64",
        internal_tags=(
            "cp311-abi3-manylinux2014_x86_64",
            "cp311-abi3-",
            "cp311-abi3-manylinux_2_17_x86_64",
        ),
    )
    with pytest.raises(ValueError, match="malformed tag"):
        guard.check_wheel(
        wheel, "0.22.0", target="x86_64-unknown-linux-gnu", manylinux="2014"
    )


def test_rejects_duplicate_filename_tags(tmp_path):
    wheel = make_wheel(
        tmp_path,
        filename_tag="manylinux2014_x86_64.manylinux_2_17_x86_64.manylinux2014_x86_64",
        internal_tags=(
            "cp311-abi3-manylinux2014_x86_64",
            "cp311-abi3-manylinux_2_17_x86_64",
        ),
    )
    with pytest.raises(ValueError, match="duplicate tags"):
        guard.check_wheel(
        wheel, "0.22.0", target="x86_64-unknown-linux-gnu", manylinux="2014"
    )


def test_rejects_platform_tags_for_another_release_target(tmp_path):
    wheel = make_wheel(
        tmp_path,
        filename_tag="manylinux2014_x86_64.manylinux_2_17_aarch64",
        internal_tags=(
            "cp311-abi3-manylinux2014_x86_64",
            "cp311-abi3-manylinux_2_17_aarch64",
        ),
    )
    with pytest.raises(ValueError, match="incompatible with release target|exactly match the configured manylinux2014"):
        guard.check_wheel(
        wheel, "0.22.0", target="x86_64-unknown-linux-gnu", manylinux="2014"
    )


def test_rejects_composite_tags_for_a_different_glibc_baseline(tmp_path):
    wheel = make_wheel(
        tmp_path,
        filename_tag="manylinux_2_28_x86_64.manylinux2014_x86_64",
        internal_tags=(
            "cp311-abi3-manylinux_2_28_x86_64",
            "cp311-abi3-manylinux2014_x86_64",
        ),
    )
    with pytest.raises(ValueError, match="exactly match the configured manylinux2014"):
        guard.check_wheel(
            wheel, "0.22.0", target="x86_64-unknown-linux-gnu", manylinux="2014"
        )


def test_rejects_multiple_macos_deployment_tags(tmp_path):
    wheel = make_wheel(
        tmp_path,
        filename_tag="macosx_14_0_arm64.macosx_11_0_arm64",
        internal_tags=(
            "cp311-abi3-macosx_14_0_arm64",
            "cp311-abi3-macosx_11_0_arm64",
        ),
    )
    with pytest.raises(ValueError, match="exactly one deployment tag"):
        guard.check_wheel(wheel, "0.22.0", target="aarch64-apple-darwin")


def test_release_pypi_build_invokes_the_guard():
    release = yaml.safe_load((ROOT / ".github/workflows/release.yml").read_text())
    release_commands = [
        step.get("run", "")
        for step in release["jobs"]["pypi-build"]["steps"]
    ]
    assert 'python3 scripts/check_wheel_metadata.py --wheel-dir dist --version "$RELEASE_VERSION" --target "${{ matrix.target }}" --manylinux "${{ matrix.manylinux }}"' in release_commands
