#!/usr/bin/env python3
"""Read-only release verification, independent of publisher credentials/job success.

Usage: python3 scripts/verify-release.py v0.6.0 --repo anvai-labs/sandhi
All four targets (pypi,crates,npm,github) are expected by default. --targets explicitly
selects a nonempty subset. Without that option, legacy EXPECT_NPM=0/EXPECT_CRATES=0
remain available for historical tags; EXPECT_PYPI/EXPECT_GITHUB follow the same rule.
An excluded target is unverified, never successful. No publishing token controls scope.

GH_TOKEN (or GITHUB_TOKEN) is optional for GitHub API rate limits; it is not printed
or sent to registries/download hosts. --repo defaults to GITHUB_REPOSITORY, then
anvai-labs/sandhi. Platform overrides describe the release matrix, not this machine.

Registry checks inspect version metadata, not installation or binary compatibility.
GitHub archives are streamed to verify byte size and SHA-256 when GitHub supplies a
digest; bytes are not saved, extracted or executed. A missing digest is disclosed.
Network reads have timeouts/size limits; a shared deadline bounds retries and streaming
checks. Use an outer process/job timeout as well for stalled OS/network operations.
Exit 0: every explicitly expected artifact verified; 1: missing/unavailable/invalid;
2: invalid configuration. HTTP authentication/rate/server errors are NOT absence.
"""

from __future__ import annotations

import argparse
from dataclasses import dataclass
from enum import Enum
import hashlib
import http.client
import json
import math
import os
import re
import sys
import time
import urllib.error
import urllib.parse
import urllib.request

USER_AGENT = "sandhi-release-verify (https://github.com/anvai-labs/sandhi)"
CRATES = ("sandhi-core", "sandhi-providers", "sandhi-store", "sandhi-proxy")
PYPI_PACKAGE = "sandhi-gateway"
NPM_PACKAGE = "@anvailabs/sandhi"
TARGETS = ("pypi", "crates", "npm", "github")
PYPI_PLATFORMS = ("linux-x86_64", "macos-arm64", "windows-amd64")
NPM_PLATFORMS = ("linux-x64-gnu", "darwin-arm64")
BINARY_TARGETS = ("x86_64-unknown-linux-gnu", "aarch64-apple-darwin")
MAX_JSON_BYTES = 8 * 1024 * 1024
MAX_JSON_DEPTH = 64
MAX_ASSET_BYTES = 128 * 1024 * 1024
SHA256 = re.compile(r"[0-9a-fA-F]{64}\Z")


class Status(str, Enum):
    OK = "OK"
    MISSING = "MISSING"
    UNAVAILABLE = "UNAVAILABLE"
    INVALID = "INVALID"


@dataclass(frozen=True)
class Result:
    status: Status
    reason: str
    data: dict | None = None


def invalid(reason):
    return Result(Status.INVALID, reason)


def stable_version(tag):
    if not isinstance(tag, str) or len(tag) > 64 or not re.fullmatch(
            r"v?(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)", tag):
        raise ValueError("tag must be a stable vX.Y.Z or X.Y.Z version without leading zeros")
    return tag.removeprefix("v")


def env_flag(name, default=True, environ=None):
    raw = (os.environ if environ is None else environ).get(name)
    if raw is None:
        return default
    if raw.strip().lower() in {"1", "true", "yes"}:
        return True
    if raw.strip().lower() in {"0", "false", "no"}:
        return False
    raise ValueError(f"{name} must explicitly be true/false, yes/no or 1/0")


def selection(raw, allowed, label):
    values = tuple(part.strip() for part in raw.split(","))
    if not values or len(values) != len(set(values)) or any(value not in allowed for value in values):
        raise ValueError(f"{label} must be a nonempty, duplicate-free subset of {','.join(allowed)}")
    return values


def bounded_json(raw):
    """Check UTF-8 JSON nesting before decoding, independent of Python recursion limits.

    Registry metadata needs only shallow nesting. The depth bound counts objects
    and arrays (including the root); quoted/escaped delimiters do not count.
    json.loads remains responsible for complete syntax validation. Decode UTF-8
    explicitly so alternate encodings cannot bypass this pre-parser.
    """
    document = raw.decode("utf-8")
    depth, quoted, escaped = 0, False, False
    for character in document:
        if quoted:
            if escaped:
                escaped = False
            elif character == "\\":
                escaped = True
            elif character == '"':
                quoted = False
        elif character == '"':
            quoted = True
        elif character in "[{":
            depth += 1
            if depth > MAX_JSON_DEPTH:
                raise ValueError("JSON nesting exceeds configured limit")
        elif character in "]}":
            depth -= 1
            if depth < 0:
                raise ValueError("invalid JSON nesting")
    return json.loads(document)


class SafeRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, request, fp, code, msg, headers, newurl):
        try:
            parsed = urllib.parse.urlsplit(newurl)
        except ValueError:
            raise urllib.error.URLError("unsafe redirect") from None
        if parsed.scheme != "https" or parsed.username or parsed.password:
            raise urllib.error.URLError("unsafe redirect")
        redirected = super().redirect_request(request, fp, code, msg, headers, newurl)
        if redirected is not None:
            # urllib normally carries Authorization across redirects. Never forward it.
            redirected.remove_header("Authorization")
        return redirected


class Client:
    def __init__(self, *, timeout=20, deadline_seconds=300, token=None, opener=None,
                 clock=time.monotonic):
        self.timeout = timeout
        self.clock = clock
        self.deadline = clock() + deadline_seconds
        self.token = token
        self.open = opener or urllib.request.build_opener(SafeRedirect()).open

    def remaining(self):
        return max(0, self.deadline - self.clock())

    def fetch(self, url, *, size=None, digest=None):
        """Only safe reasons leave this boundary, never exception text or response bodies."""
        if self.remaining() <= 0:
            return Result(Status.UNAVAILABLE, "verification deadline exceeded")
        try:
            parsed = urllib.parse.urlsplit(url)
        except ValueError:
            return invalid("unsupported resource URL")
        if parsed.scheme != "https" or parsed.username or parsed.password:
            return invalid("unsupported resource URL")
        headers = {"User-Agent": USER_AGENT, "Accept": "application/json" if size is None else "application/octet-stream"}
        if self.token and parsed.hostname == "api.github.com":
            headers["Authorization"] = "Bearer " + self.token
        request = urllib.request.Request(url, headers=headers)
        try:
            with self.open(request, timeout=min(self.timeout, self.remaining())) as response:
                if response.status != 200:
                    return Result(Status.UNAVAILABLE, "unexpected HTTP status")
                limit = MAX_JSON_BYTES if size is None else min(size, MAX_ASSET_BYTES)
                chunks, received, checksum = [], 0, hashlib.sha256()
                while True:
                    if self.remaining() <= 0:
                        return Result(Status.UNAVAILABLE, "verification deadline exceeded")
                    # Return available bytes so drip-fed bodies cannot defer the deadline
                    # check until a large read() buffer is full.
                    chunk = response.read1(min(65536, limit - received + 1))
                    if not chunk:
                        break
                    received += len(chunk)
                    if received > limit:
                        return invalid("response exceeds declared size or configured byte limit")
                    if size is None:
                        chunks.append(chunk)
                    else:
                        checksum.update(chunk)
                if size is not None:
                    if received != size:
                        return invalid("archive byte size does not match release metadata")
                    if digest is not None and checksum.hexdigest() != digest.lower():
                        return invalid("archive SHA-256 does not match release metadata")
                    return Result(Status.OK, "archive size and SHA-256 verified" if digest else
                                  "archive size verified; GitHub SHA-256 unavailable")
                data = bounded_json(b"".join(chunks))
                if not isinstance(data, dict):
                    return invalid("JSON root must be an object")
                return Result(Status.OK, "metadata received", data)
        except urllib.error.HTTPError as error:
            error.close()
            if error.code == 404:
                return Result(Status.MISSING, "HTTP 404")
            return Result(Status.UNAVAILABLE, f"HTTP {error.code}; publication state unknown")
        except (urllib.error.URLError, TimeoutError, OSError, http.client.HTTPException):
            return Result(Status.UNAVAILABLE, "network failure; publication state unknown")
        except (ValueError, UnicodeError, RecursionError):
            return invalid("invalid JSON or transport response")


def with_retries(check, label, client, *, attempts=6, backoff=20, sleep=time.sleep, emit=print):
    for attempt in range(attempts):
        result = check()
        if result.status in (Status.OK, Status.INVALID):
            return result
        if attempt + 1 == attempts or client.remaining() <= backoff:
            return result
        emit(f"  RETRY {label}: {result.status.value} ({attempt + 1}/{attempts})")
        sleep(backoff)
    raise AssertionError("positive attempts required")


def positive_integer(value):
    return type(value) is int and value > 0


def wheel_platforms(filename, version):
    if not isinstance(filename, str) or not filename.endswith(".whl"):
        return set()
    parts = filename[:-4].split("-")
    if len(parts) not in (5, 6) or parts[0] != "sandhi_gateway" or parts[1] != version:
        return set()
    if any(not re.fullmatch(r"[A-Za-z0-9_]+(?:\.[A-Za-z0-9_]+)*", part) for part in parts[-3:]):
        return set()
    if len(parts) == 6 and not re.fullmatch(r"[0-9][A-Za-z0-9_]*", parts[2]):
        return set()
    found = set()
    for tag in parts[-1].split("."):
        if re.fullmatch(r"(?:manylinux[0-9_]+|linux)_x86_64", tag):
            found.add("linux-x86_64")
        if re.fullmatch(r"macosx_[0-9]+_[0-9]+_(?:arm64|universal2)", tag):
            found.add("macos-arm64")
        if tag == "win_amd64":
            found.add("windows-amd64")
    return found


def check_pypi(client, version, platforms=PYPI_PLATFORMS):
    fetched = client.fetch(f"https://pypi.org/pypi/{PYPI_PACKAGE}/{version}/json")
    if fetched.status != Status.OK:
        return fetched
    data = fetched.data
    info, files = data.get("info"), data.get("urls")
    if not isinstance(info, dict) or info.get("name") != PYPI_PACKAGE or info.get("version") != version or not isinstance(files, list):
        return invalid("PyPI version metadata missing or mismatched")
    if not files:
        return Result(Status.MISSING, "PyPI release has no files")
    available, usable = set(), 0
    for file in files:
        if not isinstance(file, dict) or type(file.get("yanked")) is not bool:
            return invalid("PyPI file metadata is malformed")
        if file["yanked"]:
            continue
        if not positive_integer(file.get("size")):
            return invalid("PyPI file is empty or has invalid size")
        usable += 1
        if file.get("packagetype") == "bdist_wheel":
            available |= wheel_platforms(file.get("filename"), version)
    if not usable:
        return invalid("all PyPI files are yanked")
    missing = set(platforms) - available
    if missing:
        return Result(Status.MISSING, "PyPI wheel platforms absent: " + ",".join(sorted(missing)))
    return Result(Status.OK, "non-yanked, nonempty wheels cover expected platforms (metadata)")


def check_crate(client, crate, version):
    fetched = client.fetch(f"https://crates.io/api/v1/crates/{crate}/{version}")
    if fetched.status != Status.OK:
        return fetched
    record = fetched.data.get("version")
    if not isinstance(record, dict) or record.get("crate") != crate or record.get("num") != version:
        return invalid("crate version metadata missing or mismatched")
    if record.get("yanked") is not False:
        return invalid("crate is yanked or lacks a yanked-state assertion")
    return Result(Status.OK, "exact crate version is not yanked")


def npm_metadata(client, package, version):
    quoted = urllib.parse.quote(package, safe="@")
    fetched = client.fetch(f"https://registry.npmjs.org/{quoted}/{version}")
    if fetched.status != Status.OK:
        return fetched
    data = fetched.data
    if data.get("name") != package or data.get("version") != version:
        return invalid("npm version metadata missing or mismatched")
    dist = data.get("dist")
    if not isinstance(dist, dict) or not isinstance(dist.get("tarball"), str):
        return invalid("npm artifact tarball metadata absent")
    try:
        url = urllib.parse.urlsplit(dist["tarball"])
    except ValueError:
        return invalid("npm artifact tarball URL is invalid")
    if url.scheme != "https" or url.hostname != "registry.npmjs.org" or url.username or url.password:
        return invalid("npm artifact tarball URL is invalid")
    if "unpackedSize" in dist and not positive_integer(dist["unpackedSize"]):
        return invalid("npm package reports an empty or invalid unpacked size")
    return fetched


def check_npm(client, version, platforms=NPM_PLATFORMS):
    fetched = npm_metadata(client, NPM_PACKAGE, version)
    if fetched.status != Status.OK:
        return fetched
    dependencies = fetched.data.get("optionalDependencies")
    if not isinstance(dependencies, dict):
        return invalid("npm main package lacks optionalDependencies; legacy structural defect, not registry absence")
    for platform in platforms:
        package = NPM_PACKAGE + "-" + platform
        if dependencies.get(package) != version:
            return invalid("npm expected platform optionalDependency is missing or not the exact release version")
        result = npm_metadata(client, package, version)
        if result.status != Status.OK:
            return Result(result.status, platform + ": " + result.reason)
        data = result.data
        operating_system, architecture = ("linux", "x64") if platform == "linux-x64-gnu" else ("darwin", "arm64")
        if data.get("os") != [operating_system] or data.get("cpu") != [architecture]:
            return invalid("npm platform package OS/CPU declarations mismatch")
        if platform == "linux-x64-gnu" and "libc" in data and data["libc"] != ["glibc"]:
            return invalid("npm Linux GNU package declares an incompatible libc")
    return Result(Status.OK, "main and expected platform versions/dependencies verified (metadata)")


def check_github(client, version, repo, targets=BINARY_TARGETS):
    tag = "v" + version
    fetched = client.fetch(f"https://api.github.com/repos/{repo}/releases/tags/{tag}")
    if fetched.status != Status.OK:
        return fetched
    data = fetched.data
    if data.get("tag_name") != tag or data.get("draft") is not False or data.get("prerelease") is not False:
        return invalid("GitHub release is not the expected published stable tag")
    assets = data.get("assets")
    if not isinstance(assets, list) or any(not isinstance(asset, dict) for asset in assets):
        return invalid("GitHub release asset list malformed")
    notes = []
    for target in targets:
        name = f"sandhi-proxy-{tag}-{target}.tar.gz"
        matches = [asset for asset in assets if asset.get("name") == name]
        if not matches:
            return Result(Status.MISSING, "GitHub expected archive absent: " + target)
        if len(matches) != 1:
            return invalid("duplicate GitHub archive name")
        asset = matches[0]
        size = asset.get("size")
        if asset.get("state") != "uploaded" or not positive_integer(size) or size > MAX_ASSET_BYTES:
            return invalid("GitHub archive not uploaded or size outside bounds")
        digest = asset.get("digest")
        if digest is not None:
            if not isinstance(digest, str) or not digest.startswith("sha256:") or not SHA256.fullmatch(digest[7:]):
                return invalid("GitHub archive digest is malformed or unsupported")
            digest = digest[7:]
        expected_url = f"https://github.com/{repo}/releases/download/{tag}/{name}"
        if asset.get("browser_download_url") != expected_url:
            return invalid("GitHub archive download URL mismatches the expected repository/tag/name")
        checked = client.fetch(expected_url, size=size, digest=digest)
        if checked.status != Status.OK:
            return Result(checked.status, target + ": " + checked.reason)
        notes.append(target + ": " + checked.reason)
    return Result(Status.OK, "; ".join(notes))


def parse_args(argv=None, environ=None):
    environ = os.environ if environ is None else environ
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("tag")
    parser.add_argument("--repo", default=environ.get("GITHUB_REPOSITORY", "anvai-labs/sandhi"))
    parser.add_argument("--targets", help="Expected targets; overrides legacy EXPECT_* flags")
    parser.add_argument("--pypi-platforms", default=",".join(PYPI_PLATFORMS))
    parser.add_argument("--npm-platforms", default=",".join(NPM_PLATFORMS))
    parser.add_argument("--binary-targets", default=",".join(BINARY_TARGETS))
    parser.add_argument("--attempts", type=int, default=6)
    parser.add_argument("--backoff-seconds", type=float, default=20)
    parser.add_argument("--timeout-seconds", type=float, default=20)
    parser.add_argument("--deadline-seconds", type=float, default=300)
    args = parser.parse_args(argv)
    try:
        args.version = stable_version(args.tag)
        if not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9-]{0,38}/[A-Za-z0-9_.-]{1,100}", args.repo) or args.repo.split("/")[-1] in {".", ".."}:
            raise ValueError("repo must be an owner/repository pair")
        args.targets = selection(args.targets, TARGETS, "targets") if args.targets is not None else tuple(
            target for target in TARGETS if env_flag("EXPECT_" + target.upper(), environ=environ))
        if not args.targets:
            raise ValueError("at least one target must be expected")
        args.pypi_platforms = selection(args.pypi_platforms, PYPI_PLATFORMS, "PyPI platforms")
        args.npm_platforms = selection(args.npm_platforms, NPM_PLATFORMS, "npm platforms")
        args.binary_targets = selection(args.binary_targets, BINARY_TARGETS, "binary targets")
        if not 1 <= args.attempts <= 10:
            raise ValueError("attempts must be between 1 and 10")
        for name, minimum, maximum in (("backoff_seconds", 0, 60), ("timeout_seconds", .1, 60),
                                       ("deadline_seconds", .1, 900)):
            value = getattr(args, name)
            if not math.isfinite(value) or not minimum <= value <= maximum:
                raise ValueError(f"{name} must be finite in {minimum}..{maximum}")
    except ValueError as error:
        parser.error(str(error))
    return args


def main(argv=None, *, environ=None, client=None, emit=print, sleep=time.sleep):
    environ = os.environ if environ is None else environ
    args = parse_args(argv, environ)
    client = client or Client(timeout=args.timeout_seconds, deadline_seconds=args.deadline_seconds,
                              token=environ.get("GH_TOKEN") or environ.get("GITHUB_TOKEN"))
    emit(f"Verifying stable release v{args.version}; expected targets: {','.join(args.targets)}")
    checks = {
        "pypi": [(PYPI_PACKAGE, lambda: check_pypi(client, args.version, args.pypi_platforms))],
        "crates": [(crate, lambda crate=crate: check_crate(client, crate, args.version)) for crate in CRATES],
        "npm": [(NPM_PACKAGE, lambda: check_npm(client, args.version, args.npm_platforms))],
        "github": [("GitHub archives", lambda: check_github(client, args.version, args.repo, args.binary_targets))],
    }
    failed = False
    for target in TARGETS:
        if target not in args.targets:
            emit(f"SKIP {target}: explicitly not expected; unverified")
            continue
        for label, check in checks[target]:
            result = with_retries(check, label, client, attempts=args.attempts,
                                  backoff=args.backoff_seconds, sleep=sleep, emit=emit)
            emit(f"{result.status.value} {label}: {result.reason}")
            failed |= result.status != Status.OK
    emit("RELEASE NOT VERIFIED; see distinct outcomes above" if failed else "All explicitly expected targets verified")
    return int(failed)


if __name__ == "__main__":
    sys.exit(main())
