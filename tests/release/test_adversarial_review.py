"""Offline regressions for the local review gate and Git pre-push protocol."""

import importlib.util
import io
import json
import os
from pathlib import Path
import shutil
import subprocess

import pytest


ROOT = Path(__file__).parents[2]
SPEC = importlib.util.spec_from_file_location("adversarial_review", ROOT / "scripts/adversarial_review.py")
review = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(review)
ZERO = "0" * 40


def git(repo, *args):
    return subprocess.run(["git", "-C", str(repo), *args], check=True,
                          capture_output=True, text=True).stdout.strip()


@pytest.fixture
def repo(tmp_path):
    path = tmp_path / "repo"
    path.mkdir()
    git(path, "init", "-b", "feature")
    git(path, "config", "user.name", "Implementation Author")
    git(path, "config", "user.email", "author@example.test")
    git(path, "config", "commit.gpgSign", "false")
    git(path, "config", "core.hooksPath", "/dev/null")
    git(path, "commit", "--allow-empty", "-m", "base")
    git(path, "branch", "review-base")
    (path / "change.txt").write_text("reviewed change\n")
    git(path, "add", "change.txt")
    git(path, "commit", "-m", "reviewed change")
    return path


def record(repo, ref="HEAD", **kwargs):
    args = dict(base="review-base", reviewer="independent-reviewer-session",
                summary="Reviewed complete diff; no unresolved findings.", independent_review=True)
    args.update(kwargs)
    return review.record(repo, ref, **args)


def pushed(repo, ref="HEAD", remote="refs/heads/feature"):
    return f"{ref} {git(repo, 'rev-parse', ref)} {remote} {ZERO}\n"


def test_missing_then_valid_review(repo):
    with pytest.raises(review.ReviewError, match="No independent review"):
        review.verify(repo, "HEAD")
    path = record(repo)
    payload = review.verify(repo, "HEAD")
    assert payload["commit_sha"] == git(repo, "rev-parse", "HEAD")
    assert payload["tree_sha"] == git(repo, "rev-parse", "HEAD^{tree}")
    assert payload["base_sha"] == git(repo, "rev-parse", "review-base")
    assert path.stat().st_mode & 0o777 == 0o600
    assert review.verify_push(repo, io.StringIO(pushed(repo))) == 1


@pytest.mark.parametrize("kwargs", [dict(independent_review=False), dict(reviewer=" "), dict(summary="")])
def test_record_requires_explicit_reviewer_result(repo, kwargs):
    with pytest.raises(review.ReviewError):
        record(repo, **kwargs)


@pytest.mark.parametrize("field,value", [
    ("commit_sha", "a" * 40), ("tree_sha", "b" * 40), ("base_sha", "c" * 40),
    ("base_ref", "missing-ref"), ("reviewer", ""), ("summary", None),
    ("independent_review", False), ("independent_review", 1),
    ("schema_version", True), ("verdict", "findings"),
    ("reviewed_at", "bad-date"), ("reviewed_at", "2099-01-01T00:00:00+00:00"),
])
def test_tampered_or_malformed_fields_fail(repo, field, value):
    path = record(repo)
    payload = json.loads(path.read_text())
    payload[field] = value
    path.write_text(json.dumps(payload))
    with pytest.raises(review.ReviewError):
        review.verify(repo, "HEAD")


@pytest.mark.parametrize("content", ["[]", "{", "null"])
def test_malformed_document_fails(repo, content):
    path = record(repo)
    path.write_text(content)
    with pytest.raises(review.ReviewError):
        review.verify(repo, "HEAD")


def test_amend_even_same_tree_requires_review(repo):
    record(repo)
    git(repo, "commit", "--amend", "-m", "amended message")
    with pytest.raises(review.ReviewError, match="No independent review"):
        review.verify(repo, "HEAD")


def test_base_movement_invalidates_record(repo):
    record(repo)
    git(repo, "branch", "-f", "review-base", "HEAD")
    with pytest.raises(review.ReviewError, match="base_sha"):
        review.verify(repo, "HEAD")


def test_unrelated_base_cannot_be_recorded(repo):
    git(repo, "checkout", "--orphan", "unrelated")
    git(repo, "commit", "-m", "unrelated root")
    with pytest.raises(review.ReviewError):
        record(repo)


def test_actual_non_head_sha_and_all_push_refs_are_checked(repo):
    record(repo)
    git(repo, "branch", "unreviewed", "review-base")
    # A valid HEAD record cannot authorize a different branch being pushed.
    with pytest.raises(review.ReviewError, match="No independent review"):
        review.verify_push(repo, io.StringIO(pushed(repo, "unreviewed")))
    with pytest.raises(review.ReviewError, match="No independent review"):
        review.verify_push(repo, io.StringIO(pushed(repo) + pushed(repo, "unreviewed")))
    record(repo, "unreviewed")
    assert review.verify_push(repo, io.StringIO(pushed(repo) + pushed(repo, "unreviewed"))) == 2


def test_deletion_and_empty_push_need_no_record(repo):
    deletion = f"(delete) {ZERO} refs/heads/old {git(repo, 'rev-parse', 'HEAD')}\n"
    assert review.verify_push(repo, io.StringIO(deletion)) == 0
    assert review.verify_push(repo, io.StringIO("")) == 0


@pytest.mark.parametrize("line", ["\n", "HEAD\n", "HEAD bad refs/heads/main bad\n"])
def test_malformed_push_input_fails_closed(repo, line):
    with pytest.raises(review.ReviewError):
        review.verify_push(repo, io.StringIO(line))


@pytest.mark.parametrize("annotated", [False, True])
def test_tags_require_review_of_peeled_commit(repo, annotated):
    args = ["tag", "release"]
    if annotated:
        args += ["-a", "-m", "release"]
    git(repo, *args)
    lines = pushed(repo, "refs/tags/release", "refs/tags/release")
    with pytest.raises(review.ReviewError, match="No independent review"):
        review.verify_push(repo, io.StringIO(lines))
    record(repo)
    assert review.verify_push(repo, io.StringIO(lines)) == 1


def test_non_commit_tag_fails(repo):
    git(repo, "tag", "blob", "HEAD:change.txt")
    with pytest.raises(review.ReviewError):
        review.verify_push(repo, io.StringIO(pushed(repo, "refs/tags/blob", "refs/tags/blob")))


def test_record_is_shared_between_worktrees(repo, tmp_path):
    record(repo)
    worktree = tmp_path / "worktree"
    git(repo, "worktree", "add", "--detach", str(worktree), "HEAD")
    assert review.verify(worktree, "HEAD")["commit_sha"] == git(repo, "rev-parse", "HEAD")


def test_installed_hook_receives_real_git_push_protocol(repo, tmp_path):
    (repo / "scripts").mkdir()
    shutil.copy2(ROOT / "scripts/adversarial_review.py", repo / "scripts/adversarial_review.py")
    hooks = repo / ".githooks"
    hooks.mkdir()
    shutil.copy2(ROOT / ".githooks/pre-push", hooks / "pre-push")
    git(repo, "config", "core.hooksPath", ".githooks")
    # Formatting is independent of this test; isolate it from host Cargo projects.
    binaries = tmp_path / "bin"
    binaries.mkdir()
    cargo = binaries / "cargo"
    cargo.write_text("#!/bin/sh\nexit 0\n")
    cargo.chmod(0o755)
    environment = dict(os.environ, PATH=f"{binaries}{os.pathsep}{os.environ['PATH']}")
    remote = tmp_path / "remote.git"
    subprocess.run(["git", "init", "--bare", str(remote)], check=True, capture_output=True)
    git(repo, "remote", "add", "destination", str(remote))

    def push(ref):
        return subprocess.run(["git", "-C", str(repo), "push", "destination", ref],
                              capture_output=True, text=True, env=environment)

    record(repo)
    git(repo, "branch", "other", "review-base")
    rejected = push("other:refs/heads/other")
    assert rejected.returncode != 0 and "No independent review" in rejected.stderr
    record(repo, "other")
    assert push("other:refs/heads/other").returncode == 0
    assert git(remote, "rev-parse", "refs/heads/other") == git(repo, "rev-parse", "other")
    review.attestation_path(repo, git(repo, "rev-parse", "other")).unlink()
    assert push(":refs/heads/other").returncode == 0
