"""Synthetic operator decision evidence, not human sign-off or production certification.

Optional SANDHI_DECISION_EVIDENCE_DIR contains allowlisted assertions and masked PNGs only.
Artifacts describe completed journey assertions, not later pytest/fixture teardown outcomes;
use the separate pytest JUnit result for suite status. No traces, HAR or cookies are saved.
"""

from contextlib import closing
from datetime import datetime, timezone
import hashlib
import hmac
import json
import os
from pathlib import Path
import sqlite3
import tempfile
import time

import pytest
from playwright.sync_api import expect

from test_broker import SECRET, broker, broker_binary  # noqa: F401 - native fake broker fixtures
from test_dashboard import browser, page  # noqa: F401 - served browser fixtures


pytestmark = [pytest.mark.skipif(os.name != "posix", reason="Synthetic Unix broker transport"),
              pytest.mark.parametrize("broker", [{"token": "read-token"}], indirect=True)]
SUBJECT = "decision-user"
GROUP = "decision-team"
SCOPE = "group:" + GROUP
SESSION = "decision-session"
RUN = "decision-run"
BODY = {"model": "gpt-mock", "messages": [{"role": "user", "content": "synthetic decision"}]}
FACT_KEYS = {"registration_status", "request_status", "persisted_calls", "billable_tokens",
             "scope_limit", "upstream_delta", "event_delta", "lease_delta", "read_only_broker",
             "attribution_verified", "displayed_key_used", "browser_run_verified",
             "error_state", "recovered_exact_reference", "metadata_unchanged"}
SAFE_COPY = {"Registered openai:default", "Budget set for " + SCOPE,
             "Data unavailable. Retry with Refresh. Unlock the broker outside Sandhi and retry explicitly",
             "Not configured or not found. No credential at the authorized reference",
             "Access denied. This operation is not available with the current access configuration."}
MASK = "input, #v-result, #config-result, #keys, #config"


@pytest.fixture
def decision_artifacts(broker_binary, request):
    configured = os.environ.get("SANDHI_DECISION_EVIDENCE_DIR")
    if configured is None:
        return None
    if not configured.strip():
        pytest.fail("decision evidence directory must not be empty")
    root = Path(configured).absolute()
    if ".." in root.parts or any(path.is_symlink() for path in [root, *root.parents]):
        pytest.fail("decision evidence directory must not contain traversal or symlinks")
    root.mkdir(mode=0o700, parents=True, exist_ok=True)
    with broker_binary.open("rb") as executable:
        digest = hashlib.file_digest(executable, "sha256").hexdigest()
    return {"directory": Path(tempfile.mkdtemp(prefix="journey-", dir=root)),
            "proxy_executable_sha256": digest, "test_id": request.node.nodeid}


def evidence(artifact, page, name, facts, *, copy_selector="#c-result"):
    assert set(facts) <= FACT_KEYS
    assert all(type(value) in (bool, int) or value in ("locked", "missing", "denied")
               for value in facts.values())
    # Never serialize HTTP bodies, headers, token hashes, environment, page HTML or URLs.
    visible_copy = page.locator(copy_selector).last.inner_text().strip()
    copy_is_allowlisted = visible_copy in SAFE_COPY
    assert copy_is_allowlisted, "refuse unexpected visible copy in decision evidence"
    displayed_key = page.locator("#v-result code").all_text_contents()
    known_secrets = [SECRET, "broker-test-admin", "read-token", "disposable-daemon-token", *displayed_key]
    # Inspect text outside masked nodes before capture; this is a fixture-specific check,
    # not a general redaction guarantee for arbitrary dashboards or real credentials.
    unmasked_text = page.locator("body").evaluate("""(body, selector) => {
      const clone = body.cloneNode(true);
      clone.querySelectorAll(selector).forEach(node => node.remove());
      return clone.textContent;
    }""", MASK)
    secret_free = all(value not in unmasked_text and value not in visible_copy for value in known_secrets)
    assert secret_free, "refuse credential-bearing decision evidence"
    if artifact is None:
        return
    document = {"format_version": 1, "journey": name,
                "test_id": artifact["test_id"],
                "captured_at_utc": datetime.now(timezone.utc).isoformat(),
                "proxy_executable_sha256": artifact["proxy_executable_sha256"],
                "visible_copy": visible_copy,
                "evidence_scope": "completed_journey_assertions_only",
                "suite_outcome": "not_reported_use_pytest_junit_including_teardown",
                "human_acceptance": "not_performed", "facts": facts}
    encoded = json.dumps(document, indent=2, sort_keys=True) + "\n"
    # All inputs and one-time secrets are masked, including the displayed key actually used.
    # Mask whole inventory/config panels defensively; only synthetic attribution remains visible.
    page.evaluate("window.scrollTo(0, 0)")
    page.wait_for_function("window.scrollY === 0 && Math.abs(document.querySelector('header').getBoundingClientRect().top) < 1")
    screenshot = page.screenshot(full_page=True, animations="disabled", mask=[page.locator(MASK)])
    directory = artifact["directory"]
    with (directory / (name + ".png")).open("xb") as output:
        output.write(screenshot)
    with (directory / (name + ".json")).open("x") as output:
        output.write(encoded)


def connect(page, client):
    capabilities = client.get("/admin/version")
    assert capabilities.status_code == 200
    assert capabilities.json()["capabilities"]["vault"]["reference_registration"] is True
    page.set_default_timeout(5000)
    page.set_default_navigation_timeout(10000)
    page.goto(str(client.base_url) + "/dashboard")
    expect(page.locator("#usage")).to_have_attribute("data-state", "locked")
    page.get_by_label("Admin token", exact=True).fill("broker-test-admin")
    page.get_by_role("button", name="Use token", exact=True).click()
    for panel in ("usage", "keys", "budgets"):
        expect(page.locator("#" + panel)).to_have_attribute("data-state", "ready")
    page.get_by_text("Add a credential", exact=True).click()
    page.get_by_label("Credential source").select_option("reference")
    expect(page.locator("#c-secret")).to_be_disabled()
    expect(page.locator("#c-secret")).to_have_value("")


def register(page, upstream, *, label="default"):
    page.locator("#c-provider").fill("openai")
    page.locator("#c-label").fill(label)
    page.locator("#c-baseurl").fill(upstream.base_url)
    with page.expect_response(lambda response: response.url.endswith("/admin/keys/reference")) as completed:
        page.get_by_role("button", name="Add", exact=True).click()
    response = completed.value
    sent = response.request.post_data_json
    assert set(sent) == {"provider", "label", "base_url"}, "reference form must never send a secret"
    assert sent["provider"] == "openai" and sent["label"] == label
    return response


def read_only(daemon):
    assert daemon.received, "the real native broker boundary must be exercised"
    assert all(list(envelope["message"]) == ["GetExternalSecret"] for envelope in daemon.received)
    grants_are_read_only = all(envelope.get("client_token") == "read-token" for envelope in daemon.received)
    assert grants_are_read_only


def call(client, key, step):
    return client.post("/v1/chat/completions", json=BODY, headers={
        "Authorization": "Bearer " + key,
        "x-sandhi-subject-id": SUBJECT, "x-sandhi-group-id": GROUP,
        "x-sandhi-session": SESSION, "x-sandhi-run-id": RUN, "x-sandhi-step-id": step,
    })


def checkpoint(database):
    with closing(sqlite3.connect(database)) as connection:
        return tuple(connection.execute("SELECT * FROM " + table + " ORDER BY rowid").fetchall()
                     for table in ("usage_events", "budget_reservation"))


def persisted(client, calls):
    deadline = time.monotonic() + 5
    while True:
        response = client.get("/dashboard/api/usage")
        assert response.status_code == 200
        usage = response.json()
        if usage["total"]["calls"] == calls:
            return usage
        assert time.monotonic() < deadline, "expected buffered usage did not persist"
        time.sleep(0.02)


def verify_attribution(page, client, database, steps):
    usage = persisted(client, len(steps))
    assert usage["total"]["billable_tokens"] == 14 * len(steps)
    assert [(row["key"], row["calls"]) for row in usage["by_subject"]] == [(SUBJECT, len(steps))]
    assert [(row["key"], row["calls"]) for row in usage["by_group"]] == [(GROUP, len(steps))]
    with closing(sqlite3.connect(database)) as connection:
        rows = connection.execute(
            "SELECT subject_id,group_id,session_id,run_id,step_id FROM usage_events ORDER BY step_id"
        ).fetchall()
        assert rows == [(SUBJECT, GROUP, SESSION, RUN, step) for step in sorted(steps)]
        assert connection.execute(
            "SELECT tokens_in,tokens_out,cache_creation_tokens,cache_read_tokens "
            "FROM usage_events ORDER BY step_id"
        ).fetchall() == [(7, 3, 0, 4)] * len(steps)
        assert connection.execute(
            "SELECT scope,SUM(actual) FROM budget_reservation WHERE settled=1 GROUP BY scope"
        ).fetchall() == [(SCOPE, 14 * len(steps))]
        assert connection.execute("SELECT COUNT(*) FROM budget_reservation WHERE settled=0").fetchone() == (0,)
    tree_response = client.get("/admin/usage/run/" + RUN)
    assert tree_response.status_code == 200
    tree = tree_response.json()["run"]
    assert tree["total"]["calls"] == len(steps)
    assert tree["total"]["billable_tokens"] == 14 * len(steps)
    assert sorted(root["step_id"] for root in tree["roots"]) == sorted(steps)
    page.get_by_role("button", name="Refresh", exact=True).click()
    expect(page.locator("#usage")).to_have_attribute("data-state", "ready")
    expect(page.locator("#usage")).to_contain_text(SUBJECT)
    expect(page.locator("#usage")).to_contain_text(GROUP)
    page.get_by_label("Run ID", exact=True).fill(RUN)
    page.get_by_role("button", name="Look up", exact=True).click()
    expect(page.locator("#run-tree")).to_have_attribute("data-state", "ready")
    expect(page.locator("#run-tree")).to_be_visible()
    expect(page.locator("#run-tree")).to_contain_text(f"Total: {14 * len(steps)} billable tokens across {len(steps)} calls")
    for step in steps:
        rendered = page.locator("#run-tree .step").filter(has_text=step)
        expect(rendered).to_have_count(1)
        expect(rendered).to_be_visible()
        expect(rendered).to_have_text(step)


def budget(page, client, limit):
    page.locator("#b-scope").fill(SCOPE)
    page.locator("#b-limit").fill(str(limit))
    page.locator("#b-window").select_option("total")
    page.locator("#b-policy").select_option("block")
    with page.expect_response(lambda response: response.url.endswith("/admin/budget")) as completed:
        page.get_by_role("button", name="Set budget", exact=True).click()
    assert completed.value.status == 200
    expect(page.locator("#b-result")).to_contain_text("Budget set for " + SCOPE)
    expect(page.locator("#budgets")).to_have_attribute("data-state", "ready")
    result = client.get("/dashboard/api/budgets")
    assert result.status_code == 200
    saved = result.json()["budgets"]
    assert len(saved) == 1
    assert (saved[0]["scope"], saved[0]["limit_tokens"], saved[0]["policy"]) == (SCOPE, limit, "block")
    verify_budget_row(page, saved[0])
    return saved[0]


def verify_budget_row(page, saved, *, timeout=5000):
    row = page.locator("#budgets tr").filter(has_text=SCOPE)
    expect(row).to_have_count(1, timeout=timeout)
    expect(row).to_be_visible(timeout=timeout)
    cells = row.locator("td")
    for index in (0, 1, 3, 4):
        expect(cells.nth(index)).to_be_visible(timeout=timeout)
    expect(cells.nth(0)).to_have_text(saved["scope"], timeout=timeout)
    expect(cells.nth(1)).to_have_text(f"{saved['spent']:,} / {saved['limit_tokens']:,}", timeout=timeout)
    expect(cells.nth(3)).to_have_text(saved["window"], timeout=timeout)
    expect(cells.nth(4)).to_have_text(saved["policy"], timeout=timeout)


def test_budget_decision_rejects_incorrect_displayed_limit(page, broker):
    client, _daemon = broker
    connect(page, client)
    page.get_by_text("Set a budget", exact=True).click()
    saved = budget(page, client, 0)
    page.locator("#budgets tr").filter(has_text=SCOPE).locator("td").nth(1).evaluate(
        "cell => cell.textContent = '0 / 999'"
    )
    # API correctness must not hide a stale/wrong numeric limit in the served UI.
    with pytest.raises(AssertionError):
        verify_budget_row(page, saved, timeout=100)
    row = page.locator("#budgets tr").filter(has_text=SCOPE)
    row.locator("td").nth(1).evaluate("cell => cell.textContent = '0 / 0'")
    row.evaluate("row => row.style.display = 'none'")
    with pytest.raises(AssertionError):
        verify_budget_row(page, saved, timeout=100)


def test_browser_onboarding_displayed_key_attribution_and_budget_decisions(
    page, broker, upstream, decision_artifacts,
):
    client, daemon = broker
    connect(page, client)
    assert register(page, upstream).status == 201
    expect(page.locator("#c-result")).to_contain_text("Registered openai:default")
    page.get_by_text("Mint a virtual key", exact=True).click()
    page.get_by_label("Upstream", exact=True).fill("openai:default")
    page.get_by_label("Subject", exact=True).fill(SUBJECT)
    page.get_by_label("Group", exact=True).fill(GROUP)
    page.get_by_label("Models (csv)").fill("gpt-mock")
    with page.expect_response(lambda response: response.url.endswith("/admin/keys/share")) as minted:
        page.get_by_role("button", name="Mint", exact=True).click()
    assert minted.value.status == 200
    expect(page.locator("#v-result code")).to_be_visible()
    key = page.locator("#v-result code").inner_text()
    displayed_matches = hmac.compare_digest(key, minted.value.json()["virtual_key"])
    assert displayed_matches, "use the actual displayed one-time credential"
    before = len(upstream.requests)
    assert call(client, key, "first-request").status_code == 200
    assert len(upstream.requests) == before + 1
    correct_upstream_authority = hmac.compare_digest(upstream.last().headers["authorization"], "Bearer " + SECRET)
    assert correct_upstream_authority, "proxy must exchange displayed virtual key for broker authority"
    verify_attribution(page, client, daemon.database, ["first-request"])
    read_only(daemon)
    evidence(decision_artifacts, page, "first-request", {
        "registration_status": 201, "request_status": 200, "persisted_calls": 1,
        "billable_tokens": 14, "attribution_verified": True, "displayed_key_used": True,
        "browser_run_verified": True, "read_only_broker": True,
    })
    page.get_by_text("Set a budget", exact=True).click()
    budget(page, client, 0)
    before_sql = checkpoint(daemon.database)
    before_upstream = len(upstream.requests)
    denied = call(client, key, "denied-request")
    assert denied.status_code == 429
    assert len(upstream.requests) == before_upstream
    assert checkpoint(daemon.database) == before_sql
    evidence(decision_artifacts, page, "budget-denial", {
        "request_status": 429, "scope_limit": 0, "upstream_delta": 0,
        "event_delta": 0, "lease_delta": 0, "read_only_broker": True,
    }, copy_selector="#b-result")
    budget(page, client, 100000)
    assert call(client, key, "after-budget-recovery").status_code == 200
    assert len(upstream.requests) == before_upstream + 1
    verify_attribution(page, client, daemon.database, ["first-request", "after-budget-recovery"])
    read_only(daemon)
    evidence(decision_artifacts, page, "budget-recovery", {
        "request_status": 200, "scope_limit": 100000, "persisted_calls": 2,
        "billable_tokens": 28, "attribution_verified": True, "browser_run_verified": True,
        "read_only_broker": True,
    }, copy_selector="#b-result")


@pytest.mark.parametrize("failure,status,code,copy", [
    ("locked", 423, "vault_locked", "Unlock the broker outside Sandhi and retry explicitly"),
    ("missing", 404, "vault_missing", "No credential at the authorized reference"),
    ("denied", 403, "vault_denied", "Access denied. This operation is not available with the current access configuration."),
])
def test_browser_broker_error_decision_and_exact_reference_recovery(
    page, broker, upstream, decision_artifacts, failure, status, code, copy,
):
    client, daemon = broker
    daemon.mode = failure if failure != "denied" else "normal"
    connect(page, client)
    before = len(upstream.requests)
    # The read grant covers only default. A different exact label is actually denied by IPC.
    failed = register(page, upstream, label="other" if failure == "denied" else "default")
    assert failed.status == status
    assert failed.json()["code"] == code
    expect(page.locator("#toasts .toast").last).to_contain_text(copy)
    assert client.get("/admin/keys").json()["keys"] == []
    assert checkpoint(daemon.database) == ([], [])
    assert len(upstream.requests) == before
    read_only(daemon)
    evidence(decision_artifacts, page, failure + "-registration", {
        "registration_status": status, "error_state": failure, "metadata_unchanged": True,
        "upstream_delta": 0, "event_delta": 0, "lease_delta": 0, "read_only_broker": True,
    }, copy_selector="#toasts .toast")
    daemon.mode = "normal"
    assert register(page, upstream, label="default").status == 201
    expect(page.locator("#c-result")).to_contain_text("Registered openai:default")
    records = client.get("/admin/keys").json()["keys"]
    assert [(row["provider"], row["label"], row["status"]) for row in records] == [("openai", "default", "active")]
    assert len(upstream.requests) == before
    read_only(daemon)
    evidence(decision_artifacts, page, failure + "-recovery", {
        "registration_status": 201, "error_state": failure,
        "recovered_exact_reference": True, "read_only_broker": True,
    })
