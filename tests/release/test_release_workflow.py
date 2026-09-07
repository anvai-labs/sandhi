"""Offline workflow wiring contracts, not evidence of remote publishing authority.

These checks pin the checked-in workflow's trust boundaries. They cannot constrain
historical workflow versions, GitHub environment settings, or registry credentials.
"""

import copy
from pathlib import Path
import re

import pytest
import yaml


ROOT = Path(__file__).resolve().parents[2]
SOURCE = "${{ needs.authorize.outputs.source_sha }}"
CONTROL = "${{ needs.authorize.outputs.control_sha }}"
PROOF = "${{ needs.authorize.outputs.proof_id }}"
PUBLISHERS = {"create-release", "pypi-publish", "crates", "npm-publish"}
UNPRIVILEGED = {"authorize", "binaries", "pypi-build", "npm-build", "npm-package", "crates-check", "verify"}
SOURCE_JOBS = {"binaries", "pypi-build", "npm-build", "npm-package", "crates", "crates-check"}
PIN = re.compile(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_./-]+@[0-9a-f]{40}")


def workflow(name):
    # BaseLoader preserves GitHub's `on` key and booleans as strings; YAML 1.1
    # safe_load otherwise turns `on` into True and can silently weaken tests.
    return yaml.load((ROOT / ".github/workflows" / name).read_text(), Loader=yaml.BaseLoader)


def needs(job):
    value = job.get("needs", [])
    return {value} if isinstance(value, str) else set(value)


def uses(job, action):
    return [step for step in job["steps"] if step.get("uses", "").startswith(action + "@")]


def commands(job):
    return "\n".join(step.get("run", "") for step in job["steps"])


def check_pins_and_checkouts(document):
    for name, job in document["jobs"].items():
        for step in job["steps"]:
            if "uses" in step:
                assert PIN.fullmatch(step["uses"]), f"{name}: action must use a full SHA"
        checkouts = uses(job, "actions/checkout")
        assert checkouts, f"{name}: explicit immutable checkout required"
        for checkout in checkouts:
            options = checkout["with"]
            allowed = {"${{ github.sha }}"} if name == "authorize" else {SOURCE, CONTROL}
            assert options.get("ref") in allowed, f"{name}: mutable checkout ref"
            assert options.get("persist-credentials") == "false", f"{name}: persisted credentials"
            if options.get("path") == ".release-controls":
                assert options["ref"] == CONTROL, f"{name}: control code must use control SHA"
            elif name != "authorize":
                assert name in SOURCE_JOBS and options["ref"] == SOURCE, f"{name}: source/control role mismatch"
        assert all("${{ inputs." not in step.get("run", "") for step in job["steps"]), "input interpolated into shell"


def check_privileges(document):
    defaults = document["permissions"]
    assert defaults == {"contents": "read", "actions": "read"}, "release defaults must be read-only"
    jobs = document["jobs"]
    assert set(jobs) == PUBLISHERS | UNPRIVILEGED, "new release path needs an explicit threat review"
    for name in UNPRIVILEGED:
        job = jobs[name]
        permissions = job.get("permissions", defaults)
        assert all(value in {"read", "none"} for value in permissions.values()), f"{name}: build has publishing privilege"
        assert "environment" not in job, f"{name}: build must not enter credentialed environment"
        assert "secrets." not in str(job), f"{name}: build must not receive stored credentials"
    assert jobs["create-release"]["environment"] == "github-release"
    assert jobs["create-release"]["permissions"] == {"contents": "write", "actions": "read"}
    for name, environment in (("pypi-publish", "pypi"), ("npm-publish", "npm")):
        assert jobs[name]["environment"] == environment
        assert jobs[name]["permissions"] == {"contents": "read", "actions": "read", "id-token": "write"}
    assert jobs["crates"]["environment"] == "crates-io"
    assert "secrets.CRATES_RELEASE_TOKEN" in str(jobs["crates"])
    assert "secrets.CARGO_REGISTRY_TOKEN" not in str(document), "legacy repository token fallback"
    assert "cargo publish -p \"$crate\" --allow-dirty --no-verify" in commands(jobs["crates"]), "privileged Cargo must not compile build scripts"
    assert "crates-check" in needs(jobs["create-release"])
    assert "create-release" in needs(jobs["crates"])
    assert "cargo check --workspace --all-targets" in commands(jobs["crates-check"])
    assert 'cargo set-version "$RELEASE_VERSION"' in commands(jobs["crates-check"])
    # npm and wheel publisher jobs consume prebuilt packages, not package build code.
    for name in ("npm-publish", "pypi-publish"):
        assert not re.search(r"\b(?:npm\s+(?:ci|install)|cargo\s+(?:build|install)|npx)\b", commands(jobs[name]))


def check_authority(document):
    jobs = document["jobs"]
    authorize = jobs["authorize"]
    assert "needs" not in authorize
    assert authorize["outputs"]["proof_id"] == "${{ steps.proof.outputs.artifact-id }}"
    for key in ("source_sha", "control_sha", "version", "tag"):
        assert authorize["outputs"][key] == "${{ steps.guard.outputs." + key + " }}"
    assert "scripts/release_guard.py --output proof.json --github-output" in commands(authorize)
    proof_uploads = [step for step in uses(authorize, "actions/upload-artifact") if step.get("id") == "proof"]
    assert len(proof_uploads) == 1
    assert proof_uploads[0]["with"]["path"] == "proof.json"
    assert proof_uploads[0]["with"]["if-no-files-found"] == "error"
    for name, job in jobs.items():
        if name != "authorize":
            assert "authorize" in needs(job), f"{name}: authorization dependency missing"
    publication = {
        "create-release": "gh release create",
        "pypi-publish": "pypa/gh-action-pypi-publish@",
        "crates": "cargo publish",
        "npm-publish": "scripts/publish-npm-release.py",
    }
    for name in PUBLISHERS:
        job = jobs[name]
        assert job["env"]["EXPECTED_SOURCE_SHA"] == SOURCE
        assert job["env"]["EXPECTED_CONTROL_SHA"] == CONTROL
        assert job["env"]["RELEASE_TAG"] == "${{ needs.authorize.outputs.tag }}"
        assert job["env"]["RELEASE_VERSION"] == "${{ needs.authorize.outputs.version }}"
        assert any(step["with"].get("path") == ".release-controls" for step in uses(job, "actions/checkout"))
        downloads = [step for step in uses(job, "actions/download-artifact")
                     if step.get("with", {}).get("path") == ".release-proof"]
        assert len(downloads) == 1, f"{name}: authorization proof missing"
        options = downloads[0]["with"]
        assert options.get("artifact-ids") == PROOF, f"{name}: proof must use immutable artifact ID"
        assert not {"name", "pattern", "run-id", "repository"} & options.keys()
        ordered = "\n".join(step.get("run", step.get("uses", "")) for step in job["steps"])
        verification = ".release-controls/scripts/release_guard.py --verify-proof .release-proof/proof.json"
        assert verification in ordered, f"{name}: publishing revalidation missing"
        assert ordered.index(verification) < ordered.index(publication[name]), f"{name}: revalidation after publication"
    packages = [step for step in uses(jobs["npm-publish"], "actions/download-artifact")
                if step["with"].get("path") == "dist-npm"]
    assert len(packages) == 1
    assert packages[0]["with"].get("artifact-ids") == "${{ needs.npm-package.outputs.package_id }}"


def check_verification(document):
    npm = document["jobs"]["npm-publish"]
    assert needs(npm) == {"authorize", "npm-package", "create-release"}
    assert " ".join(npm["if"].split()) == (
        "!cancelled() && needs.authorize.result == 'success' && needs.npm-package.result == 'success' && "
        "(github.event_name == 'workflow_dispatch' || needs.create-release.result == 'success')"
    ), "full release requires all builds; repair explicitly tolerates skipped non-npm jobs"
    job = document["jobs"]["verify"]
    assert needs(job) == PUBLISHERS | {"authorize"}
    assert "!cancelled()" in job["if"] and "needs.authorize.result == 'success'" in job["if"]
    run = commands(job)
    assert '"$RELEASE_EVENT" = workflow_dispatch' in run
    assert '"$RELEASE_TAG" --targets npm\n' in run
    assert '"$RELEASE_TAG" --targets pypi,crates,npm,github\n' in run
    assert "EXPECT_CRATES" not in str(job) and "EXPECT_NPM" not in str(job)
    assert "inputs.npm_repair_tag" not in str(job), "verifier must consume authorized tag"


def check_ci(document):
    job = document["jobs"]["release-safeguards"]
    assert job["name"] == "Release safeguards"
    assert job["runs-on"] == "ubuntu-latest"
    assert needs(job) == {"changes"}
    assert job["if"] == "${{ always() && needs.changes.result == 'success' }}", "safeguards must not be path-filtered"
    assert "python -m pytest tests/release -q" in commands(job)
    assert "PyYAML==" in commands(job)
    assert "release-safeguards" in needs(document["jobs"]["ci-success"]), "aggregate omits release safeguards"
    for step in job["steps"]:
        if "uses" in step:
            assert PIN.fullmatch(step["uses"])
    assert uses(job, "actions/checkout")[0]["with"]["persist-credentials"] == "false"


def test_release_wiring_contracts():
    document = workflow("release.yml")
    assert set(document["on"]) == {"push", "workflow_dispatch"}
    assert document["concurrency"]["cancel-in-progress"] == "false"
    check_pins_and_checkouts(document)
    check_privileges(document)
    check_authority(document)
    check_verification(document)


def test_non_npm_publish_paths_are_skipped_for_repair():
    jobs = workflow("release.yml")["jobs"]
    for name in ("binaries", "pypi-build", "create-release", "crates", "crates-check"):
        assert jobs[name]["if"] == "github.event_name == 'push'"
    assert "create-release" in needs(jobs["pypi-publish"])
    for name in ("npm-build", "npm-package"):
        assert "if" not in jobs[name]
    check_verification(workflow("release.yml"))


def test_no_nonexistent_proxy_help_smoke():
    # The proxy is env-configured and does not parse --help: that starts a server.
    assert not re.search(r"sandhi-proxy[\"']?\s+--help", commands(workflow("release.yml")["jobs"]["binaries"]))
    assert 'python3 .release-controls/scripts/smoke-release-binaries.py --binary-dir "target/$TARGET/release"' in commands(workflow("release.yml")["jobs"]["binaries"])


def test_ci_always_checks_release_safeguards():
    check_ci(workflow("ci.yml"))


def test_legacy_crates_dispatch_is_read_only_failure_only():
    document = workflow("publish-crates.yml")
    assert document["permissions"] == {"contents": "read"}
    assert set(document["on"]) == {"workflow_dispatch"}
    assert set(document["jobs"]) == {"retired"}
    job = document["jobs"]["retired"]
    assert "environment" not in job and "permissions" not in job
    assert len(job["steps"]) == 1 and set(job["steps"][0]) == {"run"}
    run = commands(job)
    assert run.rstrip().endswith("exit 1")
    assert all(line.strip().startswith(("echo ", "exit 1")) for line in run.splitlines() if line.strip())
    assert "${{" not in run and "secrets." not in str(document)


@pytest.mark.parametrize("fault", ["floating_action", "mutable_source", "persisted_credentials", "wrong_source_role", "write_default",
                                  "build_oidc", "missing_authorization", "proof_by_name", "unchecked_publish",
                                  "wrong_source_binding", "legacy_token", "credentialed_cargo_build", "implicit_targets",
                                  "repair_build_bypass", "repair_default_skip"])
def test_release_contracts_reject_regressions(fault):
    document = copy.deepcopy(workflow("release.yml"))
    jobs = document["jobs"]
    checker = check_authority
    if fault == "floating_action":
        jobs["authorize"]["steps"][0]["uses"] = "actions/checkout@v7"
        checker = check_pins_and_checkouts
    elif fault in {"mutable_source", "persisted_credentials"}:
        options = uses(jobs["binaries"], "actions/checkout")[0]["with"]
        options["ref" if fault == "mutable_source" else "persist-credentials"] = "main" if fault == "mutable_source" else "true"
        checker = check_pins_and_checkouts
    elif fault == "wrong_source_role":
        uses(jobs["npm-build"], "actions/checkout")[0]["with"]["ref"] = CONTROL
        checker = check_pins_and_checkouts
    elif fault == "write_default":
        document["permissions"]["contents"] = "write"
        checker = check_privileges
    elif fault == "build_oidc":
        jobs["npm-build"]["permissions"] = {"id-token": "write"}
        checker = check_privileges
    elif fault == "missing_authorization":
        jobs["npm-publish"]["needs"] = ["npm-package"]
    elif fault == "proof_by_name":
        options = next(step["with"] for step in uses(jobs["npm-publish"], "actions/download-artifact")
                       if step["with"].get("path") == ".release-proof")
        options.pop("artifact-ids")
        options["name"] = "release-authorization"
    elif fault == "unchecked_publish":
        jobs["pypi-publish"]["steps"] = [step for step in jobs["pypi-publish"]["steps"] if "--verify-proof" not in step.get("run", "")]
    elif fault == "wrong_source_binding":
        jobs["npm-publish"]["env"]["EXPECTED_SOURCE_SHA"] = "${{ github.sha }}"
    elif fault == "legacy_token":
        next(step for step in jobs["crates"]["steps"] if "CARGO_REGISTRY_TOKEN" in step.get("env", {}))["env"]["CARGO_REGISTRY_TOKEN"] = "${{ secrets.CARGO_REGISTRY_TOKEN }}"
        checker = check_privileges
    elif fault == "credentialed_cargo_build":
        for step in jobs["crates"]["steps"]:
            if "run" in step:
                step["run"] = step["run"].replace(" --no-verify", "")
        checker = check_privileges
    elif fault in {"repair_build_bypass", "repair_default_skip"}:
        expression = jobs["npm-publish"]["if"]
        jobs["npm-publish"]["if"] = expression + " || true" if fault == "repair_build_bypass" else expression.replace("!cancelled() && ", "")
        checker = check_verification
    else:
        for step in jobs["verify"]["steps"]:
            if "run" in step:
                step["run"] = step["run"].replace(" --targets pypi,crates,npm,github", "")
        checker = check_verification
    with pytest.raises(AssertionError):
        checker(document)


@pytest.mark.parametrize("fault", ["path_filter", "missing_aggregate", "no_tests"])
def test_ci_contract_rejects_silent_safeguard_skips(fault):
    document = copy.deepcopy(workflow("ci.yml"))
    if fault == "path_filter":
        document["jobs"]["release-safeguards"]["if"] = "${{ needs.changes.outputs.rust == 'true' }}"
    elif fault == "missing_aggregate":
        document["jobs"]["ci-success"]["needs"].remove("release-safeguards")
    else:
        document["jobs"]["release-safeguards"]["steps"] = [
            step for step in document["jobs"]["release-safeguards"]["steps"]
            if "python -m pytest tests/release -q" not in step.get("run", "")
        ]
    with pytest.raises(AssertionError):
        check_ci(document)
