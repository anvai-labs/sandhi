"""Harness for the TD-0010 D5 SDK-conformance suite.

The claim under test is the README's: *internal clients point their `base_url` at Sandhi with a
virtual key and never see the real key.* Every other test in this repo hand-rolls the HTTP request,
which encodes the author's assumptions about how a client behaves — that is exactly how the proxy
shipped an Anthropic ingress no stock Anthropic SDK could authenticate against (it read only
`Authorization: Bearer`, while the SDK sends `x-api-key`). These tests drive the **vendors' own
SDKs** instead, so the assumptions come from the vendor.

Shape:

    openai / anthropic SDK  ──►  sandhi-proxy (real binary)  ──►  mock upstream (this file)

The mock stands in for the provider API. It records what it received, so a test can assert the
proxy swapped the virtual key for the real upstream credential rather than forwarding the client's.
No network, no vendor credentials, no vendor account.
"""

from __future__ import annotations

import json
import os
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

REPO_ROOT = Path(__file__).resolve().parents[2]

# What the mock upstream expects the proxy to present. A test asserts the client's virtual key
# never appears here — that substitution is the proxy's entire reason to exist.
REAL_OPENAI_KEY = "sk-real-upstream-openai"
REAL_ANTHROPIC_KEY = "sk-ant-real-upstream"
REAL_GEMINI_KEY = "gk-real-upstream-gemini"

# Virtual keys the proxy's demo path mints from the env vars below.
VK_OPENAI = "vk_openai_demo"
VK_ANTHROPIC = "vk_anthropic_demo"
VK_GEMINI = "vk_gemini_demo"


@dataclass
class RecordedRequest:
    path: str
    headers: dict[str, str]
    body: dict


@dataclass
class MockUpstream:
    """A stand-in provider API that answers in each vendor's native shape."""

    host: str
    port: int
    requests: list[RecordedRequest] = field(default_factory=list)

    @property
    def base_url(self) -> str:
        return f"http://{self.host}:{self.port}"

    def last(self) -> RecordedRequest:
        assert self.requests, "the upstream received no request at all"
        return self.requests[-1]


def _free_port() -> int:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as s:
        s.bind(("127.0.0.1", 0))
        return int(s.getsockname()[1])


def _openai_chat_body(streaming: bool) -> str:
    if not streaming:
        return json.dumps(
            {
                "id": "chatcmpl-mock",
                "object": "chat.completion",
                "created": 1,
                "model": "gpt-mock",
                "choices": [
                    {
                        "index": 0,
                        "message": {"role": "assistant", "content": "pong"},
                        "finish_reason": "stop",
                    }
                ],
                "usage": {
                    "prompt_tokens": 11,
                    "completion_tokens": 3,
                    "total_tokens": 14,
                    "prompt_tokens_details": {"cached_tokens": 4},
                },
            }
        )
    chunks = [
        {
            "id": "chatcmpl-mock",
            "object": "chat.completion.chunk",
            "created": 1,
            "model": "gpt-mock",
            "choices": [{"index": 0, "delta": {"role": "assistant"}, "finish_reason": None}],
        },
        {
            "id": "chatcmpl-mock",
            "object": "chat.completion.chunk",
            "created": 1,
            "model": "gpt-mock",
            "choices": [{"index": 0, "delta": {"content": "pong"}, "finish_reason": None}],
        },
        {
            "id": "chatcmpl-mock",
            "object": "chat.completion.chunk",
            "created": 1,
            "model": "gpt-mock",
            "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 11, "completion_tokens": 3, "total_tokens": 14},
        },
    ]
    return "".join(f"data: {json.dumps(c)}\n\n" for c in chunks) + "data: [DONE]\n\n"


def _anthropic_messages_body(streaming: bool) -> str:
    if not streaming:
        return json.dumps(
            {
                "id": "msg_mock",
                "type": "message",
                "role": "assistant",
                "model": "claude-mock",
                "content": [{"type": "text", "text": "pong"}],
                "stop_reason": "end_turn",
                "stop_sequence": None,
                "usage": {
                    "input_tokens": 11,
                    "output_tokens": 3,
                    "cache_creation_input_tokens": 0,
                    "cache_read_input_tokens": 4,
                },
            }
        )
    events = [
        (
            "message_start",
            {
                "type": "message_start",
                "message": {
                    "id": "msg_mock",
                    "type": "message",
                    "role": "assistant",
                    "model": "claude-mock",
                    "content": [],
                    "stop_reason": None,
                    "stop_sequence": None,
                    "usage": {"input_tokens": 11, "output_tokens": 0},
                },
            },
        ),
        (
            "content_block_start",
            {
                "type": "content_block_start",
                "index": 0,
                "content_block": {"type": "text", "text": ""},
            },
        ),
        (
            "content_block_delta",
            {
                "type": "content_block_delta",
                "index": 0,
                "delta": {"type": "text_delta", "text": "pong"},
            },
        ),
        ("content_block_stop", {"type": "content_block_stop", "index": 0}),
        (
            "message_delta",
            {
                "type": "message_delta",
                "delta": {"stop_reason": "end_turn", "stop_sequence": None},
                "usage": {"output_tokens": 3},
            },
        ),
        ("message_stop", {"type": "message_stop"}),
    ]
    return "".join(f"event: {name}\ndata: {json.dumps(payload)}\n\n" for name, payload in events)


def _gemini_generate_body(streaming: bool) -> str:
    payload = {
        "candidates": [
            {
                "content": {"role": "model", "parts": [{"text": "pong"}]},
                "finishReason": "STOP",
                "index": 0,
            }
        ],
        "usageMetadata": {
            "promptTokenCount": 11,
            "candidatesTokenCount": 3,
            "totalTokenCount": 14,
            "cachedContentTokenCount": 4,
        },
        "modelVersion": "gemini-mock",
    }
    if not streaming:
        return json.dumps(payload)
    # `?alt=sse` framing — the form the adapter's usage sniffer reads.
    return f"data: {json.dumps(payload)}\r\n\r\n"


def _make_handler(upstream: MockUpstream):
    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *_args):  # keep pytest output readable
            pass

        def do_POST(self):  # noqa: N802 - stdlib naming
            length = int(self.headers.get("content-length", "0"))
            raw = self.rfile.read(length) if length else b"{}"
            try:
                body = json.loads(raw or b"{}")
            except json.JSONDecodeError:
                body = {"_unparsed": raw.decode("utf-8", "replace")}
            upstream.requests.append(
                RecordedRequest(
                    path=self.path,
                    headers={k.lower(): v for k, v in self.headers.items()},
                    body=body,
                )
            )
            # Gemini puts the method in the path; the others use a body flag.
            if ":streamGenerateContent" in self.path:
                streaming, payload = True, _gemini_generate_body(True)
            elif ":generateContent" in self.path:
                streaming, payload = False, _gemini_generate_body(False)
            elif "/messages" in self.path:
                streaming = bool(body.get("stream"))
                payload = _anthropic_messages_body(streaming)
            else:
                streaming = bool(body.get("stream"))
                payload = _openai_chat_body(streaming)
            data = payload.encode()
            self.send_response(200)
            self.send_header(
                "content-type", "text/event-stream" if streaming else "application/json"
            )
            self.send_header("content-length", str(len(data)))
            self.end_headers()
            self.wfile.write(data)

    return Handler


@pytest.fixture(scope="session")
def upstream():
    port = _free_port()
    up = MockUpstream(host="127.0.0.1", port=port)
    server = ThreadingHTTPServer((up.host, port), _make_handler(up))
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    yield up
    server.shutdown()
    server.server_close()


@pytest.fixture(scope="session")
def proxy_binary() -> Path:
    """The real `sandhi-proxy` binary. Built here so a local run needs no separate step."""
    subprocess.run(
        ["cargo", "build", "-p", "sandhi-proxy", "--bin", "sandhi-proxy"],
        cwd=REPO_ROOT,
        check=True,
    )
    for candidate in (
        REPO_ROOT / "target" / "debug" / "sandhi-proxy",
        REPO_ROOT / "target" / "release" / "sandhi-proxy",
    ):
        if candidate.exists():
            return candidate
    raise AssertionError("sandhi-proxy binary not found after cargo build")


@pytest.fixture(scope="session")
def proxy(proxy_binary: Path, upstream: MockUpstream):
    """A live proxy whose upstreams point at the mock, exactly as an operator would configure it."""
    port = _free_port()
    env = {
        **os.environ,
        "SANDHI_BIND": f"127.0.0.1:{port}",
        "SANDHI_OPENAI_KEY": REAL_OPENAI_KEY,
        "SANDHI_OPENAI_BASE": upstream.base_url,
        "SANDHI_ANTHROPIC_KEY": REAL_ANTHROPIC_KEY,
        "SANDHI_ANTHROPIC_BASE": upstream.base_url,
        "SANDHI_GEMINI_KEY": REAL_GEMINI_KEY,
        "SANDHI_GEMINI_BASE": upstream.base_url,
    }
    proc = subprocess.Popen(
        [str(proxy_binary)],
        cwd=REPO_ROOT,
        env=env,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    base = f"http://127.0.0.1:{port}"
    deadline = time.time() + 30
    while time.time() < deadline:
        if proc.poll() is not None:
            out, err = proc.communicate()
            raise AssertionError(
                f"sandhi-proxy exited early ({proc.returncode}).\n"
                f"stdout: {out.decode(errors='replace')}\nstderr: {err.decode(errors='replace')}"
            )
        try:
            with urllib.request.urlopen(f"{base}/healthz", timeout=1):
                break
        except (urllib.error.URLError, ConnectionError, TimeoutError):
            time.sleep(0.2)
    else:
        proc.kill()
        raise AssertionError("sandhi-proxy did not become healthy within 30s")

    yield base
    proc.terminate()
    try:
        proc.wait(timeout=10)
    except subprocess.TimeoutExpired:
        proc.kill()
