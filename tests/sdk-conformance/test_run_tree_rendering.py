"""Served run lookup consumes the real API envelope; invalid data is never zero success."""

import copy
import time

import httpx
import pytest
from playwright.sync_api import expect

from conftest import VK_OPENAI
from test_dashboard import browser, page, dashboard, connect  # noqa: F401


@pytest.fixture
def visible_run(page, dashboard):
    run_id = "rendered-run"
    hostile_step = '<img src=x onerror="window.runInjected=true">'
    with httpx.Client(base_url=dashboard.base, headers=dashboard.headers, timeout=3) as client:
        response = client.post("/v1/chat/completions", headers={
            "Authorization": "Bearer " + VK_OPENAI,
            "x-sandhi-run-id": run_id, "x-sandhi-step-id": hostile_step,
        }, json={"model": "gpt-mock", "messages": [{"role": "user", "content": "synthetic"}]})
        assert response.status_code == 200, response.text
        deadline = time.monotonic() + 5
        while True:
            response = client.get("/admin/usage/run/" + run_id)
            if response.status_code == 200:
                break
            assert response.status_code == 404
            assert time.monotonic() < deadline, "run did not persist"
            time.sleep(.02)
        document = response.json()
    assert document["run"]["total"]["billable_tokens"] == 14
    connect(page, dashboard)
    page.get_by_label("Run ID", exact=True).fill(run_id)
    page.get_by_role("button", name="Look up", exact=True).click()
    expect(page.locator("#run-tree")).to_have_attribute("data-state", "ready")
    expect(page.locator("#run-tree .callout")).to_have_text("Total: 14 billable tokens across 1 calls")
    expect(page.locator("#run-tree .step")).to_have_text(hostile_step)
    assert page.locator("#run-tree img").count() == 0
    assert page.evaluate("window.runInjected") is None
    return document


def test_real_wrapped_run_and_missing_lookup_do_not_reuse_previous_results(page, visible_run):
    page.get_by_label("Run ID", exact=True).fill("missing-run")
    page.get_by_role("button", name="Look up", exact=True).click()
    expect(page.locator("#run-tree")).to_have_attribute("data-state", "unconfigured")
    expect(page.locator("#run-tree")).to_contain_text("unknown run id")
    assert page.locator("#run-tree .step").count() == 0
    assert "Total:" not in page.locator("#run-tree").inner_text()


@pytest.mark.parametrize("fault", ["unwrapped", "null_run", "wrong_run", "missing_total",
                                    "missing_count", "negative", "unsafe", "null_node",
                                    "missing_own", "invalid_children"])
def test_incomplete_run_never_looks_like_zero_or_previous_success(page, visible_run, fault):
    document = copy.deepcopy(visible_run)
    run = document["run"]
    if fault == "unwrapped":
        document = run
    elif fault == "null_run":
        document["run"] = None
    elif fault == "wrong_run":
        run["run_id"] = "another-run"
    elif fault == "missing_total":
        del run["total"]
    elif fault == "missing_count":
        del run["total"]["calls"]
    elif fault == "negative":
        run["total"]["billable_tokens"] = -1
    elif fault == "unsafe":
        run["total"]["billable_tokens"] = 2 ** 53
    elif fault == "null_node":
        run["roots"] = [None]
    elif fault == "missing_own":
        del run["roots"][0]["own"]
    else:
        run["roots"][0]["children"] = {}
    page.route("**/admin/usage/run/*", lambda route: route.fulfill(json=document))
    page.get_by_role("button", name="Look up", exact=True).click()
    expect(page.locator("#run-tree")).to_have_attribute("data-state", "unavailable")
    assert page.locator("#run-tree .step").count() == 0
    assert "Total:" not in page.locator("#run-tree").inner_text()
