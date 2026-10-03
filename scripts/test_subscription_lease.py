"""Offline regression tests; no real login, keychain or network access."""

import base64
import json
import os
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

import subscription_lease as lease


def jwt(exp):
    claims = (
        base64.urlsafe_b64encode(json.dumps({"exp": exp}).encode()).decode().rstrip("=")
    )
    return "header." + claims + ".signature"


class LeaseTests(unittest.TestCase):
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

    def test_publisher_never_follows_redirect_or_uses_proxy_env(self):
        client = lease.AdminClient("http://127.0.0.1:18789", "x" * 48)
        with patch.object(
            client.opener, "open", side_effect=OSError("secret transport detail")
        ):
            with self.assertRaisesRegex(RuntimeError, "^gateway request failed$"):
                client.post("/admin/keys", {"secret": "do-not-log"})


if __name__ == "__main__":
    unittest.main()
