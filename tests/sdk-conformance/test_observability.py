"""The proxy's telemetry actually fires, in the real binary (TD-0011 P1).

A `tracing::debug!` that nobody installed a subscriber for is dead code that still type-checks, so
the only honest test spawns the shipped binary, drives a request through it, and reads what it
emitted. This runs its own short-lived proxy rather than the session-scoped one, because it needs
`SANDHI_LOG=debug` and needs to read the process's stderr after termination.
"""

from __future__ import annotations

import os
import json
import sqlite3
import subprocess
import time
import urllib.error
import urllib.request

import httpx
import pytest
from conftest import REAL_OPENAI_KEY, VK_OPENAI, MockUpstream, _free_port


@pytest.fixture
def verbose_proxy(proxy_binary, upstream: MockUpstream, tmp_path):
    """A proxy with debug logging, torn down so its stderr can be read."""
    port = _free_port()
    env = {
        **{key: value for key, value in os.environ.items()
           if not key.startswith(("SANDHI_", "SENTINELPASS_"))},
        "SANDHI_AUTH_MODE": "tokens", "SANDHI_ALLOW_LOOPBACK_HTTP": "1", "SANDHI_BIND": f"127.0.0.1:{port}",
        "SANDHI_OPENAI_KEY": REAL_OPENAI_KEY,
        "SANDHI_OPENAI_BASE": upstream.base_url,
        "SANDHI_LOG": "debug",
        "SANDHI_STORE": str(tmp_path / "observability.db"),
        "SANDHI_LEDGER_SHARDS": "1",
    }
    proc = subprocess.Popen(
        [str(proxy_binary)],
        env=env,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    base = f"http://127.0.0.1:{port}"
    deadline = time.time() + 30
    while time.time() < deadline:
        if proc.poll() is not None:
            out, err = proc.communicate()
            raise AssertionError(f"proxy exited early: {err.decode(errors='replace')}")
        try:
            with urllib.request.urlopen(f"{base}/healthz", timeout=1):
                break
        except (urllib.error.URLError, ConnectionError, TimeoutError):
            time.sleep(0.2)
    else:
        proc.kill()
        raise AssertionError("proxy did not become healthy")

    yield base, proc, tmp_path / "observability.db"

    proc.terminate()
    try:
        proc.wait(timeout=10)
    except subprocess.TimeoutExpired:
        proc.kill()


def _stderr_after_request(verbose_proxy, *, explicit_limit=False) -> str:
    base, proc, database = verbose_proxy
    response = httpx.post(
        f"{base}/v1/chat/completions",
        headers={"authorization": f"Bearer {VK_OPENAI}"},
        json={"model": "gpt-mock", "messages": [{"role": "user", "content": "ping"}],
              **({"max_tokens": 64} if explicit_limit else {})},
        timeout=10,
    )
    assert response.status_code == 200, response.text
    request_id = response.headers["x-sandhi-request-id"]
    proc.terminate()
    _out, err = proc.communicate(timeout=15)
    assert proc.returncode == 0
    with sqlite3.connect(database) as connection:
        rows = connection.execute(
            "SELECT a.evidence_json,o.usage_json,s.charged_tokens FROM budget_request_correlation c "
            "JOIN budget_physical_attempt a USING(execution_id) "
            "JOIN budget_terminal_observation o USING(execution_id) "
            "JOIN budget_execution_intent i USING(execution_id) "
            "JOIN budget_settlement_outbox s ON s.reservation_id=i.reservation_id "
            "WHERE c.request_id=?", (request_id,),
        ).fetchall()
        assert len(rows) == 1
        attempt, usage = map(json.loads, rows[0][:2])
        assert attempt["attempt_ordinal"] == 1 and attempt["terminal"]["provider_status"] == 200
        assert usage["completeness"] == "final"
        assert (usage["tokens_in"], usage["cache_read_tokens"], usage["tokens_out"]) == (7, 4, 3)
        assert rows[0][2] == 14
        assert connection.execute("SELECT settled,actual FROM budget_reservation").fetchall() == [(1, 14)] * 2
    return err.decode(errors="replace")


@pytest.mark.parametrize("explicit_limit,plane", [(True, "transparent"), (False, "translation")])
def test_plane_selection_is_observable(verbose_proxy, explicit_limit, plane):
    """The ADR-0004 adoption signal: which plane served the call (TD-0011 D6)."""
    logs = _stderr_after_request(verbose_proxy, explicit_limit=explicit_limit)

    assert "plane selected" in logs, f"no plane event emitted; got:\n{logs[-2000:]}"
    # Same-family delivery stays transparent with an explicit bound. Injecting
    # the canonical default output bound selects the typed translation plane.
    assert plane in logs
    # Bounded fields are present and useful.
    assert "provider" in logs and "gpt-mock" in logs


def test_request_telemetry_carries_no_credentials_or_attribution(verbose_proxy):
    """TD-0011 D2 in spirit: telemetry is about the gateway, not about who called it.

    Attribution has a bounded home (the usage aggregate), so request-path telemetry deliberately
    does not repeat it — an operator tailing logs should not be accumulating per-user records.

    Scoped to the REQUEST path on purpose. The legacy demo bootstrap prints the virtual key it
    mints (`registered openai upstream + vk_openai_demo`) and that is the point of the demo path:
    there the id *is* the token, so announcing it is how an operator learns what to present.
    Operator-minted keys never have their plaintext retained, so nothing equivalent happens in
    production. What must hold either way: the request path logs no credential, and the UPSTREAM
    secret never appears anywhere.
    """
    logs = _stderr_after_request(verbose_proxy)

    # The upstream credential is unconditional — it must not appear even at startup.
    assert REAL_OPENAI_KEY not in logs, "the upstream credential must never be logged"

    # Split bootstrap from request-path telemetry at the listening banner.
    marker = "listening on"
    assert marker in logs, f"could not find the startup banner in:\n{logs[-1500:]}"
    runtime = logs.split(marker, 1)[1]

    assert VK_OPENAI not in runtime, "a caller's virtual key must not reach request telemetry"
    for forbidden in ("subject_id", "session_id", "virtual_key_id"):
        assert forbidden not in runtime, f"{forbidden} leaked into request telemetry"


def test_the_binary_installs_a_subscriber_at_all(verbose_proxy):
    """Guards the other side of D1: the libraries emit, but the BINARY must install."""
    logs = _stderr_after_request(verbose_proxy)
    # Any structured line proves a subscriber is running; without one, stderr would be empty of
    # tracing output entirely and every assertion above would be vacuous.
    assert "sandhi_proxy" in logs, f"no tracing target in output:\n{logs[-2000:]}"
