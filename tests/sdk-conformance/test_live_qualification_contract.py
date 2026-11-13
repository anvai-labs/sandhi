"""Paid live qualification must require a real durable owner and honest evidence.

Only synthetic canaries, mocked HTTP and temporary RAM credentials are used here.
"""

import importlib.util
from pathlib import Path
import stat

import httpx
import pytest

from conftest import REPO_ROOT


@pytest.fixture
def live_harness():
    spec = importlib.util.spec_from_file_location(
        "live_qualification_contract", REPO_ROOT / "scripts/qualify_zai_router.py"
    )
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


@pytest.mark.skipif(not Path('/proc/self/mountinfo').exists(), reason='Linux tmpfs qualification')
def test_live_credential_is_private_regular_ram_file_and_is_removed(live_harness):
    canary = 'synthetic-private-memory-canary'
    with live_harness.memory_credentials(canary) as directory:
        source = directory / 'zai__live'
        assert directory.parent == Path('/dev/shm').resolve()
        assert stat.S_IMODE(directory.stat().st_mode) == 0o700
        assert stat.S_IMODE(source.stat().st_mode) == 0o600
        assert source.is_file() and not source.is_symlink()
        assert source.read_text() == canary
    assert not directory.exists()


def test_live_credential_has_no_disk_fallback(live_harness, monkeypatch):
    original = Path.read_text

    def no_tmpfs(path, *args, **kwargs):
        if str(path) == '/proc/self/mountinfo':
            return '1 0 1:1 / /dev/shm rw - ext4 /dev/synthetic rw\n'
        return original(path, *args, **kwargs)

    monkeypatch.setattr(Path, 'read_text', no_tmpfs)
    with pytest.raises(RuntimeError, match='tmpfs'):
        with live_harness.memory_credentials('synthetic-memory-only'):
            pytest.fail('credential staging must refuse before creating a file')


@pytest.mark.parametrize('mode', [None, 'tracked', 'unavailable', 'paired_durable_v1'])
def test_live_probe_requires_initialized_durable_capability_before_provider_calls(
    live_harness, monkeypatch, mode
):
    calls = []

    def capabilities(url, **kwargs):
        calls.append((url, kwargs))
        return httpx.Response(
            200, request=httpx.Request('GET', url),
            json={'capabilities': {'execution_accounting': mode}},
        )

    monkeypatch.setattr(live_harness.httpx, 'get', capabilities)
    if mode == 'paired_durable_v1':
        live_harness.require_durable_gateway('http://synthetic.invalid', 'synthetic-admin')
    else:
        with pytest.raises(RuntimeError, match='paired durable'):
            live_harness.require_durable_gateway('http://synthetic.invalid', 'synthetic-admin')
    assert len(calls) == 1 and calls[0][0].endswith('/admin/version')


@pytest.fixture
def current_report(live_harness, monkeypatch):
    # These cases isolate the report envelope; real SQL joins have separate SDK tests.
    monkeypatch.setattr(live_harness, "verify_tracked", lambda report, by_step: (1, []))
    cell = {
        "backend": "chat", "client": "chat", "stream": False, "repeat": 0,
        "step_id": "synthetic-step", "status": "ok",
        "usage": [{"prompt_tokens": 2, "completion_tokens": 1}],
    }
    report = {
        "format_version": 2, "accounting": "paired_durable_v1",
        "finished_at": "synthetic-finished", "cells": [cell],
        "usage_events": [{
            "step_id": "synthetic-step", "tokens_in": 2, "tokens_out": 1,
            "cache_read_tokens": 0, "cache_creation_tokens": 0,
            "cache_read_status": "unsupported",
        }],
        "planned_cells": [{key: cell[key] for key in ("backend", "client", "stream", "repeat")}],
        "gateway_exit_code": 0,
    }
    assert live_harness.verify_report(report)["errors"] == []
    return report


@pytest.mark.parametrize("change,reason", [
    ({"accounting": "legacy"}, "requires paired durable"),
    ({"finished_at": None}, "did not finish"),
    ({"harness_error": "synthetic failure"}, "did not finish"),
    ({"planned_cells": []}, "planned inference cells"),
])
def test_current_report_rejects_incomplete_or_downgraded_qualification(
    live_harness, current_report, change, reason
):
    current_report.update(change)
    assert any(reason in error for error in live_harness.verify_report(current_report)["errors"])


def test_current_report_cannot_verify_only_a_completed_subset(live_harness, current_report):
    current_report["planned_cells"].append({
        "backend": "messages", "client": "chat", "stream": False, "repeat": 0,
    })
    assert any("planned inference cells" in error for error in live_harness.verify_report(current_report)["errors"])


def test_gemini_wire_writes_fold_without_changing_original_source(live_harness, current_report):
    cell = current_report["cells"][0]
    cell["client"] = current_report["planned_cells"][0]["client"] = "gemini"
    cell["usage"] = [{
        "prompt_token_count": 11, "cached_content_token_count": 6,
        "candidates_token_count": 4,
    }]
    source = current_report["usage_events"][0]
    source.update(tokens_in=2, cache_read_tokens=6, cache_creation_tokens=3,
                  tokens_out=4, cache_read_status="reported")
    assert live_harness.verify_report(current_report)["errors"] == []
    assert source["tokens_in"] == 2 and source["cache_creation_tokens"] == 3
    cell["usage"][0]["prompt_token_count"] = 8
    assert any("expected source projection" in error for error in live_harness.verify_report(current_report)["errors"])
