"""Offline guards for release-shaped native qualification, not hosted execution."""

from __future__ import annotations

import copy
import importlib.util
import json
from pathlib import Path
import re
from types import SimpleNamespace
import zipfile

import pytest
import yaml

ROOT = Path(__file__).resolve().parents[2]
SPEC = importlib.util.spec_from_file_location(
    "native_wheel_qualification", ROOT / "scripts/qualify_native_wheel.py"
)
qualification = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(qualification)


def workflow():
    return yaml.load((ROOT / ".github/workflows/ci.yml").read_text(), Loader=yaml.BaseLoader)


def check_native_jobs(document):
    jobs = document["jobs"]
    assert {"python-wheel-native", "node-binding-macos"} <= set(jobs["ci-success"]["needs"])
    assert document["permissions"] == {"contents": "read", "pull-requests": "read"}
    expected = {
        (os, target, version)
        for os, target in [
            ("macos-latest", "aarch64-apple-darwin"),
            ("windows-latest", "x86_64-pc-windows-msvc"),
        ]
        for version in ["3.11", "3.12", "3.13"]
    }
    python = jobs["python-wheel-native"]
    assert python["runs-on"] == "${{ matrix.os }}"
    assert python["strategy"]["fail-fast"] == "false"
    cells = python["strategy"]["matrix"]["include"]
    assert len(cells) == 6
    assert {(c["os"], c["target"], c["python"]) for c in cells} == expected
    assert python["needs"] == "changes"
    assert "needs.changes.outputs.python == 'true'" in python["if"]
    steps = python["steps"]
    setup = [s for s in steps if s.get("uses", "").startswith("actions/setup-python@")]
    assert {(s["if"], s["with"]["architecture"]) for s in setup} == {
        ("runner.os == 'Windows'", "x64"),
        ("runner.os == 'macOS'", "arm64"),
    }
    assert all(s["with"]["python-version"] == "${{ matrix.python }}" for s in setup)
    host = [s for s in steps if "--check-host" in s.get("run", "")]
    assert len(host) == 1
    assert (
        host[0]["run"]
        == "python -I scripts/qualify_native_wheel.py --check-host --target ${{ matrix.target }} --expected-python ${{ matrix.python }}"
    )
    builds = [s for s in steps if s.get("uses", "").startswith("PyO3/maturin-action@")]
    assert len(builds) == 1
    assert builds[0]["with"] == {
        "command": "build",
        "args": "--release --locked -m bindings/python/Cargo.toml --out dist",
        "target": "${{ matrix.target }}",
        "manylinux": "auto",
    }
    assert steps.index(host[0]) < steps.index(builds[0])
    tests = [s["run"] for s in steps if "--wheel-dir" in s.get("run", "")]
    assert tests == [
        'python -I scripts/qualify_native_wheel.py --target ${{ matrix.target }} --expected-python ${{ matrix.python }} --wheel-dir dist --venv "${{ runner.temp }}/native-wheel-venv" --report native-wheel-report.json'
    ]
    node = jobs["node-binding-macos"]
    assert node["runs-on"] == "macos-latest"
    assert node["needs"] == "changes"
    assert "needs.changes.outputs.node == 'true'" in node["if"]
    assert node["env"] == {"NODE_PATH": "", "NODE_OPTIONS": ""}
    node_setup = [s for s in node["steps"] if s.get("uses", "").startswith("actions/setup-node@")]
    assert len(node_setup) == 1 and node_setup[0]["with"] == {
        "node-version": "24",
        "architecture": "arm64",
    }
    runs = [s["run"] for s in node["steps"] if "run" in s]
    assert runs[0] == "node scripts/qualify_native_node.cjs --check-host"
    assert runs[1].splitlines() == [
        "npm ci --ignore-scripts --no-audit --no-fund",
        'npx --no-install napi build --platform --release --target aarch64-apple-darwin --cargo-flags="--locked"',
    ]
    assert runs[2] == "node scripts/qualify_native_node.cjs"
    for job in (python, node):
        assert "permissions" not in job and "environment" not in job
        assert "secrets." not in str(job) and "publish" not in str(job).lower()
        for step in job["steps"]:
            if "uses" in step:
                assert re.fullmatch(r"[^@]+@[a-f0-9]{40}", step["uses"])
            if step.get("uses", "").startswith("actions/checkout@"):
                assert step["with"] == {"persist-credentials": "false"}
    filters = yaml.safe_load(jobs["changes"]["steps"][1]["with"]["filters"])
    assert "scripts/qualify_native_wheel.py" in filters["python"]
    assert "scripts/qualify_native_node.cjs" in filters["node"]
    assert ".github/workflows/release.yml" in filters["node"]


def test_committed_native_qualification_jobs():
    check_native_jobs(workflow())


@pytest.mark.parametrize(
    "mutation",
    [
        "missing-gate",
        "private-runner",
        "wrong-target",
        "unlocked",
        "debug-build",
        "nonisolated",
        "venv-in-checkout",
        "wrong-interpreter",
        "node-x64",
        "node-unlocked",
    ],
)
def test_workflow_mutations_fail_closed(mutation):
    doc = copy.deepcopy(workflow())
    py = doc["jobs"]["python-wheel-native"]
    if mutation == "missing-gate":
        doc["jobs"]["ci-success"]["needs"].remove("python-wheel-native")
    elif mutation == "private-runner":
        py["strategy"]["matrix"]["include"][0]["os"] = "public-overflow"
    elif mutation == "wrong-target":
        py["strategy"]["matrix"]["include"][0]["target"] = "x86_64-apple-darwin"
    elif mutation in {"unlocked", "debug-build"}:
        step = next(s for s in py["steps"] if s.get("uses", "").startswith("PyO3/maturin-action@"))
        step["with"]["args"] = step["with"]["args"].replace(
            "--locked " if mutation == "unlocked" else "--release ", ""
        )
    elif mutation in {"nonisolated", "venv-in-checkout"}:
        step = next(s for s in py["steps"] if "--wheel-dir" in s.get("run", ""))
        step["run"] = (
            step["run"].replace("python -I", "python")
            if mutation == "nonisolated"
            else step["run"].replace("${{ runner.temp }}", ".")
        )
    elif mutation == "wrong-interpreter":
        next(s for s in py["steps"] if s.get("uses", "").startswith("actions/setup-python@"))[
            "with"
        ]["python-version"] = "3.12"
    elif mutation == "node-x64":
        next(
            s
            for s in doc["jobs"]["node-binding-macos"]["steps"]
            if s.get("uses", "").startswith("actions/setup-node@")
        )["with"]["architecture"] = "x64"
    else:
        step = next(
            s
            for s in doc["jobs"]["node-binding-macos"]["steps"]
            if "--cargo-flags" in s.get("run", "")
        )
        step["run"] = step["run"].replace('--cargo-flags="--locked"', "")
    with pytest.raises(AssertionError):
        check_native_jobs(doc)


def make_wheel(tmp_path, tag="macosx_11_0_arm64", internal=None, extension=True):
    wheel = tmp_path / f"sandhi_gateway-0.17.0-cp311-abi3-{tag}.whl"
    with zipfile.ZipFile(wheel, "w") as archive:
        archive.writestr(
            "sandhi_gateway-0.17.0.dist-info/WHEEL",
            f"Wheel-Version: 1.0\nRoot-Is-Purelib: false\nTag: cp311-abi3-{internal or tag}\n",
        )
        if extension:
            archive.writestr(
                "sandhi_gateway/sandhi_gateway.abi3." + ("pyd" if tag == "win_amd64" else "so"),
                b"synthetic-unit-test-bytes",
            )
    return wheel


@pytest.mark.parametrize(
    "target,tag",
    [("aarch64-apple-darwin", "macosx_11_0_arm64"), ("x86_64-pc-windows-msvc", "win_amd64")],
)
def test_native_wheel_tag_and_byte_evidence(tmp_path, target, tag):
    info = qualification.inspect_wheel(make_wheel(tmp_path, tag), target)
    assert info["tag"] == f"cp311-abi3-{tag}"
    assert len(info["wheel_sha256"]) == 64 and len(info["extension_sha256"]) == 64


@pytest.mark.parametrize(
    "tag",
    [
        "linux_x86_64",
        "macosx_11_0_x86_64",
        "macosx_11_0_universal2",
        "win_amd64",
        "macosx_11_0_arm64.macosx_10_9_x86_64",
    ],
)
def test_wheel_rejects_host_only_foreign_and_ambiguous_tags(tmp_path, tag):
    with pytest.raises(ValueError, match="wrong wheel tags"):
        qualification.inspect_wheel(make_wheel(tmp_path, tag), "aarch64-apple-darwin")


def test_wheel_rejects_internal_tag_mismatch_and_missing_native_module(tmp_path):
    for kwargs in [{"internal": "macosx_11_0_x86_64"}, {"extension": False}]:
        with pytest.raises(ValueError):
            qualification.inspect_wheel(make_wheel(tmp_path, **kwargs), "aarch64-apple-darwin")


def mock_host(monkeypatch, host="aarch64-apple-darwin", machine="arm64", version="3.12"):
    monkeypatch.setattr(qualification.sys, "platform", "darwin")
    monkeypatch.setattr(
        qualification.sys,
        "version_info",
        SimpleNamespace(major=3, minor=int(version.split(".")[1])),
    )
    monkeypatch.setattr(qualification.platform, "machine", lambda: machine)
    monkeypatch.setattr(qualification.platform, "python_implementation", lambda: "CPython")
    monkeypatch.setattr(qualification.platform, "python_version", lambda: version)
    monkeypatch.setattr(
        qualification.subprocess, "check_output", lambda *a, **k: f"rustc fixture\nhost: {host}\n"
    )


def test_host_requires_matching_cpu_rust_and_matrix_interpreter(monkeypatch):
    mock_host(monkeypatch)
    assert qualification.check_host("aarch64-apple-darwin", "3.12")["machine"] == "arm64"
    for kwargs in [{"machine": "x86_64"}, {"host": "x86_64-apple-darwin"}, {"version": "3.11"}]:
        mock_host(monkeypatch, **kwargs)
        with pytest.raises(ValueError):
            qualification.check_host("aarch64-apple-darwin", "3.12")


def test_qualification_uses_fresh_isolated_install_full_suite_and_two_byte_probes(
    tmp_path, monkeypatch
):
    wheel_dir = tmp_path / "dist"
    wheel_dir.mkdir()
    make_wheel(wheel_dir)
    venv = tmp_path / "fresh-venv"
    calls = []
    mock_host(monkeypatch)
    monkeypatch.setenv("PYTHONPATH", "/synthetic/shadow")
    monkeypatch.setenv("PYTHONHOME", "/synthetic/wrong-python")
    monkeypatch.setattr(qualification.shutil, "which", lambda *a, **kw: "/synthetic/openssl")

    def run(command, **kwargs):
        calls.append((command, kwargs))
        if command[2:4] == ["-m", "venv"]:
            venv.mkdir()

    probes = []

    def output(command, **kwargs):
        if command == ["rustc", "-vV"]:
            return "host: aarch64-apple-darwin\n"
        probes.append((command, kwargs))
        return json.dumps({"prefix": str(venv)})

    monkeypatch.setattr(qualification.subprocess, "run", run)
    monkeypatch.setattr(qualification.subprocess, "check_output", output)
    report = tmp_path / "report.json"
    qualification.test_wheel(wheel_dir, venv, "aarch64-apple-darwin", report, "3.12")
    assert len(calls) == 4
    assert calls[0][0][1:4] == ["-I", "-m", "venv"]
    for command, opts in calls[1:] + probes:
        assert command[:2] == [str(venv / "bin/python"), "-I"]
        assert opts["cwd"] == venv
        assert not {"PYTHONPATH", "PYTHONHOME"} & set(opts["env"])
    assert calls[1][0][2:10] == [
        "-m",
        "pip",
        "--isolated",
        "install",
        "--no-index",
        "--no-deps",
        "--only-binary=:all:",
        str(next(wheel_dir.glob("*.whl"))),
    ]
    assert calls[2][0][2:] == [
        "-m",
        "pip",
        "--isolated",
        "install",
        "-r",
        str(ROOT / "bindings/python/requirements-test.txt"),
    ]
    assert calls[3][0][2:] == ["-m", "pytest", str(ROOT / "bindings/python/tests"), "-q"]
    assert len(probes) == 2 and all(c[3] == qualification.ORIGIN_PROBE for c, _ in probes)
    assert json.loads(report.read_text())["result"] == "passed"
    with pytest.raises(ValueError, match="must be new"):
        qualification.test_wheel(wheel_dir, venv, "aarch64-apple-darwin", report, "3.12")


NODE_PROBE = r"""
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');
const filename = process.argv[1], mode = process.argv[2];
const root = path.resolve(path.dirname(filename), '..');
const expected = path.join(root, 'bindings/node/sandhi.darwin-arm64.node');
const writes = [], spawned = [], loads = [];
let reads = 0;
const childProcess = {
  execFileSync: () => 'host: ' + (mode === 'wrong-compiler' ? 'x86_64-apple-darwin' : 'aarch64-apple-darwin'),
  spawnSync: (exe, args, options) => { spawned.push({exe, args, cwd: options.cwd, env: options.env}); return {status: mode === 'failed-tests' ? 1 : 0, signal: null}; },
};
const runtime = {
  platform: 'darwin', arch: mode === 'wrong-runtime' ? 'x64' : 'arm64',
  argv: ['node', filename], execPath: '/synthetic/node', version: 'v24.fixture',
  env: {NODE_PATH: '/synthetic/shadow', NODE_OPTIONS: '--synthetic-option'},
  dlopen: (module, file) => { loads.push(file); },
};
const fakeFs = {
  realpathSync: value => value,
  readFileSync: () => Buffer.from(mode === 'changed-bytes' && reads++ > 0 ? 'different' : 'synthetic-native-bytes'),
  writeFileSync: (file, contents) => writes.push(JSON.parse(contents)),
};
function sandboxRequire(id) {
  if (id === 'node:fs') return fakeFs;
  if (id === 'node:child_process') return childProcess;
  if (id === path.join(root, 'bindings/node/sandhi.js')) {
    runtime.dlopen({}, mode === 'shadow-addon' ? '/foreign/addon.node' : expected);
    return {};
  }
  return require(id);
}
let error;
try { vm.runInNewContext(fs.readFileSync(filename, 'utf8'), {require: sandboxRequire, process: runtime, __dirname: path.dirname(filename), console}); }
catch (e) { error = e; }
if (mode === 'pass') {
  assert.equal(error, undefined);
  assert.deepEqual(loads, [expected]);
  assert.equal(spawned.length, 1);
  assert.deepEqual(Array.from(spawned[0].args), ['--test', '__test__/gateway.test.mjs']);
  assert.equal(spawned[0].cwd, path.join(root, 'bindings/node'));
  assert.equal(spawned[0].env.NODE_PATH, '');
  assert.equal(spawned[0].env.NODE_OPTIONS, '');
  assert.equal(writes.at(-1).result, 'passed');
} else {
  assert.ok(error, 'qualification incorrectly accepted ' + mode);
  assert.ok(!writes.some(r => r.result === 'passed'));
  if (['wrong-runtime', 'wrong-compiler', 'shadow-addon'].includes(mode)) assert.equal(spawned.length, 0);
}
"""


@pytest.mark.parametrize(
    "mode",
    ["pass", "wrong-runtime", "wrong-compiler", "shadow-addon", "failed-tests", "changed-bytes"],
)
def test_node_qualifier_checks_actual_load_host_bytes_and_full_suite(mode):
    import subprocess

    subprocess.run(
        ["node", "-e", NODE_PROBE, str(ROOT / "scripts/qualify_native_node.cjs"), mode],
        check=True,
        capture_output=True,
        text=True,
    )


@pytest.mark.parametrize("count", [0, 2])
def test_qualification_refuses_missing_or_ambiguous_built_artifacts(tmp_path, monkeypatch, count):
    mock_host(monkeypatch)
    wheel_dir = tmp_path / "dist"
    wheel_dir.mkdir()
    if count:
        make_wheel(wheel_dir)
        make_wheel(wheel_dir, tag="macosx_12_0_arm64")
    with pytest.raises(ValueError, match="exactly one built wheel"):
        qualification.test_wheel(
            wheel_dir, tmp_path / "venv", "aarch64-apple-darwin", tmp_path / "report.json", "3.12"
        )


def test_requirements_are_shared_exact_test_pins():
    lines = (ROOT / "bindings/python/requirements-test.txt").read_text().splitlines()
    requirements = [line.strip() for line in lines if line.strip() and not line.startswith("#")]
    assert requirements and all(
        re.fullmatch(r"[a-zA-Z0-9_-]+==[0-9.]+", line) for line in requirements
    )
