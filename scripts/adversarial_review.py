#!/usr/bin/env python3
"""Local process gate for recording an independent review of an exact commit.

These unsigned records do not authenticate reviewers or replace GitHub approval.
"""

from __future__ import annotations

import argparse
from datetime import datetime, timezone
import json
from pathlib import Path
import re
import subprocess
import sys
import tempfile


class ReviewError(RuntimeError):
    """A review is absent, stale, or malformed."""


def git(repo: Path, *args: str) -> str:
    result = subprocess.run(
        ["git", "-C", str(repo), *args], capture_output=True, text=True
    )
    if result.returncode:
        raise ReviewError(result.stderr.strip() or "Git command failed")
    return result.stdout.strip()


def commit(repo: Path, ref: str) -> str:
    return git(repo, "rev-parse", "--verify", "--end-of-options", f"{ref}^{{commit}}")


def attestation_path(repo: Path, sha: str) -> Path:
    common = Path(git(repo, "rev-parse", "--git-common-dir"))
    if not common.is_absolute():
        common = repo / common
    return common.resolve() / "sandhi" / "adversarial-reviews" / f"{sha}.json"


def review_binding(repo: Path, ref: str, base_ref: str) -> dict:
    sha, base_sha = commit(repo, ref), commit(repo, base_ref)
    git(repo, "merge-base", "--is-ancestor", base_sha, sha)
    return {
        "schema_version": 1,
        "verdict": "clean",
        "independent_review": True,
        "commit_sha": sha,
        "tree_sha": git(repo, "rev-parse", f"{sha}^{{tree}}"),
        "base_ref": base_ref,
        "base_sha": base_sha,
    }


def record(repo: Path, ref: str, base: str, reviewer: str, summary: str,
           independent_review: bool) -> Path:
    if not independent_review:
        raise ReviewError("An explicit independent reviewer result is required")
    if not reviewer.strip() or not summary.strip():
        raise ReviewError("Reviewer identity and review summary must be nonempty")
    payload = review_binding(repo, ref, base)
    payload.update(reviewer=reviewer.strip(), summary=summary.strip(),
                   reviewed_at=datetime.now(timezone.utc).isoformat())
    path = attestation_path(repo, payload["commit_sha"])
    path.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
    temporary = None
    try:
        with tempfile.NamedTemporaryFile(mode="w", encoding="utf-8", dir=path.parent,
                                         delete=False) as stream:
            temporary = Path(stream.name)
            json.dump(payload, stream, indent=2, sort_keys=True)
            stream.write("\n")
        temporary.replace(path)
    finally:
        if temporary is not None:
            temporary.unlink(missing_ok=True)
    return path


def verify(repo: Path, ref: str) -> dict:
    sha = commit(repo, ref)
    try:
        payload = json.loads(attestation_path(repo, sha).read_text(encoding="utf-8"))
    except FileNotFoundError as exc:
        raise ReviewError(f"No independent review recorded for {sha}") from exc
    except (UnicodeError, json.JSONDecodeError) as exc:
        raise ReviewError("Unreadable review attestation") from exc
    if not isinstance(payload, dict):
        raise ReviewError("Review attestation must be a JSON object")
    for field in ("base_ref", "base_sha", "reviewer", "summary", "reviewed_at"):
        if not isinstance(payload.get(field), str) or not payload[field].strip():
            raise ReviewError(f"Missing review field: {field}")
    for field, expected in review_binding(repo, sha, payload["base_ref"]).items():
        if type(payload.get(field)) is not type(expected) or payload[field] != expected:
            raise ReviewError(f"Stale or invalid review field: {field}")
    try:
        reviewed_at = datetime.fromisoformat(payload["reviewed_at"])
        if reviewed_at.tzinfo is None or reviewed_at > datetime.now(timezone.utc):
            raise ValueError("Expected a past timestamp with timezone")
    except ValueError as exc:
        raise ReviewError("Invalid review timestamp") from exc
    return payload


def verify_push(repo: Path, lines) -> int:
    """Consume Git's pre-push protocol; never substitute the checked-out HEAD."""
    verified = set()
    for line in lines:
        fields = line.split()
        if len(fields) != 4:
            raise ReviewError("Malformed pre-push input")
        _local_ref, local_sha, _remote_ref, remote_sha = fields
        if (not re.fullmatch(r"(?:[0-9a-f]{40}|[0-9a-f]{64})", local_sha)
                or not re.fullmatch(r"(?:[0-9a-f]{40}|[0-9a-f]{64})", remote_sha)
                or len(local_sha) != len(remote_sha)):
            raise ReviewError("Invalid object ID in pre-push input")
        if set(local_sha) == {"0"}:  # Ref deletion introduces no commit.
            continue
        # Peel annotated tags. Tags to blobs/trees cannot carry a commit review.
        sha = commit(repo, local_sha)
        if sha not in verified:
            verify(repo, sha)
            verified.add(sha)
    return len(verified)


def main(argv=None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo", type=Path, default=Path.cwd())
    commands = parser.add_subparsers(dest="command", required=True)
    recorder = commands.add_parser("record", help="record an already completed clean review")
    recorder.add_argument("--commit", required=True, help="exact reviewed commit/ref")
    recorder.add_argument("--base", required=True, help="reviewed ancestor ref or commit")
    recorder.add_argument("--reviewer", required=True, help="independent reviewer/session identity")
    recorder.add_argument("--summary", required=True, help="review findings and disposition")
    recorder.add_argument("--independent-review", action="store_true", required=True,
                          help="confirm this records a separate reviewer's clean result")
    checker = commands.add_parser("check", help="verify an explicit commit")
    checker.add_argument("--commit", required=True)
    commands.add_parser("check-push", help="verify every pushed object from Git's stdin")
    args = parser.parse_args(argv)
    try:
        if args.command == "record":
            path = record(args.repo, args.commit, args.base, args.reviewer,
                          args.summary, args.independent_review)
            print(f"Recorded local independent-review result: {path}")
        elif args.command == "check":
            payload = verify(args.repo, args.commit)
            print(f"Review gate passed: {payload['commit_sha']} ({payload['reviewer']})")
        else:
            count = verify_push(args.repo, sys.stdin)
            print(f"Review gate passed: {count} pushed commit(s)")
        return 0
    except (ReviewError, OSError) as exc:
        print(f"Adversarial review gate BLOCKED: {exc}", file=sys.stderr)
        print("Obtain an independent review of the exact commit and base, then record "
              "its clean result; see .githooks/README.md.", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
