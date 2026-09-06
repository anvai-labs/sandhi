"""Small deterministic contracts plus an isolated real-proxy acceptance smoke."""

import asyncio
from contextlib import closing
from dataclasses import replace
import json
import math
import os
from pathlib import Path
import sqlite3
import subprocess
import sys
import time
from types import SimpleNamespace

import httpx
import pytest

import workload_acceptance as workload


@pytest.mark.parametrize("changes", [
    {"requests": 0}, {"requests": 1}, {"tenants": 0}, {"repeats": 0},
    {"concurrency": 0}, {"max_pending": 2}, {"frames": 129},
    {"arrival_rate": 0}, {"arrival_rate": math.inf}, {"arrival_rate": math.nan},
    {"frame_delay_ms": -1}, {"timeout_seconds": 0}, {"total_seconds": 601},
])
def test_invalid_profiles_rejected(changes):
    with pytest.raises(ValueError):
        replace(workload.Config(), **changes).validate()


def test_optimized_python_cannot_publish_unchecked_acceptance():
    result = subprocess.run([sys.executable, "-O", "-c",
        "import workload_acceptance as w; w.Config().validate()"],
        cwd=Path(workload.__file__).parent, capture_output=True, text=True, timeout=10)
    assert result.returncode != 0
    assert "acceptance requires unoptimized Python" in result.stderr


@pytest.mark.parametrize("alias", ["same", "symlink", "hardlink"])
def test_summary_cannot_overwrite_full_evidence(tmp_path, alias):
    output = tmp_path / "full.json"
    output.write_text("preserve-existing-evidence")
    summary = output if alias == "same" else tmp_path / "summary.json"
    if alias == "symlink":
        summary.symlink_to(output)
    elif alias == "hardlink":
        summary.hardlink_to(output)
    result = subprocess.run([sys.executable, workload.__file__, "--binary", "/does/not/exist",
        "--output", str(output), "--summary-output", str(summary)],
        capture_output=True, text=True, timeout=10)
    assert result.returncode == 2
    assert "full and summary outputs must be different files" in result.stderr
    assert output.read_text() == "preserve-existing-evidence"


@pytest.mark.parametrize("values", [[], [-1], [math.nan], [math.inf]])
def test_invalid_samples_never_become_zero_latency(values):
    with pytest.raises(ValueError):
        workload.distribution(values)


def test_nearest_rank_percentiles_and_accounting_categories():
    assert workload.distribution(list(range(1, 101))) == {
        "samples": 100, "mean": 50.5, "p50": 50, "p95": 95, "p99": 99}
    assert [workload.components(i) for i in range(3)] == [
        (100, 0, 40, 0, 0), (70, 30, 40, 25, 0), (70, 30, 40, 90, 0)]
    assert [workload.charge(i) for i in range(3)] == [140, 165, 230]
    with pytest.raises(ValueError):
        workload.percentile([1], 0)


def test_missing_observability_is_not_a_quiet_pass():
    with pytest.raises(AssertionError):
        workload.assert_quiet({})
    snapshot = {f'sandhi_buffer_{kind}{{buffer="{buffer}"}}': 0
                for buffer in ("usage", "alerts")
                for kind in ("queued", "in_flight", "dropped_total")}
    snapshot["sandhi_shutdown_active_operations"] = 0
    workload.assert_quiet(snapshot)
    snapshot['sandhi_buffer_dropped_total{buffer="usage"}'] = 1
    with pytest.raises(AssertionError):
        workload.assert_quiet(snapshot)


def test_unavailable_process_counters_are_explicit():
    resources = workload.Resources({"missing": -12345})
    resources.start()
    assert resources.finish(0) == {
        "missing": {"available": False, "reason": "Linux /proc counters unavailable"}}


def test_credentials_and_exporters_are_not_inherited(monkeypatch):
    for key in ("SANDHI_ADMIN_TOKEN", "SENTINELPASS_SOCKET", "OTEL_EXPORTER_OTLP_ENDPOINT",
                "http_proxy", "HTTPS_PROXY", "ALL_PROXY"):
        monkeypatch.setenv(key, "must-not-leak")
    assert "must-not-leak" not in workload.clean_environment().values()


def test_payload_checker_rejects_wrong_reasoning_and_incomplete_content():
    body = {"choices": [{"message": {"content": "pong"}, "finish_reason": "stop"}], "usage": {
        "prompt_tokens": 100, "completion_tokens": 65, "total_tokens": 165,
        "prompt_tokens_details": {"cached_tokens": 30},
        "completion_tokens_details": {"reasoning_tokens": 25}}}
    workload.validate_payload("translation-unary", 1, [body], 1)
    with pytest.raises(AssertionError):
        workload.validate_payload("translation-unary", 1, [body], 2)
    body["usage"]["completion_tokens_details"]["reasoning_tokens"] = 0
    with pytest.raises(AssertionError):
        workload.validate_payload("translation-unary", 1, [body], 1)


def test_fixed_arrivals_report_overload_instead_of_becoming_closed_loop(monkeypatch):
    calls = []

    async def slow_request(_client, _base, _lane, index, _token, _run, scheduled, _config):
        started = time.monotonic()
        calls.append(index)
        await asyncio.sleep(.05)
        return {"index": index, "status": 200, "error": None,
                "schedule_lag_ms": (started - scheduled) * 1000,
                "latency_ms": 50, "scheduled_latency_ms": (time.monotonic() - scheduled) * 1000,
                "ttfb_ms": 1, "first_content_ms": None}

    monkeypatch.setattr(workload, "one_request", slow_request)
    config = workload.Config(requests=20, tenants=1, repeats=1, concurrency=1,
                             arrival_rate=10000, max_pending=1)
    result = asyncio.run(workload.drive("http://unused", "transparent-unary", "fixed",
                                       [], "unit", config, {}))
    assert not result["passed"]
    assert result["generator_rejected"]
    assert result["completed"] + len(result["generator_rejected"]) == config.requests
    assert sorted(calls + result["generator_rejected"]) == list(range(config.requests))
    assert "latency_ms" not in result  # Failure cannot look like a valid fast run.
    assert all(s["scheduled_latency_ms"] >= s["latency_ms"] for s in result["samples"])


def test_request_failures_invalidate_acceptance(monkeypatch):
    async def failing_request(_client, _base, _lane, index, *_args):
        return {"index": index, "status": 503, "error": "HTTPStatusError"}

    monkeypatch.setattr(workload, "one_request", failing_request)
    config = workload.Config(requests=2, tenants=1, repeats=1, concurrency=1, max_pending=1)
    result = asyncio.run(workload.drive("http://unused", "transparent-unary", "closed",
                                       [], "unit", config, {}))
    assert not result["passed"]
    assert result["successful"] == 0
    assert result["errors"] == {"HTTPStatusError": 2}
    assert "latency_ms" not in result


@pytest.mark.parametrize("corruption", [None, "category", "duplicate", "scope_spend", "extra_scope", "run_total"])
def test_accounting_requires_exact_events_and_independent_views(tmp_path, corruption):
    database = tmp_path / "accounting.db"
    with closing(sqlite3.connect(database)) as connection, connection:
        connection.execute("CREATE TABLE usage_events(step_id,subject_id,group_id,tokens_in,"
            "cache_read_tokens,tokens_out,reasoning_tokens,reasoning_included,run_id)")
        connection.execute("CREATE TABLE budget_reservation(scope,actual,settled)")
        connection.execute("INSERT INTO usage_events VALUES(?,?,?,?,?,?,?,?,?)",
            ("0", "subject-0", "tenant-0", *workload.components(0), "test"))
        connection.execute("INSERT INTO budget_reservation VALUES('group:tenant-0',140,1)")
        if corruption == "category":
            connection.execute("UPDATE usage_events SET reasoning_included=1")
        elif corruption == "duplicate":
            connection.execute("INSERT INTO usage_events SELECT * FROM usage_events")
        elif corruption == "scope_spend":
            connection.execute("UPDATE budget_reservation SET actual=139")
        elif corruption == "extra_scope":
            connection.execute("INSERT INTO budget_reservation VALUES('group:unexpected',1,1)")

    def get(url, **_kwargs):
        if url.endswith("/metrics"):
            lines = [f'sandhi_buffer_{kind}{{buffer="{buffer}"}} 0'
                     for buffer in ("usage", "alerts")
                     for kind in ("queued", "in_flight", "dropped_total")]
            return SimpleNamespace(text="\n".join([*lines, "sandhi_shutdown_active_operations 0"]))
        data = {"spent": 140} if url.endswith("/usage") else {
            "run": {"total": {"billable_tokens": 139 if corruption == "run_total" else 140}}}
        return SimpleNamespace(json=lambda: data)

    args = (SimpleNamespace(get=get), "http://unused", database, "test",
            workload.Config(requests=1, tenants=1), {}, time.monotonic() + 1)
    if corruption:
        with pytest.raises(AssertionError):
            workload.accounting(*args)
    else:
        evidence, _ = workload.accounting(*args)
        assert evidence["events"] == 1 and evidence["charged_tokens"] == 140
        assert evidence["unsettled_leases"] == 0


@pytest.mark.parametrize("lane", workload.LANES)
def test_valid_content_and_usage_without_terminal_finish_fails(lane):
    if lane.startswith("translation"):
        body = {"choices": [{"message": {"content": "pong"}}], "usage": {
            "prompt_tokens": 100, "completion_tokens": 40, "total_tokens": 140,
            "completion_tokens_details": {"reasoning_tokens": 0}}}
    else:
        body = {"candidates": [{"content": {"parts": [{"text": "pong"}]}}],
            "usageMetadata": {"promptTokenCount": 100, "candidatesTokenCount": 40,
                "totalTokenCount": 140, "cachedContentTokenCount": 0, "thoughtsTokenCount": 0}}
    with pytest.raises(AssertionError):
        workload.validate_payload(lane, 0, [body], 1)


def test_summary_omits_samples_but_preserves_evidence_and_full_report_digest():
    report = {"phases": [{"samples": [{"index": 0}], "resources": {"cpu_seconds": .1},
                          "latency_ms": {"p99": 3}, "accounting": {"events": 1}}],
              "host": {"logical_cpus": 2}, "config": {"requests": 1}, "passed": True}
    summary = workload.compact_summary(report)
    assert summary == workload.compact_summary(report)
    assert "samples" not in summary["phases"][0]
    assert summary["phases"][0]["accounting"] == {"events": 1}
    assert summary["host"] == report["host"] and summary["config"] == report["config"]
    assert len(summary["full_report_canonical_sha256"]) == 64
    report["phases"][0]["samples"].append({"index": 1})
    assert workload.compact_summary(report)["full_report_canonical_sha256"] != summary["full_report_canonical_sha256"]


@pytest.mark.parametrize("suffix,accepted", [("", False), ("data: [DONE]\n\n", True),
                                             ("data: [DONE]\n\ndata: {}\n\n", False)])
def test_translated_stream_requires_final_sentinel(suffix, accepted):
    document = {"choices": [{"delta": {"content": "pong"}, "finish_reason": "stop"}],
        "usage": {"prompt_tokens": 100, "completion_tokens": 40, "total_tokens": 140,
                  "completion_tokens_details": {"reasoning_tokens": 0}}}
    payload = f"data: {json.dumps(document)}\n\n" + suffix

    async def run():
        transport = httpx.MockTransport(lambda _request: httpx.Response(200, content=payload))
        async with httpx.AsyncClient(transport=transport) as client:
            return await workload.one_request(client, "http://unused", "translation-sse", 0,
                None, "unit", time.monotonic(), workload.Config(frames=1))

    result = asyncio.run(run())
    assert (result["error"] is None) == accepted


@pytest.mark.parametrize("payload", ["null", "[]", "123"])
def test_malformed_success_payload_is_a_recorded_failure(payload):
    async def run():
        transport = httpx.MockTransport(lambda _request: httpx.Response(200, content=payload))
        async with httpx.AsyncClient(transport=transport) as client:
            return await workload.one_request(client, "http://unused", "translation-unary", 0,
                None, "unit", time.monotonic(), workload.Config(frames=1))

    result = asyncio.run(run())
    assert result["status"] == 200 and result["error"] is not None


def test_loopback_workload_smoke(request, tmp_path):
    configured = os.environ.get("SANDHI_WORKLOAD_BINARY")
    binary = Path(configured) if configured else request.getfixturevalue("proxy_binary")
    config = workload.Config(requests=8, tenants=4, repeats=1, concurrency=2,
        arrival_rate=40, max_pending=8, frames=2, frame_delay_ms=1, total_seconds=45)
    report = workload.run_acceptance(binary, config)
    artifact = json.dumps(report, allow_nan=False)
    (tmp_path / "workload-smoke.json").write_text(artifact)
    assert report["passed"], report
    assert len(report["phases"]) == 12
    assert {p["lane"] for p in report["phases"]} == {*workload.LANES, "provider-unary", "provider-sse"}
    assert {p["mode"] for p in report["phases"]} == {"closed", "fixed"}
    assert all(p["completed"] == 8 and p["passed"] for p in report["phases"])
    assert all(p["accounting"]["events"] == 8 and p["accounting"]["unsettled_leases"] == 0
               for p in report["phases"] if not p["lane"].startswith("provider"))
    assert report["proxy_shutdown_exit_code"] == 0
    assert not report["milestone_complete"]
    assert report["operator_acceptance"] == "pending actual-user review"
    assert not report["host"]["binary_revision_verified"]
    assert workload.ADMIN not in artifact and workload.UPSTREAM_KEY not in artifact
