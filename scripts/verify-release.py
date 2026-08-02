#!/usr/bin/env python3
"""Verify that a release actually reached every registry it was supposed to.

`release.yml`'s publish steps `exit 0` when their credential is absent, so a job can report
**success while publishing nothing**. That is intentional for a target with no credential
configured — but the identical guard would hide a genuinely broken publish, and a green release run
is the moment nobody looks closely. This checks the artifact, not the job.

It exists because both failure modes have already happened here:

- a crates.io check that queried the API **without a `User-Agent`** got an error object back and was
  misread as "never published" — crates.io rejects UA-less requests, so the absence was fake;
- a PyPI check run seconds after the upload reported the version missing, because the JSON API lags
  the upload by up to a minute.

So: send a User-Agent, and retry before concluding anything is absent.

Which targets are *expected* is derived from configuration, not assumed. PyPI publishes via OIDC and
is always expected; crates.io and npm are expected only when their token is configured, which is how
a deliberately-unpublished package (npm, pending a Node client) stays green without weakening the
check for the others.

Usage:
    python3 scripts/verify-release.py v0.1.4
    EXPECT_CRATES=1 EXPECT_NPM=0 python3 scripts/verify-release.py v0.1.4
"""

from __future__ import annotations

import json
import os
import sys
import time
import urllib.error
import urllib.request

# crates.io returns an error object to requests without one; that error reads exactly like
# "not published" to a naive parser. This is the single most important line in the file.
USER_AGENT = "sandhi-release-verify (https://github.com/anvai-labs/sandhi)"

CRATES = ("sandhi-core", "sandhi-providers", "sandhi-store", "sandhi-proxy")
PYPI_PACKAGE = "sandhi-gateway"
NPM_PACKAGE = "@anvai-labs/sandhi"

# Registries index asynchronously; a miss immediately after upload means nothing.
ATTEMPTS = 6
BACKOFF_SECONDS = 20


def _get_json(url: str) -> dict | None:
    request = urllib.request.Request(url, headers={"User-Agent": USER_AGENT})
    try:
        with urllib.request.urlopen(request, timeout=20) as response:
            return json.loads(response.read())
    except urllib.error.HTTPError as exc:
        if exc.code == 404:
            return None
        print(f"    (http {exc.code} from {url})")
        return None
    except (urllib.error.URLError, TimeoutError, json.JSONDecodeError) as exc:
        print(f"    (transient: {exc})")
        return None


def _with_retries(check, label: str) -> bool:
    for attempt in range(1, ATTEMPTS + 1):
        if check():
            return True
        if attempt < ATTEMPTS:
            print(f"    {label}: not visible yet (attempt {attempt}/{ATTEMPTS}), waiting…")
            time.sleep(BACKOFF_SECONDS)
    return False


def pypi_has(version: str) -> bool:
    data = _get_json(f"https://pypi.org/pypi/{PYPI_PACKAGE}/json")
    return bool(data and version in data.get("releases", {}) and data["releases"][version])


def crate_has(crate: str, version: str) -> bool:
    data = _get_json(f"https://crates.io/api/v1/crates/{crate}")
    if not data or "versions" not in data:
        return False
    return any(v.get("num") == version for v in data["versions"])


def npm_has(version: str) -> bool:
    quoted = NPM_PACKAGE.replace("/", "%2F")
    data = _get_json(f"https://registry.npmjs.org/{quoted}")
    return bool(data and version in data.get("versions", {}))


def env_flag(name: str, default: bool) -> bool:
    raw = os.environ.get(name)
    if raw is None:
        return default
    return raw.strip().lower() in {"1", "true", "yes"}


def main() -> int:
    if len(sys.argv) != 2:
        print(__doc__)
        return 2
    tag = sys.argv[1]
    version = tag[1:] if tag.startswith("v") else tag

    # Expectation comes from configuration. A target with no credential is *expected absent*, which
    # is reported plainly rather than silently passing — the point is that the state is stated.
    expect_crates = env_flag("EXPECT_CRATES", True)
    expect_npm = env_flag("EXPECT_NPM", False)

    print(f"verifying release {tag} (version {version})\n")
    failures: list[str] = []

    print("  PyPI:")
    if _with_retries(lambda: pypi_has(version), PYPI_PACKAGE):
        print(f"    OK   {PYPI_PACKAGE} {version}")
    else:
        failures.append(f"PyPI {PYPI_PACKAGE} {version} missing")
        print(f"    FAIL {PYPI_PACKAGE} {version} not published")

    print("  crates.io:")
    if expect_crates:
        for crate in CRATES:
            if _with_retries(lambda c=crate: crate_has(c, version), crate):
                print(f"    OK   {crate} {version}")
            else:
                failures.append(f"crates.io {crate} {version} missing")
                print(f"    FAIL {crate} {version} not published")
    else:
        print("    SKIP no CARGO_REGISTRY_TOKEN configured — not expected")

    print("  npm:")
    if expect_npm:
        if _with_retries(lambda: npm_has(version), NPM_PACKAGE):
            print(f"    OK   {NPM_PACKAGE} {version}")
        else:
            failures.append(f"npm {NPM_PACKAGE} {version} missing")
            print(f"    FAIL {NPM_PACKAGE} {version} not published")
    else:
        # Deliberate: there is no Node client yet, so NPM_TOKEN is unset on purpose. Said out loud
        # so "npm is missing" is never mistaken for a regression.
        print("    SKIP no NPM_TOKEN configured — intentionally unpublished")

    print()
    if failures:
        print("RELEASE INCOMPLETE — a publish job reported success without shipping:")
        for failure in failures:
            print(f"  - {failure}")
        return 1
    print("all expected targets verified")
    return 0


if __name__ == "__main__":
    sys.exit(main())
