"""The Google Gemini SDK, unmodified, against the proxy (TD-0010 D4a).

Gemini is the first dialect whose **model and streaming choice live in the URL**
(`/v1beta/models/{model}:generateContent`) rather than the body, and whose credential is
`x-goog-api-key`. D4a admits it on the transparent plane only: a Gemini client must resolve to a
Gemini upstream, where the body is forwarded byte-for-byte and metered in flight.

The `?key=` query form Google also documents is deliberately NOT accepted — it would put a live
virtual key into access logs and crash reports. That gap is stated in the README matrix rather
than quietly closed.
"""

from __future__ import annotations

import httpx
import pytest
from conftest import REAL_GEMINI_KEY, VK_GEMINI

genai = pytest.importorskip("google.genai", reason="google-genai SDK not installed")


@pytest.fixture
def client(proxy: str):
    # The two things a user changes: where it points, and the key it presents.
    return genai.Client(
        api_key=VK_GEMINI,
        http_options=genai.types.HttpOptions(base_url=proxy),
    )


def test_generate_content_works_unmodified(client, upstream):
    response = client.models.generate_content(
        model="gemini-mock",
        contents="ping",
    )

    assert response.text == "pong"
    assert response.usage_metadata.prompt_token_count == 11
    assert response.usage_metadata.candidates_token_count == 3
    # The model rode in the path all the way to the upstream.
    assert ":generateContent" in upstream.last().path
    assert "gemini-mock" in upstream.last().path


def test_upstream_receives_the_real_key_not_the_virtual_one(client, upstream):
    client.models.generate_content(model="gemini-mock", contents="ping")
    received = upstream.last()

    assert received.headers.get("x-goog-api-key") == REAL_GEMINI_KEY
    flattened = " ".join(f"{k}:{v}" for k, v in received.headers.items())
    assert VK_GEMINI not in flattened, "the virtual key must never reach the provider"
    assert VK_GEMINI not in received.path, "nor may it leak through the URL"


def test_streaming_works_unmodified(client, upstream):
    chunks = [
        chunk.text
        for chunk in client.models.generate_content_stream(model="gemini-mock", contents="ping")
        if chunk.text
    ]

    assert "".join(chunks) == "pong"
    # The streaming verb is a path segment, and `?alt=sse` is what makes the framing SSE.
    assert ":streamGenerateContent" in upstream.last().path
    assert "alt=sse" in upstream.last().path


def test_the_query_key_form_is_not_accepted(proxy: str):
    """`?key=` is a documented Gemini scheme we deliberately refuse (TD-0010 D4a).

    Accepting it would put a live virtual key in a URL — access logs, crash reports, browser
    history. The refusal must be explicit, not incidental.
    """
    response = httpx.post(
        f"{proxy}/v1beta/models/gemini-mock:generateContent",
        params={"key": VK_GEMINI},
        json={"contents": [{"role": "user", "parts": [{"text": "ping"}]}]},
        timeout=10,
    )
    assert response.status_code == 401


def test_errors_come_back_in_googles_shape(proxy: str):
    """A google-genai client classifies failures from `error.code` / `error.status`."""
    response = httpx.post(
        f"{proxy}/v1beta/models/gemini-mock:generateContent",
        headers={"x-goog-api-key": "vk_not_a_real_key"},
        json={"contents": [{"role": "user", "parts": [{"text": "ping"}]}]},
        timeout=10,
    )
    assert response.status_code == 401
    error = response.json()["error"]
    assert error["code"] == 401
    assert error["status"] == "UNAUTHENTICATED"


def test_an_unsupported_method_is_refused_in_googles_shape(proxy: str):
    response = httpx.post(
        f"{proxy}/v1beta/models/gemini-mock:countTokens",
        headers={"x-goog-api-key": VK_GEMINI},
        json={"contents": [{"role": "user", "parts": [{"text": "ping"}]}]},
        timeout=10,
    )
    assert response.status_code == 501
    assert response.json()["error"]["status"] == "UNIMPLEMENTED"
