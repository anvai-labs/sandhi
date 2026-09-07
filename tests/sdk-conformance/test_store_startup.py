"""Configured durability must not silently become uncapped volatile enforcement.

Real binary, disposable SQLite inputs, synthetic loopback provider; no personal vault.
"""

from contextlib import closing, contextmanager
import os
from pathlib import Path
import shutil
import sqlite3
import subprocess
import time

import httpx
import pytest

from conftest import REAL_OPENAI_KEY, VK_OPENAI, _free_port
from test_shutdown import BODY, gated_provider  # noqa: F401 - synthetic provider fixture


@pytest.fixture(scope="module")
def store_startup_binary(proxy_binary, tmp_path_factory):
    # Feature fixtures can rebuild target/debug; every case uses this fixed executable.
    executable = tmp_path_factory.mktemp("store-startup-binary") / "sandhi-proxy"
    shutil.copy2(proxy_binary, executable)
    return executable


@pytest.fixture
def store_process(store_startup_binary, gated_provider, tmp_path):
    @contextmanager
    def launch(store=None, *, shards=1):
        env = {key: value for key, value in os.environ.items()
               if not key.startswith(("SANDHI_", "SENTINELPASS_"))}
        env.update(SANDHI_BIND=f"127.0.0.1:{_free_port()}",
                   SANDHI_ADMIN_TOKEN="store-startup-admin",
                   SANDHI_VAULT_BACKEND="store-fixture-unavailable",
                   SANDHI_OPENAI_KEY=REAL_OPENAI_KEY, SANDHI_OPENAI_BASE=gated_provider.base,
                   SANDHI_LEDGER_SHARDS=str(shards), SANDHI_SHUTDOWN_QUIESCE_MS="0")
        if store is not None:
            env["SANDHI_STORE"] = str(store)
        log = tmp_path / "startup.stderr"
        with log.open("wb") as stderr:
            process = subprocess.Popen([str(store_startup_binary)], cwd=tmp_path, env=env,
                                       stdout=subprocess.DEVNULL, stderr=stderr)
            try:
                with httpx.Client(base_url="http://" + env["SANDHI_BIND"], timeout=1) as client:
                    deadline = time.monotonic() + 8
                    ready = False
                    while process.poll() is None:
                        try:
                            ready = client.get("/readyz").status_code == 200
                            if ready:
                                break
                        except httpx.TransportError:
                            pass
                        assert time.monotonic() < deadline, "startup neither exited nor became ready"
                        time.sleep(0.02)
                    yield process, client, ready, log
            finally:
                if process.poll() is None:
                    process.terminate()
                    try:
                        process.wait(timeout=5)
                    except subprocess.TimeoutExpired:
                        process.kill()
                        process.wait(timeout=2)
    return launch


def assert_closed(result, component):
    process, client, ready, log = result
    assert not ready, "configured durable initialization failure admitted a listener"
    assert process.poll() == 2, log.read_text()
    with pytest.raises(httpx.TransportError):
        client.get("/readyz")
    diagnostic = f"sandhi-proxy: startup failed: configured_store_unavailable component={component}"
    assert diagnostic in log.read_text().splitlines()
    assert "falling back to in-memory" not in log.read_text()
    assert REAL_OPENAI_KEY not in log.read_text()


def test_failed_ledger_open_cannot_bypass_persisted_zero_block_cap(store_process, gated_provider, tmp_path):
    database = tmp_path / "usage.db"
    with closing(sqlite3.connect(database)) as connection:
        connection.executescript(
            "CREATE TABLE budget_limit(scope TEXT PRIMARY KEY,limit_tokens INTEGER,window TEXT,policy TEXT);"
            "INSERT INTO budget_limit VALUES('group:demo',0,'total','block');"
            # Usage/vault/key/alert schemas can initialize; only ledger indexing fails.
            "CREATE TABLE budget_reservation(id INTEGER PRIMARY KEY);"
        )
        connection.commit()
    with store_process(database) as result:
        if result[2]:
            # On the old binary this returns 200 and dispatches despite the persisted zero cap.
            response = result[1].post("/v1/chat/completions", json=BODY,
                                      headers={"Authorization": f"Bearer {VK_OPENAI}"})
            assert not gated_provider.requests, f"hard cap bypass: upstream dispatched, status={response.status_code}"
        assert_closed(result, "ledger")
    assert gated_provider.requests == []
    with closing(sqlite3.connect(database)) as connection:
        assert connection.execute("SELECT * FROM budget_limit").fetchall() == [("group:demo", 0, "total", "block")]


@pytest.mark.parametrize("fault", ["corrupt", "directory", "missing-parent"])
def test_unopenable_configured_base_fails_startup(store_process, gated_provider, tmp_path, fault):
    database = tmp_path / "usage.db"
    if fault == "corrupt":
        database.write_bytes(b"not a SQLite database")
    elif fault == "directory":
        database.mkdir()
    else:
        database = tmp_path / "missing" / "usage.db"
    with store_process(database) as result:
        assert_closed(result, "usage")
    assert gated_provider.requests == []


def test_healthy_base_with_unopenable_shard_fails_startup(store_process, gated_provider, tmp_path):
    database = tmp_path / "usage.db"
    with closing(sqlite3.connect(database)) as connection:
        connection.execute("CREATE TABLE retained_data(value TEXT)")
        connection.execute("INSERT INTO retained_data VALUES('unchanged')")
        connection.commit()
    Path(str(database) + "-ledger-shard-0.db").mkdir()
    with store_process(database, shards=2) as result:
        assert_closed(result, "ledger")
    assert gated_provider.requests == []
    with closing(sqlite3.connect(database)) as connection:
        assert connection.execute("SELECT * FROM retained_data").fetchall() == [("unchanged",)]


@pytest.mark.parametrize("component,table", [("vault", "vault"), ("vkeys", "virtual_keys"), ("alerts", "alert_rules")])
def test_configured_component_initialization_failure_is_fatal(store_process, tmp_path, component, table):
    database = tmp_path / "usage.db"
    with closing(sqlite3.connect(database)) as connection:
        connection.execute("CREATE TABLE index_target(value TEXT)")
        # An index with the required table's name makes CREATE TABLE fail, without
        # corrupting SQLite or preventing the preceding components from opening.
        connection.execute(f"CREATE INDEX {table} ON index_target(value)")
        connection.commit()
    with store_process(database) as result:
        assert_closed(result, component)


@pytest.mark.parametrize("configured", [
    "", "   ", ":memory:", "file::memory:?cache=shared",
    "file:temporary?mode=memory&cache=shared", "file:usage.db?mode=rwc",
])
def test_invalid_store_configuration_is_not_implicit_memory(store_process, configured):
    with store_process(configured) as result:
        assert_closed(result, "configuration")


@pytest.mark.skipif(os.name != "posix", reason="POSIX environment permits non-UTF-8 bytes")
def test_non_utf8_store_configuration_is_not_implicit_memory(store_process):
    with store_process(os.fsdecode(b"invalid-store-\xff")) as result:
        assert_closed(result, "configuration")


@pytest.mark.parametrize("durable", [False, True])
def test_explicit_no_store_and_valid_store_remain_healthy_with_unavailable_broker(store_process, gated_provider, tmp_path, durable):
    database = tmp_path / "usage.db"
    if durable:
        with closing(sqlite3.connect(database)) as connection:
            connection.executescript(
                "CREATE TABLE vault(provider TEXT,label TEXT,scheme TEXT,base_url TEXT,"
                "created_at TEXT,status TEXT,PRIMARY KEY(provider,label));"
                "INSERT INTO vault VALUES('openai','unavailable','bearer',NULL,"
                "'2026-09-06T00:00:00Z','active');"
            )
            connection.commit()
    with store_process(database if durable else None) as result:
        process, client, ready, _log = result
        assert ready and process.poll() is None
        if durable:
            assert "credential not activated" in _log.read_text()
        response = client.post("/v1/chat/completions", json=BODY,
                               headers={"Authorization": f"Bearer {VK_OPENAI}"})
        assert response.status_code == 200, response.text
        assert len(gated_provider.requests) == 1
