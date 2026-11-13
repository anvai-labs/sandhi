"""Canonical startup refuses topology changes before touching historical state."""
import os
import socket
import sqlite3
import subprocess

import pytest


def _environment(tmp_path, store):
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        port = sock.getsockname()[1]
    env = {"PATH": os.environ.get("PATH", "/usr/bin:/bin"), "LANG": "C.UTF-8",
           "SANDHI_AUTH_MODE": "tokens", "SANDHI_BIND": f"127.0.0.1:{port}",
           "SANDHI_VAULT_BACKEND": "accounting-fixture-unavailable",
           "SANDHI_ADMIN_TOKEN": "synthetic-accounting-admin", "SANDHI_LOG": "warn"}
    if store is not None:
        env["SANDHI_STORE"] = str(store)
    return env


def _historical_store(path):
    with sqlite3.connect(path) as connection:
        connection.executescript("CREATE TABLE budget_limit(scope TEXT PRIMARY KEY,limit_tokens INTEGER,window TEXT NOT NULL DEFAULT 'total',policy TEXT NOT NULL DEFAULT 'block');"
            "CREATE TABLE budget_reservation(id INTEGER PRIMARY KEY AUTOINCREMENT,scope TEXT NOT NULL,ceiling INTEGER NOT NULL,actual INTEGER NOT NULL DEFAULT 0,settled INTEGER NOT NULL DEFAULT 0,settled_at INTEGER,expires_at INTEGER NOT NULL);"
            "CREATE TABLE retained_marker(value TEXT);INSERT INTO retained_marker VALUES('original');"
            "INSERT INTO budget_limit VALUES('group:original',100,'total','block'),('upstream:original',20,'daily','warn');"
            "INSERT INTO budget_reservation VALUES(41,'group:original',11,7,1,1,4102444800),(42,'group:original',13,0,0,NULL,4102444800);")


def _refusal(binary, env, tmp_path):
    process = subprocess.Popen([str(binary)], env=env, cwd=tmp_path, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    try:
        stdout, stderr = process.communicate(timeout=2)
    except subprocess.TimeoutExpired:
        process.terminate()
        try:
            stdout, stderr = process.communicate(timeout=3)
        except subprocess.TimeoutExpired:
            process.kill()
            stdout, stderr = process.communicate(timeout=2)
        pytest.fail("canonical startup bound a server or did not refuse promptly: " + stderr.decode(errors="replace"))
    assert process.returncode == 2, stderr.decode(errors="replace")
    with socket.socket() as sock:
        assert sock.connect_ex(("127.0.0.1", int(env["SANDHI_BIND"].rsplit(":",1)[1]))) != 0
    return stderr.decode(errors="replace")


@pytest.mark.parametrize("shards", ["0", "2", "64", "invalid", "", "1 "])
def test_unsupported_topology_is_rejected_before_any_store_open(proxy_binary, tmp_path, shards):
    database = tmp_path / "original.db"
    _historical_store(database)
    original = database.read_bytes()
    inventory = sorted(path.name for path in tmp_path.iterdir())
    env = _environment(tmp_path, database)
    env["SANDHI_LEDGER_SHARDS"] = shards
    diagnostic = _refusal(proxy_binary, env, tmp_path)
    assert "SANDHI_LEDGER_SHARDS=1" in diagnostic
    assert database.read_bytes() == original
    assert sorted(path.name for path in tmp_path.iterdir()) == inventory


@pytest.mark.parametrize("mode", ["off", "tracked", ""])
def test_removed_execution_mode_cannot_reenable_a_permissive_path(proxy_binary, tmp_path, mode):
    database = tmp_path / "original.db"
    _historical_store(database)
    original = database.read_bytes()
    env = _environment(tmp_path, database)
    env["SANDHI_BUFFERED_ACCOUNTING"] = mode
    diagnostic = _refusal(proxy_binary, env, tmp_path)
    assert "was removed" in diagnostic
    assert database.read_bytes() == original
    assert sorted(path.name for path in tmp_path.iterdir()) == ["original.db"]


@pytest.mark.parametrize("store", [None, "", "   ", ":memory:", "file::memory:?cache=shared", "file:original.db?mode=rwc"])
def test_canonical_startup_requires_explicit_ordinary_durable_path(proxy_binary, tmp_path, store):
    _refusal(proxy_binary, _environment(tmp_path, store), tmp_path)
    assert not list(tmp_path.iterdir())


@pytest.mark.parametrize("flag", ["--help", "--version"])
def test_informational_flags_do_not_initialize_durability(proxy_binary, tmp_path, flag):
    env = _environment(tmp_path, None)
    env.update(SANDHI_LEDGER_SHARDS="2", SANDHI_BUFFERED_ACCOUNTING="off")
    result = subprocess.run([str(proxy_binary), flag], env=env, cwd=tmp_path, capture_output=True, timeout=5)
    assert result.returncode == 0
    assert not list(tmp_path.iterdir())


@pytest.mark.parametrize("maximum", ["0", "1000001", "-1", "invalid", ""])
def test_invalid_output_default_refuses_before_store_open(proxy_binary, tmp_path, maximum):
    database = tmp_path / "original.db"
    _historical_store(database)
    original = database.read_bytes()
    env = _environment(tmp_path, database)
    env["SANDHI_DEFAULT_MAX_OUTPUT_TOKENS"] = maximum
    _refusal(proxy_binary, env, tmp_path)
    assert database.read_bytes() == original
    assert sorted(path.name for path in tmp_path.iterdir()) == ["original.db"]


def test_default_binary_initializes_the_original_ledger_and_advertises_its_contract(proxy_binary, tmp_path):
    import json
    import time
    import urllib.error
    import urllib.request

    database = tmp_path / "original.db"
    _historical_store(database)
    with sqlite3.connect(database) as connection:
        historical = {table: connection.execute(f"SELECT * FROM {table} ORDER BY 1").fetchall()
                      for table in ["budget_limit", "budget_reservation", "retained_marker"]}
    env = _environment(tmp_path, database)
    process = subprocess.Popen([str(proxy_binary)], env=env, cwd=tmp_path,
                               stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    url = f"http://{env['SANDHI_BIND']}"
    try:
        deadline = time.monotonic() + 5
        while True:
            assert process.poll() is None, process.communicate()[1].decode(errors="replace")
            try:
                request = urllib.request.Request(url + "/admin/version", headers={
                    "Authorization": "Bearer synthetic-accounting-admin"})
                with urllib.request.urlopen(request, timeout=0.2) as response:
                    version = json.load(response)
                break
            except (OSError, urllib.error.URLError):
                if time.monotonic() >= deadline:
                    pytest.fail("canonical binary did not expose its initialized accounting contract")
                time.sleep(0.02)
        assert version["capabilities"]["execution_accounting"] == "paired_durable_v1"
        assert version["capabilities"]["durable_ledger"] is True
        with urllib.request.urlopen(url + "/readyz", timeout=1) as response:
            assert response.status == 200
        with sqlite3.connect(database) as connection:
            for table, original in historical.items():
                assert connection.execute(f"SELECT * FROM {table} ORDER BY 1").fetchall() == original
            assert connection.execute("SELECT count(*) FROM budget_settlement_outbox").fetchone()[0] == 0
        assert not list(tmp_path.glob("original.db.shard-*"))
        assert not list(tmp_path.glob("original.db.shards*"))
    finally:
        if process.poll() is None:
            process.terminate()
        try:
            process.communicate(timeout=5)
        except subprocess.TimeoutExpired:
            process.kill()
            process.communicate(timeout=2)
