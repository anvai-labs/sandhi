"""W02 commit truth against real HTTP handlers, SQLite failures and process restart."""

import json
import os
import sqlite3
import subprocess
import time
from concurrent.futures import ThreadPoolExecutor

import httpx
import pytest
from playwright.sync_api import expect

from conftest import REPO_ROOT, _free_port
from test_dashboard import ADMIN_TOKEN, browser, connect, dashboard, page  # noqa: F401


def write_config(dashboard, config):
    dashboard.database.with_name("config.json").write_text(json.dumps(config))


def fail_inserts(dashboard, table, condition="1"):
    # Targets are test constants in this module, never operator input.
    with sqlite3.connect(dashboard.database) as conn:
        conn.execute(f"CREATE TRIGGER reject_write BEFORE INSERT ON {table} "
                     f"WHEN {condition} BEGIN SELECT RAISE(ABORT, 'synthetic-write-failure'); END")


def budgets(client):
    response = client.get("/admin/budget")
    assert response.status_code == 200
    return response.json()


@pytest.mark.parametrize("changes", [
    {"scope": " "}, {"window": "weekly"}, {"policy": "blok"},
    {"limit_tokens": 2**63}, {"alert_thresholds": [50, 101]},
])
def test_invalid_budget_intent_has_no_side_effects(dashboard, changes):
    with httpx.Client(base_url=dashboard.base, headers=dashboard.headers) as client:
        before = budgets(client)
        response = client.post("/admin/budget", json={
            "scope": "group:dashboard", "limit_tokens": 50, **changes,
        })
        assert response.status_code == 400, response.text
        assert budgets(client) == before
        assert client.get("/admin/alerts").json()["alerts"] == []


@pytest.mark.parametrize("fault", [False, True])
def test_budget_commit_and_failed_update_survive_restart(dashboard, proxy_binary, fault):
    desired = {"scope": "group:dashboard", "limit_tokens": 71, "window": "daily", "policy": "block"}
    with httpx.Client(base_url=dashboard.base, headers=dashboard.headers) as client:
        before = budgets(client)
        if fault:
            fail_inserts(dashboard, "budget_limit")
        response = client.post("/admin/budget", json=desired)
        assert response.status_code == (503 if fault else 200), response.text
        assert "synthetic-write-failure" not in response.text
        live = budgets(client)
        assert live == (before if fault else {"budgets": [desired]})
    # Stop the actual server and boot a fresh process on the same database.
    dashboard.process.terminate()
    dashboard.process.communicate(timeout=10)
    port = _free_port()
    env = {k: v for k, v in os.environ.items() if not k.startswith(("SANDHI_", "SENTINELPASS_"))}
    env.update(SANDHI_BIND=f"127.0.0.1:{port}", SANDHI_STORE=str(dashboard.database),
               SANDHI_ADMIN_TOKEN=ADMIN_TOKEN)
    process = subprocess.Popen([str(proxy_binary)], env=env, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
    try:
        with httpx.Client(base_url=f"http://127.0.0.1:{port}", headers=dashboard.headers, timeout=2) as client:
            deadline = time.monotonic() + 15
            while True:
                assert process.poll() is None, "restarted gateway exited"
                try:
                    if client.get("/healthz").status_code == 200:
                        break
                except httpx.TransportError:
                    pass
                assert time.monotonic() < deadline, "restart timed out"
                time.sleep(0.05)
            assert budgets(client) == live
    finally:
        process.terminate()
        try:
            process.communicate(timeout=10)
        except subprocess.TimeoutExpired:
            process.kill()
            process.communicate(timeout=5)


def test_concurrent_budget_writers_publish_committed_order(dashboard):
    def update(i):
        response = httpx.post(dashboard.base + "/admin/budget", headers=dashboard.headers, json={
            "scope": "group:dashboard", "limit_tokens": i,
            "window": "daily" if i % 2 else "monthly", "policy": "warn" if i % 2 else "block",
        })
        assert response.status_code == 200, response.text
    with ThreadPoolExecutor(max_workers=8) as workers:
        list(workers.map(update, range(1, 33)))
    with sqlite3.connect(dashboard.database) as conn:
        limit, window, policy = conn.execute(
            "SELECT limit_tokens, window, policy FROM budget_limit WHERE scope='group:dashboard'"
        ).fetchone()
    response = httpx.get(dashboard.base + "/admin/budget", headers=dashboard.headers)
    assert response.json()["budgets"] == [{"scope": "group:dashboard", "limit_tokens": limit,
                                            "window": window, "policy": policy}]


def test_inline_alert_failure_reports_committed_budget(dashboard):
    fail_inserts(dashboard, "alert_rules", "NEW.threshold_pct = 90")
    response = httpx.post(dashboard.base + "/admin/budget", headers=dashboard.headers, json={
        "scope": "group:dashboard", "limit_tokens": 200, "alert_thresholds": [50, 90],
    })
    assert response.status_code == 503
    data = response.json()
    assert data["ok"] is False and data["budget_applied"] is True
    assert [a["threshold_pct"] for a in data["alerts_created"]] == [50]
    assert [a["threshold_pct"] for a in data["alerts_failed"]] == [90]
    with sqlite3.connect(dashboard.database) as conn:
        assert conn.execute("SELECT limit_tokens FROM budget_limit").fetchone()[0] == 200
        assert conn.execute("SELECT threshold_pct FROM alert_rules").fetchall() == [(50,)]


def test_cli_reports_partial_budget_and_exits_nonzero(dashboard):
    subprocess.run(["cargo", "build", "-p", "sandhi-proxy", "--bin", "sandhi"],
                   cwd=REPO_ROOT, check=True, capture_output=True, timeout=120)
    fail_inserts(dashboard, "alert_rules")
    result = subprocess.run([
        str(REPO_ROOT / "target/debug/sandhi"), "--admin-url", dashboard.base,
        "--admin-token", ADMIN_TOKEN, "budget", "set", "group:dashboard", "200", "--alert", "90",
    ], capture_output=True, text=True, timeout=15)
    assert result.returncode == 1, result.stderr
    assert json.loads(result.stdout)["budget_applied"] is True
    assert "budget committed" in result.stderr


def test_failed_budget_ui_does_not_retain_previous_success(page, dashboard):
    connect(page, dashboard)
    page.get_by_text("Set a budget", exact=True).click()
    page.get_by_label("Scope", exact=True).fill("group:dashboard")
    page.get_by_label("Limit (tokens)").fill("700")
    page.get_by_role("button", name="Set budget", exact=True).click()
    expect(page.locator("#b-result")).to_contain_text("Budget set")
    fail_inserts(dashboard, "budget_limit")
    page.get_by_label("Limit (tokens)").fill("800")
    page.get_by_role("button", name="Set budget", exact=True).click()
    expect(page.locator("#toasts")).to_contain_text("budget unchanged")
    expect(page.locator("#b-result")).to_be_empty()


def test_config_invalid_budget_preflight_prevents_earlier_valid_write(dashboard):
    write_config(dashboard, {"budgets": [
        {"scope": "group:dashboard", "limit_tokens": 1},
        {"scope": "group:new", "limit_tokens": 1, "policy": "blok"},
    ]})
    with httpx.Client(base_url=dashboard.base, headers=dashboard.headers) as client:
        before = budgets(client)
        response = client.post("/admin/config/apply")
        assert response.status_code == 400
        assert budgets(client) == before


def test_config_failed_budget_does_not_create_dependent_alerts(dashboard):
    fail_inserts(dashboard, "budget_limit", "NEW.scope = 'group:failed'")
    write_config(dashboard, {"budgets": [
        {"scope": "group:failed", "limit_tokens": 2, "alert_thresholds": [80]},
        {"scope": "group:good", "limit_tokens": 3},
    ]})
    response = httpx.post(dashboard.base + "/admin/config/apply", headers=dashboard.headers)
    assert response.status_code == 503
    data = response.json()
    assert data["ok"] is False
    assert data["budgets"]["applied"] == [{"scope": "group:good", "limit_tokens": 3}]
    assert data["failures"][0]["scope"] == "group:failed"
    assert data["alerts"] == {"created": [], "skipped": []}


@pytest.mark.parametrize("table,config,component", [
    ("alert_rules", {"alerts": [{"scope": "g", "threshold_pct": 50}]}, "alert"),
    ("virtual_keys", {"vkeys": [{"upstream": "openai", "subject": "smoke"}]}, "vkey"),
])
def test_config_store_failures_are_not_success_or_skipped(dashboard, table, config, component):
    fail_inserts(dashboard, table)
    write_config(dashboard, config)
    response = httpx.post(dashboard.base + "/admin/config/apply", headers=dashboard.headers)
    assert response.status_code == 503, response.text
    assert response.json()["failures"][0]["component"] == component
    assert "synthetic-write-failure" not in response.text
    assert response.json()["alerts"]["skipped"] == []
    assert response.json()["vkeys"]["skipped"] == []


@pytest.mark.parametrize("table,config", [
    ("alert_rules", {"alerts": [{"scope": "g", "threshold_pct": 50}]}),
    ("virtual_keys", {"vkeys": [{"upstream": "openai"}]}),
])
def test_config_dedup_read_failure_is_not_an_empty_inventory(dashboard, table, config):
    with sqlite3.connect(dashboard.database) as conn:
        conn.execute(f"DROP TABLE {table}")
    write_config(dashboard, config)
    response = httpx.post(dashboard.base + "/admin/config/apply", headers=dashboard.headers)
    assert response.status_code == 503
    assert len(response.json()["failures"]) == 1
    assert response.json()["alerts"]["skipped"] == []
    assert response.json()["vkeys"]["skipped"] == []


def test_partial_config_ui_preserves_one_time_key_and_reports_failure(page, dashboard):
    write_config(dashboard, {"vkeys": [{"upstream": "openai", "subject": "smoke"},
                                       {"upstream": "not-configured", "subject": "failed"}]})
    connect(page, dashboard)
    page.get_by_role("button", name="Apply config", exact=True).click()
    result = page.locator("#config-result")
    expect(result).to_contain_text("Incomplete")
    expect(result).to_contain_text("not rolled back")
    expect(result).to_contain_text("not-configured")
    expect(result).to_contain_text("vk_")
    # A retry skips the already-minted key and still reports the failing item.
    response = httpx.post(dashboard.base + "/admin/config/apply", headers=dashboard.headers)
    assert response.status_code == 503
    assert response.json()["vkeys"]["minted"] == []
    assert len(response.json()["vkeys"]["skipped"]) == 1
    page.get_by_role("button", name="Clear token", exact=True).click()
    expect(result).to_be_empty()


def test_config_success_and_retry_are_truthful(dashboard):
    write_config(dashboard, {"budgets": [{"scope": "g", "limit_tokens": 20, "alert_thresholds": [50]}],
                             "vkeys": [{"upstream": "openai", "subject": "smoke"}]})
    with httpx.Client(base_url=dashboard.base, headers=dashboard.headers) as client:
        first = client.post("/admin/config/apply")
        assert first.status_code == 200, first.text
        assert first.json()["ok"] is True and first.json()["failures"] == []
        assert len(first.json()["alerts"]["created"]) == 1
        assert len(first.json()["vkeys"]["minted"]) == 1
        second = client.post("/admin/config/apply")
        assert second.status_code == 200
        assert len(second.json()["alerts"]["skipped"]) == 1
        assert len(second.json()["vkeys"]["skipped"]) == 1
