"""Keep current test installers above the pytest tmpdir advisory's patched floor.

This guards test tooling only; it does not claim a packaged-runtime dependency or
replace actual native/async/SDK qualification of the pinned pair.
"""

from pathlib import Path
import re

from packaging.specifiers import SpecifierSet
from packaging.version import Version
import pytest
import yaml

ROOT = Path(__file__).resolve().parents[2]
MANIFEST = "bindings/python/requirements-test.txt"


def check_toolchain(binding, workflow, sdk, evaluator):
    pins = dict(re.findall(r"(?m)^(pytest(?:-asyncio)?)==([0-9.]+)$", binding))
    assert set(pins) == {"pytest", "pytest-asyncio"}, "both async test dependencies must be pinned"
    assert Version(pins["pytest"]) >= Version("9.0.3"), "pytest tmpdir advisory requires >=9.0.3"
    installs = [
        step["run"]
        for step in workflow["jobs"]["release-safeguards"]["steps"]
        if "run" in step and "pip install" in step["run"]
    ]
    assert any(f"-r {MANIFEST}" in command for command in installs)
    assert not any(re.search(r"\bpytest(?:[=<>~!]|\s|$)", command) for command in installs)
    filters = yaml.safe_load(workflow["jobs"]["changes"]["steps"][1]["with"]["filters"])
    assert (
        MANIFEST in filters["sdk_conformance"]
    ), "shared pin changes must trigger SDK qualification"
    assert f"-r ../../{MANIFEST}" in sdk.splitlines(), "SDK and binding tests must share pins"
    assert not re.search(r"(?m)^pytest(?:[=<>~!]|$)", sdk), "no independent SDK pytest resolution"
    lines = [line for line in evaluator.splitlines() if line.startswith("pytest")]
    assert len(lines) == 1
    spec = SpecifierSet(lines[0].removeprefix("pytest"))
    assert not any(version in spec for version in ("8.4.1", "9.0.2"))
    assert pins["pytest"] in spec


def inputs():
    return (
        (ROOT / MANIFEST).read_text(),
        yaml.load((ROOT / ".github/workflows/ci.yml").read_text(), Loader=yaml.BaseLoader),
        (ROOT / "tests/sdk-conformance/requirements.txt").read_text(),
        (ROOT / "templates/python-evaluator/requirements-dev.txt").read_text(),
    )


def test_current_installers_use_patched_shared_test_toolchain():
    check_toolchain(*inputs())


@pytest.mark.parametrize(
    "mutation",
    [
        "old-pytest",
        "missing-plugin",
        "inline-ci-pin",
        "sdk-unpinned",
        "sdk-filter-missing",
        "old-evaluator-floor",
    ],
)
def test_test_toolchain_guard_rejects_vulnerable_or_divergent_installers(mutation):
    binding, workflow, sdk, evaluator = inputs()
    if mutation == "old-pytest":
        binding = re.sub(r"(?m)^pytest==[0-9.]+$", "pytest==9.0.2", binding)
    elif mutation == "missing-plugin":
        binding = re.sub(r"(?m)^pytest-asyncio==[0-9.]+\n?", "", binding)
    elif mutation == "inline-ci-pin":
        for step in workflow["jobs"]["release-safeguards"]["steps"]:
            if "run" in step:
                step["run"] = step["run"].replace(f"-r {MANIFEST}", "pytest==8.4.1")
    elif mutation == "sdk-unpinned":
        sdk = sdk.replace(f"-r ../../{MANIFEST}", "pytest")
    elif mutation == "sdk-filter-missing":
        options = workflow["jobs"]["changes"]["steps"][1]["with"]
        filters = yaml.safe_load(options["filters"])
        filters["sdk_conformance"].remove(MANIFEST)
        options["filters"] = yaml.safe_dump(filters)
    else:
        evaluator = evaluator.replace("pytest>=9.0.3,<10", "pytest>=8,<10")
    with pytest.raises(AssertionError):
        check_toolchain(binding, workflow, sdk, evaluator)
