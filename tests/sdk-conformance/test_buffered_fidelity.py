"""Real SDKs distinguish portable text from native state and source accounting.

Synthetic origins only. A failed projection never causes another inference call.
"""
import json
import sqlite3
import time

import anthropic
from google import genai
from google.genai import types
import httpx
import openai
import pytest

from test_backend_registry import backend_binaries  # noqa: F401
from test_tracked_protocols import (FAMILIES, evidence, qualified_origin,  # noqa: F401
                                    response_for, tracked_gateway)


def source_response(family, opaque):
    body = response_for(family)
    if family == FAMILIES[0]:
        message = body["choices"][0]["message"]
        message["reasoning_content"] = "materialized"
        if opaque:
            message["audio"] = {"id": "audio_fixture", "data": "opaque", "transcript": "sample", "expires_at": 4_000_000_000}
    elif family == FAMILIES[1]:
        body["content"].insert(0, {"type": "thinking", "thinking": "materialized", "signature": "opaque" if opaque else ""})
    elif family == FAMILIES[2]:
        body["output"].insert(0, {"type": "reasoning", "id": "rs_fixture", "summary": [
            {"type": "summary_text", "text": "materialized"}], "encrypted_content": "opaque" if opaque else None})
    else:
        part = {"text": "materialized", "thought": True}
        if opaque:
            part["thoughtSignature"] = "b3BhcXVl"
        body["candidates"][0]["content"]["parts"].insert(0, part)
    return body


def call_with_reasoning(instance, token, ingress):
    headers = {"x-sandhi-session": "buffered-fidelity"}
    if ingress in (FAMILIES[0], FAMILIES[2]):
        with openai.OpenAI(base_url=instance.base + "/v1", api_key=token, default_headers=headers) as client:
            if ingress == FAMILIES[0]:
                response = client.chat.completions.create(model="agent", max_tokens=64,
                    messages=[{"role": "user", "content": "hello"}])
                message = response.choices[0].message
                return message.content, message.reasoning_content, response.model_dump(exclude_none=True)
            response = client.responses.create(model="agent", input="hello", max_output_tokens=64)
            reasoning = "".join(part.text for item in response.output if item.type == "reasoning" for part in item.summary)
            return response.output_text, reasoning, response.model_dump(exclude_none=True)
    if ingress == FAMILIES[1]:
        with anthropic.Anthropic(base_url=instance.base, api_key=token, default_headers=headers) as client:
            response = client.messages.create(model="agent", max_tokens=64, messages=[{"role": "user", "content": "hello"}])
            return ("".join(block.text for block in response.content if block.type == "text"),
                    "".join(block.thinking for block in response.content if block.type == "thinking"),
                    response.model_dump(exclude_none=True))
    with genai.Client(api_key=token, http_options=types.HttpOptions(base_url=instance.base, headers=headers,
        retry_options=types.HttpRetryOptions(attempts=1))) as client:
        response = client.models.generate_content(model="agent", contents="hello",
            config=types.GenerateContentConfig(max_output_tokens=64))
        parts = response.candidates[0].content.parts
        return ("".join(part.text for part in parts if part.text and not part.thought),
                "".join(part.text for part in parts if part.text and part.thought),
                response.model_dump(exclude_none=True, mode="json"))


@pytest.mark.parametrize("family", FAMILIES)
@pytest.mark.parametrize("ingress", FAMILIES)
@pytest.mark.parametrize("opaque", [False, True])
def test_buffered_sdk_fidelity_refusals_preserve_source_counts_and_both_settlements(
    tmp_path, backend_binaries, qualified_origin, family, ingress, opaque,
):
    origin, calls, control = qualified_origin
    control["response_override"] = source_response(family, opaque)
    with tracked_gateway(tmp_path, backend_binaries, origin, family) as (instance, token):
        refuses = opaque and family != ingress
        if refuses:
            if ingress in (FAMILIES[0], FAMILIES[2]):
                error_type = openai.APIStatusError
            elif ingress == FAMILIES[1]:
                error_type = anthropic.APIStatusError
            else:
                error_type = genai.errors.ServerError
            with pytest.raises(error_type) as caught:
                call_with_reasoning(instance, token, ingress)
            if ingress != FAMILIES[3]:
                assert caught.value.status_code == 502
                assert caught.value.response.headers["x-should-retry"] == "false"
            else:
                assert caught.value.code == 502
        else:
            reply, reasoning, parsed = call_with_reasoning(instance, token, ingress)
            assert reply == "reply" and reasoning == "materialized"
            if opaque:
                assert ("b3BhcXVl" if family == FAMILIES[3] else "opaque") in json.dumps(parsed), "native SDK output lost source state"
        assert len(calls) == 1
        row, context, usage, charge = evidence(instance.database)
        assert context["ingress"] == ingress and context["endpoint_family"] == family
        assert usage["completeness"] == "final" and usage["upstream_request_id"] == "synthetic-origin-request"
        assert usage["outcome"] == ("error" if refuses else "success")
        assert charge == (7 if family == FAMILIES[3] else 5)
        instance.stop()
        with sqlite3.connect(instance.database) as connection:
            assert connection.execute("SELECT charged_tokens FROM budget_settlement_outbox").fetchall() == [(charge,)]
            assert connection.execute("SELECT settled,actual FROM budget_reservation").fetchall() == [(1, charge)] * 2
            assert connection.execute("SELECT count(*) FROM usage_events").fetchone() == (1,)
            assert connection.execute("SELECT execution_id FROM budget_terminal_observation").fetchone() == (row[0],)


@pytest.mark.parametrize("family,ingress", [
    (family, ingress) for family in FAMILIES for ingress in FAMILIES[:3] if family != ingress
])
def test_deterministic_output_refusal_does_not_retry_sdk_inference(
    tmp_path, backend_binaries, qualified_origin, family, ingress,
):
    origin, calls, control = qualified_origin
    control["response_override"] = source_response(family, True)
    with tracked_gateway(tmp_path, backend_binaries, origin, family) as (instance, token):
        error_type = anthropic.APIStatusError if ingress == FAMILIES[1] else openai.APIStatusError
        with pytest.raises(error_type) as caught:
            call_with_reasoning(instance, token, ingress)
        assert caught.value.status_code == 502
        assert len(calls) == 1, "deterministic output refusal must not repeat paid inference"
        assert caught.value.response.headers["x-should-retry"] == "false"
        error = caught.value.response.json()["error"]
        assert error["code"] == "output_projection_error"
        assert error["retryable"] is False


@pytest.mark.parametrize("family", FAMILIES)
def test_refused_buffered_projection_recovers_original_receipt_without_inference_replay(
    tmp_path, backend_binaries, qualified_origin, family,
):
    origin, calls, control = qualified_origin
    control["response_override"] = source_response(family, True)
    with tracked_gateway(tmp_path, backend_binaries, origin, family) as (instance, token):
        with sqlite3.connect(instance.database) as connection:
            connection.execute("CREATE TRIGGER fail_projection_receipt BEFORE INSERT ON budget_settlement_outbox BEGIN SELECT RAISE(ABORT,'synthetic receipt failure'); END")
        path, request = ("/v1/responses", {"model": "agent", "input": "hello", "max_output_tokens": 64}) if family == FAMILIES[0] else (
            "/v1/chat/completions", {"model": "agent", "max_tokens": 64, "messages": [{"role": "user", "content": "hello"}]})
        response = httpx.post(instance.base + path, headers={"authorization": "Bearer " + token}, json=request)
        assert response.status_code == 502 and response.headers["x-should-retry"] == "false"
        original, _, usage, charge = evidence(instance.database)
        assert usage["completeness"] == "final" and usage["outcome"] == "error"
        with sqlite3.connect(instance.database) as connection:
            assert connection.execute("SELECT count(*) FROM budget_settlement_outbox").fetchone() == (0,)
            assert connection.execute("SELECT settled,actual FROM budget_reservation").fetchall() == [(0, 0)] * 2
        instance.process.kill()
        assert instance.process.wait(timeout=3) < 0
        instance.stop()
        with sqlite3.connect(instance.database) as connection:
            connection.execute("DROP TRIGGER fail_projection_receipt")
        instance.start(SANDHI_CACHE_READ_WEIGHT="10000")
        deadline = time.monotonic() + 5
        while True:
            with sqlite3.connect(instance.database) as connection:
                receipts = connection.execute("SELECT charged_tokens FROM budget_settlement_outbox").fetchall()
            if receipts == [(charge,)]:
                break
            assert time.monotonic() < deadline
            time.sleep(.02)
        assert evidence(instance.database)[0] == original
        assert len(calls) == 1
        instance.stop()
        with sqlite3.connect(instance.database) as connection:
            assert connection.execute("SELECT settled,actual FROM budget_reservation").fetchall() == [(1, charge)] * 2
