"""Disposable HTTPS authority for real browser-to-proxy SSO journeys.

Only the identity provider is simulated. The shipped proxy owns discovery, JWT
verification, cookies, authorization, CSRF, dashboard rendering and accounting.
"""
import base64
import hashlib
import json
import secrets
import ssl
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import parse_qs, urlencode, urlsplit

import jwt
import pytest
from cryptography.hazmat.primitives.serialization import load_pem_private_key

from conftest import REPO_ROOT


@pytest.fixture
def oidc_authority():
    fixture = REPO_ROOT / "crates/sandhi-proxy/tests/fixtures/tls"
    key = load_pem_private_key((fixture / "localhost-key.pem").read_bytes(), password=None)
    jwk = json.loads(jwt.algorithms.RSAAlgorithm.to_jwk(key.public_key()))
    jwk.update(kid="browser-fixture", use="sig", alg="RS256")
    authority = type("Authority", (), {})()
    authority.subject = "admin"
    authority.lifetime = 300
    authority.codes = {}
    authority.exchanges = 0

    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *_):
            pass

        def send_json(self, value, status=200):
            body = json.dumps(value).encode()
            self.send_response(status)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def do_GET(self):
            path = urlsplit(self.path)
            if path.path == "/oidc/.well-known/openid-configuration":
                self.send_json({
                    "issuer": authority.issuer,
                    "authorization_endpoint": authority.base + "/authorize",
                    "token_endpoint": authority.base + "/token",
                    "jwks_uri": authority.base + "/jwks",
                    "introspection_endpoint": authority.base + "/introspect",
                    "introspection_endpoint_auth_methods_supported": ["none"],
                    "response_types_supported": ["code"],
                    "subject_types_supported": ["public"],
                    "id_token_signing_alg_values_supported": ["RS256"],
                    "code_challenge_methods_supported": ["S256"],
                    "token_endpoint_auth_methods_supported": ["none"],
                })
            elif path.path == "/jwks":
                self.send_json({"keys": [jwk]})
            elif path.path == "/authorize":
                query = {k: v[0] for k, v in parse_qs(path.query).items()}
                assert query["client_id"] == "sandhi-browser"
                assert query["code_challenge_method"] == "S256"
                assert query["redirect_uri"] == authority.redirect
                code = secrets.token_urlsafe(24)
                authority.codes[code] = query
                self.send_response(303)
                self.send_header("Location", authority.redirect + "?" + urlencode({
                    "code": code, "state": query["state"],
                }))
                self.end_headers()
            else:
                self.send_json({"error": "not found"}, 404)

        def do_POST(self):
            query = {k: v[0] for k, v in parse_qs(
                self.rfile.read(int(self.headers["Content-Length"])).decode()).items()}
            if self.path == "/token":
                authorization = authority.codes.pop(query.get("code"), None)
                challenge = base64.urlsafe_b64encode(hashlib.sha256(
                    query.get("code_verifier", "").encode()).digest()).rstrip(b"=").decode()
                if not authorization or challenge != authorization["code_challenge"]:
                    return self.send_json({"error": "invalid_grant"}, 400)
                assert query["redirect_uri"] == authority.redirect
                authority.exchanges += 1
                now = int(time.time())
                claims = {"iss": authority.issuer, "aud": "sandhi-browser",
                          "sub": authority.subject, "iat": now,
                          "exp": now + authority.lifetime, "nonce": authorization["nonce"]}
                self.send_json({"access_token": "fixture-access", "token_type": "Bearer",
                                "id_token": jwt.encode(claims, key, algorithm="RS256",
                                                       headers={"kid": "browser-fixture"})})
            elif self.path == "/introspect":
                self.send_json({"active": query.get("token") == "fixture-access",
                                "iss": authority.issuer, "aud": "sandhi-browser",
                                "sub": "admin", "exp": int(time.time()) + 300,
                                "token_type": "Bearer"})
            else:
                self.send_json({"error": "not found"}, 404)

    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    tls = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    tls.load_cert_chain(fixture / "localhost-cert.pem", fixture / "localhost-key.pem")
    server.socket = tls.wrap_socket(server.socket, server_side=True)
    authority.base = f"https://localhost:{server.server_port}"
    authority.issuer = authority.base + "/oidc"
    authority.certificate = fixture / "localhost-cert.pem"
    authority.key = fixture / "localhost-key.pem"
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        yield authority
    finally:
        server.shutdown()
        server.server_close()
        thread.join(timeout=5)
