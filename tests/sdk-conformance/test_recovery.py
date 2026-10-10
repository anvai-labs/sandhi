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
import ssl
import subprocess
import threading
import time
from contextlib import closing, contextmanager
from dataclasses import dataclass, field
from datetime import datetime, timezone
from pathlib import Path

import httpx
import pytest

from conftest import REAL_OPENAI_KEY, REPO_ROOT, _free_port
from recovery_snapshot import restore, snapshot
from oidc_fixture import oidc_authority  # noqa: F401 - same disposable HTTPS authority
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
    admin_headers: dict = field(default_factory=lambda: dict(ADMIN))
    verify: ssl.SSLContext | bool = True
    member_token: str | None = None

    def stop(self, *, expected_status=0):
        self.process.terminate()
        assert self.process.wait(timeout=6) == expected_status


@pytest.fixture
def recovery_gateway(proxy_binary, gated_provider, tmp_path, request):
    mode = getattr(request, "param", "tokens")
    assert mode in ("tokens", "oidc")
    authority = request.getfixturevalue("oidc_authority") if mode == "oidc" else None
    if authority:
        authority.access_tokens["recovery-member-access"] = "recovery-member"
    # Other feature fixtures rebuild target/debug. Every restart in a drill must
    # execute the same immutable binary, not whichever build ran most recently.
    executable = tmp_path / "recovery-proxy"
    shutil.copy2(proxy_binary, executable)
    with executable.open("rb") as binary:
        executable_sha256 = hashlib.file_digest(binary, "sha256").hexdigest()
    launches = 0

    @contextmanager
    def launch(database, *, shards=1, provider_enabled=True, tracked=False, oidc_grant=True):
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
            "SANDHI_AUTH_MODE": "tokens", "SANDHI_BIND": f"127.0.0.1:{_free_port()}",
            "SANDHI_STORE": str(database), "SANDHI_LEDGER_SHARDS": str(shards),
            "SANDHI_CONFIG": str(config), "SANDHI_ADMIN_TOKEN": "recovery-test-admin",
            "SANDHI_SHUTDOWN_GRACE_SECS": "3", "SANDHI_SHUTDOWN_QUIESCE_MS": "100",
            # No OS keyring or real broker reads, even if restored metadata is malformed.
            "SANDHI_VAULT_BACKEND": "recovery-fixture-unavailable",
        })
        if tracked:
            env["SANDHI_BUFFERED_ACCOUNTING"] = "tracked"
        verify = True
        admin_headers = dict(ADMIN)
        member_token = None
        scheme = "http"
        if authority:
            scheme = "https"
            config.write_text(json.dumps({"tls": {
                "cert": str(authority.certificate), "key": str(authority.key),
            }}))
            policy = tmp_path / f"oidc-{launches}.json"
            policy.write_text(json.dumps({
                "issuer": authority.issuer, "client_id": "sandhi-browser",
                "redirect_url": f"https://localhost:{env['SANDHI_BIND'].rsplit(':', 1)[1]}/auth/callback",
                "ca_file": str(authority.certificate),
                "subjects": {
                    "admin": {"role": "admin"},
                    "recovery-member": {"role": "viewer", "grants": {"member": {
                        "upstream": "openai", "models": ["gpt-mock"],
                        "group": "recovery-a", "budget_scope": SCOPES[0],
                    }} if oidc_grant else {}},
                },
            }))
            env.pop("SANDHI_AUTH_MODE")
            env["SANDHI_OIDC_CONFIG"] = str(policy)
            verify = ssl.create_default_context(cafile=authority.certificate)
            admin_headers = {"Authorization": "Bearer fixture-access"}
            member_token = "recovery-member-access"
        if provider_enabled:
            env.update(SANDHI_OPENAI_KEY=REAL_OPENAI_KEY, SANDHI_OPENAI_BASE=gated_provider.base)
        with (tmp_path / f"launch-{launches}.stderr").open("wb") as logs:
            process = subprocess.Popen([str(executable)], cwd=REPO_ROOT, env=env,
                                       stdout=subprocess.DEVNULL, stderr=logs)
            try:
                with httpx.Client(base_url=scheme + "://" + env["SANDHI_BIND"],
                                  timeout=3, verify=verify) as client:
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
                    yield Gateway(process, client, database, shards, executable_sha256, version.json(),
                                  admin_headers, verify, member_token)
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


def get(client, path, *, headers=ADMIN):
    response = client.get(path, headers=headers)
    assert response.status_code == 200, response.text
    return response.json()


def call(client, secret, *, step="seed", stream=False):
    return client.post("/v1/chat/completions", headers={
        "Authorization": "Bearer " + secret, "x-sandhi-run-id": "recovery-run",
        "x-sandhi-step-id": step, "x-sandhi-session": "recovery-session",
    }, json={**BODY, "stream": stream, "max_tokens": 64})


def wait_usage(client, count, *, headers=ADMIN):
    deadline = time.monotonic() + 5
    while True:
        usage = get(client, "/dashboard/api/usage", headers=headers)
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


def tracked_rows(database):
    """One read transaction observes durable state, never the lost process's RAM."""
    with closing(sqlite3.connect(database, timeout=1)) as connection:
        connection.execute("BEGIN")
        return {table: connection.execute(f"SELECT * FROM {table} ORDER BY rowid").fetchall()
                for table in ("budget_execution_intent", "budget_request_correlation",
                              "budget_dispatch_fence", "budget_terminal_observation",
                              "budget_settlement_outbox", "budget_reservation", "usage_events")}


def await_terminal(database):
    deadline = time.monotonic() + 5
    while True:
        rows = tracked_rows(database)
        if len(rows["budget_terminal_observation"]) == 1:
            return rows
        assert time.monotonic() < deadline, rows
        time.sleep(0.02)


def await_receipt(database):
    deadline = time.monotonic() + 8
    while True:
        rows = tracked_rows(database)
        if len(rows["budget_settlement_outbox"]) == 1:
            return rows
        assert time.monotonic() < deadline, rows
        time.sleep(0.02)


def observed_tokens(client):
    """Exact series for this isolated, single-request synthetic drill only."""
    response = client.get("/metrics", headers=ADMIN)
    assert response.status_code == 200, response.text
    prefix = ('sandhi_tokens_total{provider="openai",model="gpt-mock",'
              'dialect="openai",plane="transparent",outcome="success",kind="')
    return {line[len(prefix):].split('"}', 1)[0]: int(line.rsplit(" ", 1)[1])
            for line in response.text.splitlines() if line.startswith(prefix)}


@pytest.mark.parametrize("crash", [False, True], ids=["retained-retry", "lost-before-persistence"])
def test_tracked_terminal_observed_during_sqlite_contention(
    recovery_gateway, gated_provider, tmp_path, record_property, crash,
):
    source = tmp_path / "tracked-contended.db"
    gated_provider.gate_buffered = True
    outcomes = []
    with recovery_gateway(source, tracked=True) as gateway:
        configured = gateway.client.post("/admin/budget", headers=ADMIN, json={
            "scope": SCOPES[0], "limit_tokens": 10000, "window": "total", "policy": "block",
        })
        assert configured.status_code == 200, configured.text
        key = mint(gateway.client)["virtual_key"]
        assert observed_tokens(gateway.client) == {}

        def request():
            try:
                with httpx.Client(base_url=str(gateway.client.base_url), timeout=10) as client:
                    outcomes.append(call(client, key))
            except httpx.TransportError as error:
                outcomes.append(error)

        thread = threading.Thread(target=request, daemon=True)
        thread.start()
        try:
            assert gated_provider.entered.wait(5), "origin never accepted buffered request"
            admitted = tracked_rows(source)
            intent, = admitted["budget_execution_intent"]
            correlation, = admitted["budget_request_correlation"]
            fence, = admitted["budget_dispatch_fence"]
            assert correlation[0] == fence[0] == intent[0]
            assert correlation[1] and fence[1] == 1 and fence[2] is not None
            assert admitted["budget_terminal_observation"] == []
            assert admitted["budget_settlement_outbox"] == admitted["usage_events"] == []
            held, = leases(source, 1)
            assert held[1] == intent[1] and held[2] > 0 and held[3:5] == (0, 0)

            with closing(sqlite3.connect(source, timeout=1)) as blocker:
                blocker.execute("BEGIN IMMEDIATE")
                try:
                    gated_provider.release.set()
                    # Provider-send completion is insufficient. A positive gateway
                    # observation proves qualified usage reached the retained owner.
                    expected = {"fresh_input": 7, "cache_read": 4, "output": 3, "billable": 14}
                    deadline = time.monotonic() + 5
                    while observed_tokens(gateway.client) != expected:
                        assert time.monotonic() < deadline, gateway.client.get(
                            "/metrics", headers=ADMIN).text
                        time.sleep(0.02)
                    thread.join(timeout=5)
                    assert not thread.is_alive(), "accounting wait did not return"
                    response, = outcomes
                    assert isinstance(response, httpx.Response), response
                    assert response.status_code == 502, response.text
                    assert response.headers["x-sandhi-request-id"] == correlation[1]
                    # Wait for the original blocked accounting operation to end,
                    # not just its shorter HTTP wait. Survival must require a
                    # retained retry rather than completion of the first write.
                    deadline = time.monotonic() + 12
                    while True:
                        metrics = gateway.client.get("/metrics", headers=ADMIN)
                        assert metrics.status_code == 200, metrics.text
                        if "sandhi_shutdown_active_operations 0" in metrics.text.splitlines():
                            break
                        assert time.monotonic() < deadline, metrics.text
                        time.sleep(0.02)
                    assert tracked_rows(source) == admitted, "lock did not prevent publication"
                    if crash:
                        gateway.process.kill()
                        assert gateway.process.wait(timeout=3) == -9
                finally:
                    blocker.rollback()
            if not crash:
                recovered = await_receipt(source)
                terminal, = recovered["budget_terminal_observation"]
                receipt, = recovered["budget_settlement_outbox"]
                assert terminal[0] == intent[0] and terminal[3] == 14
                assert receipt[1:4] == (intent[1], held[0], 14)
                for table in ("budget_execution_intent", "budget_request_correlation",
                              "budget_dispatch_fence"):
                    assert recovered[table] == admitted[table], table
                assert leases(source, 1)[0][3:5] == (14, 1)
                assert observed_tokens(gateway.client) == expected
                gateway.stop()
                recovered = tracked_rows(source)
                # The locked best-effort sink may lose its event. It must never
                # duplicate or misattribute one; it is not a settlement outbox.
                with closing(sqlite3.connect(source)) as connection:
                    events = connection.execute(
                        "SELECT request_id, session_id, run_id, step_id, tokens_in, tokens_out, "
                        "cache_creation_tokens, cache_read_tokens FROM usage_events"
                    ).fetchall()
                assert events in ([], [(correlation[1], "recovery-session", "recovery-run",
                                       "seed", 7, 3, 0, 4)])
            record_property("executable_sha256", gateway.executable_sha256)
        finally:
            gated_provider.release.set()
            thread.join(timeout=11)
            assert not thread.is_alive(), "client thread leaked"

    with recovery_gateway(source, tracked=True) as restarted:
        assert restarted.executable_sha256 == gateway.executable_sha256
        if crash:
            changed = restarted.client.post("/admin/budget", headers=ADMIN, json={
                "scope": held[0], "limit_tokens": held[2], "window": "total", "policy": "block",
            })
            assert changed.status_code == 200, changed.text
            assert call(restarted.client, key, step="held-capacity").status_code == 429
            restarted.stop(expected_status=124)
            assert tracked_rows(source) == admitted
        else:
            assert tracked_rows(source) == recovered
            budget, = get(restarted.client, "/dashboard/api/budgets")["budgets"]
            assert budget["spent"] == 14
            restarted.stop()
            assert tracked_rows(source) == recovered
    assert len(gated_provider.requests) == 1, "accounting recovery replayed inference"


@pytest.mark.parametrize("recovery_gateway", ["tokens", "oidc"], indirect=True)
def test_tracked_sigkill_after_origin_acceptance_retains_unknown_liability(
    recovery_gateway, gated_provider, tmp_path, record_property, request,
):
    source = tmp_path / "tracked-unknown.db"
    gated_provider.gate_buffered = True
    outcomes = []
    with recovery_gateway(source, tracked=True) as gateway:
        expected_scheme = "https" if request.node.callspec.params["recovery_gateway"] == "oidc" else "http"
        assert gateway.client.base_url.scheme == expected_scheme
        key = gateway.member_token if gateway.member_token is not None else mint(gateway.client)["virtual_key"]

        def send_request():
            try:
                with httpx.Client(base_url=str(gateway.client.base_url), timeout=10,
                                  verify=gateway.verify) as client:
                    outcomes.append(call(client, key))
            except httpx.TransportError as error:
                outcomes.append(error)

        thread = threading.Thread(target=send_request, daemon=True)
        thread.start()
        try:
            assert gated_provider.entered.wait(5), "origin never accepted buffered request"
            admitted = tracked_rows(source)
            intent, = admitted["budget_execution_intent"]
            correlation, = admitted["budget_request_correlation"]
            fence, = admitted["budget_dispatch_fence"]
            assert correlation[0] == fence[0] == intent[0]
            assert correlation[1] and fence[1] == 1 and fence[2] is not None
            assert admitted["budget_terminal_observation"] == []
            assert admitted["budget_settlement_outbox"] == []
            assert admitted["usage_events"] == []
            held, = leases(source, 1)
            assert held[0] == SCOPES[0]
            assert held[1] == intent[1] and held[2] > 0 and held[3:5] == (0, 0)
            gateway.process.kill()
            assert gateway.process.wait(timeout=3) == -9
        finally:
            gated_provider.release.set()
            thread.join(timeout=5)
            assert not thread.is_alive(), "client did not observe gateway death"
        assert len(outcomes) == 1 and isinstance(outcomes[0], httpx.TransportError), outcomes
        record_property("executable_sha256", gateway.executable_sha256)

    with recovery_gateway(source, tracked=True) as restarted:
        assert restarted.executable_sha256 == gateway.executable_sha256
        # The held ceiling must still deny new work after all RAM owners are lost.
        changed = restarted.client.post("/admin/budget", headers=restarted.admin_headers, json={
            "scope": held[0], "limit_tokens": held[2], "window": "total", "policy": "block",
        })
        assert changed.status_code == 200, changed.text
        denied = call(restarted.client, key, step="held-capacity")
        assert denied.status_code == 429, denied.text
        assert denied.json()["error"]["message"] == "budget exhausted"
        restarted.stop(expected_status=124)
    assert tracked_rows(source) == admitted
    assert len(gated_provider.requests) == 1


@pytest.mark.parametrize("recovery_gateway", ["tokens", "oidc"], indirect=True)
def test_tracked_sigkill_after_terminal_persistence_recovers_one_receipt(
    recovery_gateway, gated_provider, tmp_path, record_property, request,
):
    source = tmp_path / "tracked-ready.db"
    with recovery_gateway(source, tracked=True) as gateway:
        expected_scheme = "https" if request.node.callspec.params["recovery_gateway"] == "oidc" else "http"
        assert gateway.client.base_url.scheme == expected_scheme
        configured = gateway.client.post("/admin/budget", headers=gateway.admin_headers, json={
            "scope": SCOPES[0], "limit_tokens": 10000, "window": "total", "policy": "block",
        })
        assert configured.status_code == 200, configured.text
        key = gateway.member_token if gateway.member_token is not None else mint(gateway.client)["virtual_key"]
        # Synthetic fault injection at the existing transaction boundary; this
        # does not claim a naturally occurring disk failure or a timed crash gap.
        with closing(sqlite3.connect(source, timeout=1)) as connection:
            connection.execute("""CREATE TRIGGER reject_test_receipt
                BEFORE INSERT ON budget_settlement_outbox BEGIN
                SELECT RAISE(ABORT, 'synthetic receipt failure'); END""")
            connection.commit()
        response = call(gateway.client, key)
        assert response.status_code == 502, response.text
        request_id = response.headers["x-sandhi-request-id"]
        before = await_terminal(source)
        intent, = before["budget_execution_intent"]
        terminal, = before["budget_terminal_observation"]
        assert terminal[0] == intent[0] and terminal[3] == 14
        assert before["budget_request_correlation"] == [(intent[0], request_id)]
        assert before["budget_settlement_outbox"] == []
        held, = leases(source, 1)
        assert held[0] == SCOPES[0]
        assert held[1] == intent[1] and held[3:5] == (0, 0)
        wait_usage(gateway.client, 1, headers=gateway.admin_headers)
        subject = "recovery-member" if gateway.member_token is not None else "recovery-subject"
        with closing(sqlite3.connect(source)) as connection:
            assert connection.execute(
                "SELECT subject_id, group_id, session_id, run_id, step_id FROM usage_events"
            ).fetchall() == [(subject, "recovery-a", "recovery-session", "recovery-run", "seed")]
        before = tracked_rows(source)
        gateway.process.kill()
        assert gateway.process.wait(timeout=3) == -9
        record_property("executable_sha256", gateway.executable_sha256)

    # Remove only the disposable fault while its sole writer is stopped.
    with closing(sqlite3.connect(source, timeout=1)) as connection:
        connection.execute("DROP TRIGGER reject_test_receipt")
        connection.commit()
    with recovery_gateway(source, tracked=True, oidc_grant=False) as restarted:
        assert restarted.executable_sha256 == gateway.executable_sha256
        recovered = await_receipt(source)
        receipt, = recovered["budget_settlement_outbox"]
        assert receipt[1:4] == (intent[1], held[0], 14)
        settled, = leases(source, 1)
        assert settled[1] == intent[1] and settled[3:5] == (14, 1)
        for table in ("budget_execution_intent", "budget_request_correlation",
                      "budget_dispatch_fence", "budget_terminal_observation", "usage_events"):
            assert recovered[table] == before[table], table
        budget, = get(restarted.client, "/dashboard/api/budgets", headers=restarted.admin_headers)["budgets"]
        assert budget["spent"] == 14
        if restarted.member_token is not None:
            denied = call(restarted.client, key, step="revoked-grant")
            assert denied.status_code == 403, denied.text
            assert tracked_rows(source) == recovered
            assert get(restarted.client, "/dashboard/api/budgets",
                       headers=restarted.admin_headers)["budgets"] == [budget]
            assert len(gated_provider.requests) == 1
        restarted.stop()
    # Another process boundary must preserve receipt identity, time and spend.
    with recovery_gateway(source, tracked=True, oidc_grant=False) as again:
        assert again.executable_sha256 == gateway.executable_sha256
        assert tracked_rows(source) == recovered
        budget, = get(again.client, "/dashboard/api/budgets", headers=again.admin_headers)["budgets"]
        assert budget["spent"] == 14
        again.stop()
    assert tracked_rows(source) == recovered
    assert len(gated_provider.requests) == 1
