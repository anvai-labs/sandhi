"""The OpenAI SDK, unmodified, against the proxy (TD-0010 D5).

This dialect already worked — the SDK sends `Authorization: Bearer`, which is what the proxy read.
The suite exists anyway: it is the regression guard that keeps the working dialect working while
D1's per-dialect credential schemes and D2–D4 change the surrounding code, and it holds both
dialects to one standard of proof.
"""

from __future__ import annotations

import openai
import pytest
from conftest import REAL_OPENAI_KEY, VK_OPENAI


@pytest.fixture
def client(proxy: str) -> openai.OpenAI:
    # `/v1` because the SDK appends paths to the base it is given.
    return openai.OpenAI(base_url=f"{proxy}/v1", api_key=VK_OPENAI, max_retries=0)


def test_chat_completion_works_unmodified(client: openai.OpenAI):
    completion = client.chat.completions.create(
        model="gpt-mock",
        messages=[{"role": "user", "content": "ping"}],
    )

    assert completion.choices[0].message.content == "pong"
    assert completion.usage.prompt_tokens == 11
    assert completion.usage.completion_tokens == 3


def test_upstream_receives_the_real_key_not_the_virtual_one(client: openai.OpenAI, upstream):
    client.chat.completions.create(
        model="gpt-mock",
        messages=[{"role": "user", "content": "ping"}],
    )
    received = upstream.last()

    assert received.headers.get("authorization") == f"Bearer {REAL_OPENAI_KEY}"
    flattened = " ".join(f"{k}:{v}" for k, v in received.headers.items())
    assert VK_OPENAI not in flattened, "the virtual key must never reach the provider"


def test_streaming_works_unmodified(client: openai.OpenAI):
    chunks = [
        chunk.choices[0].delta.content
        for chunk in client.chat.completions.create(
            model="gpt-mock",
            messages=[{"role": "user", "content": "ping"}],
            stream=True,
        )
        if chunk.choices and chunk.choices[0].delta.content
    ]

    assert "".join(chunks) == "pong"


def test_x_api_key_is_not_accepted_on_the_openai_dialect(proxy: str):
    """Anthropic's scheme must not silently work here (TD-0010 D1: dialect-native only)."""
    import httpx

    response = httpx.post(
        f"{proxy}/v1/chat/completions",
        headers={"x-api-key": VK_OPENAI, "content-type": "application/json"},
        json={"model": "gpt-mock", "messages": [{"role": "user", "content": "ping"}]},
        timeout=10,
    )
    assert response.status_code == 401


def test_a_wrong_virtual_key_is_rejected(proxy: str):
    bad = openai.OpenAI(base_url=f"{proxy}/v1", api_key="vk_not_a_real_key", max_retries=0)
    with pytest.raises(openai.APIStatusError) as excinfo:
        bad.chat.completions.create(
            model="gpt-mock",
            messages=[{"role": "user", "content": "ping"}],
        )
    assert excinfo.value.status_code == 401
