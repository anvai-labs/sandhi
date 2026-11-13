"""Three real-SDK turns join Gemini function identities to actual result wires."""
import json
import sqlite3
import time

import anthropic
from google import genai
from google.genai import types
import openai
import pytest

from test_backend_registry import backend_binaries  # noqa: F401
from test_tracked_protocols import FAMILIES, tracked_gateway, qualified_origin  # noqa: F401


@pytest.mark.parametrize("ingress", FAMILIES)
@pytest.mark.parametrize("streaming", [False, True])
@pytest.mark.parametrize("supplied_id", [False, True])
def test_gemini_three_turn_tool_history_keeps_result_names_and_call_identity(
    tmp_path, backend_binaries, qualified_origin, ingress, streaming, supplied_id,
):
    origin, calls, control = qualified_origin
    control.update(tool_loop=True, supplied_tool_id=supplied_id)
    schema = {"type": "object", "properties": {}}
    names = ("alpha", "beta")
    with tracked_gateway(tmp_path, backend_binaries, origin, FAMILIES[3], streaming=streaming) as (instance, token):
        headers = {"x-sandhi-session": "three-turn-tools"}
        if ingress in (FAMILIES[0], FAMILIES[2]):
            client = openai.OpenAI(base_url=instance.base + "/v1", api_key=token, max_retries=0, default_headers=headers)
        elif ingress == FAMILIES[1]:
            client = anthropic.Anthropic(base_url=instance.base, api_key=token, max_retries=0, default_headers=headers)
        else:
            client = genai.Client(api_key=token, http_options=types.HttpOptions(base_url=instance.base, headers=headers,
                retry_options=types.HttpRetryOptions(attempts=1)))
        history = [{"role": "user", "content": "hello"}]
        if ingress == FAMILIES[3]:
            history = [types.Content(role="user", parts=[types.Part.from_text(text="hello")])]
        identifiers = []
        try:
            for turn in range(3):
                if ingress == FAMILIES[0]:
                    tools = [{"type": "function", "function": {"name": name, "parameters": schema}} for name in names]
                    if streaming:
                        response = client.chat.completions.create(model="agent", max_tokens=64, messages=history, tools=tools, stream=True)
                        assembled, reply = {}, ""
                        for event in response:
                            if not event.choices:
                                continue
                            delta = event.choices[0].delta
                            reply += delta.content or ""
                            for call in delta.tool_calls or []:
                                item = assembled.setdefault(call.index, {"id": "", "name": "", "arguments": ""})
                                if call.id:
                                    item["id"] = call.id
                                if call.function:
                                    item["name"] += call.function.name or ""
                                    item["arguments"] += call.function.arguments or ""
                        tool_calls = [{"id": item["id"], "type": "function", "function": {"name": item["name"], "arguments": item["arguments"]}} for item in assembled.values()]
                    else:
                        response = client.chat.completions.create(model="agent", max_tokens=64, messages=history, tools=tools)
                        reply = response.choices[0].message.content or ""
                        tool_calls = [call.model_dump(exclude_none=True) for call in response.choices[0].message.tool_calls or []]
                    if turn < 2:
                        call = tool_calls[0]
                        assert call["function"]["name"] == names[turn]
                        identifiers.append(call["id"])
                        history += [{"role": "assistant", "tool_calls": tool_calls},
                                    {"role": "tool", "tool_call_id": call["id"], "content": names[turn] + "-result"}]
                elif ingress == FAMILIES[1]:
                    tools = [{"name": name, "input_schema": schema} for name in names]
                    if streaming:
                        with client.messages.stream(model="agent", max_tokens=64, messages=history, tools=tools) as stream:
                            response = stream.get_final_message()
                    else:
                        response = client.messages.create(model="agent", max_tokens=64, messages=history, tools=tools)
                    reply = "".join(block.text for block in response.content if block.type == "text")
                    if turn < 2:
                        call = next(block for block in response.content if block.type == "tool_use")
                        assert call.name == names[turn]
                        identifiers.append(call.id)
                        history += [{"role": "assistant", "content": [block.model_dump(exclude_none=True) for block in response.content]},
                                    {"role": "user", "content": [{"type": "tool_result", "tool_use_id": call.id, "content": names[turn] + "-result"}]}]
                elif ingress == FAMILIES[2]:
                    tools = [{"type": "function", "name": name, "parameters": schema} for name in names]
                    response = client.responses.create(model="agent", max_output_tokens=64, input=history, tools=tools, stream=streaming)
                    if streaming:
                        response = list(response)[-1].response
                    reply = response.output_text
                    if turn < 2:
                        call = next(item for item in response.output if item.type == "function_call")
                        assert call.name == names[turn]
                        identifiers.append(call.call_id)
                        history += [item.model_dump(exclude_none=True) for item in response.output]
                        history.append({"type": "function_call_output", "call_id": call.call_id, "output": names[turn] + "-result"})
                else:
                    config = types.GenerateContentConfig(max_output_tokens=64, tools=[{"function_declarations": [
                        {"name": name, "parameters": schema} for name in names]}])
                    if streaming:
                        events = list(client.models.generate_content_stream(model="agent", contents=history, config=config))
                        response = next(event for event in events if event.candidates and event.candidates[0].content and event.candidates[0].content.parts)
                    else:
                        response = client.models.generate_content(model="agent", contents=history, config=config)
                    reply = response.text or ""
                    if turn < 2:
                        call = response.function_calls[0]
                        assert call.name == names[turn]
                        identifiers.append(call.id)
                        history += [response.candidates[0].content, types.Content(role="user", parts=[types.Part(
                            function_response=types.FunctionResponse(name=call.name, id=call.id, response={"output": names[turn] + "-result"}))])]
                if turn == 2:
                    assert reply == "reply"
        finally:
            client.close()
        if supplied_id:
            assert identifiers == ["upstream-reused"] * 2
        elif ingress != FAMILIES[3]:
            assert len(set(identifiers)) == 2
        assert len(calls) == 3
        results = [part["functionResponse"] for content in calls[2][1]["contents"] for part in content["parts"] if "functionResponse" in part]
        assert [result["name"] for result in results] == list(names)
        assert all(names[index] + "-result" in json.dumps(result["response"]) for index, result in enumerate(results))
        deadline = time.monotonic() + 5
        while True:
            with sqlite3.connect(instance.database) as connection:
                receipts = connection.execute("SELECT charged_tokens FROM budget_settlement_outbox").fetchall()
            if len(receipts) == 3:
                break
            assert time.monotonic() < deadline
            time.sleep(.02)
        assert receipts == [(5,)] * 3
        with sqlite3.connect(instance.database) as connection:
            assert connection.execute("SELECT settled,actual FROM budget_reservation").fetchall() == [(1, 5)] * 6
