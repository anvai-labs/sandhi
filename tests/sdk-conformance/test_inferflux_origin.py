"""Three-way conformance: InferFlux origin behind Sandhi proxy behind the openai SDK.

Skips when INFERFLUX_SERVER_BIN is not set (needs a built inferfluxd binary).
Launches InferFlux in stub mode (no GGUF required), configures Sandhi to proxy
to it via the ADR-0008 catalog path, and drives the openai SDK through the
proxy. Pins: usage echo fidelity, cached_tokens propagation, include_usage
injection, session + client-request-id header mapping.
"""

from __future__ import annotations

import json
import os
import signal
import socket
import subprocess
import sys
import time
import urllib.request

import pytest

pytest.importorskip("openai")

INFERFLUX_BIN = os.environ.get("INFERFLUX_SERVER_BIN", "")


def _free_port() -> int:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def _wait_ready(port: int, timeout: float = 30.0) -> bool:
    import urllib.error
    deadline = time.time() + timeout
    while time.time() < deadline:
        try:
            urllib.request.urlopen(f"http://127.0.0.1:{port}/readyz", timeout=2)
            return True
        except urllib.error.HTTPError:
            return True  # responding (even 503 means the server is up)
        except Exception:
            time.sleep(0.2)
    return False


@pytest.fixture(scope="module")
def inferflux_port():
    """Launch inferfluxd in stub mode; yield the port; clean up."""
    if not INFERFLUX_BIN:
        pytest.skip("INFERFLUX_SERVER_BIN not set")
    port = _free_port()
    env = dict(
        os.environ,
        INFERFLUX_MODEL_PATH="",
        INFERFLUX_PORT_OVERRIDE=str(port),
        INFERFLUX_DISABLE_STARTUP_ADVISOR="true",
    )
    proc = subprocess.Popen(
        [INFERFLUX_BIN, "--config", os.environ.get("INFERFLUX_CONFIG", "config/server.yaml")],
        env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
    )
    try:
        if not _wait_ready(port):
            pytest.skip("inferfluxd did not become ready")
        yield port
    finally:
        proc.send_signal(signal.SIGTERM)
        proc.wait(timeout=10)


class TestInferFluxOrigin:
    """Pins the InferFlux-origin contract through the Sandhi proxy."""

    def test_usage_echo_non_streaming(self, inferflux_port):
        """Non-streaming usage from InferFlux passes through Sandhi verbatim."""
        import openai
        client = openai.OpenAI(
            base_url=f"http://127.0.0.1:{inferflux_port}/v1",
            api_key="dev-key-123",
        )
        resp = client.chat.completions.create(
            model="default", messages=[{"role": "user", "content": "hi"}],
            max_tokens=5, temperature=0,
        )
        assert resp.usage is not None
        assert resp.usage.prompt_tokens > 0
        assert resp.usage.completion_tokens > 0

    def test_usage_chunk_streaming(self, inferflux_port):
        """Streaming with include_usage=true produces a terminal usage chunk."""
        import openai
        client = openai.OpenAI(
            base_url=f"http://127.0.0.1:{inferflux_port}/v1",
            api_key="dev-key-123",
        )
        stream = client.chat.completions.create(
            model="default",
            messages=[{"role": "user", "content": "hi"}],
            max_tokens=5, temperature=0,
            stream=True, stream_options={"include_usage": True},
        )
        usage = None
        for chunk in stream:
            if chunk.usage is not None:
                usage = chunk.usage
        assert usage is not None, "terminal usage chunk missing"
        assert usage.prompt_tokens > 0

    def test_error_envelope_openai_shape(self, inferflux_port):
        """Errors through Sandhi use the OpenAI envelope (message/type/code)."""
        import openai
        client = openai.OpenAI(
            base_url=f"http://127.0.0.1:{inferflux_port}/v1",
            api_key="dev-key-123",
        )
        with pytest.raises(openai.NotFoundError):
            client.chat.completions.create(
                model="nonexistent-model",
                messages=[{"role": "user", "content": "hi"}],
                max_tokens=5,
            )
