"""Actual-process listener transport admission, before authority/state side effects."""
from contextlib import contextmanager
import json
import os
from pathlib import Path
import ssl
import socket
import subprocess
import shutil
import sys
import time

import httpx
import pytest

from conftest import REPO_ROOT, _free_port
from oidc_fixture import oidc_authority  # noqa: F401


@pytest.fixture(scope="module")
def transport_binary(request):
    override = os.environ.get("SANDHI_TRANSPORT_TEST_BINARY")
    return Path(override).resolve(strict=True) if override else request.getfixturevalue("proxy_binary")


def environment(tmp_path, mode="tokens", bind=None, tls=False, **extra):
    port = _free_port()
    (tmp_path / "vault").mkdir(mode=0o700, exist_ok=True)
    certificate = REPO_ROOT / "crates/sandhi-proxy/tests/fixtures/tls/localhost-cert.pem"
    config = tmp_path / "config.json"
    config.write_text(json.dumps({"tls": {"cert": str(certificate),
        "key": str(certificate.with_name("localhost-key.pem"))}} if tls else {}))
    env = {"PATH": os.defpath, "LANG": "C", "SANDHI_LOG": "off",
           "SANDHI_AUTH_MODE": mode, "SANDHI_BIND": bind or f"127.0.0.1:{port}",
           "SANDHI_STORE": str(tmp_path / "ledger.db"), "SANDHI_CONFIG": str(config),
           "SANDHI_VAULT_BACKEND": "credstore", "SANDHI_CREDENTIALS_DIR": str(tmp_path / "vault"),
           "SANDHI_SHUTDOWN_QUIESCE_MS": "0"}
    if mode == "oidc":
        env["SANDHI_OIDC_CONFIG"] = str(tmp_path / "must-not-read-oidc.json")
    env.update(extra)
    return env, port, certificate


@pytest.mark.parametrize("mode", ["tokens", "oidc"])
@pytest.mark.parametrize("bind,tls,companion,exception", [
    (None, False, None, None),
    ("0.0.0.0:0", False, None, "1"),
    ("[::]:0", False, None, "1"),
    ("192.0.2.1:0", False, None, "1"),
    ("[::ffff:192.0.2.1]:0", False, None, "1"),
    ("[::ffff:127.0.0.1]:0", False, None, "1"),
    (None, True, "127.0.0.1:0", None),
    (None, True, "0.0.0.0:0", "1"),
    (None, True, "[::]:0", "1"),
    (None, True, "192.0.2.1:0", "1"),
])
def test_authenticated_plaintext_refused_before_authority_or_state(
        transport_binary, tmp_path, mode, bind, tls, companion, exception):
    env, _, _ = environment(tmp_path, mode, bind, tls)
    if companion is not None:
        env["SANDHI_HTTP_BIND"] = companion
    if exception is not None:
        env["SANDHI_ALLOW_LOOPBACK_HTTP"] = exception
    result = subprocess.run([str(transport_binary)], env=env, cwd=tmp_path,
                            capture_output=True, timeout=5)
    assert result.returncode == 1, result.stderr
    assert b"authenticated listeners require TLS" in result.stderr
    assert not (tmp_path / "ledger.db").exists()
    assert not list((tmp_path / "vault").iterdir())


@pytest.mark.parametrize("credential", ["SANDHI_ADMIN_TOKEN", "SANDHI_CLIENT_CREDENTIALS_FILE"])
def test_starter_credential_surfaces_require_secure_transport(transport_binary, tmp_path, credential):
    env, _, _ = environment(tmp_path, "none", **{credential: "fixture-only"})
    result = subprocess.run([str(transport_binary)], env=env, capture_output=True, timeout=5)
    assert result.returncode == 1
    assert b"authenticated listeners require TLS" in result.stderr
    assert not (tmp_path / "ledger.db").exists()


@pytest.mark.parametrize("value", ["", "true", "false", "01", " 1", "1 ", "yes", "2",
    pytest.param(b"\xff", marks=pytest.mark.skipif(os.name != "posix", reason="byte environment"))])
def test_invalid_exception_fails_closed_even_for_starter(transport_binary, tmp_path, value):
    env, _, _ = environment(tmp_path, "none", SANDHI_ALLOW_LOOPBACK_HTTP=value)
    result = subprocess.run([str(transport_binary)], env=env, capture_output=True, timeout=5)
    assert result.returncode == 1
    assert b"SANDHI_ALLOW_LOOPBACK_HTTP must be unset, 0 or 1" in result.stderr
    assert not (tmp_path / "ledger.db").exists()


@contextmanager
def running(binary, env, url, certificate, command=None):
    process = subprocess.Popen(command or [str(binary)], env=env, stdin=subprocess.DEVNULL,
                               stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
    try:
        verify = ssl.create_default_context(cafile=certificate) if url.startswith("https:") else True
        with httpx.Client(verify=verify, trust_env=False, timeout=1) as client:
            deadline = time.monotonic() + 10
            while True:
                assert process.poll() is None, process.stderr.read().decode()
                try:
                    response = client.get(url + "/readyz")
                    if response.status_code == 200:
                        assert response.text == "ready"
                        break
                except httpx.TransportError:
                    pass
                assert time.monotonic() < deadline, "listener did not become ready"
                time.sleep(.03)
            yield client
    finally:
        process.terminate()
        try:
            assert process.wait(timeout=5) == 0
        finally:
            if process.poll() is None:
                process.kill()
                process.wait(timeout=5)
            process.stderr.close()


@pytest.mark.parametrize("mode,tls,exception", [("none", False, None), ("none", False, "0"),
    ("tokens", True, None), ("tokens", False, "1"), ("oidc", True, None), ("oidc", False, "1")])
def test_admitted_transport_and_authentication_stay_distinct(
        transport_binary, tmp_path, oidc_authority, mode, tls, exception):
    env, port, certificate = environment(tmp_path, mode, tls=tls)
    if exception is not None:
        env["SANDHI_ALLOW_LOOPBACK_HTTP"] = exception
    if mode == "oidc":
        policy = tmp_path / "oidc.json"
        policy.write_text(json.dumps({"issuer": oidc_authority.issuer,
            "client_id": "sandhi-browser", "redirect_url": f"https://localhost:{port}/auth/callback",
            "ca_file": str(oidc_authority.certificate), "subjects": {}}))
        env["SANDHI_OIDC_CONFIG"] = str(policy)
    url = f"https://localhost:{port}" if tls else f"http://127.0.0.1:{port}"
    with running(transport_binary, env, url, certificate) as client:
        response = client.post(url + "/v1/chat/completions", json={})
        if mode != "none":
            assert response.status_code == 401
        if tls:
            with pytest.raises(httpx.TransportError):
                client.get(f"http://127.0.0.1:{port}/readyz")


def test_explicit_loopback_companion_preserves_tls_primary(transport_binary, tmp_path):
    env, port, certificate = environment(tmp_path, tls=True, SANDHI_ALLOW_LOOPBACK_HTTP="1")
    companion = _free_port()
    env["SANDHI_HTTP_BIND"] = f"127.0.0.1:{companion}"
    with running(transport_binary, env, f"https://localhost:{port}", certificate) as client:
        assert client.get(f"http://127.0.0.1:{companion}/readyz").status_code == 200
        assert client.post(f"http://127.0.0.1:{companion}/v1/chat/completions", json={}).status_code == 401


def test_explicit_ipv6_loopback_exception(transport_binary, tmp_path):
    try:
        with socket.socket(socket.AF_INET6, socket.SOCK_STREAM) as listener:
            listener.bind(("::1", 0))
            port = listener.getsockname()[1]
    except OSError:
        pytest.skip("host has no IPv6 loopback listener")
    env, _, certificate = environment(tmp_path, bind=f"[::1]:{port}", SANDHI_ALLOW_LOOPBACK_HTTP="1")
    with running(transport_binary, env, f"http://[::1]:{port}", certificate) as client:
        assert client.post(f"http://[::1]:{port}/v1/chat/completions", json={}).status_code == 401


def test_implicit_oidc_is_protected_before_discovery(transport_binary, tmp_path):
    env, _, _ = environment(tmp_path, "oidc")
    env.pop("SANDHI_AUTH_MODE")
    result = subprocess.run([str(transport_binary)], env=env, capture_output=True, timeout=5)
    assert result.returncode == 1
    assert b"authenticated listeners require TLS" in result.stderr
    assert not (tmp_path / "ledger.db").exists()


def test_explicit_loopback_exception_does_not_remove_starter_admin_auth(transport_binary, tmp_path):
    env, port, certificate = environment(tmp_path, "none", SANDHI_ALLOW_LOOPBACK_HTTP="1",
                                        SANDHI_ADMIN_TOKEN="fixture-admin-token")
    url = f"http://127.0.0.1:{port}"
    with running(transport_binary, env, url, certificate) as client:
        assert client.get(url + "/admin/version").status_code == 401
        assert client.get(url + "/admin/version", headers={"Authorization": "Bearer fixture-admin-token"}).status_code == 200


def service_fixture(binary, tmp_path):
    env, port, certificate = environment(tmp_path, tls=True)
    shutil.copyfile(REPO_ROOT / "scripts/subscription_lease.py", tmp_path / "subscription_lease.py")
    (tmp_path / "sandhi-proxy").symlink_to(binary)
    config = {"auth_mode": "tokens", "bind": f"127.0.0.1:{port}",
              "gateway": f"https://localhost:{port}", "listener_config": env["SANDHI_CONFIG"]}
    for name, data in (("service.json", config), ("admin.json", {"admin_token": "fixture-admin-" + "x" * 32})):
        path = tmp_path / name
        path.write_text(json.dumps(data))
        path.chmod(0o600)
    command = [sys.executable, str(REPO_ROOT / "scripts/subscription_service.py"),
               "gateway", "--state", str(tmp_path)]
    return env, config, certificate, command


def test_private_service_runner_supplies_native_tls(transport_binary, tmp_path):
    env, config, certificate, command = service_fixture(transport_binary, tmp_path)
    with running(transport_binary, env, config["gateway"], certificate, command) as client:
        assert client.get(config["gateway"] + "/admin/version").status_code == 401
        assert client.get(config["gateway"] + "/admin/version", headers={
            "Authorization": "Bearer fixture-admin-" + "x" * 32}).status_code == 200
        with pytest.raises(httpx.TransportError):
            client.get(config["gateway"].replace("https:", "http:") + "/readyz")


@pytest.mark.parametrize("value", ["", " ", None, 17])
def test_private_service_runner_refuses_invalid_listener_config(transport_binary, tmp_path, value):
    env, config, _, command = service_fixture(transport_binary, tmp_path)
    config["listener_config"] = value
    (tmp_path / "service.json").write_text(json.dumps(config))
    result = subprocess.run(command, env=env, capture_output=True, timeout=5)
    assert result.returncode != 0
    assert b"listener_config must be a nonempty" in result.stderr
    assert not (tmp_path / "usage.db").exists()
