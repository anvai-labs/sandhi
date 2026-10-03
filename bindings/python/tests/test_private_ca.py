"""Real TLS handshakes in isolated processes; no OS trust-store changes."""

import json
import os
from pathlib import Path
import ssl
import subprocess
import sys
import threading
from http.server import BaseHTTPRequestHandler, HTTPServer

import pytest

PROBE = r"""
import asyncio,json,sys
import sandhi_gateway as sg
async def run():
    handle=sg.ProviderRuntime().provider('inferflux','fixture','synthetic-invalid',base_url=sys.argv[1],max_retries=0,timeout_secs=3)
    try:
        await handle.complete_json(json.dumps({'model':'fixture','messages':[{'role':'user','content':[{'type':'text','text':'synthetic'}]}]}))
    except Exception as e:
        print(json.loads(str(e))['code'])
asyncio.run(run())
"""


@pytest.fixture
def tls_gateway(tmp_path):
    cert, key = tmp_path / "cert.pem", tmp_path / "key.pem"
    subprocess.run(
        [
            "openssl",
            "req",
            "-x509",
            "-newkey",
            "rsa:2048",
            "-nodes",
            "-keyout",
            str(key),
            "-out",
            str(cert),
            "-days",
            "1",
            "-subj",
            "/CN=localhost",
            "-addext",
            "subjectAltName=DNS:localhost",
            "-addext",
            "basicConstraints=critical,CA:TRUE",
        ],
        check=True,
        capture_output=True,
    )
    ca = tmp_path / "ca.pem"
    ca.write_bytes(cert.read_bytes())
    ca_key = tmp_path / "ca-key.pem"
    ca_key.write_bytes(key.read_bytes())
    csr = tmp_path / "leaf.csr"
    ext = tmp_path / "leaf.ext"
    ext.write_text(
        "subjectAltName=DNS:localhost\nbasicConstraints=critical,CA:FALSE\nkeyUsage=critical,digitalSignature,keyEncipherment\nextendedKeyUsage=serverAuth\n"
    )
    subprocess.run(
        [
            "openssl",
            "req",
            "-new",
            "-newkey",
            "rsa:2048",
            "-nodes",
            "-keyout",
            str(key),
            "-out",
            str(csr),
            "-subj",
            "/CN=localhost",
        ],
        check=True,
        capture_output=True,
    )
    subprocess.run(
        [
            "openssl",
            "x509",
            "-req",
            "-in",
            str(csr),
            "-CA",
            str(ca),
            "-CAkey",
            str(ca_key),
            "-CAcreateserial",
            "-out",
            str(cert),
            "-days",
            "1",
            "-extfile",
            str(ext),
        ],
        check=True,
        capture_output=True,
    )

    class Handler(BaseHTTPRequestHandler):
        def do_POST(self):
            self.rfile.read(int(self.headers.get("Content-Length", "0")))
            self.send_response(401)
            self.send_header("Content-Type", "application/json")
            self.end_headers()
            self.wfile.write(b'{"error":{"message":"synthetic invalid key"}}')

        def log_message(self, *args):
            pass

    server = HTTPServer(("127.0.0.1", 0), Handler)
    context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    context.load_cert_chain(cert, key)
    server.socket = context.wrap_socket(server.socket, server_side=True)
    worker = threading.Thread(target=server.serve_forever, daemon=True)
    worker.start()
    yield ca, server.server_port
    server.shutdown()
    server.server_close()
    worker.join(timeout=2)


@pytest.mark.parametrize(
    "trusted,host,expected",
    [
        (True, "localhost", "authentication_error"),
        (False, "localhost", "transport_error"),
        (True, "127.0.0.1", "transport_error"),
    ],
)
def test_private_ca_preserves_chain_and_hostname_checks(
    tls_gateway, tmp_path, trusted, host, expected
):
    cert, port = tls_gateway
    env = {
        k: v
        for k, v in os.environ.items()
        if k.upper()
        not in {
            "HTTPS_PROXY",
            "HTTP_PROXY",
            "ALL_PROXY",
            "SSL_CERT_FILE",
            "SSL_CERT_DIR",
        }
    }
    if trusted:
        env["SSL_CERT_FILE"] = str(cert)
    env["NO_PROXY"] = "localhost,127.0.0.1"
    result = subprocess.run(
        [sys.executable, "-c", PROBE, f"https://{host}:{port}/v1"],
        env=env,
        capture_output=True,
        text=True,
        timeout=8,
    )
    assert result.returncode == 0, result.stderr
    assert result.stdout.strip() == expected
