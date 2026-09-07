"""Actual SIGTERM against the shipped HTTP/TLS binary and a gated synthetic provider.

Readiness is process admission state, not a live-provider/vault/storage health promise.
Every database, key and upstream in this suite is disposable synthetic test data.
"""

from __future__ import annotations

import json
import os
import socket
import sqlite3
import subprocess
import threading
import time
from contextlib import contextmanager
from dataclasses import dataclass, field
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

import httpx
import pytest

from conftest import REAL_OPENAI_KEY, REPO_ROOT, VK_OPENAI, _free_port, _openai_chat_body


pytestmark = pytest.mark.skipif(os.name != "posix", reason="This suite exercises POSIX SIGTERM")

ADMIN = {"Authorization": "Bearer shutdown-test-admin"}
CLIENT = {"Authorization": f"Bearer {VK_OPENAI}"}
BODY = {"model": "gpt-mock", "messages": [{"role": "user", "content": "ping"}]}


@dataclass
class GatedProvider:
    requests: list = field(default_factory=list)
    entered: threading.Event = field(default_factory=threading.Event)
    release: threading.Event = field(default_factory=threading.Event)
    base: str = ""


@pytest.fixture
def gated_provider():
    provider = GatedProvider()

    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *_args):
            pass

        def do_POST(self):  # noqa: N802
            body = json.loads(self.rfile.read(int(self.headers["content-length"])))
            provider.requests.append(body)
            streaming = body.get("stream", False)
            payload = _openai_chat_body(streaming).encode()
            self.send_response(200)
            self.send_header("content-type", "text/event-stream" if streaming else "application/json")
            self.send_header("content-length", str(len(payload)))
            self.end_headers()
            try:
                if streaming:
                    # Send one complete event before blocking: the downstream owns an
                    # active response body/admission slot, not just a pending header read.
                    first, remainder = payload.split(b"\n\n", 1)
                    self.wfile.write(first + b"\n\n")
                    self.wfile.flush()
                    provider.entered.set()
                    if not provider.release.wait(20):
                        return
                    self.wfile.write(remainder)
                else:
                    self.wfile.write(payload)
            except (BrokenPipeError, ConnectionResetError):
                pass  # The forced-shutdown test intentionally abandons this response.

    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    provider.base = f"http://127.0.0.1:{server.server_port}"
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        yield provider
    finally:
        provider.release.set()
        server.shutdown()
        server.server_close()
        thread.join(timeout=2)


@pytest.fixture
def shutdown_gateway(proxy_binary, gated_provider, tmp_path):
    @contextmanager
    def launch(*, tls=False, grace=5, quiesce=1800, max_connections=64):
        port = _free_port()
        database = tmp_path / "shutdown.db"
        config = {"providers": [], "vkeys": [], "budgets": [], "alerts": []}
        if tls:
            fixtures = REPO_ROOT / "crates/sandhi-proxy/tests/fixtures/tls"
            config["tls"] = {
                "cert": str(fixtures / "localhost-cert.pem"),
                "key": str(fixtures / "localhost-key.pem"),
            }
        config_path = tmp_path / "shutdown.json"
        config_path.write_text(json.dumps(config))
        env = {k: v for k, v in os.environ.items()
               if not k.startswith(("SANDHI_", "SENTINELPASS_"))}
        env.update({
            "SANDHI_BIND": f"127.0.0.1:{port}",
            "SANDHI_OPENAI_KEY": REAL_OPENAI_KEY,
            "SANDHI_OPENAI_BASE": gated_provider.base,
            "SANDHI_ADMIN_TOKEN": "shutdown-test-admin",
            "SANDHI_CONFIG": str(config_path),
            "SANDHI_STORE": str(database),
            "SANDHI_MAX_IN_FLIGHT_AI_REQUESTS": "1",
            "SANDHI_MAX_CONNECTIONS": str(max_connections),
            "SANDHI_SHUTDOWN_GRACE_SECS": str(grace),
            "SANDHI_SHUTDOWN_QUIESCE_MS": str(quiesce),
        })
        base = f"{'https' if tls else 'http'}://127.0.0.1:{port}"
        # A file avoids stderr pipe backpressure contaminating the deadline test.
        with (tmp_path / "shutdown.stderr").open("wb") as log:
            process = subprocess.Popen([str(proxy_binary)], env=env, cwd=REPO_ROOT,
                                       stdout=subprocess.DEVNULL, stderr=log)
            try:
                with httpx.Client(base_url=base, verify=False, timeout=2) as client:
                    deadline = time.monotonic() + 15
                    while True:
                        assert process.poll() is None, "gateway exited before startup"
                        try:
                            if client.get("/healthz").status_code == 200:
                                break
                        except httpx.TransportError:
                            pass
                        assert time.monotonic() < deadline, "gateway did not start"
                        time.sleep(0.02)
                    response = client.post("/admin/budget", headers=ADMIN, json={
                        "scope": "group:demo", "limit_tokens": 100000,
                        "policy": "block", "window": "total",
                    })
                    assert response.status_code == 200, response.text
                yield process, base, database, port
            finally:
                if process.poll() is None:
                    process.terminate()
                    try:
                        process.wait(timeout=grace + 2)
                    except subprocess.TimeoutExpired:
                        process.kill()
                        process.wait(timeout=2)

    return launch


def await_draining(client):
    deadline = time.monotonic() + 1.5
    while True:
        response = client.get("/readyz")
        if response.status_code == 503:
            return response
        assert response.status_code == 200, response.text
        assert time.monotonic() < deadline, "SIGTERM never changed readiness"
        time.sleep(0.01)


def assert_draining(response, *, administrative=False):
    assert response.status_code == 503, response.text
    assert response.headers.get("retry-after") == "1", response.headers
    payload = response.json()
    code = payload["code"] if administrative else payload["error"]["code"]
    assert code == "gateway_draining", response.text


def evidence_counts(database):
    with sqlite3.connect(database, timeout=1) as connection:
        return tuple(connection.execute(f"SELECT COUNT(*) FROM {table}").fetchone()[0]
                     for table in ("usage_events", "budget_reservation"))


@pytest.mark.parametrize("tls", [False, True], ids=["http", "tls"])
def test_sigterm_keeps_fresh_and_keepalive_probes_reachable_and_rejects_work(
    shutdown_gateway, gated_provider, tls,
):
    with shutdown_gateway(tls=tls) as (process, base, database, _port):
        with httpx.Client(base_url=base, verify=False, timeout=2) as existing:
            # No admin bearer: readiness and liveness are independent probe endpoints.
            running = existing.get("/readyz")
            assert running.status_code == 200
            keepalive_connection = running.extensions["network_stream"]
            before = evidence_counts(database)
            process.terminate()
            draining = await_draining(existing)
            assert draining.extensions["network_stream"] is keepalive_connection
            assert existing.get("/healthz").status_code == 200
            # New TCP/TLS connection after cutoff, not an in-process router call.
            with httpx.Client(base_url=base, verify=False, timeout=2) as fresh:
                assert fresh.get("/readyz").status_code == 503
                assert fresh.get("/healthz").status_code == 200
                assert_draining(fresh.post("/v1/chat/completions", headers=CLIENT, json=BODY))
                assert_draining(fresh.post("/admin/budget", headers=ADMIN, json={
                    "scope": "group:forbidden", "limit_tokens": 1,
                    "policy": "block", "window": "total",
                }), administrative=True)
            assert evidence_counts(database) == before
            with sqlite3.connect(database, timeout=1) as connection:
                assert connection.execute(
                    "SELECT COUNT(*) FROM budget_limit WHERE scope = ?", ("group:forbidden",)
                ).fetchone()[0] == 0
            assert gated_provider.requests == []
        assert process.wait(timeout=6) == 0


@pytest.mark.parametrize("tls", [False, True], ids=["http", "tls"])
def test_inflight_sse_finishes_but_queued_request_cannot_dispatch_after_sigterm(
    shutdown_gateway, gated_provider, tls,
):
    with shutdown_gateway(tls=tls) as (process, base, database, _port):
        with httpx.Client(base_url=base, verify=False, timeout=6) as stream_client:
            with stream_client.stream("POST", "/v1/chat/completions", headers=CLIENT,
                                      json={**BODY, "stream": True}) as stream:
                assert stream.status_code == 200
                chunks = stream.iter_bytes()
                assert b"data:" in next(chunks)
                assert gated_provider.entered.wait(1)
                result = {}
                started = threading.Event()

                def queued_request():
                    try:
                        with httpx.Client(base_url=base, verify=False, timeout=6) as queued:
                            started.set()
                            result["response"] = queued.post("/v1/chat/completions", headers=CLIENT, json=BODY)
                    except Exception as error:
                        result["error"] = error

                thread = threading.Thread(target=queued_request, daemon=True)
                thread.start()
                assert started.wait(1)
                time.sleep(0.1)  # Let the second connection reach the held admission slot.
                assert "response" not in result
                assert len(gated_provider.requests) == 1
                with httpx.Client(base_url=base, verify=False, timeout=2) as probe:
                    process.terminate()
                    await_draining(probe)
                    gated_provider.release.set()
                    assert b"[DONE]" in b"".join(chunks)
                thread.join(timeout=3)
                assert not thread.is_alive(), "queued admission ignored shutdown"
                assert "error" not in result, result
                assert_draining(result["response"])
        assert process.wait(timeout=6) == 0
        assert len(gated_provider.requests) == 1
        assert evidence_counts(database) == (1, 1)


def test_body_started_before_cutoff_cannot_dispatch_when_completed_after_cutoff(
    shutdown_gateway, gated_provider,
):
    with shutdown_gateway() as (process, base, database, port):
        body = json.dumps(BODY).encode()
        with socket.create_connection(("127.0.0.1", port), timeout=3) as slow:
            slow.sendall((f"POST /v1/chat/completions HTTP/1.1\r\nHost: localhost\r\n"
                          f"Authorization: Bearer {VK_OPENAI}\r\nContent-Type: application/json\r\n"
                          f"Content-Length: {len(body)}\r\nConnection: close\r\n\r\n").encode()
                         + body[:1])
            time.sleep(0.1)
            with httpx.Client(base_url=base, timeout=2) as probe:
                process.terminate()
                await_draining(probe)
            slow.sendall(body[1:])
            response = bytearray()
            while chunk := slow.recv(4096):
                response.extend(chunk)
            assert b"503" in response.split(b"\r\n", 1)[0], response
            assert b"gateway_draining" in response, response
        assert process.wait(timeout=6) == 0
        assert gated_provider.requests == []
        assert evidence_counts(database) == (0, 0)


def test_hung_sse_obeys_one_total_shutdown_deadline(shutdown_gateway, gated_provider):
    with shutdown_gateway(grace=2, quiesce=500) as (process, base, _database, _port):
        with httpx.Client(base_url=base, timeout=5) as client:
            with client.stream("POST", "/v1/chat/completions", headers=CLIENT,
                               json={**BODY, "stream": True}) as stream:
                assert stream.status_code == 200
                chunks = stream.iter_bytes()
                assert b"data:" in next(chunks)
                assert gated_provider.entered.wait(1)
                started = time.monotonic()
                process.terminate()
                status = process.wait(timeout=4)
                elapsed = time.monotonic() - started
                assert elapsed < 3.5, f"shutdown exceeded one 2s grace: {elapsed:.3f}s"
                assert status == 124, f"grace-exhausted stream must not report clean exit: {status}"
                assert len(gated_provider.requests) == 1


def test_probe_reachability_during_quiesce_still_requires_connection_capacity(shutdown_gateway):
    with shutdown_gateway(max_connections=1) as (process, base, _database, _port):
        with httpx.Client(base_url=base, timeout=2) as admitted:
            assert admitted.get("/readyz").status_code == 200
            process.terminate()
            await_draining(admitted)
            # The already-admitted keepalive owns the sole connection slot. Quiesce
            # preserves the cap: it does not create an unlimited privileged probe lane.
            with httpx.Client(base_url=base, timeout=1) as excess:
                with pytest.raises(httpx.TransportError):
                    excess.get("/readyz")
            assert admitted.get("/readyz").status_code == 503
            metrics = admitted.get("/metrics", headers=ADMIN)
            assert metrics.status_code == 200
            shed = next(line for line in metrics.text.splitlines()
                        if line.startswith("sandhi_connections_shed_total "))
            assert int(shed.split()[1]) >= 1
        assert process.wait(timeout=6) == 0


def test_locked_settlement_cannot_extend_shutdown_past_watchdog(
    shutdown_gateway, gated_provider,
):
    with shutdown_gateway(grace=1, quiesce=200) as (process, base, database, _port):
        with httpx.Client(base_url=base, timeout=5) as client:
            with client.stream("POST", "/v1/chat/completions", headers=CLIENT,
                               json={**BODY, "stream": True}) as stream:
                assert stream.status_code == 200
                chunks = stream.iter_bytes()
                assert b"data:" in next(chunks)
                assert gated_provider.entered.wait(1)
                # The reservation is committed before the provider stream starts.
                # Hold the SQLite writer lock while final metering attempts to settle;
                # its normal 5s busy timeout exceeds this process's total 1s grace.
                with sqlite3.connect(database, timeout=1) as blocker:
                    blocker.execute("BEGIN IMMEDIATE")
                    gated_provider.release.set()
                    time.sleep(0.1)
                    started = time.monotonic()
                    process.terminate()
                    status = process.wait(timeout=3)
                    elapsed = time.monotonic() - started
                    assert elapsed < 2.5, f"locked settlement extended shutdown: {elapsed:.3f}s"
                    assert status == 124, f"stuck SQLite cleanup must be an explicit forced exit, got {status}"
                assert len(gated_provider.requests) == 1
