"""Offline authorization regressions: no secrets, tag mutation or publishing."""

import copy
import importlib.util
from pathlib import Path

import pytest

SPEC = importlib.util.spec_from_file_location("release_guard", Path(__file__).parents[2] / "scripts/release_guard.py")
guard = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(guard)
SOURCE, MAIN, TAG = "a" * 40, "b" * 40, "c" * 40


class FixtureAPI:
    def __init__(self):
        self.tag_type, self.tag_oid, self.source = "commit", SOURCE, SOURCE
        self.compare, self.protected = "ahead", True
        self.workflow = {"id": guard.WORKFLOW_ID, "path": guard.WORKFLOW_PATH, "state": "active"}
        self.runs = [{"id": 99, "run_attempt": 1, "workflow_id": guard.WORKFLOW_ID,
                      "path": guard.WORKFLOW_PATH, "event": "push", "head_branch": "main",
                      "head_sha": SOURCE, "repository": {"full_name": guard.REPO},
                      "head_repository": {"full_name": guard.REPO}, "status": "completed", "conclusion": "success"}]
        self.jobs = [{"name": name, "status": "completed", "conclusion": "success"} for name in guard.REQUIRED_JOBS]
        self.deleted, self.total_jobs, self.total_runs = False, None, None

    def get(self, path):
        if "/git/ref/tags/" in path:
            if self.deleted:
                raise guard.Denied("missing")
            return {"ref": "refs/tags/v0.6.0", "object": {"type": self.tag_type, "sha": self.tag_oid}}
        if "/git/tags/" in path:
            return {"sha": self.tag_oid, "object": {"type": "commit", "sha": self.source}}
        if path.endswith("/branches/main"):
            return {"name": "main", "protected": self.protected, "commit": {"sha": MAIN}}
        if "/compare/" in path:
            return {"status": self.compare, "merge_base_commit": {"sha": path.split("/compare/")[1].split("...")[0]}}
        if "/runs?" in path:
            return {"total_count": self.total_runs if self.total_runs is not None else len(self.runs), "workflow_runs": self.runs}
        if "/jobs?" in path:
            return {"total_count": self.total_jobs if self.total_jobs is not None else len(self.jobs), "jobs": self.jobs}
        if path.endswith(str(guard.WORKFLOW_ID)):
            return self.workflow
        raise AssertionError(path)


def authorize(api, **overrides):
    args = dict(repository=guard.REPO, event="push", ref="refs/tags/v0.6.0", event_sha=SOURCE)
    args.update(overrides)
    return guard.authorize(api, **args)


@pytest.mark.parametrize("tag", ["v0.6.0", "v1.0.0", "v10.20.30"])
def test_stable_versions(tag):
    assert guard.stable_tag(tag) == tag


@pytest.mark.parametrize("tag", ["0.6.0", "v01.2.3", "v1.2.3-rc1", "v1.2.3+build", "v1x.2y.3z",
                                 "v1.2.3\n", "v١.2.3", "v1.2.3/evil", "v1.2.3;echo bad", "v1.2", ""])
def test_nonstable_or_ambiguous_versions_rejected(tag):
    with pytest.raises(guard.Denied):
        guard.stable_tag(tag)


def test_valid_lightweight_and_annotated_proof():
    api = FixtureAPI()
    proof = authorize(api)
    assert proof["source_sha"] == SOURCE and proof["ci_run_id"] == 99
    assert guard.revalidate(api, proof) == proof
    api.tag_type, api.tag_oid = "tag", TAG
    proof = authorize(api)
    assert proof["tag_oid"] == TAG and proof["source_sha"] == SOURCE


@pytest.mark.parametrize("fault", [None, "source_sha", "control_sha", "tag", "version", "missing", "symlink", "oversize"])
def test_proof_file_independently_bound_to_job_outputs(tmp_path, monkeypatch, fault):
    import json
    api = FixtureAPI()
    proof = authorize(api)
    for field, variable in (("source_sha", "EXPECTED_SOURCE_SHA"), ("control_sha", "EXPECTED_CONTROL_SHA"),
                            ("tag", "RELEASE_TAG"), ("version", "RELEASE_VERSION")):
        monkeypatch.setenv(variable, "wrong" if fault == field else proof[field])
    if fault == "missing": monkeypatch.delenv("EXPECTED_SOURCE_SHA")
    path = tmp_path / "proof.json"
    path.write_text(json.dumps(proof) if fault != "oversize" else " " * 16385)
    if fault == "symlink":
        alias = tmp_path / "alias.json"
        alias.symlink_to(path)
        path = alias
    if fault is None:
        assert guard.verify_proof_file(api, path) == proof
    else:
        with pytest.raises(guard.Denied): guard.verify_proof_file(api, path)


@pytest.mark.parametrize("field,value", [("event", "pull_request"), ("control_sha", "bad"), ("control_sha", MAIN)])
def test_invalid_proof_control_authority(field, value):
    api = FixtureAPI()
    proof = authorize(api)
    proof[field] = value
    with pytest.raises(guard.Denied): guard.revalidate(api, proof)


@pytest.mark.parametrize("override", [dict(repository="other/repo"), dict(event="pull_request"),
    dict(ref="refs/heads/v0.6.0"), dict(event_sha=MAIN), dict(repair_tag="v0.6.0"),
    dict(event="workflow_dispatch", ref="refs/heads/develop", repair_tag="v0.6.0")])
def test_event_authority(override):
    with pytest.raises(guard.Denied):
        authorize(FixtureAPI(), **override)


def test_dispatch_from_main_resolves_tag_separately():
    proof = authorize(FixtureAPI(), event="workflow_dispatch", ref="refs/heads/main", event_sha=MAIN, repair_tag="v0.6.0")
    assert proof["source_sha"] == SOURCE and proof["control_sha"] == MAIN


@pytest.mark.parametrize("field,value", [("workflow_id", 1), ("path", ".github/workflows/fake.yml"),
    ("event", "pull_request"), ("head_branch", "develop"), ("head_sha", MAIN),
    ("status", "in_progress"), ("conclusion", "skipped"), ("conclusion", "failure"),
    ("repository", {"full_name": "fork/repo"}), ("head_repository", {"full_name": "fork/repo"}),
    ("run_attempt", 0)])
def test_wrong_ci_provenance_and_outcome(field, value):
    api = FixtureAPI()
    api.runs[0][field] = value
    with pytest.raises(guard.Denied):
        authorize(api)


def test_newer_failed_run_is_not_hidden_by_older_success():
    api = FixtureAPI()
    newer = copy.deepcopy(api.runs[0])
    newer.update(id=100, conclusion="failure")
    api.runs.append(newer)
    with pytest.raises(guard.Denied):
        authorize(api)


@pytest.mark.parametrize("fault", ["skipped", "missing", "duplicate", "unfinished", "partial"])
def test_required_job_must_really_execute(fault):
    api = FixtureAPI()
    if fault == "skipped":
        api.jobs[0]["conclusion"] = "skipped"
    elif fault == "missing":
        api.jobs.pop()
    elif fault == "duplicate":
        api.jobs.append(copy.deepcopy(api.jobs[0]))
    elif fault == "unfinished":
        api.jobs[0]["status"] = "in_progress"
    else:
        api.total_jobs = 101
    with pytest.raises(guard.Denied):
        authorize(api)


@pytest.mark.parametrize("fault", ["diverged", "unprotected", "missing_ci", "partial_ci", "workflow"])
def test_main_and_ci_fail_closed(fault):
    api = FixtureAPI()
    if fault == "diverged": api.compare = "diverged"
    elif fault == "unprotected": api.protected = False
    elif fault == "missing_ci": api.runs = []
    elif fault == "partial_ci": api.total_runs = 101
    else: api.workflow["state"] = "disabled_manually"
    with pytest.raises(guard.Denied): authorize(api)


@pytest.mark.parametrize("fault", ["tag_moved", "tag_deleted", "new_attempt", "new_run", "ci_failed"])
def test_publish_revalidation_pins_original_evidence(fault):
    api = FixtureAPI()
    proof = authorize(api)
    if fault == "tag_moved": api.tag_oid = MAIN
    elif fault == "tag_deleted": api.deleted = True
    elif fault == "new_attempt": api.runs[0]["run_attempt"] = 2
    elif fault == "new_run": api.runs[0]["id"] = 100
    else: api.runs[0]["conclusion"] = "failure"
    with pytest.raises(guard.Denied): guard.revalidate(api, proof)
