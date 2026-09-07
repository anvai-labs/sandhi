#!/usr/bin/env python3
"""Read-only, fail-closed release authorization. Never creates a tag or publishes.

GitHub-side ref/environment controls are required in addition to this check: an old or
modified workflow can omit a repository script. No API response or credential is logged.
"""

import argparse
import json
import os
from pathlib import Path
import re
import sys
import urllib.error
import urllib.parse
import urllib.request


REPO = "anvai-labs/sandhi"
WORKFLOW_ID = 316034660
WORKFLOW_PATH = ".github/workflows/ci.yml"
STABLE = re.compile(r"v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)", re.ASCII)
SHA = re.compile(r"[0-9a-f]{40}", re.ASCII)
REQUIRED_JOBS = {"CI Success", "Release safeguards"}


class Denied(ValueError):
    pass


def require(condition, message):
    if not condition:
        raise Denied(message)


def stable_tag(value):
    require(isinstance(value, str) and STABLE.fullmatch(value) is not None,
            "release requires an exact stable vX.Y.Z tag")
    return value


def sha(value):
    require(isinstance(value, str) and SHA.fullmatch(value) is not None, "invalid commit identity")
    return value


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        raise Denied("GitHub API redirect refused")


class API:
    def __init__(self, token):
        self.token = token
        self.opener = urllib.request.build_opener(NoRedirect())

    def get(self, path):
        require(path.startswith(f"/repos/{REPO}/") and "\n" not in path, "invalid API route")
        request = urllib.request.Request("https://api.github.com" + path, headers={
            "Authorization": "Bearer " + self.token,
            "Accept": "application/vnd.github+json", "User-Agent": "sandhi-release-guard",
            "X-GitHub-Api-Version": "2022-11-28",
        })
        try:
            with self.opener.open(request, timeout=20) as response:
                raw = response.read(2_000_001)
            require(len(raw) <= 2_000_000, "GitHub API response exceeds bound")
            value = json.loads(raw)
            require(isinstance(value, dict), "invalid GitHub API document")
            return value
        except (urllib.error.URLError, TimeoutError, OSError, json.JSONDecodeError) as error:
            raise Denied("GitHub API unavailable or invalid; release authorization denied") from error


def resolve_tag(api, tag):
    stable_tag(tag)
    ref = api.get(f"/repos/{REPO}/git/ref/tags/{tag}")
    require(ref.get("ref") == "refs/tags/" + tag, "tag reference mismatch")
    obj = ref.get("object", {})
    tag_oid = sha(obj.get("sha"))
    visited = set()
    for _ in range(8):
        oid = sha(obj.get("sha"))
        require(oid not in visited, "cyclic annotated tag")
        visited.add(oid)
        if obj.get("type") == "commit":
            return tag_oid, oid
        require(obj.get("type") == "tag", "tag must resolve to a commit")
        annotated = api.get(f"/repos/{REPO}/git/tags/{oid}")
        require(annotated.get("sha") == oid, "annotated tag identity mismatch")
        obj = annotated.get("object", {})
    raise Denied("annotated tag nesting exceeds bound")


def main_ancestor(api, source):
    main = api.get(f"/repos/{REPO}/branches/main")
    require(main.get("name") == "main" and main.get("protected") is True, "main must be protected")
    tip = sha(main.get("commit", {}).get("sha"))
    compare = api.get(f"/repos/{REPO}/compare/{source}...{tip}")
    require(compare.get("status") in {"ahead", "identical"}
            and compare.get("merge_base_commit", {}).get("sha") == source,
            "release commit is not on protected main")
    return tip


def validate_run(run, source):
    require(isinstance(run, dict), "invalid CI run")
    require(run.get("workflow_id") == WORKFLOW_ID and run.get("path") == WORKFLOW_PATH
            and run.get("event") == "push" and run.get("head_branch") == "main"
            and run.get("head_sha") == source
            and run.get("repository", {}).get("full_name") == REPO
            and run.get("head_repository", {}).get("full_name") == REPO,
            "CI provenance does not match canonical main push")
    require(run.get("status") == "completed" and run.get("conclusion") == "success",
            "latest exact-source main CI has not succeeded")
    for key in ("id", "run_attempt"):
        require(type(run.get(key)) is int and run[key] > 0, "invalid CI run identity")


def validate_jobs(api, run):
    result = api.get(f"/repos/{REPO}/actions/runs/{run['id']}/attempts/{run['run_attempt']}/jobs?per_page=100")
    jobs = result.get("jobs")
    require(isinstance(jobs, list) and type(result.get("total_count")) is int
            and result["total_count"] == len(jobs) and 0 < len(jobs) <= 100,
            "incomplete CI job evidence")
    require(all(isinstance(job, dict) and job.get("status") == "completed"
                and job.get("conclusion") in {"success", "skipped"} for job in jobs),
            "CI contains an unsuccessful or unfinished job")
    for name in REQUIRED_JOBS:
        matching = [job for job in jobs if job.get("name") == name]
        require(len(matching) == 1 and matching[0].get("conclusion") == "success",
                "required CI evidence job did not execute successfully")


def latest_ci(api, source):
    workflow = api.get(f"/repos/{REPO}/actions/workflows/{WORKFLOW_ID}")
    require(workflow.get("id") == WORKFLOW_ID and workflow.get("path") == WORKFLOW_PATH
            and workflow.get("state") == "active", "canonical CI workflow unavailable")
    query = urllib.parse.urlencode({"event": "push", "branch": "main", "head_sha": source, "per_page": 100})
    response = api.get(f"/repos/{REPO}/actions/workflows/{WORKFLOW_ID}/runs?{query}")
    runs = response.get("workflow_runs")
    require(isinstance(runs, list) and type(response.get("total_count")) is int
            and response["total_count"] == len(runs) and 0 < len(runs) <= 100,
            "exact-source main CI evidence missing or exceeds bound")
    require(all(isinstance(run, dict) and type(run.get("id")) is int for run in runs),
            "invalid CI run list")
    # Do not query only successful runs: that hides a newer failed run or rerun.
    run = max(runs, key=lambda item: item["id"])
    validate_run(run, source)
    validate_jobs(api, run)
    return run


def authorize(api, *, repository, event, ref, event_sha, repair_tag=""):
    require(repository == REPO, "release repository not authorized")
    sha(event_sha)
    if event == "push":
        require(ref.startswith("refs/tags/"), "release push must be a tag")
        tag = stable_tag(ref.removeprefix("refs/tags/"))
        require(not repair_tag, "repair input not allowed for tag push")
    else:
        require(event == "workflow_dispatch" and ref == "refs/heads/main",
                "npm repair must dispatch the workflow from main")
        tag = stable_tag(repair_tag)
    tag_oid, source = resolve_tag(api, tag)
    require(event != "push" or event_sha == source, "tag event commit mismatch")
    main_tip = main_ancestor(api, source)
    # Dispatch control code must also come from protected main, not an arbitrary SHA.
    if event == "workflow_dispatch":
        main_ancestor(api, event_sha)
    run = latest_ci(api, source)
    return {"schema_version": 1, "repository": REPO, "event": event, "tag": tag,
            "version": tag[1:], "tag_oid": tag_oid, "source_sha": source,
            "control_sha": event_sha, "main_tip_at_authorization": main_tip,
            "ci_run_id": run["id"], "ci_run_attempt": run["run_attempt"], "ci_workflow_id": WORKFLOW_ID}


def revalidate(api, proof):
    require(isinstance(proof, dict) and proof.get("schema_version") == 1
            and proof.get("repository") == REPO and proof.get("ci_workflow_id") == WORKFLOW_ID,
            "invalid authorization proof")
    tag = stable_tag(proof.get("tag"))
    source = sha(proof.get("source_sha"))
    control = sha(proof.get("control_sha"))
    require(proof.get("event") in {"push", "workflow_dispatch"}, "invalid proof event")
    require(proof["event"] != "push" or control == source, "push control/source mismatch")
    require(proof.get("version") == tag[1:], "proof version mismatch")
    tag_oid, current = resolve_tag(api, tag)
    require(tag_oid == proof.get("tag_oid") and current == source, "release tag moved")
    main_ancestor(api, source)
    if proof["event"] == "workflow_dispatch":
        main_ancestor(api, control)
    run = latest_ci(api, source)
    require(run["id"] == proof.get("ci_run_id") and run["run_attempt"] == proof.get("ci_run_attempt"),
            "CI run or attempt changed since release authorization")
    return proof


def verify_proof_file(api, path):
    require(path.stat().st_size <= 16384 and not path.is_symlink(), "unsafe proof file")
    proof = json.loads(path.read_text())
    require(isinstance(proof, dict), "invalid proof")
    # Job outputs are a separate authority channel from the artifact's JSON payload.
    for field, variable in (("source_sha", "EXPECTED_SOURCE_SHA"),
                            ("control_sha", "EXPECTED_CONTROL_SHA"),
                            ("tag", "RELEASE_TAG"), ("version", "RELEASE_VERSION")):
        expected = os.environ.get(variable, "")
        require(bool(expected) and proof.get(field) == expected, "proof/job output mismatch")
    return revalidate(api, proof)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--verify-proof", type=Path)
    parser.add_argument("--output", type=Path)
    parser.add_argument("--github-output", type=Path)
    args = parser.parse_args()
    token = os.environ.get("GH_TOKEN", "")
    try:
        require(bool(token), "read-only GitHub token required")
        api = API(token)
        if args.verify_proof:
            require(not args.output and not args.github_output, "verification does not emit new authorization")
            verify_proof_file(api, args.verify_proof)
        else:
            require(args.output is not None, "proof output required")
            proof = authorize(api, repository=os.environ.get("GITHUB_REPOSITORY", ""),
                              event=os.environ.get("GITHUB_EVENT_NAME", ""),
                              ref=os.environ.get("GITHUB_REF", ""),
                              event_sha=os.environ.get("GITHUB_SHA", ""),
                              repair_tag=os.environ.get("REPAIR_TAG", ""))
            with args.output.open("x") as target:
                json.dump(proof, target, indent=2, sort_keys=True)
                target.write("\n")
            if args.github_output:
                with args.github_output.open("a") as target:
                    for key in ("tag", "version", "source_sha", "control_sha"):
                        target.write(f"{key}={proof[key]}\n")
        print("release authorization verified; no publication performed")
        return 0
    except (Denied, OSError, ValueError, TypeError, AttributeError):
        # API data can include arbitrary server text; no raw exception/body/token output.
        print("release authorization denied: invalid, unavailable or unsuccessful source/CI evidence", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
