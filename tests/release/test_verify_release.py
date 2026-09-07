"""Read-only verifier contract; every network response is synthetic and deterministic."""

import contextlib
import copy
import hashlib
import http.client
import importlib.util
import io
import json
from pathlib import Path
import sys
import unittest
from unittest.mock import patch
import urllib.error
import urllib.parse
import urllib.request

SPEC = importlib.util.spec_from_file_location(
    "verify_release", Path(__file__).resolve().parents[2] / "scripts" / "verify-release.py")
verify = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = verify
SPEC.loader.exec_module(verify)

VERSION = "0.5.1"
REPO = "anvai-labs/sandhi"
PYPI_URL = f"https://pypi.org/pypi/sandhi-gateway/{VERSION}/json"
GITHUB_URL = f"https://api.github.com/repos/{REPO}/releases/tags/v{VERSION}"


def npm_url(package=verify.NPM_PACKAGE):
    return f"https://registry.npmjs.org/{urllib.parse.quote(package, safe='@')}/{VERSION}"


class Response(io.BytesIO):
    status = 200


class Network:
    def __init__(self, routes):
        self.routes = routes
        self.requests = []

    def __call__(self, request, *, timeout):
        self.requests.append((request, timeout))
        response = self.routes[request.full_url]
        if isinstance(response, Exception):
            raise response
        return Response(response if isinstance(response, bytes) else json.dumps(response).encode())


def fixtures():
    routes = {PYPI_URL: {"info": {"name": verify.PYPI_PACKAGE, "version": VERSION}, "urls": [
        {"filename": f"sandhi_gateway-{VERSION}-cp311-abi3-{platform}.whl", "yanked": False,
         "size": 100, "packagetype": "bdist_wheel"} for platform in (
             "manylinux_2_17_x86_64.manylinux2014_x86_64", "macosx_11_0_arm64", "win_amd64")
    ]}}
    for crate in verify.CRATES:
        routes[f"https://crates.io/api/v1/crates/{crate}/{VERSION}"] = {
            "version": {"crate": crate, "num": VERSION, "yanked": False}}
    for suffix in ("", "-linux-x64-gnu", "-darwin-arm64"):
        package = verify.NPM_PACKAGE + suffix
        record = {"name": package, "version": VERSION, "dist": {
            "tarball": f"https://registry.npmjs.org/{package}/-/sandhi-{VERSION}.tgz", "unpackedSize": 100}}
        if not suffix:
            record["optionalDependencies"] = {verify.NPM_PACKAGE + "-" + platform: VERSION
                                               for platform in verify.NPM_PLATFORMS}
        else:
            record.update(os=["linux" if suffix.startswith("-linux") else "darwin"],
                          cpu=["x64" if suffix.startswith("-linux") else "arm64"])
        routes[npm_url(package)] = record
    assets = []
    for target in verify.BINARY_TARGETS:
        name = f"sandhi-proxy-v{VERSION}-{target}.tar.gz"
        url = f"https://github.com/{REPO}/releases/download/v{VERSION}/{name}"
        payload = ("synthetic archive " + target).encode()
        routes[url] = payload
        assets.append({"name": name, "state": "uploaded", "size": len(payload),
                       "digest": "sha256:" + hashlib.sha256(payload).hexdigest(),
                       "browser_download_url": url})
    routes[GITHUB_URL] = {"tag_name": "v" + VERSION, "draft": False,
                          "prerelease": False, "assets": assets}
    return routes


class VerifyReleaseTests(unittest.TestCase):
    def setUp(self):
        self.routes = fixtures()
        self.network = Network(self.routes)
        self.client = verify.Client(opener=self.network, token="synthetic-token-do-not-log")

    def test_strict_stable_versions(self):
        for value in ("0.0.0", "v0.5.1", "123.456.789"):
            with self.subTest(value=value):
                self.assertEqual(verify.stable_version(value), value.removeprefix("v"))
        for value in ("", "v01.2.3", "1.02.3", "1.2.03", "1.2", "1.2.3-rc1", "1.2.3+build",
                      " 1.2.3", "1.2.3\n", "vv1.2.3", "1.2.3/path", None, "1" * 65 + ".2.3"):
            with self.subTest(value=value), self.assertRaises(ValueError):
                verify.stable_version(value)

    def test_all_targets_default_independent_of_publish_credentials(self):
        for env in ({}, {"CARGO_REGISTRY_TOKEN": "", "NPM_TOKEN": ""}):
            self.assertEqual(verify.parse_args([VERSION], env).targets, verify.TARGETS)

    def test_legacy_explicit_overrides_and_cli_precedence(self):
        env = {"EXPECT_CRATES": "false", "EXPECT_NPM": "0", "GITHUB_REPOSITORY": "owner/repo"}
        args = verify.parse_args([VERSION], env)
        self.assertEqual(args.targets, ("pypi", "github"))
        self.assertEqual(args.repo, "owner/repo")
        args = verify.parse_args([VERSION, "--targets", "crates,npm", "--repo", REPO], env)
        self.assertEqual(args.targets, ("crates", "npm"))
        self.assertEqual(args.repo, REPO)

    def test_invalid_configuration_never_contacts_network(self):
        options = [("--targets", ""), ("--targets", "npm,npm"), ("--targets", "unknown"),
                   ("--repo", "https://example.com"), ("--repo", "owner/.."),
                   ("--attempts", "0"), ("--attempts", "11"), ("--backoff-seconds", "nan"),
                   ("--timeout-seconds", "inf"), ("--deadline-seconds", "0"),
                   ("--pypi-platforms", "any"), ("--npm-platforms", "windows"),
                   ("--binary-targets", "unknown")]
        for option in options:
            with self.subTest(option=option), contextlib.redirect_stderr(io.StringIO()), self.assertRaises(SystemExit) as error:
                verify.main([VERSION, *option], environ={}, client=self.client)
            self.assertEqual(error.exception.code, 2)
        for env in ({"EXPECT_NPM": "maybe"}, {"EXPECT_" + target.upper(): "0" for target in verify.TARGETS}):
            with contextlib.redirect_stderr(io.StringIO()), self.assertRaises(SystemExit) as error:
                verify.parse_args([VERSION], env)
            self.assertEqual(error.exception.code, 2)
        self.assertEqual(self.network.requests, [])

    def test_complete_release_and_credential_isolation(self):
        output = []
        self.assertEqual(verify.main([VERSION, "--attempts", "1"], environ={}, client=self.client, emit=output.append), 0)
        self.assertEqual(len(self.network.requests), 11)
        for request, timeout in self.network.requests:
            self.assertGreater(timeout, 0)
            self.assertLessEqual(timeout, 20)
            self.assertEqual(request.get_header("User-agent"), verify.USER_AGENT)
            expected = "Bearer synthetic-token-do-not-log" if request.host == "api.github.com" else None
            self.assertEqual(request.get_header("Authorization"), expected)
        self.assertNotIn("synthetic-token-do-not-log", "\n".join(output))

    def test_http_missing_is_distinct_from_unavailable(self):
        for code in (404, 401, 403, 429, 500, 503):
            with self.subTest(code=code):
                self.routes[PYPI_URL] = urllib.error.HTTPError(PYPI_URL, code, "sensitive-body", {}, None)
                result = self.client.fetch(PYPI_URL)
                self.assertEqual(result.status, verify.Status.MISSING if code == 404 else verify.Status.UNAVAILABLE)
                self.assertNotIn("sensitive-body", result.reason)

    def test_malformed_json_and_transport_errors_are_safe(self):
        cases = [(b"not-json-sensitive", verify.Status.INVALID), (b"[]", verify.Status.INVALID),
                 (b"null", verify.Status.INVALID), (b"\xff", verify.Status.INVALID),
                 (urllib.error.URLError("sensitive-body"), verify.Status.UNAVAILABLE),
                 (TimeoutError("sensitive-body"), verify.Status.UNAVAILABLE),
                 (http.client.IncompleteRead(b"sensitive-body"), verify.Status.UNAVAILABLE),
                 (http.client.BadStatusLine("sensitive-body"), verify.Status.UNAVAILABLE)]
        for value, status in cases:
            with self.subTest(value=type(value).__name__):
                self.routes[PYPI_URL] = value
                result = self.client.fetch(PYPI_URL)
                self.assertEqual(result.status, status)
                self.assertNotIn("sensitive-body", result.reason)
        self.assertEqual(self.client.fetch("https://[broken").status, verify.Status.INVALID)

    def test_http_error_response_is_closed(self):
        body = io.BytesIO(b"private response")
        self.routes[PYPI_URL] = urllib.error.HTTPError(PYPI_URL, 503, "private", {}, body)
        self.assertEqual(self.client.fetch(PYPI_URL).status, verify.Status.UNAVAILABLE)
        self.assertTrue(body.closed)

    def test_unexpected_success_status_and_deep_json_fail_closed(self):
        class UnexpectedResponse(Response):
            status = 206
        client = verify.Client(opener=lambda *a, **k: UnexpectedResponse(b"{}"))
        self.assertEqual(client.fetch(PYPI_URL).status, verify.Status.UNAVAILABLE)
        self.routes[PYPI_URL] = b'{"x":' * 2000 + b'0' + b'}' * 2000
        self.assertEqual(self.client.fetch(PYPI_URL).status, verify.Status.INVALID)

    def test_malformed_registry_records_fail_closed(self):
        checks = ((PYPI_URL, lambda: verify.check_pypi(self.client, VERSION)),
                  (npm_url(), lambda: verify.check_npm(self.client, VERSION)),
                  (GITHUB_URL, lambda: verify.check_github(self.client, VERSION, REPO)),
                  (f"https://crates.io/api/v1/crates/sandhi-core/{VERSION}",
                   lambda: verify.check_crate(self.client, "sandhi-core", VERSION)))
        for url, check in checks:
            for record in ({}, {"info": [], "urls": {}}, {"version": []}):
                with self.subTest(url=url, record=record):
                    self.routes[url] = record
                    self.assertEqual(check().status, verify.Status.INVALID)

    def test_redirects_strip_authorization_and_reject_unsafe_urls(self):
        request = urllib.request.Request(GITHUB_URL, headers={"Authorization": "Bearer secret"})
        redirector = verify.SafeRedirect()
        for destination in ("https://objects.githubusercontent.com/asset", "https://api.github.com/redirect"):
            redirected = redirector.redirect_request(request, None, 302, "", {}, destination)
            self.assertIsNone(redirected.get_header("Authorization"))
        for destination in ("http://example.com", "file:///etc/passwd", "https://user:pass@example.com", "https://[broken"):
            with self.subTest(destination=destination), self.assertRaises(urllib.error.URLError):
                redirector.redirect_request(request, None, 302, "", {}, destination)

    def test_retries_are_bounded_and_invalid_is_terminal(self):
        for status, calls_expected in ((verify.Status.MISSING, 3), (verify.Status.UNAVAILABLE, 3), (verify.Status.INVALID, 1)):
            calls, sleeps = [], []
            def check():
                calls.append(1)
                return verify.Result(status, "safe")
            result = verify.with_retries(check, "fixture", self.client, attempts=3, backoff=1,
                                         sleep=sleeps.append, emit=lambda _: None)
            self.assertEqual(result.status, status)
            self.assertEqual(len(calls), calls_expected)
            self.assertEqual(sleeps, [1] * (calls_expected - 1))

    def test_retry_recovers_but_does_not_sleep_past_deadline(self):
        results = iter([verify.Result(verify.Status.MISSING, "absent"), verify.Result(verify.Status.OK, "ready")])
        sleeps = []
        result = verify.with_retries(lambda: next(results), "fixture", self.client, attempts=3,
                                     backoff=1, sleep=sleeps.append, emit=lambda _: None)
        self.assertEqual(result.status, verify.Status.OK)
        self.assertEqual(sleeps, [1])
        client = verify.Client(opener=self.network, deadline_seconds=1, clock=lambda: 0)
        verify.with_retries(lambda: verify.Result(verify.Status.UNAVAILABLE, "busy"), "fixture", client,
                            backoff=2, sleep=lambda _: self.fail("slept beyond deadline"))

    def test_deadline_and_byte_limits(self):
        now = [0]
        client = verify.Client(opener=self.network, deadline_seconds=1, clock=lambda: now[0])
        now[0] = 2
        self.assertEqual(client.fetch(PYPI_URL).status, verify.Status.UNAVAILABLE)
        self.assertEqual(self.network.requests, [])
        with patch.object(verify, "MAX_JSON_BYTES", 2):
            self.assertEqual(self.client.fetch(PYPI_URL).status, verify.Status.INVALID)
        for size in (1, 200):
            self.routes[PYPI_URL] = b"123"
            self.assertEqual(self.client.fetch(PYPI_URL, size=size).status, verify.Status.INVALID)

    def test_streaming_obeys_shared_deadline(self):
        now = [0]
        class DripResponse(Response):
            def read1(self, size):
                now[0] += 1
                return b"x"
        client = verify.Client(opener=lambda *a, **k: DripResponse(), deadline_seconds=2, clock=lambda: now[0])
        self.assertEqual(client.fetch(PYPI_URL).status, verify.Status.UNAVAILABLE)

    def test_pypi_empty_yanked_missing_platform_and_invalid_metadata(self):
        original = copy.deepcopy(self.routes[PYPI_URL])
        cases = []
        for mode, status in (("empty", verify.Status.MISSING), ("yanked", verify.Status.INVALID),
                             ("zero", verify.Status.INVALID), ("platform", verify.Status.MISSING),
                             ("sdist", verify.Status.MISSING), ("version", verify.Status.INVALID),
                             ("unknown_yank", verify.Status.INVALID)):
            record = copy.deepcopy(original)
            if mode == "empty": record["urls"] = []
            elif mode == "yanked":
                for file in record["urls"]: file["yanked"] = True
            elif mode == "zero": record["urls"][0]["size"] = 0
            elif mode == "platform": record["urls"].pop()
            elif mode == "sdist":
                for file in record["urls"]: file["packagetype"] = "sdist"
            elif mode == "version": record["info"]["version"] = "0.0.0"
            elif mode == "unknown_yank": del record["urls"][0]["yanked"]
            cases.append((mode, record, status))
        for mode, record, status in cases:
            with self.subTest(mode=mode):
                self.routes[PYPI_URL] = record
                self.assertEqual(verify.check_pypi(self.client, VERSION).status, status)

    def test_wheel_platform_matching_is_package_and_version_scoped(self):
        self.assertEqual(verify.wheel_platforms(f"sandhi_gateway-{VERSION}-cp311-abi3-macosx_11_0_universal2.whl", VERSION), {"macos-arm64"})
        for filename in (f"other-{VERSION}-cp311-abi3-win_amd64.whl", "sandhi_gateway-0.0.0-cp311-abi3-win_amd64.whl",
                         f"sandhi_gateway-{VERSION}-py3-none-any.whl", "invalid", f"sandhi_gateway-{VERSION}---win_amd64.whl"):
            with self.subTest(filename=filename):
                self.assertEqual(verify.wheel_platforms(filename, VERSION), set())

    def test_all_four_crates_require_exact_non_yanked_version(self):
        for crate in verify.CRATES:
            url = f"https://crates.io/api/v1/crates/{crate}/{VERSION}"
            self.assertEqual(verify.check_crate(self.client, crate, VERSION).status, verify.Status.OK)
            for value in (True, None, "false"):
                self.routes[url]["version"]["yanked"] = value
                self.assertEqual(verify.check_crate(self.client, crate, VERSION).status, verify.Status.INVALID)
            self.routes[url]["version"] = {"crate": "other", "num": VERSION, "yanked": False}
            self.assertEqual(verify.check_crate(self.client, crate, VERSION).status, verify.Status.INVALID)

    def test_legacy_npm_root_without_dependencies_is_invalid_not_missing(self):
        del self.routes[npm_url()]["optionalDependencies"]
        result = verify.check_npm(self.client, VERSION)
        self.assertEqual(result.status, verify.Status.INVALID)
        self.assertIn("legacy structural defect", result.reason)

    def test_npm_platform_dependency_identity_and_tarball_checks(self):
        platform = verify.NPM_PACKAGE + "-linux-x64-gnu"
        for mode in ("range", "dependency", "version", "cpu", "os", "libc", "empty", "tarball", "malformed_url"):
            with self.subTest(mode=mode):
                self.routes.clear()
                self.routes.update(fixtures())
                main, child = self.routes[npm_url()], self.routes[npm_url(platform)]
                if mode == "range": main["optionalDependencies"][platform] = "^" + VERSION
                elif mode == "dependency": del main["optionalDependencies"][platform]
                elif mode == "version": child["version"] = "0.0.0"
                elif mode == "cpu": child["cpu"] = ["arm64"]
                elif mode == "os": child["os"] = ["darwin"]
                elif mode == "libc": child["libc"] = ["musl"]
                elif mode == "empty": child["dist"]["unpackedSize"] = 0
                elif mode == "tarball": child["dist"]["tarball"] = "https://evil.example/package.tgz"
                elif mode == "malformed_url": child["dist"]["tarball"] = "https://[broken"
                self.assertEqual(verify.check_npm(self.client, VERSION).status, verify.Status.INVALID)

    def test_npm_platform_http_failure_preserves_classification(self):
        url = npm_url(verify.NPM_PACKAGE + "-darwin-arm64")
        for code, status in ((404, verify.Status.MISSING), (403, verify.Status.UNAVAILABLE), (503, verify.Status.UNAVAILABLE)):
            self.routes[url] = urllib.error.HTTPError(url, code, "private", {}, None)
            self.assertEqual(verify.check_npm(self.client, VERSION).status, status)

    def test_github_required_archives_sizes_digests_and_origin(self):
        for mode in ("missing", "zero", "oversize", "size", "digest", "digest_format", "url", "duplicate", "draft", "prerelease", "tag", "state"):
            with self.subTest(mode=mode):
                self.routes.clear()
                self.routes.update(fixtures())
                release = self.routes[GITHUB_URL]
                asset = release["assets"][0]
                if mode == "missing": release["assets"].pop()
                elif mode == "zero": asset["size"] = 0
                elif mode == "oversize": asset["size"] = verify.MAX_ASSET_BYTES + 1
                elif mode == "size": asset["size"] += 1
                elif mode == "digest": asset["digest"] = "sha256:" + "0" * 64
                elif mode == "digest_format": asset["digest"] = "sha256:bad"
                elif mode == "url": asset["browser_download_url"] = "https://evil.example/asset"
                elif mode == "duplicate": release["assets"].append(copy.deepcopy(asset))
                elif mode == "draft": release["draft"] = True
                elif mode == "prerelease": release["prerelease"] = True
                elif mode == "tag": release["tag_name"] = "v0.0.0"
                elif mode == "state": asset["state"] = "new"
                expected = verify.Status.MISSING if mode == "missing" else verify.Status.INVALID
                self.assertEqual(verify.check_github(self.client, VERSION, REPO).status, expected)

    def test_github_digest_unavailable_is_explicitly_disclosed(self):
        for asset in self.routes[GITHUB_URL]["assets"]:
            del asset["digest"]
        result = verify.check_github(self.client, VERSION, REPO)
        self.assertEqual(result.status, verify.Status.OK)
        self.assertIn("SHA-256 unavailable", result.reason)

    def test_failure_exit_codes_and_explicit_skips(self):
        for payload, outcome in ((urllib.error.HTTPError(PYPI_URL, 404, "", {}, None), "MISSING"),
                                 (urllib.error.URLError("private"), "UNAVAILABLE"), (b"[]", "INVALID")):
            self.routes[PYPI_URL] = payload
            output = []
            self.assertEqual(verify.main([VERSION, "--targets", "pypi", "--attempts", "1"],
                                          environ={}, client=self.client, emit=output.append), 1)
            self.assertTrue(any(line.startswith(outcome) for line in output))
            self.assertIn("SKIP crates: explicitly not expected; unverified", output)
            self.assertNotIn("All explicitly expected targets verified", output)

    def test_all_four_crates_still_checked_after_one_fails(self):
        missing = f"https://crates.io/api/v1/crates/{verify.CRATES[0]}/{VERSION}"
        self.routes[missing] = urllib.error.HTTPError(missing, 404, "absent", {}, None)
        output = []
        self.assertEqual(verify.main([VERSION, "--targets", "crates", "--attempts", "1"],
                                      environ={}, client=self.client, emit=output.append), 1)
        self.assertEqual(len(self.network.requests), 4)
        self.assertEqual(sum(line.startswith("OK sandhi-") for line in output), 3)


if __name__ == "__main__":
    unittest.main()
