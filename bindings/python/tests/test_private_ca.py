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
    ca_file=sys.argv[2] if sys.argv[2] != '-' else None
    try:
        handle=sg.ProviderRuntime().provider(
            'inferflux','fixture','synthetic-invalid',
            base_url=sys.argv[1],max_retries=0,timeout_secs=3,ca_file=ca_file,
        )
    except ValueError:
        print('constructor_error')
        return
    try:
        await handle.complete_json(json.dumps({'model':'fixture','messages':[{'role':'user','content':[{'type':'text','text':'synthetic'}]}]}))
    except Exception as e:
        print(json.loads(str(e))['code'])
asyncio.run(run())
"""

ISOLATION_PROBE = r"""
import asyncio,json,sys
import sandhi_gateway as sg
async def attempt(ca_file):
    handle=sg.ProviderRuntime().provider(
        'inferflux','fixture','synthetic-invalid',
        base_url=sys.argv[1],max_retries=0,timeout_secs=3,ca_file=ca_file,
    )
    try:
        await handle.complete_json(json.dumps({'model':'fixture','messages':[{'role':'user','content':[{'type':'text','text':'synthetic'}]}]}))
    except Exception as e:
        return json.loads(str(e))['code']
async def run():
    print((await attempt(sys.argv[2])) + ',' + (await attempt(sys.argv[3])))
asyncio.run(run())
"""

ENV_AFTER_HANDLE_PROBE = r"""
import asyncio,json,os,sys
import sandhi_gateway as sg
async def run():
    handle=sg.ProviderRuntime().provider(
        'inferflux','fixture','synthetic-invalid',
        base_url=sys.argv[1],max_retries=0,timeout_secs=3,ca_file=sys.argv[2],
    )
    os.environ['SSL_CERT_FILE'] = sys.argv[3]
    os.environ['SSL_CERT_DIR'] = sys.argv[3]
    try:
        await handle.complete_json(json.dumps({'model':'fixture','messages':[{'role':'user','content':[{'type':'text','text':'synthetic'}]}]}))
    except Exception as e:
        print(json.loads(str(e))['code'])
asyncio.run(run())
"""

GATEWAY_PROBE = r"""
import asyncio,json,sys
import sandhi_gateway as sg
async def run():
    ca_file=sys.argv[2] if sys.argv[2] != '-' else None
    try:
        handle=sg.ProviderRuntime().provider(
            'inferflux','fixture','synthetic-invalid',base_url=sys.argv[1],
            max_retries=0,timeout_secs=3,protocol='chat_completions',
            peer_role='sandhi_gateway_v1',ca_file=ca_file,
        )
    except ValueError:
        print('constructor_error')
        return
    try:
        await handle.complete_json(json.dumps({'model':'fixture','messages':[{'role':'user','content':[{'type':'text','text':'synthetic'}]}]}))
    except Exception as e:
        print(json.loads(str(e))['code'])
asyncio.run(run())
"""


@pytest.fixture
def tls_gateway(tmp_path, request):
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
    callspec = getattr(request.node, "callspec", None)
    tls_version = callspec.params.get("tls_version") if callspec else None
    if tls_version is not None:
        version = getattr(ssl.TLSVersion, tls_version)
        context.minimum_version = version
        context.maximum_version = version
    server.socket = context.wrap_socket(server.socket, server_side=True)
    worker = threading.Thread(target=server.serve_forever, daemon=True)
    worker.start()
    yield ca, server.server_port
    server.shutdown()
    server.server_close()
    worker.join(timeout=2)


@pytest.mark.parametrize(
    "ca_path,host,expected",
    [
        ("ca", "localhost", "authentication_error"),
        ("preamble", "localhost", "authentication_error"),
        ("-", "localhost", "transport_error"),
        ("ca", "127.0.0.1", "transport_error"),
        ("missing", "localhost", "constructor_error"),
        ("malformed", "localhost", "constructor_error"),
    ],
)
def test_private_ca_preserves_chain_and_hostname_checks(
    tls_gateway, tmp_path, ca_path, host, expected
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
    malformed = tmp_path / "malformed.pem"
    malformed.write_bytes(b"not a certificate\n")
    preamble = tmp_path / "preamble-ca.pem"
    preamble.write_bytes(b"# managed by operator\n\n" + cert.read_bytes())
    ca_paths = {
        "preamble": str(preamble),
        "ca": str(cert),
        "missing": str(tmp_path / "missing.pem"),
        "malformed": str(malformed),
        "-": "-",
    }
    env["NO_PROXY"] = "localhost,127.0.0.1"
    result = subprocess.run(
        [sys.executable, "-c", PROBE, f"https://{host}:{port}/v1", ca_paths[ca_path]],
        env=env,
        capture_output=True,
        text=True,
        timeout=8,
    )
    assert result.returncode == 0, result.stderr
    assert result.stdout.strip() == expected


def test_provider_ca_file_is_optional_and_preserves_positional_ffi_arguments():
    import inspect

    import sandhi_gateway as sg

    signature = inspect.signature(sg.ProviderRuntime.provider)
    assert list(signature.parameters)[1:] == [
        "provider",
        "model",
        "api_key",
        "base_url",
        "timeout_secs",
        "stream_idle_timeout_secs",
        "max_retries",
        "headers_json",
        "auth_scheme",
        "protocol",
        "stream_setup_timeout_secs",
        "endpoint_profile",
        "peer_role",
        "ca_file",
    ]
    assert signature.parameters["ca_file"].default is None


@pytest.mark.skipif(sys.platform == "win32", reason="FIFOs are not portable to Windows")
def test_private_ca_rejects_a_fifo_without_blocking_validation(tmp_path):
    fifo = tmp_path / "ca-fifo.pem"
    os.mkfifo(fifo)
    env = {
        k: v
        for k, v in os.environ.items()
        if k.upper() not in {"HTTPS_PROXY", "HTTP_PROXY", "ALL_PROXY"}
    }
    env["NO_PROXY"] = "localhost,127.0.0.1"
    result = subprocess.run(
        [sys.executable, "-c", PROBE, "https://localhost/v1", str(fifo)],
        env=env,
        capture_output=True,
        text=True,
        timeout=3,
    )
    assert result.returncode == 0, result.stderr
    assert result.stdout.strip() == "constructor_error"


def test_private_ca_accepts_der_containing_a_pem_delimiter(tmp_path):
    import sandhi_gateway as sg

    pem = tmp_path / "embedded-delimiter-ca.pem"
    subprocess.run(
        [
            "openssl",
            "req",
            "-x509",
            "-newkey",
            "rsa:2048",
            "-nodes",
            "-keyout",
            str(tmp_path / "embedded-delimiter-ca-key.pem"),
            "-out",
            str(pem),
            "-days",
            "1",
            "-subj",
            "/CN=-----BEGIN CERTIFICATE-----",
        ],
        check=True,
        capture_output=True,
    )
    der = tmp_path / "embedded-delimiter-ca.der"
    subprocess.run(
        ["openssl", "x509", "-in", str(pem), "-outform", "DER", "-out", str(der)],
        check=True,
        capture_output=True,
    )
    handle = sg.ProviderRuntime().provider(
        "inferflux",
        "fixture",
        "synthetic-invalid",
        base_url="https://localhost/v1",
        max_retries=0,
        timeout_secs=3,
        ca_file=der,
    )
    assert handle is not None


def test_private_ca_does_not_load_invalid_ambient_tls_env(tls_gateway, tmp_path):
    cert, port = tls_gateway
    invalid = tmp_path / "invalid-ambient-ca.pem"
    invalid.write_bytes(b"not a certificate\n")
    env = {
        k: v
        for k, v in os.environ.items()
        if k.upper() not in {"HTTPS_PROXY", "HTTP_PROXY", "ALL_PROXY"}
    }
    env["SSL_CERT_FILE"] = str(invalid)
    env["SSL_CERT_DIR"] = str(invalid)
    env["NO_PROXY"] = "localhost,127.0.0.1"
    result = subprocess.run(
        [sys.executable, "-c", PROBE, f"https://localhost:{port}/v1", str(cert)],
        env=env,
        capture_output=True,
        text=True,
        timeout=8,
    )
    assert result.returncode == 0, result.stderr
    assert result.stdout.strip() == "authentication_error"


def test_provider_ca_file_accepts_path_like_values(tls_gateway):
    cert, _port = tls_gateway
    import sandhi_gateway as sg

    handle = sg.ProviderRuntime().provider(
        "inferflux",
        "fixture",
        "synthetic-invalid",
        base_url="https://localhost/v1",
        max_retries=0,
        timeout_secs=3,
        ca_file=Path(cert),
    )
    assert handle is not None


def test_two_scoped_runtime_cas_do_not_contaminate_one_another(tls_gateway, tmp_path):
    cert, port = tls_gateway
    other_ca = tmp_path / "other-ca.pem"
    subprocess.run(
        [
            "openssl",
            "req",
            "-x509",
            "-newkey",
            "rsa:2048",
            "-nodes",
            "-keyout",
            str(tmp_path / "other-ca-key.pem"),
            "-out",
            str(other_ca),
            "-days",
            "1",
            "-subj",
            "/CN=other-ca",
        ],
        check=True,
        capture_output=True,
    )
    env = {
        k: v
        for k, v in os.environ.items()
        if k.upper()
        not in {"HTTPS_PROXY", "HTTP_PROXY", "ALL_PROXY", "SSL_CERT_FILE", "SSL_CERT_DIR"}
    }
    env["NO_PROXY"] = "localhost,127.0.0.1"
    result = subprocess.run(
        [
            sys.executable,
            "-c",
            ISOLATION_PROBE,
            f"https://localhost:{port}/v1",
            str(cert),
            str(other_ca),
        ],
        env=env,
        capture_output=True,
        text=True,
        timeout=8,
    )
    assert result.returncode == 0, result.stderr
    assert result.stdout.strip() == "authentication_error,transport_error"


@pytest.mark.parametrize("ca_path,expected", [("ca", "authentication_error"), ("-", "transport_error")])
def test_gateway_peer_private_ca_is_scoped(tls_gateway, tmp_path, ca_path, expected):
    cert, port = tls_gateway
    ca_paths = {"ca": str(cert), "-": "-"}
    env = {
        k: v
        for k, v in os.environ.items()
        if k.upper()
        not in {"HTTPS_PROXY", "HTTP_PROXY", "ALL_PROXY", "SSL_CERT_FILE", "SSL_CERT_DIR"}
    }
    env["NO_PROXY"] = "localhost,127.0.0.1"
    result = subprocess.run(
        [sys.executable, "-c", GATEWAY_PROBE, f"https://localhost:{port}/v1", ca_paths[ca_path]],
        env=env,
        capture_output=True,
        text=True,
        timeout=8,
    )
    assert result.returncode == 0, result.stderr
    assert result.stdout.strip() == expected


def test_private_ca_ignores_tls_env_changes_after_handle_construction(tls_gateway, tmp_path):
    cert, port = tls_gateway
    unrelated_ca = tmp_path / "unrelated-ca.pem"
    unrelated_ca.write_bytes(b"not a certificate\n")
    env = {
        k: v
        for k, v in os.environ.items()
        if k.upper()
        not in {"HTTPS_PROXY", "HTTP_PROXY", "ALL_PROXY", "SSL_CERT_FILE", "SSL_CERT_DIR"}
    }
    env["NO_PROXY"] = "localhost,127.0.0.1"
    result = subprocess.run(
        [
            sys.executable,
            "-c",
            ENV_AFTER_HANDLE_PROBE,
            f"https://localhost:{port}/v1",
            str(cert),
            str(unrelated_ca),
        ],
        env=env,
        capture_output=True,
        text=True,
        timeout=8,
    )
    assert result.returncode == 0, result.stderr
    assert result.stdout.strip() == "authentication_error"


@pytest.mark.filterwarnings("ignore:ssl.TLSVersion.TLSv1:DeprecationWarning")
@pytest.mark.parametrize(
    "tls_version,expected",
    [
        ("TLSv1", "transport_error"),
        ("TLSv1_1", "transport_error"),
        ("TLSv1_2", "authentication_error"),
        ("TLSv1_3", "authentication_error"),
    ],
)
def test_private_ca_preserves_rustls_tls_12_minimum(tls_gateway, tls_version, expected):
    cert, port = tls_gateway
    env = {
        k: v
        for k, v in os.environ.items()
        if k.upper()
        not in {"HTTPS_PROXY", "HTTP_PROXY", "ALL_PROXY", "SSL_CERT_FILE", "SSL_CERT_DIR"}
    }
    env["NO_PROXY"] = "localhost,127.0.0.1"
    result = subprocess.run(
        [sys.executable, "-c", PROBE, f"https://localhost:{port}/v1", str(cert)],
        env=env,
        capture_output=True,
        text=True,
        timeout=8,
    )
    assert result.returncode == 0, result.stderr
    assert result.stdout.strip() == expected
