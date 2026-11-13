"""Public-repository runner policy across CI, release and maintenance workflows."""

import copy
from pathlib import Path

import pytest
import yaml


ROOT = Path(__file__).resolve().parents[2]
HOSTED_IMAGES = {
    "ubuntu-24.04", "ubuntu-24.04-arm", "macos-latest", "windows-latest"
}


def workflow(path):
    return yaml.load(path.read_text(), Loader=yaml.BaseLoader)


def check_hosted_runners(document):
    for name, job in document["jobs"].items():
        selection = job["runs-on"]
        if selection == "${{ matrix.os }}":
            matrix = job["strategy"]["matrix"]
            images = [entry["os"] for entry in matrix["include"]]
            assert images and set(images) <= HOSTED_IMAGES, name
            assert set(matrix) == {"include"}, "unreviewed runner matrix expansion"
        else:
            assert isinstance(selection, str) and selection in HOSTED_IMAGES, name


def test_every_workflow_uses_only_declared_github_hosted_images():
    for path in sorted((ROOT / ".github/workflows").glob("*.yml")):
        check_hosted_runners(workflow(path))


@pytest.mark.parametrize("selection", [
    ["self-hosted", "Linux", "X64", "public-overflow"],
    "${{ vars.RUNNER }}", "ubuntu-latest",
])
def test_runner_policy_rejects_private_dynamic_and_colliding_labels(selection):
    document = {"jobs": {"fixture": {"runs-on": selection}}}
    with pytest.raises(AssertionError):
        check_hosted_runners(document)


def test_release_matrix_cannot_hide_a_private_runner():
    document = copy.deepcopy(workflow(ROOT / ".github/workflows/release.yml"))
    document["jobs"]["binaries"]["strategy"]["matrix"]["include"].append(
        {"os": "public-overflow", "target": "synthetic"}
    )
    with pytest.raises(AssertionError):
        check_hosted_runners(document)


def test_public_ci_has_one_unprivileged_pr_path_and_an_unconditional_aggregate():
    document = workflow(ROOT / ".github/workflows/ci.yml")
    assert set(document["on"]) == {"push", "pull_request"}
    assert document["permissions"] == {"contents": "read", "pull-requests": "read"}
    assert "OWNER_PRIVATE_CI_ENABLED" not in str(document)
    for job in document["jobs"].values():
        assert "environment" not in job
        for step in job.get("steps", []):
            if step.get("uses", "").startswith("actions/checkout@"):
                assert step["with"]["persist-credentials"] == "false"
                if "repository" not in step["with"]:
                    assert "ref" not in step["with"], "use the ordinary PR merge checkout"
    assert "if" not in document["jobs"]["changes"]
    assert document["jobs"]["ci-success"]["if"] == "always()"
    assert set(document["jobs"]["ci-success"]["needs"]) == set(document["jobs"]) - {"ci-success"}
