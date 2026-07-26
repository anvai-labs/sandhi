"""The Anthropic SDK, unmodified, against the proxy (TD-0010 D1/D5).

This is the suite that would have caught the shipped defect. The proxy read only
`Authorization: Bearer`, while `anthropic.Anthropic` authenticates with `x-api-key` — so every
stock client got a 401, and no test noticed because every test hand-rolled the header the proxy
happened to want.

Nothing here sets a header by hand. `api_key=` is passed the way a user would pass it, and the SDK
decides how to present it.
"""

from __future__ import annotations

import anthropic
import pytest
from conftest import REAL_ANTHROPIC_KEY, VK_ANTHROPIC


@pytest.fixture
def client(proxy: str) -> anthropic.Anthropic:
    # Exactly the two arguments the README tells a user to change.
    return anthropic.Anthropic(base_url=proxy, api_key=VK_ANTHROPIC, max_retries=0)


def test_messages_create_works_unmodified(client: anthropic.Anthropic, upstream):
    message = client.messages.create(
        model="claude-mock",
        max_tokens=64,
        messages=[{"role": "user", "content": "ping"}],
    )

    assert message.content[0].text == "pong"
    assert message.usage.input_tokens == 11
    assert message.usage.output_tokens == 3

    # The SDK authenticated its own way (x-api-key) and the proxy accepted it — reaching the
    # upstream at all is the proof, since a rejected credential never gets this far.
    assert upstream.last().path.endswith("/v1/messages")


def test_the_client_never_sends_and_upstream_never_sees_the_virtual_key(
    client: anthropic.Anthropic, upstream
):
    client.messages.create(
        model="claude-mock",
        max_tokens=64,
        messages=[{"role": "user", "content": "ping"}],
    )
    received = upstream.last()

    # The whole point of an inline gate: the virtual key stops at the proxy and the real upstream
    # credential is substituted server-side.
    assert received.headers.get("x-api-key") == REAL_ANTHROPIC_KEY
    flattened = " ".join(f"{k}:{v}" for k, v in received.headers.items())
    assert VK_ANTHROPIC not in flattened, "the virtual key must never reach the provider"


def test_streaming_works_unmodified(client: anthropic.Anthropic):
    chunks: list[str] = []
    with client.messages.stream(
        model="claude-mock",
        max_tokens=64,
        messages=[{"role": "user", "content": "ping"}],
    ) as stream:
        for text in stream.text_stream:
            chunks.append(text)

    assert "".join(chunks) == "pong"


def test_a_wrong_virtual_key_is_rejected(proxy: str):
    bad = anthropic.Anthropic(base_url=proxy, api_key="vk_not_a_real_key", max_retries=0)
    with pytest.raises(anthropic.APIStatusError) as excinfo:
        bad.messages.create(
            model="claude-mock",
            max_tokens=8,
            messages=[{"role": "user", "content": "ping"}],
        )
    assert excinfo.value.status_code == 401


def test_models_list_works_unmodified(client: anthropic.Anthropic):
    page = client.models.list()
    assert page.data, "discovery should return the key's permitted models"
    assert all(m.type == "model" for m in page.data)
