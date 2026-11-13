"""Offline regression tests; no real login, keychain or network access."""

import base64
import json
import os
import sys
from pathlib import Path
import tempfile
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import unittest
from unittest.mock import patch

import subscription_lease as lease
import subscription_service as service


def jwt(exp):
    claims = (
        base64.urlsafe_b64encode(json.dumps({"exp": exp}).encode()).decode().rstrip("=")
    )
    return "header." + claims + ".signature"


class LeaseTests(unittest.TestCase):
    def test_only_omission_selects_noauth_while_valid_file_preserves_credential(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "admin.json"
            path.write_text(json.dumps({"admin_token": "synthetic-admin-" + "x" * 32}))
            path.chmod(0o600)
            for configured in (False, True):
                with self.subTest(configured=configured):
                    args = ["lease", "--account", "synthetic"]
                    if configured:
                        args.extend(["--admin-file", str(path)])
                    with patch.object(sys, "argv", args), patch.object(lease, "publish") as publish:
                        self.assertEqual(lease.main(), 0)
                        client = publish.call_args.args[0]
                        self.assertEqual(client.token, "synthetic-admin-" + "x" * 32 if configured else None)

    def test_configured_admin_file_is_validated_before_publication(self):
        for value in ({}, {"admin_token": None}, {"admin_token": 123},
                      {"admin_token": ""}, {"admin_token": "short"},
                      {"admin_token": " " * 48}):
            with self.subTest(value=value), tempfile.TemporaryDirectory() as tmp:
                path = Path(tmp) / "admin.json"
                path.write_text(json.dumps(value))
                path.chmod(0o600)
                with patch.object(sys, "argv", ["lease", "--admin-file", str(path),
                                                "--account", "synthetic"]), \
                        patch.object(lease, "publish") as publish:
                    self.assertEqual(lease.main(), 1)
                    publish.assert_not_called()

    def test_quickstart_bootstrap_refuses_cross_origin_redirect(self):
        source = (Path(__file__).with_name("quickstart.sh")).read_text()
        bootstrap = source.split("<<'PYBOOT'\n", 1)[1].split("\nPYBOOT", 1)[0]
        received = []
        class Sink(BaseHTTPRequestHandler):
            def log_message(self, *_args):
                pass
            def do_GET(self):
                received.append(self.headers.get("Authorization"))
                body = b'{"package_version":"0.18.0","mode":"none","keys":[]}'
                self.send_response(200)
                self.end_headers()
                self.wfile.write(body)
        sink = ThreadingHTTPServer(("127.0.0.1", 0), Sink)
        class Redirect(BaseHTTPRequestHandler):
            def log_message(self, *_args):
                pass
            def do_GET(self):
                self.send_response(302)
                self.send_header("Location", f"http://127.0.0.1:{sink.server_port}/capture")
                self.end_headers()
        origin = ThreadingHTTPServer(("127.0.0.1", 0), Redirect)
        threads = [threading.Thread(target=server.serve_forever, daemon=True)
                   for server in (origin, sink)]
        for thread in threads:
            thread.start()
        try:
            with tempfile.TemporaryDirectory() as tmp:
                providers = Path(tmp) / "providers.json"
                providers.write_text("[]")
                arguments = ["bootstrap", str(providers), f"127.0.0.1:{origin.server_port}",
                             "none", "0.18.0"]
                with patch.object(sys, "argv", arguments), patch.dict(
                    os.environ, {"SANDHI_ADMIN_TOKEN": "synthetic-review-token-" + "x" * 32},
                    clear=True,
                ):
                    with self.assertRaisesRegex(SystemExit, "Gateway bootstrap failed"):
                        exec(compile(bootstrap, "quickstart-bootstrap", "exec"), {})
                self.assertEqual(received, [])
        finally:
            for server in (origin, sink):
                server.shutdown()
                server.server_close()
            for thread in threads:
                thread.join(timeout=5)

    def test_expiring_access_only_and_account_binding(self):
        auth = {
            "tokens": {
                "access_token": jwt(5000),
                "refresh_token": "NEVER-PUBLISH",
                "account_id": "a",
            }
        }
        result = lease.make_lease(auth, "a", now=1000)
        self.assertEqual(result["expires_at"], 1300)
        self.assertEqual(set(result), {"access_token", "account_id", "expires_at"})
        self.assertNotIn("NEVER-PUBLISH", json.dumps(result))
        with self.assertRaises(ValueError):
            lease.make_lease(auth, "different-account", now=1000)

    def test_expired_and_api_key_auth_rejected(self):
        for auth in (
            {"OPENAI_API_KEY": "secret"},
            {"tokens": {"access_token": jwt(1010), "account_id": "a"}},
        ):
            with self.assertRaises(ValueError):
                lease.make_lease(auth, "a", now=1000)

    def test_private_regular_file_and_no_symlink(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "auth.json"
            path.write_text("{}")
            path.chmod(0o600)
            self.assertEqual(lease.read_private_json(path), {})
            path.chmod(0o644)
            with self.assertRaises(ValueError):
                lease.read_private_json(path)
            link = Path(tmp) / "link"
            link.symlink_to(path)
            with self.assertRaises((OSError, ValueError)):
                lease.read_private_json(link)

    def test_gateway_rejects_remote_cleartext_and_url_credentials(self):
        for url in (
            "http://remote.example",
            "https://user:pass@example.com",
            "https://example.com?token=x",
        ):
            with self.assertRaises(ValueError):
                lease.validate_gateway(url)
        lease.validate_gateway("http://127.0.0.1:18789")

    def test_optional_operator_credential_is_not_an_empty_configured_credential(self):
        lease.AdminClient("http://127.0.0.1:18789")
        for value in ("", "short", 1):
            with self.subTest(value=value), self.assertRaises(ValueError):
                lease.AdminClient("http://127.0.0.1:18789", value)

    def test_noauth_operator_wire_has_no_header_and_protected_failure_does_not_retry(self):
        requests = []
        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *_args):
                pass
            def do_POST(self):
                requests.append(self.headers.get("Authorization"))
                payload = b'{}'
                self.send_response(401 if self.path == "/protected" else 200)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(payload)))
                self.end_headers()
                self.wfile.write(payload)
        server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        try:
            client = lease.AdminClient(f"http://127.0.0.1:{server.server_port}")
            self.assertEqual(client.post("/open", {}), {})
            with self.assertRaisesRegex(RuntimeError, "^gateway request failed$"):
                client.post("/protected", {})
            self.assertEqual(requests, [None, None])
        finally:
            server.shutdown()
            server.server_close()
            thread.join(timeout=5)

    def test_service_modes_are_optional_without_fallback_from_configured_auth(self):
        self.assertEqual(service.auth_mode({}), "none")
        self.assertEqual(service.auth_mode({"oidc_config": "/private/oidc.json"}), "oidc")
        self.assertEqual(service.auth_mode({"auth_mode": "tokens"}), "tokens")
        for config in ({"auth_mode": ""}, {"auth_mode": "bad"},
                       {"auth_mode": "oidc"}, {"oidc_config": ""},
                       {"auth_mode": "none", "oidc_config": "/private/oidc.json"}):
            with self.subTest(config=config), self.assertRaises(ValueError):
                service.auth_mode(config)

    def test_service_client_cannot_erase_declared_credentials_into_noauth(self):
        config = {"victor_worktree": "/fixture/victor"}
        env = service.client_environment({"gateway": {"url": "http://localhost:8787"}}, config,
                                         {"PATH": "/bin", "SANDHI_GATEWAY_VIRTUAL_KEY_OPENAI": "stale"})
        self.assertNotIn("SANDHI_GATEWAY_VIRTUAL_KEY_OPENAI", env)
        self.assertEqual(env["SANDHI_GATEWAY_URL"], "http://localhost:8787")
        invalid = [{"virtual_key": ""}, {"virtual_key": None}, {"api_key": "misspelled"},
                   {"auth_mode": "tokens"}, {"auth_mode": None},
                   {"auth_mode": "none", "virtual_key": "configured"}]
        for supplied in invalid:
            with self.subTest(supplied=supplied), self.assertRaises(ValueError):
                service.client_environment({"gateway": {"url": "http://localhost:8787", **supplied}}, config, {})
        protected = service.client_environment(
            {"gateway": {"url": "http://localhost:8787", "virtual_key": "configured"}}, config, {})
        self.assertEqual(protected["SANDHI_GATEWAY_VIRTUAL_KEY_OPENAI"], "configured")

    def test_publisher_never_follows_redirect_or_uses_proxy_env(self):
        client = lease.AdminClient("http://127.0.0.1:18789", "x" * 48)
        with patch.object(
            client.opener, "open", side_effect=OSError("secret transport detail")
        ):
            with self.assertRaisesRegex(RuntimeError, "^gateway request failed$"):
                client.post("/admin/keys", {"secret": "do-not-log"})


if __name__ == "__main__":
    unittest.main()
