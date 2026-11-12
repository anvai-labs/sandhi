"""Browser regressions against the dashboard served by the real gateway.

Only the provider is mocked. Authentication, usage, budgets and faulted SQLite reads use the
shipped HTTP API. All credentials and databases are disposable test data; no OS-vault writes.
"""

from __future__ import annotations

import json
import os
import sqlite3
import ssl
import subprocess
import time
from dataclasses import dataclass
from pathlib import Path
from urllib.parse import unquote

import httpx
import pytest
from playwright.sync_api import Error, expect, sync_playwright

from conftest import REAL_GEMINI_KEY, REAL_OPENAI_KEY, REPO_ROOT, VK_OPENAI, _free_port
from oidc_fixture import oidc_authority  # noqa: F401 - shared HTTPS authority fixture


ADMIN_TOKEN = "dashboard-test-admin"


@dataclass
class Dashboard:
    base: str
    database: Path
    process: subprocess.Popen
    admin_token: str = ADMIN_TOKEN
    seed_request_id: str | None = None
    origin_requests_before: int = 0

    @property
    def headers(self):
        return {"Authorization": f"Bearer {self.admin_token}"}


@pytest.fixture
def dashboard(proxy_binary, upstream, tmp_path, request):
    options = getattr(request, "param", {})
    database = tmp_path / "usage.db"
    config = tmp_path / "config.json"
    config.write_text(json.dumps({"providers": [], "budgets": [], "alerts": [], "vkeys": []}))
    port = _free_port()
    env = {k: v for k, v in os.environ.items() if not k.startswith(("SANDHI_", "SENTINELPASS_"))}
    env.update({
        "SANDHI_AUTH_MODE": "tokens", "SANDHI_BIND": f"127.0.0.1:{port}",
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
    if options.get("tracked"):
        env["SANDHI_BUFFERED_ACCOUNTING"] = "tracked"
    if options.get("public"):
        env["SANDHI_DASHBOARD_PUBLIC"] = "1"
    authority = request.getfixturevalue("oidc_authority") if options.get("oidc") else None
    if authority:
        authority.redirect = f"https://localhost:{port}/auth/callback"
        policy = tmp_path / "oidc.json"
        policy.write_text(json.dumps({
            "issuer": authority.issuer, "client_id": "sandhi-browser",
            "redirect_url": authority.redirect, "ca_file": str(authority.certificate),
            "subjects": {role: {"role": role, "allow_diagnostics": options.get("diagnostics", False), "grants": {
                "fixture": {"upstream": "openai", "models": ["gpt-mock"],
                            **({"group": "dashboard"} if options.get("tracked") else {})}
            } if role == "admin" else {}} for role in ("viewer", "operator", "admin")},
        }))
        config.write_text(json.dumps({"tls": {
            "cert": str(authority.certificate), "key": str(authority.key),
        }}))
        env.pop("SANDHI_AUTH_MODE")  # Exercise the shipped SSO default.
        env["SANDHI_OIDC_CONFIG"] = str(policy)
    for option, variable in [("usage_capacity", "SANDHI_USAGE_BUFFER_CAPACITY"),
                             ("alert_capacity", "SANDHI_ALERT_BUFFER_CAPACITY")]:
        if option in options:
            env[variable] = str(options[option])
    proc = subprocess.Popen([str(proxy_binary)], env=env, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
    server = Dashboard(f"https://localhost:{port}" if authority else f"http://127.0.0.1:{port}",
                       database, proc, "fixture-access" if authority else ADMIN_TOKEN)
    server.origin_requests_before = len(upstream.requests)
    try:
        deadline = time.monotonic() + 15
        verify = ssl.create_default_context(cafile=authority.certificate) if authority else True
        with httpx.Client(base_url=server.base, timeout=2, verify=verify) as client:
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
                        "policy": "block" if options.get("tracked") else "warn", "window": "total",
                    })
                    assert response.status_code == 200, response.text
                response = client.post("/v1/chat/completions", headers={
                    **(server.headers if authority else {"Authorization": f"Bearer {VK_OPENAI}"}),
                    **({"x-sandhi-session": "sso-session", "x-sandhi-run-id": "sso-run",
                        "x-sandhi-step-id": "seed"} if options.get("tracked") else {}),
                }, json={
                    "model": "gpt-mock", "messages": [{"role": "user", "content": "ping"}],
                    **({"max_tokens": 10} if options.get("tracked") else {}),
                })
                assert response.status_code == 200, response.text
                server.seed_request_id = response.headers.get("x-sandhi-request-id")
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
def page(browser, request):
    # Only disposable browser-fixture certificates; the proxy verifies its IdP CA normally.
    context = browser.new_context(ignore_https_errors=getattr(request, "param", {}).get("tls", False))
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


def hold_authenticated_request(page, dashboard, path):
    """Expose a browser barrier only after intercepting the fixture's authenticated read."""
    held = []

    def intercept(route):
        if not held and route.request.headers.get("authorization") == dashboard.headers["Authorization"]:
            held.append(route)
            page.evaluate("document.documentElement.dataset.heldRequest = 'true'")
        else:
            route.continue_()

    page.route("**" + path, intercept)
    return held


@pytest.mark.parametrize("dashboard", [{"oidc": True, "tracked": True}], indirect=True)
@pytest.mark.parametrize("denial", ["bearer", "model", "attribution"])
def test_tracked_oidc_accounting_and_pre_admission_denials(
    dashboard, oidc_authority, upstream, denial,
):
    """A real bearer call settles; rejected proposals never own an execution."""
    tables = ("budget_execution_intent", "budget_request_correlation",
              "budget_settlement_outbox", "usage_events")

    def counts():
        with sqlite3.connect(dashboard.database) as connection:
            return tuple(connection.execute(f"SELECT COUNT(*) FROM {table}").fetchone()[0]
                         for table in tables)

    assert counts() == (1, 1, 1, 1)
    with sqlite3.connect(dashboard.database) as connection:
        rows = connection.execute(
            "SELECT c.request_id, e.subject_id, e.group_id, e.session_id, e.run_id, e.step_id, "
            "e.tokens_in, e.tokens_out, e.cache_read_tokens, r.scope, r.charged_tokens "
            "FROM budget_execution_intent i "
            "JOIN budget_request_correlation c USING (execution_id) "
            "JOIN usage_events e ON e.request_id = c.request_id "
            "JOIN budget_settlement_outbox r ON r.reservation_id = i.reservation_id"
        ).fetchall()
    assert rows == [(dashboard.seed_request_id, "admin", "dashboard", "sso-session",
                     "sso-run", "seed", 7, 3, 4, "group:dashboard", 14)]
    assert len(upstream.requests) == dashboard.origin_requests_before + 1
    headers = dashboard.headers
    body = {"model": "gpt-mock", "messages": [{"role": "user", "content": "ping"}],
            "max_tokens": 10}
    if denial == "bearer":
        headers["Authorization"] = "Bearer invalid-access"
    elif denial == "model":
        body["model"] = "not-permitted"
    else:
        headers["x-sandhi-subject-id"] = "another-user"
    with httpx.Client(base_url=dashboard.base, timeout=5,
                      verify=ssl.create_default_context(cafile=oidc_authority.certificate)) as client:
        budgets = client.get("/dashboard/api/budgets", headers=dashboard.headers)
        assert budgets.status_code == 200, budgets.text
        assert budgets.json()["budgets"] == [{
            "scope": "group:dashboard", "limit_tokens": 10000, "spent": 14,
            "remaining": 9986, "window": "total", "policy": "block",
        }]
        response = client.post("/v1/chat/completions", headers=headers, json=body)
        assert response.status_code == (401 if denial == "bearer" else 403), response.text
        after = client.get("/dashboard/api/budgets", headers=dashboard.headers)
        assert after.status_code == 200, after.text
        assert after.json() == budgets.json()
    assert counts() == (1, 1, 1, 1)
    assert len(upstream.requests) == dashboard.origin_requests_before + 1


@pytest.mark.parametrize("dashboard", [{"oidc": True}, {"oidc": True, "tracked": True}],
                         ids=["legacy", "tracked"], indirect=True)
@pytest.mark.parametrize("page", [{"tls": True}], indirect=True)
@pytest.mark.parametrize("role", ["viewer", "operator", "admin"])
def test_sso_browser_roles_cookie_mutations_and_logout(page, dashboard, oidc_authority, role):
    oidc_authority.subject = role
    page.goto(dashboard.base + "/dashboard")
    expect(page.get_by_label("Admin token", exact=True)).to_be_hidden()
    page.get_by_role("link", name="Sign in with SSO").click()
    expect(page.locator("#usage")).to_have_attribute("data-state", "ready")
    expect(page.locator("#auth-status")).to_contain_text(role)
    assert oidc_authority.exchanges == 1
    cookie = next(c for c in page.context.cookies() if c["name"] == "__Host-sandhi-session")
    assert cookie["secure"] and cookie["httpOnly"] and cookie["sameSite"] == "Lax"
    assert "__Host-sandhi-session" not in page.evaluate("document.cookie")
    expect(page.locator("#config")).to_have_attribute("data-state", "ready" if role == "admin" else "forbidden")
    page.get_by_role("navigation", name="Dashboard sections").get_by_role("link", name='Budgets', exact=True).click()
    page.get_by_text("Set a budget", exact=True).click()
    budget = page.get_by_role("button", name="Set budget", exact=True)
    if role == "viewer":
        expect(budget).to_be_disabled()
    else:
        page.get_by_label("Scope", exact=True).fill("group:sso-browser")
        page.get_by_label("Limit (tokens)").fill("700")
        budget.click()
        expect(page.locator("#budgets")).to_contain_text("group:sso-browser")
    page.get_by_role("button", name="Sign out").click()
    expect(page.locator("#usage")).to_have_attribute("data-state", "locked")
    assert "gpt-mock" not in page.locator("body").inner_text()
    page.reload()
    expect(page.get_by_role("link", name="Sign in with SSO")).to_be_visible()
    assert page.evaluate("sessionStorage.length + localStorage.length") == 0


@pytest.mark.parametrize("dashboard", [{"oidc": True}], indirect=True)
@pytest.mark.parametrize("page", [{"tls": True}], indirect=True)
def test_sso_expiry_clears_visible_data_without_refresh(page, dashboard, oidc_authority):
    oidc_authority.lifetime = 3
    page.goto(dashboard.base + "/dashboard")
    page.get_by_role("link", name="Sign in with SSO").click()
    expect(page.locator("#usage")).to_have_attribute("data-state", "ready")
    expect(page.locator("#usage")).to_have_attribute("data-state", "locked", timeout=6000)
    assert "gpt-mock" not in page.locator("body").inner_text()
    expect(page.get_by_role("link", name="Sign in with SSO")).to_be_visible()


@pytest.mark.parametrize("status,label", [
    ("reported", "0"), ("absent", "not reported"), ("malformed", "malformed"),
    ("unsupported", "unsupported"), ("unknown", "unknown"), (None, "unknown"),
])
def test_cache_reporting_is_not_inferred_from_zero(page, dashboard, status, label):
    connect(page, dashboard)
    row = {"calls": 1, "cache_read_tokens": 0}
    if status:
        row["cache_read_coverage"] = dict.fromkeys(
            ("reported", "absent", "malformed", "unsupported", "unknown"), 0)
        row["cache_read_coverage"][status] = 1
    assert page.evaluate("row => cacheRead(row)", row) == label
    assert page.evaluate("row => cacheCoverageLabel(row)", row).startswith(
        "1 / 1 reported" if status == "reported" else "unknown / 1 calls" if status is None else "0 / 1 reported")
    row["cache_read_tokens"] = 17
    if status is None:
        assert page.evaluate("row => cacheRead(row)", row) == "unknown"


def test_cache_reporting_mixed_and_invalid_coverage(page, dashboard):
    connect(page, dashboard)
    row = {"calls": 5, "cache_read_tokens": 25, "cache_read_coverage": {
        "reported": 1, "absent": 1, "malformed": 1, "unsupported": 1, "unknown": 1}}
    assert "mixed reporting" in page.evaluate("row => cacheRead(row)", row)
    assert "1 / 5 reported" in page.evaluate("row => cacheCoverageLabel(row)", row)
    row["cache_read_coverage"]["reported"] = 2
    assert page.evaluate("row => cacheRead(row)", row) == "unknown"


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

    page.get_by_role("navigation", name="Dashboard sections").get_by_role("link", name='Budgets', exact=True).click()
    page.get_by_text("Set a budget", exact=True).click()
    page.get_by_label("Scope", exact=True).fill("group:keyboard")
    page.get_by_label("Limit (tokens)").fill("700")
    page.get_by_role("button", name="Set budget", exact=True).focus()
    page.keyboard.press("Enter")
    expect(page.locator("#budgets")).to_contain_text("group:keyboard")

    page.get_by_role("navigation", name="Dashboard sections").get_by_role("link", name='Keys & credentials', exact=True).click()
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
    page.get_by_role("navigation", name="Dashboard sections").get_by_role("link", name='Budgets', exact=True).click()
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
    held = hold_authenticated_request(page, dashboard, "/dashboard/api/usage")
    page.get_by_role("button", name="Refresh", exact=True).click()
    # Loading is set before fetch reaches interception. Wait for the actual old
    # authenticated request, never the anonymous refresh started by Clear token.
    expect(page.locator("html")).to_have_attribute("data-held-request", "true")
    expect(page.locator("#usage")).to_have_attribute("data-state", "loading")
    assert held[0].request.headers["authorization"] == dashboard.headers["Authorization"]
    page.get_by_role("button", name="Clear token", exact=True).click()
    expect(page.locator("#usage")).to_have_attribute("data-state", "locked")
    try:
        held[0].fulfill(json=old_data)
    except Error:
        pass  # Cancellation may have already closed the routed request.
    expect(page.locator("#usage")).to_have_attribute("data-state", "locked")
    assert "gpt-mock" not in page.locator("#usage").inner_text()
    assert page.locator("#cards").count() == 0


@pytest.mark.parametrize("superseded", [False, True], ids=["current-fails-closed", "old-cancelled"])
def test_session_check_failure_only_invalidates_its_own_auth_revision(page, dashboard, superseded):
    connect(page, dashboard)
    held = hold_authenticated_request(page, dashboard, "/auth/session")
    page.get_by_role("button", name="Refresh", exact=True).click()
    expect(page.locator("html")).to_have_attribute("data-held-request", "true")
    assert held[0].request.headers["authorization"] == dashboard.headers["Authorization"]
    if superseded:
        # A fresh token submission cancels the pending check in the same event turn.
        page.get_by_label("Admin token", exact=True).press("Enter")
    try:
        held[0].abort("failed")
    except Error:
        pass  # A newer submission can already have cancelled the old request.
    if superseded:
        expect(page.locator("#usage")).to_have_attribute("data-state", "ready")
        expect(page.locator("#usage")).to_contain_text("gpt-mock")
        expect(page.locator("#config")).to_have_attribute("data-state", "ready")
    else:
        for panel in ("usage", "keys", "budgets", "alerts", "config", "run-tree"):
            expect(page.locator(f"#{panel}")).to_have_attribute("data-state", "locked")
        expect(page.locator("#auth-status")).to_contain_text("Access verification unavailable")
        assert page.locator("#cards").count() == 0


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
    page.get_by_role("navigation", name="Dashboard sections").get_by_role("link", name='Keys & credentials', exact=True).click()
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


@pytest.mark.parametrize("width", [390, 1440])
@pytest.mark.parametrize("theme", ["light", "dark"])
def test_served_assets_and_csp(page, dashboard, width, theme):
    page.emulate_media(color_scheme=theme)
    response = page.goto(dashboard.base + "/dashboard")
    assert response.headers["cache-control"] == "no-store"
    assert "script-src 'self';" in response.headers["content-security-policy"]
    assert "frame-ancestors 'none'" in response.headers["content-security-policy"]
    for asset, content_type in [("dashboard.js", "text/javascript"), ("dashboard.css", "text/css")]:
        response = page.request.get(dashboard.base + "/dashboard/assets/" + asset)
        assert response.status == 200
        assert response.headers["content-type"].startswith(content_type)
        assert response.headers["x-content-type-options"] == "nosniff"
    page.set_viewport_size({"width": width, "height": 844})
    assert page.evaluate("document.documentElement.scrollWidth <= window.innerWidth")

    page.get_by_label("Admin token", exact=True).fill(ADMIN_TOKEN)
    page.get_by_role("button", name="Use token", exact=True).click()
    expect(page.locator("#tables")).to_be_visible()
    primary = page.get_by_role("button", name="Use token", exact=True)
    primary.hover()
    colors = primary.evaluate("button => { const s = getComputedStyle(button); "
                              "return [s.color, s.backgroundColor]; }")
    assert colors[0] != colors[1], "primary button text disappears on hover"
    expect(page.get_by_role("navigation", name="Dashboard sections")).to_be_visible()
    assert page.evaluate("document.documentElement.scrollWidth <= window.innerWidth")
    assert page.locator(".card").evaluate_all(
        "cards => cards.every(card => card.scrollWidth <= card.clientWidth)"
    )
    tables = page.get_by_role("region", name="By model attribution")
    expect(tables).to_have_attribute("tabindex", "0")
    tables.focus()
    expect(tables).to_be_focused()


def test_workspace_navigation_history_filter_and_deep_link(page, dashboard):
    connect(page, dashboard)
    nav = page.get_by_role("navigation", name="Dashboard sections")
    expect(nav.get_by_role("link", name="Overview", exact=True)).to_have_attribute("aria-current", "page")
    expect(page.locator('[data-view="access"]')).to_be_hidden()
    nav.get_by_role("link", name="Backends & routes", exact=True).click()
    expect(page).to_have_url(dashboard.base + "/dashboard#routing-section")
    expect(page.locator("#backends")).to_have_attribute("data-state", "ready")
    expect(page.locator("#routes")).to_have_attribute("data-state", "ready")
    expect(page.locator("#usage")).to_be_hidden()
    page.go_back()
    expect(page.locator("#usage")).to_be_visible()
    before = page.locator("#cards").inner_text()
    page.get_by_role("searchbox", name="Filter attribution").fill("missing-attribution-fixture")
    expect(page.locator("#tables")).to_contain_text("No matching rows")
    assert page.locator("#cards").inner_text() == before
    page.get_by_role("searchbox", name="Filter attribution").fill("gpt-mock")
    expect(page.get_by_role("region", name="By model attribution")).to_contain_text("gpt-mock")
    nav.get_by_role("link", name="Keys & credentials", exact=True).focus()
    page.keyboard.press("Enter")
    expect(page.locator('[data-view="access"]')).to_be_visible()
    expect(page.locator("#access")).to_be_focused()
    page.reload()
    expect(page.locator('[data-view="access"]')).to_be_visible()
    expect(page.get_by_label("Admin token", exact=True)).to_have_value("")
    expect(page.locator("#keys")).to_have_attribute("data-state", "locked")
    assert "gpt-mock" not in page.locator("body").text_content()


@pytest.mark.parametrize("width", [390, 1440])
def test_routing_cache_guide_connects_operator_tasks_and_keeps_history(page, dashboard, width):
    page.set_viewport_size({"width": width, "height": 900})
    connect(page, dashboard)
    shortcut = page.get_by_role("link", name="Reuse context Follow routing, cache controls and token evidence")
    shortcut.focus()
    page.keyboard.press("Enter")
    expect(page).to_have_url(dashboard.base + "/dashboard#flow-section")
    expect(page.locator("#flow-section")).to_be_focused()
    guide = page.locator('[data-view="flow-section"]')
    expect(guide.get_by_role("list", name="Request path")).to_be_visible()
    expect(guide).to_contain_text("2 fresh + 6 read + 2 write")
    expect(guide).to_contain_text("4 uncached + 6 read")
    expect(guide).to_contain_text("do not prove that every call settled")
    expect(guide).to_contain_text("not currency saved")
    assert page.evaluate("document.documentElement.scrollWidth <= window.innerWidth")
    for link in guide.locator('a[target="_blank"]').all():
        assert link.get_attribute("href").startswith("https://anvai-labs.github.io/sandhi/")
        assert "noopener" in link.get_attribute("rel")
    guide.get_by_role("link", name="Inspect backends & routes", exact=True).click()
    expect(page.locator("#routing-section")).to_be_focused()
    expect(page.locator("#backends")).to_have_attribute("data-state", "ready")
    page.go_back()
    expect(guide).to_be_visible()
    guide.get_by_role("link", name="Inspect usage evidence", exact=True).click()
    expect(page.locator("#evidence-section")).to_be_focused()
    page.go_back()
    page.reload()
    expect(guide).to_be_visible()
    expect(page.get_by_label("Admin token", exact=True)).to_have_value("")
    expect(page.locator("#usage")).to_have_attribute("data-state", "locked")


def test_usage_labels_do_not_present_observations_as_money_or_settlement(page, dashboard):
    connect(page, dashboard)
    expect(page.locator("#cards")).to_contain_text("observed units")
    assert "billable tokens" not in page.locator("#cards").inner_text()
    expect(page.locator("#usage")).to_contain_text("do not establish per-request settlement")
    run_id = "synthetic-release-run"
    with httpx.Client(base_url=dashboard.base, timeout=3) as client:
        response = client.post("/v1/chat/completions", headers={
            "Authorization": f"Bearer {VK_OPENAI}", "x-sandhi-run-id": run_id,
            "x-sandhi-step-id": "guide-check",
        }, json={"model": "gpt-mock", "max_tokens": 10,
                 "messages": [{"role": "user", "content": "synthetic dashboard check"}]})
        assert response.status_code == 200, response.text
        deadline = time.monotonic() + 3
        while client.get(f"/admin/usage/run/{run_id}", headers=dashboard.headers).status_code != 200:
            assert time.monotonic() < deadline, "run observation did not persist"
            time.sleep(0.02)
    page.get_by_role("navigation", name="Dashboard sections").get_by_role("link", name="Run explorer", exact=True).click()
    page.get_by_label("Run ID", exact=True).fill(run_id)
    page.get_by_role("button", name="Look up", exact=True).click()
    expect(page.locator("#run-tree")).to_have_attribute("data-state", "ready")
    expect(page.locator("#run-tree")).to_contain_text("observed token units")


def test_backend_route_inventory_is_escaped_and_cleared_on_auth_change(page, dashboard):
    hostile = '<img src=x onerror="window.inventoryInjected=true">'
    backend = {"id": "preview", "provider": "fixture", "credential_ref": "fixture:default",
               "endpoint": {"endpoint_family": "openai_chat_completions"},
               "models": [{"id": hostile}], "cache_policy": {"models": {hostile: {"profile": "native_only"}}}}
    route = {"id": "coding", "models": [{"id": hostile, "targets": [{"backend": "preview", "model": hostile}]}]}
    with sqlite3.connect(dashboard.database) as connection:
        connection.execute("INSERT INTO backends (id,definition_json,revision,status,created_at,updated_at,upstream_budget_scope) VALUES (?,?,1,'active','fixture','fixture','upstream:fixture')", ("preview", json.dumps(backend)))
        connection.execute("INSERT INTO logical_routes VALUES (?,?,1,'active')", ("coding", json.dumps(route)))
    connect(page, dashboard)
    page.get_by_role("navigation", name="Dashboard sections").get_by_role("link", name="Backends & routes", exact=True).click()
    expect(page.locator("#backends")).to_have_attribute("data-state", "ready")
    expect(page.locator("#routes")).to_have_attribute("data-state", "ready")
    expect(page.locator("#backends")).to_contain_text("Unavailable")
    expect(page.locator("#backends")).to_contain_text("native_only")
    expect(page.locator("#routes")).to_contain_text(hostile)
    assert page.locator("#backends img, #routes img").count() == 0
    assert page.evaluate("window.inventoryInjected") is None
    page.get_by_label("Admin token", exact=True).fill("changed-token")
    for panel in ("backends", "routes", "topology"):
        expect(page.locator("#" + panel)).to_have_attribute("data-state", "locked")
    assert hostile not in page.locator("body").text_content()


def test_input_composition_chart_requires_complete_reporting(page, dashboard):
    data = httpx.get(dashboard.base + "/dashboard/api/usage", headers=dashboard.headers).json()
    data["total"].update(tokens_in=3, cache_read_tokens=5, cache_creation_tokens=2,
                         cache_read_coverage={"reported": 1, "absent": 0, "malformed": 0, "unsupported": 0, "unknown": 0})
    page.route("**/dashboard/api/usage", lambda route: route.fulfill(json=data))
    connect(page, dashboard)
    expect(page.get_by_role("img", name="3 fresh, 5 cache-read and 2 cache-write input tokens")).to_be_visible()
    expect(page.locator(".token-insight")).to_contain_text("50.0% cache-read share")
    expect(page.locator(".token-insight")).to_contain_text("not a percentage cost saving")
    data["total"]["cache_read_coverage"] = {"reported": 0, "absent": 1, "malformed": 0, "unsupported": 0, "unknown": 0}
    page.get_by_role("button", name="Refresh", exact=True).click()
    expect(page.locator("#usage")).to_have_attribute("data-state", "ready")
    expect(page.locator(".token-bar")).to_have_count(0)
    expect(page.locator(".token-insight")).to_contain_text("Missing measurements are not treated as zero")


def test_usage_evidence_queries_real_store_and_clears_different_failed_query(page, dashboard):
    with sqlite3.connect(dashboard.database) as connection:
        persisted_id = connection.execute("SELECT request_id FROM usage_events").fetchone()[0]
    assert persisted_id
    connect(page, dashboard)
    page.get_by_role("navigation", name="Dashboard sections").get_by_role("link", name="Usage evidence", exact=True).click()
    page.get_by_label("Exact identifier").fill(persisted_id)
    with page.expect_request("**/admin/usage/diagnostics") as pending:
        page.get_by_role("button", name="Inspect usage", exact=True).click()
    assert pending.value.post_data_json == {
        "selector": {"kind": "request", "value": persisted_id}, "limit": 100,
    }
    evidence = page.locator("#request-evidence")
    expect(evidence).to_have_attribute("data-state", "ready")
    expect(evidence).to_contain_text("1 persisted records")
    row = evidence.get_by_role("region", name="Persisted usage records").locator("tbody tr")
    expect(row).to_contain_text("gpt-mock")
    assert [cell.inner_text() for cell in row.locator("td").all()][2:6] == ["7", "4", "0", "3"]
    expect(evidence).to_contain_text("physical_attempts")
    page.get_by_label("Exact identifier").fill("missing-record")
    page.get_by_role("button", name="Inspect usage", exact=True).click()
    expect(evidence).to_contain_text("0 persisted records")
    expect(evidence).to_contain_text("No persisted records matched")
    assert "gpt-mock" not in evidence.inner_text()
    page.route("**/admin/usage/diagnostics", lambda route: route.fulfill(status=503, json={"error": "synthetic unavailable"}))
    page.get_by_label("Exact identifier").fill("different-failed-query")
    page.get_by_role("button", name="Inspect usage", exact=True).click()
    expect(evidence).to_have_attribute("data-state", "unavailable")
    assert "persisted records" not in evidence.inner_text()
    page.get_by_role("button", name="Clear token", exact=True).click()
    expect(evidence).to_have_attribute("data-state", "locked")
    expect(page.get_by_label("Exact identifier")).to_have_value("")


@pytest.mark.parametrize("dashboard", [{"oidc": True}, {"oidc": True, "diagnostics": True}],
                         ids=["read-is-insufficient", "explicit-diagnostics"], indirect=True)
@pytest.mark.parametrize("page", [{"tls": True}], indirect=True)
def test_usage_evidence_respects_separate_sso_diagnostics_permission(page, dashboard, oidc_authority, request):
    oidc_authority.subject = "viewer"
    page.goto(dashboard.base + "/dashboard")
    page.get_by_role("link", name="Sign in with SSO").click()
    expect(page.locator("#usage")).to_have_attribute("data-state", "ready")
    page.get_by_role("navigation", name="Dashboard sections").get_by_role("link", name="Usage evidence", exact=True).click()
    inspect = page.get_by_role("button", name="Inspect usage", exact=True)
    if request.node.callspec.params["dashboard"].get("diagnostics"):
        expect(inspect).to_be_enabled()
        with sqlite3.connect(dashboard.database) as connection:
            persisted_id = connection.execute("SELECT request_id FROM usage_events").fetchone()[0]
        page.get_by_label("Exact identifier").fill(persisted_id)
        inspect.click()
        expect(page.locator("#request-evidence")).to_have_attribute("data-state", "ready")
        expect(page.locator("#request-evidence")).to_contain_text("gpt-mock")
    else:
        expect(inspect).to_be_disabled()
    page.get_by_role("button", name="Sign out").click()
    expect(page.locator("#usage")).to_have_attribute("data-state", "locked")
    expect(page.locator("#request-evidence")).to_have_attribute("data-state", "locked")
    assert "gpt-mock" not in page.locator("body").text_content()
