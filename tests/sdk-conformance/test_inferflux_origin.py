"""Pinned InferFlux origin behind the real Sandhi proxy and OpenAI SDK.

The suite is skip-gated by ``INFERFLUX_SERVER_BIN``. CI builds the exact commit
recorded in ``inferflux_pin`` in CPU/stub mode; local runs may point at any
compatible ``inferfluxd``. Two canned completion families must produce the same
public reasoning/content shape.
"""

from __future__ import annotations

import json
import os
import re
import sqlite3
import socket
import subprocess
import threading
import time
import urllib.error
import urllib.request
from dataclasses import dataclass, field
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

import pytest

pytest.importorskip("openai")

INFERFLUX_BIN = os.environ.get("INFERFLUX_SERVER_BIN", "")
INFERFLUX_CONFIG = os.environ.get("INFERFLUX_CONFIG", "config/server.yaml")
REPO_ROOT = Path(__file__).resolve().parents[2]
TRACE_ID = "4bf92f3577b34da6a3ce929d0e0e4736"
TRACEPARENT = f"00-{TRACE_ID}-00f067aa0ba902b7-01"


@dataclass(frozen=True)
class ReasoningCase:
    family: str
    completion: str


REASONING_CASES = (
    ReasoningCase("think-tags", "<think>chain of thought</think>visible answer"),
    ReasoningCase(
        "harmony-channels",
        "<|channel|>analysis<|message|>chain of thought<|end|>"
        "<|start|>assistant<|channel|>final<|message|>visible answer<|return|>",
    ),
)


@dataclass
class RecordedRequest:
    headers: dict[str, str]
    body: dict


@dataclass
class RecordingForwarder:
    origin: str
    requests: list[RecordedRequest] = field(default_factory=list)


@dataclass(frozen=True)
class OriginRuntime:
    case: ReasoningCase
    base_url: str


@dataclass(frozen=True)
class ProxyRuntime:
    case: ReasoningCase
    base_url: str
    recorder: RecordingForwarder
    store_path: Path


def _free_port() -> int:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as sock:
        sock.bind(("127.0.0.1", 0))
        return int(sock.getsockname()[1])


def _wait_ready(url: str, process: subprocess.Popen, timeout: float = 30.0) -> None:
    deadline = time.time() + timeout
    while time.time() < deadline:
        if process.poll() is not None:
            raise AssertionError(
                f"process exited early with status {process.returncode}"
            )
        try:
            urllib.request.urlopen(url, timeout=2).read()
            return
        except urllib.error.HTTPError:
            return
        except (urllib.error.URLError, ConnectionError, TimeoutError):
            time.sleep(0.2)
    raise AssertionError(f"process did not become ready at {url}")


def _stop(process: subprocess.Popen) -> None:
    if process.poll() is not None:
        return
    process.terminate()
    try:
        process.wait(timeout=5)
    except subprocess.TimeoutExpired:
        process.kill()
        process.wait(timeout=5)


def _recording_handler(recorder: RecordingForwarder):
    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *_args):
            pass

        def do_POST(self):  # noqa: N802 - stdlib callback name
            length = int(self.headers.get("content-length", "0"))
            raw = self.rfile.read(length) if length else b"{}"
            recorder.requests.append(
                RecordedRequest(
                    headers={key.lower(): value for key, value in self.headers.items()},
                    body=json.loads(raw),
                )
            )
            forwarded_headers = {
                key: value
                for key, value in self.headers.items()
                if key.lower() not in {"host", "content-length", "connection"}
            }
            forwarded_headers["Accept-Encoding"] = "identity"
            request = urllib.request.Request(
                recorder.origin + self.path,
                data=raw,
                headers=forwarded_headers,
                method="POST",
            )
            try:
                upstream = urllib.request.urlopen(request, timeout=30)
            except urllib.error.HTTPError as error:
                upstream = error
            payload = upstream.read()
            self.send_response(upstream.status)
            for key, value in upstream.headers.items():
                if key.lower() not in {
                    "connection",
                    "content-length",
                    "keep-alive",
                    "transfer-encoding",
                }:
                    self.send_header(key, value)
            self.send_header("content-length", str(len(payload)))
            self.end_headers()
            self.wfile.write(payload)

    return Handler


@pytest.fixture(scope="module", params=REASONING_CASES, ids=lambda case: case.family)
def inferflux_origin(request, tmp_path_factory):
    if not INFERFLUX_BIN:
        pytest.skip("INFERFLUX_SERVER_BIN not set")
    case = request.param
    port = _free_port()
    policy_store = tmp_path_factory.mktemp(f"inferflux-{case.family}") / "policy.conf"
    env = {
        **os.environ,
        "INFERFLUX_MODEL_PATH": "",
        "INFERFLUX_PORT_OVERRIDE": str(port),
        "INFERFLUX_DISABLE_STARTUP_ADVISOR": "true",
        "INFERFLUX_STUB_COMPLETION": case.completion,
        "INFERFLUX_POLICY_STORE": str(policy_store),
    }
    process = subprocess.Popen(
        [INFERFLUX_BIN, "--config", INFERFLUX_CONFIG],
        env=env,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    try:
        _wait_ready(f"http://127.0.0.1:{port}/readyz", process)
        yield OriginRuntime(case=case, base_url=f"http://127.0.0.1:{port}")
    finally:
        _stop(process)


@pytest.fixture(scope="module")
def recording_origin(inferflux_origin):
    port = _free_port()
    recorder = RecordingForwarder(origin=inferflux_origin.base_url)
    server = ThreadingHTTPServer(("127.0.0.1", port), _recording_handler(recorder))
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        yield recorder, f"http://127.0.0.1:{port}"
    finally:
        server.shutdown()
        server.server_close()
        thread.join(timeout=2)


@pytest.fixture(scope="module")
def inferflux_proxy(
    proxy_binary: Path, inferflux_origin, recording_origin, tmp_path_factory
):
    recorder, recording_base = recording_origin
    port = _free_port()
    store_path = (
        tmp_path_factory.mktemp(f"sandhi-{inferflux_origin.case.family}") / "usage.db"
    )
    env = {
        **os.environ,
        "SANDHI_BIND": f"127.0.0.1:{port}",
        "SANDHI_INFERFLUX_KEY": "dev-key-123",
        "SANDHI_INFERFLUX_BASE": recording_base + "/v1",
        "SANDHI_STORE": str(store_path),
    }
    process = subprocess.Popen(
        [str(proxy_binary)],
        cwd=REPO_ROOT,
        env=env,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    try:
        base_url = f"http://127.0.0.1:{port}"
        _wait_ready(base_url + "/healthz", process)
        yield ProxyRuntime(
            case=inferflux_origin.case,
            base_url=base_url + "/v1",
            recorder=recorder,
            store_path=store_path,
        )
    finally:
        _stop(process)


@pytest.fixture(scope="module")
def inferflux_client(inferflux_proxy):
    import openai

    return openai.OpenAI(base_url=inferflux_proxy.base_url, api_key="vk_inferflux_demo")


def _request_options() -> dict:
    return {
        "model": "default",
        "messages": [{"role": "user", "content": "hi"}],
        "max_tokens": 64,
        "temperature": 0,
    }


def _latest_latency(proxy: ProxyRuntime, *, expect_ttft: bool):
    deadline = time.time() + 5
    while time.time() < deadline:
        with sqlite3.connect(proxy.store_path) as connection:
            row = connection.execute(
                "SELECT duration_ms,duration_source,time_to_first_token_ms,"
                "time_to_first_token_source FROM usage_events ORDER BY rowid DESC LIMIT 1"
            ).fetchone()
        if row is not None and (row[2] is not None) == expect_ttft:
            return row
        time.sleep(0.05)
    raise AssertionError("timed out waiting for the buffered usage event")


def test_buffered_reasoning_and_usage_shape(inferflux_client, inferflux_proxy):
    response = inferflux_client.chat.completions.create(**_request_options())
    payload = response.to_dict()
    message = payload["choices"][0]["message"]
    assert message["reasoning_content"] == "chain of thought"
    assert message["content"] == "visible answer"
    assert "chain of thought" not in message["content"]
    usage = payload["usage"]
    assert usage["prompt_tokens_details"]["cached_tokens"] >= 0
    assert usage["completion_tokens_details"]["reasoning_tokens"] > 0
    duration, duration_source, ttft, ttft_source = _latest_latency(
        inferflux_proxy, expect_ttft=False
    )
    assert duration is not None
    assert duration_source == "origin"
    assert ttft is None
    assert ttft_source is None


def test_streaming_reasoning_and_terminal_usage(inferflux_client, inferflux_proxy):
    stream = inferflux_client.chat.completions.create(
        **_request_options(),
        stream=True,
        stream_options={"include_usage": True},
    )
    reasoning, content, usage = [], [], None
    for chunk in stream:
        payload = chunk.to_dict()
        if payload.get("usage"):
            usage = payload["usage"]
        if payload.get("choices"):
            delta = payload["choices"][0].get("delta") or {}
            reasoning.append(delta.get("reasoning_content") or "")
            content.append(delta.get("content") or "")
    assert "".join(reasoning) == "chain of thought"
    assert "".join(content) == "visible answer"
    assert "chain of thought" not in "".join(content)
    assert usage is not None, "terminal usage chunk missing"
    assert usage["prompt_tokens_details"]["cached_tokens"] >= 0
    assert usage["completion_tokens_details"]["reasoning_tokens"] > 0
    duration, duration_source, ttft, ttft_source = _latest_latency(
        inferflux_proxy, expect_ttft=True
    )
    assert duration is not None
    assert duration_source == "origin"
    assert ttft is not None
    assert ttft_source == "origin"


def test_correlation_and_trace_headers_round_trip(inferflux_client, inferflux_proxy):
    raw = inferflux_client.chat.completions.with_raw_response.create(
        **_request_options(),
        extra_headers={"traceparent": TRACEPARENT},
    )
    payload = raw.parse().to_dict()
    correlation = raw.headers["x-inferflux-client-request-id"]
    assert correlation.startswith("req_")
    assert payload["client_request_id"] == correlation
    child = raw.headers["traceparent"]
    assert re.fullmatch(rf"00-{TRACE_ID}-[0-9a-f]{{16}}-01", child)
    assert child != TRACEPARENT


def test_key_attribution_never_crosses_provider_seam(inferflux_client, inferflux_proxy):
    inferflux_client.chat.completions.create(**_request_options())
    request = inferflux_proxy.recorder.requests[-1]
    assert request.headers["authorization"] == "Bearer dev-key-123"
    assert not any(name.startswith("x-sandhi-") for name in request.headers)
    forbidden = {
        "subject_id",
        "group_id",
        "virtual_key_id",
        "run_id",
        "step_id",
        "parent_id",
    }
    assert forbidden.isdisjoint(request.body)


def test_origin_error_envelope_openai_shape(inferflux_origin):
    import openai

    client = openai.OpenAI(
        base_url=inferflux_origin.base_url + "/v1", api_key="dev-key-123"
    )
    with pytest.raises(openai.NotFoundError) as caught:
        client.chat.completions.create(
            **{**_request_options(), "model": "nonexistent-model"}
        )
    error = caught.value.body
    assert error["message"]
    assert error["type"] == "inferflux_error"
    assert error["code"] == "model_not_found"
