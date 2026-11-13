"""Current clients consume qualified Responses output and return tool results.

Synthetic HTTP origins only; this does not qualify a subscription issuer.
"""
import copy
import json
import sqlite3
import time

import anthropic
from google import genai
from google.genai import types
import openai
import pytest

from test_backend_registry import backend_binaries  # noqa: F401
from test_tracked_protocols import (FAMILIES, HTTP_PROTOCOL_ERRORS, evidence, qualified_origin,  # noqa: F401
                                    response_for, tracked_gateway)


def source_events(mode):
    response = response_for(FAMILIES[2])
    events = [{"type": "response.created", "response": {"id": "resp_fixture",
        "model": "fixture", "status": "in_progress", "output": []}}]
    if mode == "obfuscation":
        events.append({"type": "response.output_text.delta", "output_index": 0,
            "content_index": 0, "item_id": "msg_fixture", "delta": "reply", "obfuscation": "padding"})
    elif mode == "interim_native":
        events.append({"type": "response.output_item.added", "output_index": 0,
            "item": {"type": "reasoning", "id": "rs_fixture", "summary": [], "encrypted_content": "opaque-interim"}})
    elif mode in ("terminal_tool", "tool_identity_drift"):
        call = {"type": "function_call", "id": "fc_fixture", "call_id": "call_fixture",
            "name": "inspect", "arguments": "{}", "status": "completed"}
        if mode == "tool_identity_drift":
            added = dict(call, arguments="", status="in_progress")
            events += [{"type": "response.output_item.added", "output_index": 0, "item": added},
                {"type": "response.function_call_arguments.delta", "output_index": 0, "item_id": "fc_fixture", "delta": "{}"}]
            call["call_id"] = "different"
            events.append({"type": "response.output_item.done", "output_index": 0, "item": copy.deepcopy(call)})
        response["output"] = [call]
    events.append({"type": "response.completed", "response": response})
    return events


def stream_turn(client, ingress, history):
    schema = {"type": "object", "properties": {}}
    if ingress == FAMILIES[0]:
        with client.chat.completions.stream(model="agent", max_tokens=64, messages=history,
            tools=[{"type": "function", "function": {"name": "inspect", "parameters": schema}}]) as stream:
            message = stream.get_final_completion().choices[0].message
        tool = next(iter(message.tool_calls or []), None)
        if tool:
            assert (tool.id, tool.function.name, json.loads(tool.function.arguments)) == ("call_fixture", "inspect", {})
            history += [{"role": "assistant", "tool_calls": [tool.model_dump(exclude_none=True)]},
                        {"role": "tool", "tool_call_id": tool.id, "content": "inspected"}]
        return message.content or "", tool
    if ingress == FAMILIES[1]:
        with client.messages.stream(model="agent", max_tokens=64, messages=history,
            tools=[{"name": "inspect", "input_schema": schema}]) as stream:
            message = stream.get_final_message()
        tool = next((part for part in message.content if part.type == "tool_use"), None)
        if tool:
            assert (tool.id, tool.name, tool.input) == ("call_fixture", "inspect", {})
            history += [{"role": "assistant", "content": [part.model_dump(exclude_none=True) for part in message.content]},
                {"role": "user", "content": [{"type": "tool_result", "tool_use_id": tool.id, "content": "inspected"}]}]
        return "".join(part.text for part in message.content if part.type == "text"), tool
    parts = []
    for chunk in client.models.generate_content_stream(model="agent", contents=history,
        config=types.GenerateContentConfig(max_output_tokens=64,
            tools=[types.Tool(function_declarations=[types.FunctionDeclaration(name="inspect", parameters=schema)])])):
        for candidate in chunk.candidates or []:
            if candidate.content:
                parts += candidate.content.parts or []
    tool = next((part.function_call for part in parts if part.function_call), None)
    if tool:
        assert (tool.id, tool.name, tool.args) == ("call_fixture", "inspect", {})
        history += [types.Content(role="model", parts=parts),
            types.Content(role="user", parts=[types.Part(function_response=types.FunctionResponse(
                id=tool.id, name=tool.name, response={"result": "inspected"}))])]
    return "".join(part.text for part in parts if part.text and not part.thought), tool


@pytest.mark.parametrize("ingress", (FAMILIES[0], FAMILIES[1], FAMILIES[3]))
@pytest.mark.parametrize("mode", ("terminal_text", "terminal_tool", "obfuscation", "interim_native", "tool_identity_drift"))
def test_responses_source_current_sdk_accumulation_refusal_and_tool_continuation(
    tmp_path, backend_binaries, qualified_origin, ingress, mode,
):
    origin, calls, control = qualified_origin
    control["responses_events"] = source_events(mode)
    with tracked_gateway(tmp_path, backend_binaries, origin, FAMILIES[2], streaming=True) as (instance, token):
        headers = {"x-sandhi-session": "responses-source-fidelity"}
        history = [{"role": "user", "content": "hello"}]
        if ingress == FAMILIES[0]:
            client = openai.OpenAI(base_url=instance.base + "/v1", api_key=token, default_headers=headers)
            error_type = (openai.APIError,) + HTTP_PROTOCOL_ERRORS
        elif ingress == FAMILIES[1]:
            client = anthropic.Anthropic(base_url=instance.base, api_key=token, default_headers=headers)
            error_type = (anthropic.APIError,) + HTTP_PROTOCOL_ERRORS
        else:
            client = genai.Client(api_key=token, http_options=types.HttpOptions(base_url=instance.base,
                headers=headers, retry_options=types.HttpRetryOptions(attempts=1)))
            history = [types.Content(role="user", parts=[types.Part.from_text(text="hello")])]
            error_type = (genai.errors.APIError,) + HTTP_PROTOCOL_ERRORS
        expected_calls = 1
        try:
            if mode in ("interim_native", "tool_identity_drift"):
                with pytest.raises(error_type) as caught:
                    stream_turn(client, ingress, history)
                # Once SSE headers are sent, refusal aborts the body instead
                # of changing HTTP status. Both outcomes must avoid replay.
                if isinstance(caught.value, genai.errors.APIError):
                    assert caught.value.code == 502
                elif isinstance(caught.value, (openai.APIStatusError, anthropic.APIStatusError)):
                    assert caught.value.status_code == 502
                    assert caught.value.response.headers["x-should-retry"] == "false"
            else:
                text, tool = stream_turn(client, ingress, history)
                if mode == "terminal_tool":
                    assert tool is not None
                    control["responses_events"] = source_events("terminal_text")
                    assert stream_turn(client, ingress, history) == ("reply", None)
                    expected_calls = 2
                    sent = calls[-1][1]
                    assert any(item.get("type") == "function_call_output" and item["call_id"] == "call_fixture"
                        and "inspected" in item["output"] for item in sent["input"])
                else:
                    assert (text, tool) == ("reply", None)
        finally:
            client.close()
        deadline = time.monotonic() + 5
        while True:
            with sqlite3.connect(instance.database) as connection:
                receipts = connection.execute("SELECT charged_tokens FROM budget_settlement_outbox").fetchall()
            if len(receipts) == expected_calls:
                break
            assert time.monotonic() < deadline
            time.sleep(.02)
        assert len(calls) == expected_calls and receipts == [(5,)] * expected_calls
        _, context, usage, charge = evidence(instance.database)
        assert context["plane"] == "translation" and usage["completeness"] == "final"
        assert (usage["tokens_in"], usage["cache_read_tokens"], usage["cache_creation_tokens"],
            usage["tokens_out"], usage["reasoning_tokens"], charge) == (2, 2, 1, 2, 1, 5)
        with sqlite3.connect(instance.database) as connection:
            assert connection.execute("SELECT settled,actual FROM budget_reservation").fetchall() == [(1, 5)] * (2 * expected_calls)
