"""Browser regressions against the dashboard served by the real gateway.

Only the provider is mocked. Authentication, usage, budgets and faulted SQLite reads use the
shipped HTTP API. All credentials and databases are disposable test data; no OS-vault writes.
"""

from __future__ import annotations

import json
import os
import sqlite3
import subprocess
import time
from dataclasses import dataclass
from pathlib import Path
from urllib.parse import unquote

import httpx
import pytest
from playwright.sync_api import Error, expect, sync_playwright

from conftest import REAL_GEMINI_KEY, REAL_OPENAI_KEY, REPO_ROOT, VK_OPENAI, _free_port


ADMIN_TOKEN = "dashboard-test-admin"


@dataclass
class Dashboard:
    base: str
    database: Path
    process: subprocess.Popen

    @property
    def headers(self):
        return {"Authorization": f"Bearer {ADMIN_TOKEN}"}


@pytest.fixture
def dashboard(proxy_binary, upstream, tmp_path, request):
    options = getattr(request, "param", {})
    database = tmp_path / "usage.db"
    config = tmp_path / "config.json"
    config.write_text(json.dumps({"providers": [], "budgets": [], "alerts": [], "vkeys": []}))
    port = _free_port()
    env = {k: v for k, v in os.environ.items() if not k.startswith(("SANDHI_", "SENTINELPASS_"))}
    env.update({
        "SANDHI_BIND": f"127.0.0.1:{port}",
        "SANDHI_OPENAI_KEY": REAL_OPENAI_KEY,
        "SANDHI_OPENAI_BASE": upstream.base_url,
        "SANDHI_GEMINI_KEY": REAL_GEMINI_KEY,
        "SANDHI_GEMINI_BASE": upstream.base_url,
        "SANDHI_CONFIG": str(config),
    })
    if options.get("admin", True):
        env["SANDHI_ADMIN_TOKEN"] = ADMIN_TOKEN
    if options.get("store", True):
        env["SANDHI_STORE"] = str(database)
    if options.get("public"):
        env["SANDHI_DASHBOARD_PUBLIC"] = "1"
    proc = subprocess.Popen([str(proxy_binary)], env=env, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
    server = Dashboard(f"http://127.0.0.1:{port}", database, proc)
    try:
        deadline = time.monotonic() + 15
        with httpx.Client(base_url=server.base, timeout=2) as client:
            while True:
                if proc.poll() is not None:
                    pytest.fail(f"proxy exited: {proc.stderr.read().decode()}")
                try:
                    if client.get("/healthz").status_code == 200:
                        break
                except httpx.TransportError:
                    pass
                if time.monotonic() >= deadline:
                    pytest.fail("dashboard proxy startup timed out")
                time.sleep(0.05)
            if options.get("store", True):
                if options.get("admin", True):
                    response = client.post("/admin/budget", headers=server.headers, json={
                        "scope": "group:dashboard", "limit_tokens": 10000,
                        "policy": "warn", "window": "total",
                    })
                    assert response.status_code == 200, response.text
                response = client.post("/v1/chat/completions", headers={"Authorization": f"Bearer {VK_OPENAI}"}, json={
                    "model": "gpt-mock", "messages": [{"role": "user", "content": "ping"}],
                })
                assert response.status_code == 200, response.text
                # The production observer is buffered; wait for the seeded call to be queryable.
                while client.get("/dashboard/api/usage", headers=server.headers).json()["total"]["calls"] != 1:
                    assert time.monotonic() < deadline, "usage never persisted"
                    time.sleep(0.05)
        yield server
    finally:
        proc.terminate()
        try:
            proc.communicate(timeout=10)
        except subprocess.TimeoutExpired:
            proc.kill()
            proc.communicate(timeout=5)


@pytest.fixture(scope="module")
def browser():
    with sync_playwright() as playwright:
        browser = playwright.chromium.launch()
        yield browser
        browser.close()


@pytest.fixture
def page(browser):
    context = browser.new_context()
    page = context.new_page()
    errors = []
    page.on("pageerror", lambda error: errors.append(str(error)))
    yield page
    context.close()
    assert not errors, f"dashboard JavaScript errors: {errors}"


def connect(page, dashboard):
    page.goto(dashboard.base + "/dashboard")
    page.get_by_label("Admin token", exact=True).fill(ADMIN_TOKEN)
    page.get_by_label("Admin token", exact=True).press("Enter")
    expect(page.locator("#usage")).to_have_attribute("data-state", "ready")


def test_authenticated_keyboard_journey_and_one_time_key(page, dashboard):
    page.goto(dashboard.base + "/dashboard")
    expect(page.locator("#usage")).to_have_attribute("data-state", "locked")
    assert page.locator("#cards").count() == 0
    page.get_by_label("Admin token", exact=True).fill("wrong-token")
    page.get_by_label("Admin token", exact=True).press("Enter")
    expect(page.locator("#usage")).to_have_attribute("data-state", "locked")
    page.get_by_label("Admin token", exact=True).fill(ADMIN_TOKEN)
    page.get_by_label("Admin token", exact=True).press("Enter")
    for panel in ("usage", "keys", "budgets", "alerts", "config"):
        expect(page.locator(f"#{panel}")).to_have_attribute("data-state", "ready")
    expect(page.locator("#usage")).to_contain_text("gpt-mock")
    expect(page.locator("#budgets")).to_contain_text("group:dashboard")
    page.screenshot(path=str(REPO_ROOT / "target/dashboard-authenticated.png"), full_page=True)
    assert page.evaluate("sessionStorage.getItem('sandhi_admin_token')") is None

    page.get_by_text("Set a budget", exact=True).click()
    page.get_by_label("Scope", exact=True).fill("group:keyboard")
    page.get_by_label("Limit (tokens)").fill("700")
    page.get_by_role("button", name="Set budget", exact=True).focus()
    page.keyboard.press("Enter")
    expect(page.locator("#budgets")).to_contain_text("group:keyboard")

    page.get_by_text("Mint a virtual key", exact=True).click()
    page.get_by_label("Upstream", exact=True).fill("openai")
    page.get_by_label("Subject", exact=True).fill("keyboard-user")
    page.get_by_role("button", name="Mint", exact=True).click()
    expect(page.locator("#keys")).to_contain_text("keyboard-user")
    expect(page.locator("#v-result code")).to_be_visible()
    secret = page.locator("#v-result code").inner_text()
    assert secret
    page.get_by_role("button", name="Refresh", exact=True).click()
    expect(page.locator("#keys")).to_have_attribute("data-state", "ready")
    expect(page.locator("#v-result code")).to_have_text(secret)

    page.get_by_role("button", name="Clear token", exact=True).click()
    expect(page.locator("#keys")).to_have_attribute("data-state", "locked")
    assert "keyboard-user" not in page.locator("body").inner_text()
    assert secret not in page.locator("body").inner_text()
    assert page.locator("#v-result").inner_text() == ""
    page.reload()
    expect(page.get_by_label("Admin token", exact=True)).to_have_value("")


@pytest.mark.parametrize("dashboard", [{"public": True}, {"admin": False}], indirect=True)
def test_public_reads_remain_available_but_admin_actions_are_locked(page, dashboard):
    page.goto(dashboard.base + "/dashboard")
    expect(page.locator("#usage")).to_have_attribute("data-state", "ready")
    expect(page.locator("#usage")).to_contain_text("gpt-mock")
    expect(page.locator("#config")).to_have_attribute("data-state", "locked")
    page.get_by_text("Set a budget", exact=True).click()
    expect(page.get_by_role("button", name="Set budget", exact=True)).to_be_disabled()


@pytest.mark.parametrize("dashboard", [{"store": False}], indirect=True)
def test_missing_store_is_not_zero_usage(page, dashboard):
    page.goto(dashboard.base + "/dashboard")
    page.get_by_label("Admin token", exact=True).fill(ADMIN_TOKEN)
    page.get_by_role("button", name="Use token", exact=True).click()
    expect(page.locator("#usage")).to_have_attribute("data-state", "unconfigured")
    assert page.locator("#cards").count() == 0


@pytest.mark.parametrize("status,state", [(401, "locked"), (403, "forbidden"), (404, "unconfigured"), (503, "unavailable"), (200, "unavailable")])
def test_failed_or_incomplete_reads_never_become_empty_success(page, dashboard, status, state):
    page.route("**/dashboard/api/usage", lambda route: route.fulfill(status=status, json={"error": "fixture failure"}))
    page.goto(dashboard.base + "/dashboard")
    page.get_by_label("Admin token", exact=True).fill(ADMIN_TOKEN)
    page.get_by_role("button", name="Use token", exact=True).click()
    expect(page.locator("#usage")).to_have_attribute("data-state", state)
    assert page.locator("#cards").count() == 0


def test_failed_refresh_marks_previous_data_stale(page, dashboard):
    connect(page, dashboard)
    page.route("**/dashboard/api/usage", lambda route: route.abort("failed"))
    page.get_by_role("button", name="Refresh", exact=True).click()
    expect(page.locator("#usage")).to_have_attribute("data-state", "stale")
    expect(page.locator("#usage")).to_contain_text("Showing stale data loaded at")
    expect(page.locator("#usage")).to_contain_text("gpt-mock")


def test_old_authenticated_response_cannot_restore_data_after_clear(page, dashboard):
    connect(page, dashboard)
    with httpx.Client() as client:
        old_data = client.get(dashboard.base + "/dashboard/api/usage", headers=dashboard.headers).json()
    held = []
    page.route("**/dashboard/api/usage", lambda route: held.append(route), times=1)
    page.get_by_role("button", name="Refresh", exact=True).click()
    expect(page.locator("#usage")).to_have_attribute("data-state", "loading")
    page.get_by_role("button", name="Clear token", exact=True).click()
    expect(page.locator("#usage")).to_have_attribute("data-state", "locked")
    assert held
    try:
        held[0].fulfill(json=old_data)
    except Error:
        pass  # Cancellation may have already closed the routed request.
    expect(page.locator("#usage")).to_have_attribute("data-state", "locked")
    assert "gpt-mock" not in page.locator("#usage").inner_text()


def test_credential_metadata_is_inert_and_actions_preserve_exact_labels(page, dashboard):
    label = "qa');window.dashboardInjected=true;//\"<&"
    with sqlite3.connect(dashboard.database) as conn:
        conn.execute("INSERT INTO vault(provider,label,scheme,created_at,status) VALUES (?,?,?,?,?)",
                     ("openai", label, "bearer", "2026-09-04T00:00:00Z", "active"))
    connect(page, dashboard)
    expect(page.locator("#keys")).to_contain_text(label)
    assert page.locator("[onclick], [onerror], script:not([src])").count() == 0
    assert page.evaluate("window.dashboardInjected") is None
    deletes = []

    def intercept_delete(route):
        deletes.append(route.request.url)
        route.fulfill(json={"deleted": True})

    page.route("**/admin/keys/openai/*", intercept_delete)
    page.locator('[data-action="revokeCred"]').click()
    expect(page.locator("#toasts")).to_contain_text("Credential disabled locally")
    assert unquote(deletes[0].split("/admin/keys/openai/", 1)[1]) == label
    assert page.evaluate("window.dashboardInjected") is None


@pytest.mark.parametrize("table,panel", [
    ("usage_events", "usage"), ("virtual_keys", "keys"), ("vault", "keys"),
    ("alert_rules", "alerts"), ("budget_reservation", "budgets"),
])
def test_real_store_read_failure_is_503(dashboard, table, panel):
    # Fault only this test's disposable database. The running gateway retains its connections.
    with sqlite3.connect(dashboard.database) as conn:
        conn.execute(f"DROP TABLE {table}")
    response = httpx.get(dashboard.base + f"/dashboard/api/{panel}", headers=dashboard.headers)
    assert response.status_code == 503, response.text
    assert response.headers["cache-control"] == "no-store"
    assert "dashboard data unavailable" in response.text
    assert table not in response.text


def test_served_assets_and_csp(page, dashboard):
    response = page.goto(dashboard.base + "/dashboard")
    assert response.headers["cache-control"] == "no-store"
    assert "script-src 'self';" in response.headers["content-security-policy"]
    assert "frame-ancestors 'none'" in response.headers["content-security-policy"]
    for asset, content_type in [("dashboard.js", "text/javascript"), ("dashboard.css", "text/css")]:
        response = page.request.get(dashboard.base + "/dashboard/assets/" + asset)
        assert response.status == 200
        assert response.headers["content-type"].startswith(content_type)
        assert response.headers["x-content-type-options"] == "nosniff"
    page.set_viewport_size({"width": 390, "height": 844})
    assert page.evaluate("document.documentElement.scrollWidth <= window.innerWidth")
