"""Opt-in release probe: actual Victor policy + built binding + proxy + origin.

Run with VICTOR_CODESIGN_E2E=1, Victor on PYTHONPATH, the candidate Sandhi wheel
installed, and the same INFERFLUX_SERVER_BIN/CONFIG as test_inferflux_origin.py.
This intentionally does not silently use whichever Victor happens to be installed in CI.
"""

import os

import pytest

if os.environ.get("VICTOR_CODESIGN_E2E") != "1":
    pytest.skip("explicit three-repository release probe only", allow_module_level=True)

from test_inferflux_origin import (  # noqa: E402,F401
    inferflux_origin,
    inferflux_proxy,
    recording_origin,
)
from victor.agent.stream_handler import StreamMetrics  # noqa: E402
from victor.providers.base import Message  # noqa: E402
from victor.providers.inferflux_provider import InferfluxProvider  # noqa: E402


@pytest.mark.asyncio
async def test_victor_consumes_real_origin_through_gateway(inferflux_proxy):
    provider = InferfluxProvider(
        api_key="unused-direct-credential",
        gateway={
            "url": inferflux_proxy.base_url,
            "virtual_key": "vk_inferflux_demo",
        },
        max_retries=0,
    )
    messages = [Message(role="user", content="Say hello")]
    response = await provider.chat(messages, model="default", max_tokens=32)
    assert response.content == "visible answer"
    assert response.usage["reasoning_tokens"] > 0
    assert response.usage["reasoning_included"] is True

    chunks = [
        chunk
        async for chunk in provider.stream(messages, model="default", max_tokens=32)
    ]
    assert "".join(chunk.content or "" for chunk in chunks) == "visible answer"
    assert (
        "".join((chunk.metadata or {}).get("reasoning_content", "") for chunk in chunks)
        == "chain of thought"
    )
    terminal = chunks[-1]
    assert terminal.is_final
    assert terminal.usage["reasoning_tokens"] > 0
    assert terminal.usage["reasoning_included"] is True
    metrics = StreamMetrics()
    metrics.record_usage(terminal.usage)
    metrics.record_wire_latency(terminal.metadata["sandhi_usage"])
    assert metrics.wire_duration_ms is not None
    assert metrics.wire_ttft_ms is not None
    assert metrics.metadata["duration_source"] == "origin"
    assert metrics.metadata["time_to_first_token_source"] == "origin"
    request = inferflux_proxy.recorder.requests[-1]
    assert request.headers["authorization"] == "Bearer dev-key-123"
    assert not any(key.startswith("x-sandhi-") for key in request.headers)
