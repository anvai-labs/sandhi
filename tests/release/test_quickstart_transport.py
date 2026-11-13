"""Drive the HTTP-only helper with owned listeners and synthetic credentials."""
import ipaddress
import json
import os
from pathlib import Path
import shutil
import socket
import subprocess
import tempfile
import threading
import tomllib
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

ROOT = Path(__file__).resolve().parents[2]
SCRIPT = Path(os.environ.get("SANDHI_QUICKSTART_TEST_SCRIPT", ROOT / "scripts/quickstart.sh"))
VERSION = tomllib.loads((ROOT / "Cargo.toml").read_text())["workspace"]["package"]["version"]


@unittest.skipUnless(os.name == "posix" and shutil.which("bash") and shutil.which("curl"),
                     "quickstart requires POSIX bash and curl")
class QuickstartTransportTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.root = Path(self.directory.name)
        self.repo, self.home = self.root / "repo", self.root / "home"
        (self.repo / "scripts").mkdir(parents=True)
        self.home.mkdir(mode=0o700)
        shutil.copyfile(SCRIPT, self.repo / "scripts/quickstart.sh")
        (self.repo / "Cargo.toml").write_text(f'[workspace.package]\nversion = "{VERSION}"\n')
        binaries = self.repo / "target/release"
        binaries.mkdir(parents=True)
        for name in ("sandhi", "sandhi-proxy"):
            binary = binaries / name
            binary.write_text(f'#!/bin/sh\nif [ "$1" = --version ]; then echo "{name} {VERSION}"; exit 0; fi\nprintf launched >> "$SANDHI_HOME/launches"\nexit 17\n')
            binary.chmod(0o700)
        (self.home / "providers.json").write_text("[]")
        self.requests = []
        self.provider_uploads = []
        self.servers = []

    def tearDown(self):
        for server, thread in self.servers:
            server.shutdown(); server.server_close(); thread.join(timeout=2)
        self.directory.cleanup()

    def listener(self, host="127.0.0.1", mode="tokens"):
        recorded = self.requests
        uploads = self.provider_uploads
        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *_): pass
            def do_GET(self):
                recorded.append((self.path, bool(self.headers.get("Authorization"))))
                data = {"package_version": VERSION} if self.path == "/version" else {"mode": mode} if self.path == "/auth/session" else {"keys": []}
                payload = json.dumps(data).encode()
                self.send_response(200); self.send_header("Content-Length", str(len(payload))); self.end_headers(); self.wfile.write(payload)
            def do_POST(self):
                recorded.append((self.path, bool(self.headers.get("Authorization"))))
                payload = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
                uploads.append(payload.get("secret") == "synthetic-upstream-proof")
                self.send_response(200); self.send_header("Content-Length", "2"); self.end_headers(); self.wfile.write(b"{}")
        class Server(ThreadingHTTPServer):
            address_family = socket.AF_INET6 if ":" in host else socket.AF_INET
        server = Server((host, 0), Handler)
        thread = threading.Thread(target=server.serve_forever, daemon=True); thread.start()
        self.servers.append((server, thread))
        return server.server_port

    def run_helper(self, bind, *, mode="tokens", flag="1", existing_pid=True, bind_file=False):
        (self.home / "proxy.pid").unlink(missing_ok=True)
        if existing_pid:
            (self.home / "proxy.pid").write_text(str(os.getpid()))  # Only kill -0; never stop this fixture.
        env = {"PATH": os.defpath, "HOME": str(self.root), "LANG": "C.UTF-8",
               "SANDHI_HOME": str(self.home), "SANDHI_AUTH_MODE": mode,
               "NO_PROXY": "*", "no_proxy": "*"}
        if bind_file:
            (self.home / "bind").write_text(bind)
        else:
            env["SANDHI_BIND"] = bind
        if flag is not None: env["SANDHI_ALLOW_LOOPBACK_HTTP"] = flag
        if mode == "tokens": env["SANDHI_ADMIN_TOKEN"] = "synthetic-operator-proof"
        return subprocess.run(["bash", str(self.repo / "scripts/quickstart.sh")],
                              env=env, capture_output=True, text=True, timeout=6)

    def lan_address(self):
        # Local interface metadata only; no network probe or guessed remote host.
        try: addresses = subprocess.check_output(["hostname", "-I"], text=True).split()
        except (OSError, subprocess.CalledProcessError): self.skipTest("no local interface inventory")
        for raw in addresses:
            address = ipaddress.ip_address(raw)
            if address.version == 4 and not address.is_loopback and not address.is_unspecified:
                return str(address)
        self.skipTest("no owned non-loopback IPv4 interface")

    def test_existing_listener_never_receives_lan_bearer_after_reuse_or_child_refusal(self):
        host = self.lan_address()
        port = self.listener("0.0.0.0")
        for existing_pid in (True, False):
            with self.subTest(existing_pid=existing_pid):
                self.requests.clear()
                result = self.run_helper(f"{host}:{port}", existing_pid=existing_pid)
                self.assertEqual(self.requests, [], "helper contacted an existing HTTP listener")
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("literal loopback", result.stderr + result.stdout)
                self.assertFalse((self.home / "launches").exists())

    def test_non_loopback_bind_shapes_refused_before_reusing_pid(self):
        host = self.lan_address()
        port = self.listener("0.0.0.0")
        for address in ("0.0.0.0", "localhost", host, "[::]", "[::ffff:127.0.0.1]", f"[::ffff:{host}]", "[::1%lo]"):
            with self.subTest(address=address):
                self.requests.clear()
                result = self.run_helper(f"{address}:{port}")
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("literal loopback", result.stderr + result.stdout)
                self.assertEqual(self.requests, [])

    def test_starter_cannot_upload_provider_secrets_to_lan(self):
        host = self.lan_address()
        port = self.listener("0.0.0.0", mode="none")
        (self.home / "providers.json").write_text(json.dumps([
            {"provider": "fixture", "base_url": "http://127.0.0.1:1", "secret": "synthetic-upstream-proof"}]))
        result = self.run_helper(f"{host}:{port}", mode="none", flag=None)
        self.assertEqual(self.requests, [])
        self.assertEqual(self.provider_uploads, [])
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("literal loopback", result.stderr + result.stdout)

    def test_invalid_socket_forms_and_ports_never_probe(self):
        port = self.listener()
        for bind in ("", "127.0.0.1", "127.0.0.1:0", "127.0.0.1:65536",
                     "127.0.0.1:abc", "127.0.0.1:١٢٣", "::1:8787", "[::1]",
                     f"synthetic-user:synthetic-pass@127.0.0.1:{port}"):
            with self.subTest(bind=bind):
                result = self.run_helper(bind)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("literal loopback", result.stderr + result.stdout)
                self.assertNotIn("synthetic-pass", result.stderr + result.stdout)
                self.assertEqual(self.requests, [])

    def test_cached_bind_file_is_validated_before_pid_reuse(self):
        port = self.listener()
        result = self.run_helper(f"0.0.0.0:{port}", bind_file=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("literal loopback", result.stderr + result.stdout)
        self.assertEqual(self.requests, [])

    def test_invalid_exception_never_uses_running_starter(self):
        port = self.listener(mode="none")
        for flag in ("", "true", "01", " 1", "2"):
            with self.subTest(flag=flag):
                result = self.run_helper(f"127.0.0.1:{port}", mode="none", flag=flag)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("SANDHI_ALLOW_LOOPBACK_HTTP", result.stderr + result.stdout)
                self.assertEqual(self.requests, [])

    def test_ipv4_loopback_range_and_credential_free_starter_remain_available(self):
        for host, mode, flag in (("127.0.0.1", "tokens", "1"), ("127.23.45.67", "tokens", "1"), ("127.0.0.1", "none", None)):
            with self.subTest(host=host, mode=mode):
                port = self.listener(host, mode)
                result = self.run_helper(f"{host}:{port}", mode=mode, flag=flag)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertTrue(self.requests)
                self.requests.clear()

    def test_ipv6_loopback_remains_available_when_supported(self):
        try: port = self.listener("::1")
        except OSError: self.skipTest("IPv6 loopback unavailable")
        result = self.run_helper(f"[::1]:{port}")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertTrue(self.requests)


if __name__ == "__main__": unittest.main()
