"""Disposable single-node recovery drills, not a production restore or live-vault tool.

Snapshots require stopped writers and unchanged shard topology. Old security state is
restored without provider credentials until explicit revocation reconciliation completes.
"""

from __future__ import annotations

import hashlib
import json
import os
import shutil
import sqlite3
import subprocess
import time
from contextlib import closing, contextmanager
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path

import httpx
import pytest

from conftest import REAL_OPENAI_KEY, REPO_ROOT, _free_port
from recovery_snapshot import restore, snapshot
from test_shutdown import BODY, gated_provider  # noqa: F401 - synthetic gated upstream fixture


pytestmark = pytest.mark.skipif(os.name != "posix", reason="Actual POSIX restart/SIGKILL drills")
ADMIN = {"Authorization": "Bearer recovery-test-admin"}
SCOPES = ("group:recovery-a", "group:recovery-b")
TABLES = ("usage_events", "virtual_keys", "vault", "alert_rules", "budget_limit",
          "budget_reservation", "idempotency_dedup", "budget_settlement_outbox")


@dataclass
class Gateway:
    process: subprocess.Popen
    client: httpx.Client
    database: Path
    shards: int
    executable_sha256: str
    contract_version: dict

    def stop(self):
        self.process.terminate()
        assert self.process.wait(timeout=6) == 0


@pytest.fixture
def recovery_gateway(proxy_binary, gated_provider, tmp_path):
    # Other feature fixtures rebuild target/debug. Every restart in a drill must
    # execute the same immutable binary, not whichever build ran most recently.
    executable = tmp_path / "recovery-proxy"
    shutil.copy2(proxy_binary, executable)
    with executable.open("rb") as binary:
        executable_sha256 = hashlib.file_digest(binary, "sha256").hexdigest()
    launches = 0

    @contextmanager
    def launch(database, *, shards=1, provider_enabled=True):
        nonlocal launches
        launches += 1
        database = Path(database)
        database.parent.mkdir(parents=True, exist_ok=True)
        # Configuration is separately provisioned, not smuggled into the DB snapshot.
        config = tmp_path / f"launch-{launches}.json"
        config.write_text(json.dumps({"providers": [], "vkeys": [], "budgets": [], "alerts": []}))
        env = {key: value for key, value in os.environ.items()
               if not key.startswith(("SANDHI_", "SENTINELPASS_"))}
        env.update({
            "SANDHI_BIND": f"127.0.0.1:{_free_port()}",
            "SANDHI_STORE": str(database), "SANDHI_LEDGER_SHARDS": str(shards),
            "SANDHI_CONFIG": str(config), "SANDHI_ADMIN_TOKEN": "recovery-test-admin",
            "SANDHI_SHUTDOWN_GRACE_SECS": "3", "SANDHI_SHUTDOWN_QUIESCE_MS": "100",
            # No OS keyring or real broker reads, even if restored metadata is malformed.
            "SANDHI_VAULT_BACKEND": "recovery-fixture-unavailable",
        })
        if provider_enabled:
            env.update(SANDHI_OPENAI_KEY=REAL_OPENAI_KEY, SANDHI_OPENAI_BASE=gated_provider.base)
        with (tmp_path / f"launch-{launches}.stderr").open("wb") as logs:
            process = subprocess.Popen([str(executable)], cwd=REPO_ROOT, env=env,
                                       stdout=subprocess.DEVNULL, stderr=logs)
            try:
                with httpx.Client(base_url="http://" + env["SANDHI_BIND"], timeout=3) as client:
                    deadline = time.monotonic() + 15
                    while True:
                        assert process.poll() is None, "recovery gateway exited during startup"
                        try:
                            if client.get("/readyz").status_code == 200:
                                break
                        except httpx.TransportError:
                            pass
                        assert time.monotonic() < deadline, "recovery gateway did not start"
                        time.sleep(0.02)
                    version = client.get("/version")
                    assert version.status_code == 200, version.text
                    yield Gateway(process, client, database, shards, executable_sha256, version.json())
            finally:
                if process.poll() is None:
                    process.terminate()
                    try:
                        process.wait(timeout=5)
                    except subprocess.TimeoutExpired:
                        process.kill()
                        process.wait(timeout=2)

    return launch


def snapshot_metadata(gateway, *, fixture):
    return {
        "fixture": fixture,
        "capture_started_at_utc": datetime.now(timezone.utc).isoformat(),
        "executable_sha256": gateway.executable_sha256,
        "contract_version": gateway.contract_version,
        "writers_stopped": True,
        "shutdown_exit": gateway.process.returncode,
    }


def databases(base, shards):
    return [base] if shards == 1 else [base] + [
        Path(f"{base}-ledger-shard-{index}.db") for index in range(shards)
    ]


def persisted_rows(base, shards):
    result = []
    for database in databases(base, shards):
        with closing(sqlite3.connect(database)) as connection:
            assert connection.execute("PRAGMA integrity_check").fetchone() == ("ok",)
            present = {row[0] for row in connection.execute("SELECT name FROM sqlite_master WHERE type='table'")}
            result.append({table: connection.execute(f"SELECT * FROM {table} ORDER BY rowid").fetchall()
                           for table in TABLES if table in present})
    return result


def leases(base, shards):
    rows = []
    for database in databases(base, shards):
        with closing(sqlite3.connect(database)) as connection:
            if connection.execute("SELECT 1 FROM sqlite_master WHERE name='budget_reservation'").fetchone():
                rows.extend(connection.execute(
                    "SELECT scope, id, ceiling, actual, settled, expires_at FROM budget_reservation ORDER BY id"
                ).fetchall())
    return sorted(rows)


def get(client, path):
    response = client.get(path, headers=ADMIN)
    assert response.status_code == 200, response.text
    return response.json()


def call(client, secret, *, step="seed", stream=False):
    return client.post("/v1/chat/completions", headers={
        "Authorization": "Bearer " + secret, "x-sandhi-run-id": "recovery-run",
        "x-sandhi-step-id": step, "x-sandhi-session": "recovery-session",
    }, json={**BODY, "stream": stream, "max_tokens": 64})


def wait_usage(client, count):
    deadline = time.monotonic() + 5
    while True:
        usage = get(client, "/dashboard/api/usage")
        if usage["total"]["calls"] == count:
            return usage
        assert time.monotonic() < deadline, usage
        time.sleep(0.02)


def mint(client, *, scope=SCOPES[0], expires_at=None):
    desired = {"upstream": "openai", "subject": "recovery-subject", "group": scope[6:],
               "budget_scope": scope, "models": ["gpt-mock"]}
    if expires_at:
        desired["expires_at"] = expires_at
    response = client.post("/admin/keys/share", headers=ADMIN, json=desired)
    assert response.status_code == 200, response.text
    return response.json()


def seed(gateway):
    client = gateway.client
    for scope in SCOPES:
        response = client.post("/admin/budget", headers=ADMIN, json={
            "scope": scope, "limit_tokens": 10000, "window": "total", "policy": "block",
        })
        assert response.status_code == 200, response.text
    response = client.post("/admin/alerts", headers=ADMIN,
                           json={"scope": SCOPES[0], "threshold_pct": 0})
    assert response.status_code == 201, response.text
    keys = {"active": mint(client), "other_scope": mint(client, scope=SCOPES[1]),
            "revoked": mint(client), "expired": mint(client, expires_at="2000-01-01T00:00:00Z")}
    response = client.delete("/admin/vkeys/" + keys["revoked"]["id"], headers=ADMIN)
    assert response.status_code == 200, response.text
    for name in ("active", "other_scope"):
        response = call(client, keys[name]["virtual_key"], step=name)
        assert response.status_code == 200, response.text
    wait_usage(client, 2)
    with closing(sqlite3.connect(gateway.database)) as connection:
        assert connection.execute(
            "SELECT subject_id, group_id, session_id, run_id, step_id FROM usage_events ORDER BY step_id"
        ).fetchall() == [
            ("recovery-subject", "recovery-a", "recovery-session", "recovery-run", "active"),
            ("recovery-subject", "recovery-b", "recovery-session", "recovery-run", "other_scope"),
        ]
    deadline = time.monotonic() + 5
    while not get(client, "/admin/alerts")["alerts"][0]["last_fired_at"]:
        assert time.monotonic() < deadline, "alert fire was not committed"
        time.sleep(0.02)
    return keys


def public_evidence(client):
    budgets = get(client, "/dashboard/api/budgets")["budgets"]
    return {
        "usage": get(client, "/dashboard/api/usage"),
        "budgets": sorted(budgets, key=lambda budget: budget["scope"]),
        "alerts": get(client, "/admin/alerts"),
        "keys": get(client, "/admin/keys/virtual"),
        "run": get(client, "/admin/usage/run/recovery-run"),
    }


def assert_security_state(gateway, keys):
    assert gateway.client.get("/dashboard/api/usage").status_code == 401
    assert gateway.client.get("/admin/keys/virtual").status_code == 401
    for name in ("revoked", "expired"):
        assert call(gateway.client, keys[name]["virtual_key"]).status_code in (401, 403)
    with closing(sqlite3.connect(gateway.database)) as connection:
        records = {row[0]: row[1:] for row in connection.execute(
            "SELECT id, secret_hash, revoked_at, expires_at FROM virtual_keys"
        )}
    for name, key in keys.items():
        hashed, revoked, expires = records[key["id"]]
        assert hashed == hashlib.sha256(key["virtual_key"].encode()).hexdigest()
        assert hashed != key["virtual_key"]
        assert (revoked is not None) == (name == "revoked")
        assert (expires is not None) == (name == "expired")
        assert key["virtual_key"] not in json.dumps(get(gateway.client, "/admin/keys/virtual"))


@pytest.mark.parametrize("shards", [1, 2], ids=["single-file", "two-shards"])
def test_clean_restart_and_isolated_restore_preserve_evidence_and_continue_accounting(
    recovery_gateway, gated_provider, tmp_path, shards, record_property,
):
    source = tmp_path / "source" / "state.db"
    with recovery_gateway(source, shards=shards) as gateway:
        keys = seed(gateway)
        baseline = public_evidence(gateway.client)
        assert baseline["usage"]["total"]["billable_tokens"] == 28
        assert [budget["spent"] for budget in baseline["budgets"]] == [14, 14]
        assert_security_state(gateway, keys)
        gateway.stop()
    committed = persisted_rows(source, shards)
    assert all(key["virtual_key"] not in repr(committed) for key in keys.values())
    if shards == 2:
        assert all(part.get("budget_reservation") for part in committed[1:])
    with recovery_gateway(source, shards=shards) as restarted:
        assert public_evidence(restarted.client) == baseline
        assert_security_state(restarted, keys)
        restarted.stop()
    assert persisted_rows(source, shards) == committed
    saved = tmp_path / "snapshot"
    manifest = snapshot(source, saved, shards=shards,
                        metadata=snapshot_metadata(restarted, fixture="recovery-drill"))
    assert manifest == saved / "manifest.json"
    restore_started = time.monotonic()
    recovered = restore(saved, tmp_path / "restored", expected_shards=shards)
    # A measured disposable local drill, not a production RTO promise.
    record_property("restore_elapsed_seconds", f"{time.monotonic() - restore_started:.6f}")
    record_property("executable_sha256", restarted.executable_sha256)
    record_property("source_database_count", len(committed))
    record_property("verified_source_rows", sum(len(rows) for part in committed for rows in part.values()))
    assert persisted_rows(recovered, shards) == committed
    with recovery_gateway(recovered, shards=shards) as restored:
        assert public_evidence(restored.client) == baseline
        assert_security_state(restored, keys)
        before = len(gated_provider.requests)
        response = call(restored.client, keys["active"]["virtual_key"], step="after-restore")
        assert response.status_code == 200, response.text
        total = wait_usage(restored.client, 3)["total"]
        assert total["billable_tokens"] == 42
        budget = next(row for row in get(restored.client, "/dashboard/api/budgets")["budgets"]
                      if row["scope"] == SCOPES[0])
        assert budget["spent"] == 28
        assert len(gated_provider.requests) == before + 1
        restored.stop()
    assert persisted_rows(source, shards) == committed, "restore/continued accounting mutated source"


@pytest.mark.parametrize("shards", [1, 2], ids=["single-file", "two-shards"])
def test_real_sigkill_preserves_unexpired_lease_without_fabricating_usage(
    recovery_gateway, gated_provider, tmp_path, shards,
):
    source = tmp_path / "crashed" / "state.db"
    with recovery_gateway(source, shards=shards) as gateway:
        keys = seed(gateway)
        baseline = public_evidence(gateway.client)
        with gateway.client.stream("POST", "/v1/chat/completions", headers={
            "Authorization": "Bearer " + keys["active"]["virtual_key"],
        }, json={**BODY, "stream": True, "max_tokens": 64}) as response:
            assert response.status_code == 200
            chunks = response.iter_bytes()
            assert b"data:" in next(chunks)
            assert gated_provider.entered.wait(1)
            admitted = leases(source, shards)
            dangling = [row for row in admitted if row[4] == 0]
            assert len(dangling) == 1 and dangling[0][2] > 0 and dangling[0][3] == 0
            assert dangling[0][5] > time.time(), "test must not use expired/retimed fixture leases"
            gateway.process.kill()
            assert gateway.process.wait(timeout=3) == -9
    gated_provider.release.set()
    with recovery_gateway(source, shards=shards) as restarted:
        assert leases(source, shards) == admitted
        assert public_evidence(restarted.client) == baseline
        assert len(gated_provider.requests) == 3
        with closing(sqlite3.connect(source)) as connection:
            assert connection.execute("SELECT COUNT(*) FROM usage_events").fetchone() == (2,)
        for part in persisted_rows(source, shards):
            assert part.get("budget_settlement_outbox", []) == [], "proxy must not invent inactive W05a receipts"
        # Prove the surviving lease still holds admission capacity, not merely a
        # historical row. Set the cap to exactly settled spend plus its ceiling.
        scope, _lease_id, ceiling, _actual, _settled, _expires = dangling[0]
        spent = next(row["spent"] for row in baseline["budgets"] if row["scope"] == scope)
        changed = restarted.client.post("/admin/budget", headers=ADMIN, json={
            "scope": scope, "limit_tokens": spent + ceiling, "window": "total", "policy": "block",
        })
        assert changed.status_code == 200, changed.text
        denied = call(restarted.client, keys["active"]["virtual_key"], step="held-capacity")
        assert denied.status_code == 429, denied.text
        assert len(gated_provider.requests) == 3
        assert leases(source, shards) == admitted
        assert get(restarted.client, "/dashboard/api/usage") == baseline["usage"]
        restarted.stop()


def test_old_snapshot_revocation_is_reconciled_without_provider_credentials_before_admission(
    recovery_gateway, gated_provider, tmp_path,
):
    source = tmp_path / "source" / "state.db"
    with recovery_gateway(source) as gateway:
        key = mint(gateway.client)
        gateway.stop()
    saved = tmp_path / "old-snapshot"
    snapshot(source, saved, shards=1,
             metadata=snapshot_metadata(gateway, fixture="old-security-state"))
    with recovery_gateway(source) as current:
        response = current.client.delete("/admin/vkeys/" + key["id"], headers=ADMIN)
        assert response.status_code == 200
        assert call(current.client, key["virtual_key"]).status_code == 401
        current.stop()
    recovered = restore(saved, tmp_path / "quarantined", expected_shards=1)
    # A snapshot is historical state, not a revocation oracle. Demonstrate the
    # hazard in SQLite without ever launching it with an upstream credential.
    with closing(sqlite3.connect(recovered)) as connection:
        assert connection.execute("SELECT revoked_at FROM virtual_keys WHERE id = ?", (key["id"],)).fetchone() == (None,)
    with recovery_gateway(recovered, provider_enabled=False) as quarantine:
        before = len(gated_provider.requests)
        assert call(quarantine.client, key["virtual_key"]).status_code >= 400
        assert len(gated_provider.requests) == before
        response = quarantine.client.delete("/admin/vkeys/" + key["id"], headers=ADMIN)
        assert response.status_code == 200
        assert call(quarantine.client, key["virtual_key"]).status_code == 401
        quarantine.stop()
    with recovery_gateway(recovered, provider_enabled=True) as reconciled:
        assert call(reconciled.client, key["virtual_key"]).status_code == 401
        assert gated_provider.requests == []
        reconciled.stop()
