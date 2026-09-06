"""Offline metadata recovery with synthetic broker grants, never a user's vault.

The restored process is isolated on loopback and never receives production traffic. This
tests startup resolution, not live daemon certification or invalidation of cached handles.
"""

import os
import sqlite3
import subprocess
import time
from contextlib import closing, contextmanager

import httpx
import pytest

from conftest import REPO_ROOT, _free_port
from recovery_snapshot import restore, snapshot
from test_broker import ADMIN, SECRET, FakeBroker, broker_binary, broker_plain_binary  # noqa: F401

pytestmark = pytest.mark.skipif(os.name != "posix", reason="Disposable Unix broker transport")


@contextmanager
def recovered_broker_process(binary, database, runtime_dir, daemon, *, token="read-token"):
    runtime_dir.mkdir()
    token_dir = runtime_dir / "config/PasswordManager"
    token_dir.mkdir(parents=True)
    (token_dir / "ipc.token").write_text("disposable-daemon-token")
    config = runtime_dir / "empty.json"
    config.write_text("{}")
    env = {k: v for k, v in os.environ.items()
           if not k.startswith(("SANDHI_", "SENTINELPASS_"))}
    port = _free_port()
    env.update(XDG_CONFIG_HOME=str(runtime_dir / "config"),
               SANDHI_STORE=str(database), SANDHI_CONFIG=str(config),
               SANDHI_BIND=f"127.0.0.1:{port}", SANDHI_ADMIN_TOKEN="broker-test-admin",
               SANDHI_VAULT_BACKEND="sentinelpass", SANDHI_SENTINELPASS_SOCKET=daemon.path,
               SANDHI_SENTINELPASS_TIMEOUT_MS="250", SENTINELPASS_CLIENT_TOKEN=token,
               SANDHI_SHUTDOWN_QUIESCE_MS="0", SANDHI_SHUTDOWN_GRACE_SECS="3")
    log_path = runtime_dir / "proxy.log"
    with log_path.open("wb") as log:
        process = subprocess.Popen([str(binary)], cwd=REPO_ROOT, env=env,
                                   stdout=subprocess.DEVNULL, stderr=log)
        try:
            with httpx.Client(base_url=f"http://127.0.0.1:{port}", headers=ADMIN, timeout=3) as client:
                deadline = time.monotonic() + 15
                while True:
                    assert process.poll() is None, "isolated broker gateway exited during startup"
                    try:
                        if client.get("/readyz").status_code == 200:
                            break
                    except httpx.TransportError:
                        pass
                    assert time.monotonic() < deadline, "isolated broker startup timed out"
                    time.sleep(0.02)
                yield client
        finally:
            process.terminate()
            try:
                result = process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait(timeout=2)
                raise
            assert result == 0, f"clean broker drill shutdown failed: {result}"
    assert SECRET not in log_path.read_text()
    assert "disposable-daemon-token" not in log_path.read_text()


@pytest.mark.parametrize("failure", ["locked", "missing", "denied", "plain"])
def test_restored_metadata_is_not_broker_authority(
    broker_binary, broker_plain_binary, upstream, tmp_path, failure,
):
    daemon = FakeBroker(tmp_path / "broker.sock")
    source_dir = tmp_path / "source"
    source_dir.mkdir()
    source = source_dir / "usage.db"
    reference = {"provider": "openai", "label": "default", "base_url": upstream.base_url}
    body = {"model": "gpt-mock", "messages": [{"role": "user", "content": "synthetic recovery"}]}
    try:
        with recovered_broker_process(broker_binary, source, tmp_path / "initial", daemon) as client:
            registered = client.post("/admin/keys/reference", json=reference)
            assert registered.status_code == 201, registered.text
            minted = client.post("/admin/keys/share", json={
                "upstream": "openai:default", "subject": "recovery", "group": "recovery",
            })
            assert minted.status_code == 200, minted.text
            key = minted.json()["virtual_key"]
            inventory = client.get("/admin/keys").json()
        # No process is using the source when the snapshot is taken.
        archive = tmp_path / "snapshot"
        snapshot(source, archive, shards=1, metadata={"drain_exit": 0, "purpose": "synthetic broker recovery"})
        for item in archive.iterdir():
            if item.is_file():
                assert SECRET.encode() not in item.read_bytes()
                assert key.encode() not in item.read_bytes(), "only virtual-key hashes belong in SQLite"
        restored = restore(archive, tmp_path / "restored", expected_shards=1)
        daemon.mode = failure if failure in ("locked", "missing") else "normal"
        unavailable_binary = broker_plain_binary if failure == "plain" else broker_binary
        before = len(upstream.requests)
        with recovered_broker_process(
            unavailable_binary, restored, tmp_path / "unavailable", daemon,
            token="wrong-token" if failure == "denied" else "read-token",
        ) as client:
            assert client.get("/readyz").status_code == 200, "readiness is lifecycle-only"
            assert client.get("/admin/keys").json() == inventory
            denied = client.post("/v1/chat/completions", headers={"Authorization": f"Bearer {key}"}, json=body)
            assert denied.status_code == 502, denied.text
            assert len(upstream.requests) == before
            attempt = client.post("/admin/keys/reference", json=reference)
            expected = {"locked": (423, "vault_locked"), "missing": (404, "vault_missing"),
                        "denied": (403, "vault_denied"), "plain": (503, "vault_configuration")}[failure]
            assert (attempt.status_code, attempt.json()["code"]) == expected
            assert SECRET not in denied.text + attempt.text
        with closing(sqlite3.connect(restored)) as connection:
            assert connection.execute("SELECT COUNT(*) FROM usage_events").fetchone()[0] == 0
            assert connection.execute("SELECT COUNT(*) FROM budget_reservation").fetchone()[0] == 0
        # Explicitly re-provision a synthetic read grant and re-register the exact reference.
        # No claim of dynamic grant revalidation on already cached provider handles is made.
        daemon.mode = "normal"
        with recovered_broker_process(broker_binary, restored, tmp_path / "authorized", daemon) as client:
            assert client.post("/admin/keys/reference", json=reference).status_code == 201
            response = client.post("/v1/chat/completions", headers={"Authorization": f"Bearer {key}"}, json=body)
            assert response.status_code == 200, response.text
        assert len(upstream.requests) == before + 1
        assert upstream.last().headers["authorization"] == f"Bearer {SECRET}"
        assert all(list(envelope["message"]) == ["GetExternalSecret"] for envelope in daemon.received)
        with closing(sqlite3.connect(restored)) as connection:
            assert connection.execute("SELECT COUNT(*) FROM usage_events").fetchone()[0] == 1
    finally:
        daemon.close()
