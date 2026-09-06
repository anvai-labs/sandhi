"""Native broker boundary through a real proxy; synthetic grants, no user vault access."""

import json
import os
import socket
import sqlite3
import shutil
import struct
import subprocess
import threading
import time
from concurrent.futures import ThreadPoolExecutor

import httpx
import pytest

from conftest import REPO_ROOT, _free_port

pytestmark = pytest.mark.skipif(os.name != "posix", reason="Unix fake-daemon transport")
ADMIN = {"Authorization": "Bearer broker-test-admin"}
SECRET = "synthetic-provider-secret-never-log"


class FakeBroker:
    def __init__(self, path):
        self.path = str(path)
        self.listener = socket.socket(socket.AF_UNIX)
        self.listener.bind(self.path)
        self.listener.listen()
        self.listener.settimeout(0.1)
        self.stop = threading.Event()
        self.received = []
        self.mode = "normal"
        self.thread = threading.Thread(target=self.run, daemon=True)
        self.thread.start()

    @staticmethod
    def read_exact(conn, count):
        data = b""
        while len(data) < count:
            chunk = conn.recv(count - len(data))
            if not chunk:
                raise EOFError()
            data += chunk
        return data

    def run(self):
        while not self.stop.is_set():
            try:
                conn, _ = self.listener.accept()
            except socket.timeout:
                continue
            except OSError:
                break
            with conn:
                conn.settimeout(2)
                try:
                    size = struct.unpack(">I", self.read_exact(conn, 4))[0]
                    assert 0 < size <= 65536
                    envelope = json.loads(self.read_exact(conn, size))
                    self.received.append(envelope)
                    if self.mode == "hang":
                        self.stop.wait(2)
                        continue
                    kind, request = next(iter(envelope["message"].items()))
                    allowed = (envelope["token"] == "disposable-daemon-token"
                               and envelope.get("origin") == "cli"
                               and envelope.get("client_token") in ("read-token", "write-token")
                               and request["client_id"] == "sandhi"
                               and request["domain"] == "sandhi:openai:default")
                    locked = self.mode == "locked"
                    if kind == "SaveSecret":
                        success = allowed and envelope.get("client_token") == "write-token" and not locked
                        response = {"SaveSecretResponse": {"success": success, "locked": locked,
                                    "error": None if success else SECRET}}
                    elif kind == "GetExternalSecret":
                        response = {"GetExternalSecretResponse": {
                            "authorized": allowed, "locked": locked,
                            "value": SECRET if allowed and not locked and self.mode != "missing" else None,
                            "error": None if allowed else SECRET}}
                    else:
                        response = {"DeleteSecretResponse": {"deleted": False, "error": SECRET}}
                    data = json.dumps(response).encode()
                    conn.sendall(struct.pack(">I", len(data)) + data)
                except (OSError, EOFError):
                    pass

    def close(self):
        self.stop.set()
        self.listener.close()
        self.thread.join(timeout=3)
        assert not self.thread.is_alive()


@pytest.fixture(scope="module")
def broker_binary(tmp_path_factory):
    subprocess.run(["cargo", "build", "-p", "sandhi-proxy", "--bins",
                    "--features", "sentinelpass-ipc"], cwd=REPO_ROOT, check=True)
    # Keep each feature build immutable even when another fixture rebuilds the normal target.
    target = tmp_path_factory.mktemp("native-broker-bin") / "sandhi-proxy"
    shutil.copy2(REPO_ROOT / "target/debug/sandhi-proxy", target)
    return target


@pytest.fixture(scope="module")
def broker_plain_binary(tmp_path_factory):
    subprocess.run(["cargo", "build", "-p", "sandhi-proxy", "--bin", "sandhi-proxy"], cwd=REPO_ROOT, check=True)
    target = tmp_path_factory.mktemp("plain-broker-bin") / "sandhi-proxy"
    shutil.copy2(REPO_ROOT / "target/debug/sandhi-proxy", target)
    return target


@pytest.fixture
def broker(broker_binary, tmp_path, request):
    options = getattr(request, "param", {})
    if not options.get("native", True):
        broker_binary = request.getfixturevalue("broker_plain_binary")
    daemon = FakeBroker(tmp_path / "broker.sock")
    daemon.database = tmp_path / "usage.db"
    token_dir = tmp_path / "config/PasswordManager"
    token_dir.mkdir(parents=True)
    (token_dir / "ipc.token").write_text("disposable-daemon-token")
    config = tmp_path / "empty.json"
    config.write_text("{}")
    port = _free_port()
    env = {k: v for k, v in os.environ.items() if not k.startswith(("SANDHI_", "SENTINELPASS_"))}
    env.update({"XDG_CONFIG_HOME": str(tmp_path / "config"),
                "SANDHI_STORE": str(tmp_path / "usage.db"), "SANDHI_CONFIG": str(config),
                "SANDHI_BIND": f"127.0.0.1:{port}", "SANDHI_ADMIN_TOKEN": "broker-test-admin",
                "SANDHI_VAULT_BACKEND": options.get("backend", "sentinelpass"), "SANDHI_SENTINELPASS_SOCKET": daemon.path,
                "SANDHI_SENTINELPASS_TIMEOUT_MS": "250",
                "SENTINELPASS_CLIENT_TOKEN": options.get("token", "write-token")})
    if options.get("cli"):
        env.update({"SANDHI_SENTINELPASS_FALLBACK_CLI": "1", "SANDHI_SENTINELPASS_BIN": "/does-not-exist-fixture"})
    proc = subprocess.Popen([str(broker_binary)], env=env, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
    with httpx.Client(base_url=f"http://127.0.0.1:{port}", headers=ADMIN, timeout=5) as client:
        try:
            deadline = time.monotonic() + 15
            while True:
                assert proc.poll() is None, proc.stderr.read().decode()
                try:
                    if client.get("/healthz").status_code == 200:
                        break
                except httpx.TransportError:
                    pass
                assert time.monotonic() < deadline
                time.sleep(0.05)
            yield client, daemon
        finally:
            proc.terminate()
            try:
                _, logs = proc.communicate(timeout=5)
            except subprocess.TimeoutExpired:
                proc.kill()
                _, logs = proc.communicate(timeout=5)
            daemon.close()
            assert SECRET.encode() not in logs
            assert b"Cannot start a runtime" not in logs
            assert b"Cannot drop a runtime" not in logs


def test_native_save_uses_real_admin_handler_without_nested_runtime(broker):
    client, daemon = broker
    result = client.post("/admin/keys", json={"provider": "openai", "secret": SECRET})
    assert result.status_code == 201, result.text
    assert result.json()["credential_id"] == "openai:default"
    assert len(daemon.received) == 1
    assert client.get("/healthz").status_code == 200


@pytest.mark.parametrize("broker", [{"token": "read-token"}], indirect=True)
def test_read_grant_registers_reference_but_cannot_save(broker):
    client, daemon = broker
    denied = client.post("/admin/keys", json={"provider": "openai", "secret": SECRET})
    assert denied.status_code == 403
    assert denied.json()["code"] == "vault_denied"
    assert SECRET not in denied.text
    assert client.get("/admin/keys").json()["keys"] == []
    registered = client.post("/admin/keys/reference", json={"provider": "openai"})
    assert registered.status_code == 201, registered.text
    assert SECRET not in registered.text
    assert list(daemon.received[-1]["message"]) == ["GetExternalSecret"]
    assert client.get("/admin/keys").json()["keys"][0]["status"] == "active"
    count = len(daemon.received)
    revoked = client.delete("/admin/keys/openai/default").json()
    assert revoked["revoked"] is True
    assert revoked["secret_deletion"] == "unsupported"
    assert revoked["broker_grant_revoked"] is False
    assert revoked["provider_key_revoked"] is False
    assert len(daemon.received) == count, "unsupported deletion must not contact the daemon"


@pytest.mark.parametrize("mode,status,code", [
    ("locked", 423, "vault_locked"), ("missing", 404, "vault_missing")])
def test_reference_failure_states_are_distinct_and_do_not_register(broker, mode, status, code):
    client, daemon = broker
    daemon.mode = mode
    response = client.post("/admin/keys/reference", json={"provider": "openai"})
    assert response.status_code == status, response.text
    assert response.json()["code"] == code
    assert SECRET not in response.text
    assert client.get("/admin/keys").json()["keys"] == []


@pytest.mark.parametrize("broker", [{"token": "wrong-token"}, {"token": ""}], indirect=True)
def test_invalid_or_missing_client_token_never_falls_back(broker):
    client, daemon = broker
    response = client.post("/admin/keys/reference", json={"provider": "openai"})
    assert response.status_code in (403, 503), response.text
    assert response.json()["code"] in ("vault_denied", "vault_configuration")
    assert SECRET not in response.text
    assert client.get("/admin/keys").json()["keys"] == []
    if response.status_code == 503:
        assert daemon.received == []
        assert client.get("/admin/version").json()["capabilities"]["vault"]["backend"] == "unavailable"


def test_capabilities_are_support_not_authorization_and_reference_is_exact(broker):
    client, daemon = broker
    capabilities = client.get("/admin/version").json()["capabilities"]["vault"]
    assert capabilities["operations"] == {"read": True, "write": True, "delete": False, "bounded_io": True}
    assert capabilities["grant_status"] == "not_checked"
    assert capabilities["credential_generations"] is False
    wrong = client.post("/admin/keys/reference", json={"provider": "openai", "label": "other"})
    assert wrong.status_code == 403
    before = len(daemon.received)
    for label in ("UPPER", "bad:label", "*", " white ", "with/path", "default."):
        assert client.post("/admin/keys/reference", json={"provider": "openai", "label": label}).status_code == 400
    assert client.post("/admin/keys/reference", json={"provider": "openai", "secret": SECRET}).status_code == 422
    assert len(daemon.received) == before


def test_locked_write_is_rejected_and_redacted(broker):
    client, daemon = broker
    daemon.mode = "locked"
    result = client.post("/admin/keys", json={"provider": "openai", "secret": SECRET})
    assert result.status_code == 423
    assert result.json()["code"] == "vault_locked"
    assert SECRET not in result.text
    assert client.get("/admin/keys").json()["keys"] == []


def test_reference_requires_admin_and_metadata_commit(broker):
    client, daemon = broker
    denied = client.post("/admin/keys/reference", headers={"Authorization": "Bearer wrong"}, json={"provider": "openai"})
    assert denied.status_code == 401
    assert daemon.received == []
    with sqlite3.connect(daemon.database) as conn:
        conn.execute("CREATE TRIGGER deny_insert BEFORE INSERT ON vault BEGIN SELECT RAISE(ABORT, 'synthetic failure'); END")
    failed = client.post("/admin/keys/reference", json={"provider": "openai"})
    assert failed.status_code == 503
    assert SECRET not in failed.text
    assert client.get("/admin/keys").json()["keys"] == []


def test_inventory_failure_is_not_an_empty_success(broker):
    client, daemon = broker
    with sqlite3.connect(daemon.database) as conn:
        conn.execute("DROP TABLE vault")
    result = client.get("/admin/keys")
    assert result.status_code == 503
    assert result.json()["code"] == "vault_unavailable"


@pytest.mark.parametrize("broker", [{"token": "read-token"}], indirect=True)
def test_cli_reference_does_not_consume_or_forward_stdin(broker):
    client, daemon = broker
    env = {**os.environ, "SANDHI_ADMIN_TOKEN": "broker-test-admin",
           "SANDHI_ADMIN_URL": str(client.base_url)}
    result = subprocess.run([str(REPO_ROOT / "target/debug/sandhi"), "keys", "reference", "openai", "default"],
                            env=env, input=SECRET, capture_output=True, text=True, timeout=5)
    assert result.returncode == 0, result.stderr
    assert "openai:default" in result.stdout
    assert SECRET not in result.stdout + result.stderr
    assert len(daemon.received) == 1
    assert list(daemon.received[0]["message"]) == ["GetExternalSecret"]


@pytest.mark.parametrize("broker", [{"native": False}, {"backend": "misspelled"}], indirect=True)
def test_unavailable_configuration_does_not_select_another_backend(broker):
    client, daemon = broker
    vault = client.get("/admin/version").json()["capabilities"]["vault"]
    assert vault["backend"] == "unavailable"
    assert vault["reference_registration"] is False
    assert not any(vault["operations"].values())
    result = client.post("/admin/keys", json={"provider": "openai", "secret": SECRET})
    assert result.status_code == 503
    assert result.json()["code"] == "vault_configuration"
    assert daemon.received == []


@pytest.mark.parametrize("broker", [{"cli": True}], indirect=True)
def test_explicit_cli_capabilities_disable_unsupported_onboarding(broker):
    client, daemon = broker
    vault = client.get("/admin/version").json()["capabilities"]["vault"]
    assert vault["backend"] == "sentinelpass"
    assert vault["operations"] == {"read": True, "write": False, "delete": False, "bounded_io": False}
    assert vault["reference_registration"] is False
    for endpoint, body in [("/admin/keys/reference", {"provider": "openai"}),
                           ("/admin/keys", {"provider": "openai", "secret": SECRET})]:
        result = client.post(endpoint, json=body)
        assert result.status_code == 501
        assert result.json()["code"] == "vault_unsupported"
    assert daemon.received == []


def test_hung_write_is_bounded_health_remains_live_and_no_retry_occurs(broker):
    client, daemon = broker
    daemon.mode = "hang"
    with ThreadPoolExecutor(max_workers=1) as executor:
        started = time.monotonic()
        pending = executor.submit(client.post, "/admin/keys", json={"provider": "openai", "secret": SECRET})
        while not daemon.received:
            assert time.monotonic() - started < 2
            time.sleep(0.005)
        assert client.get("/healthz").status_code == 200
        busy = client.post("/admin/keys", json={"provider": "openai", "secret": SECRET})
        assert busy.status_code == 503, busy.text
        assert busy.json()["code"] == "vault_busy"
        response = pending.result(timeout=2)
    assert response.status_code == 504, response.text
    assert response.json()["reconcile_before_retry"] is True
    assert time.monotonic() - started < 2
    assert len(daemon.received) == 1
    assert client.get("/admin/keys").json()["keys"] == []


@pytest.mark.parametrize("broker", [{"token": "read-token"}], indirect=True)
def test_browser_registers_a_reference_without_sending_a_secret(broker):
    from playwright.sync_api import sync_playwright, expect
    client, daemon = broker
    with sync_playwright() as playwright:
        browser = playwright.chromium.launch()
        try:
            page = browser.new_page()
            page.goto(str(client.base_url) + "/dashboard")
            page.get_by_label("Admin token", exact=True).fill("broker-test-admin")
            page.get_by_role("button", name="Use token", exact=True).click()
            page.get_by_text("Add a credential", exact=True).click()
            page.get_by_label("Credential source").select_option("reference")
            expect(page.locator("#c-secret")).to_be_disabled()
            page.locator("#c-provider").fill("openai")
            page.get_by_role("button", name="Add", exact=True).click()
            expect(page.locator("#c-result")).to_contain_text("Registered openai:default")
            assert len(daemon.received) == 1
            assert list(daemon.received[0]["message"]) == ["GetExternalSecret"]
            assert SECRET not in page.locator("body").inner_text()
        finally:
            browser.close()
