"""Native Cohere/Ollama preservation through all four real client SDKs.

Synthetic loopback sources only. Physical source counts, original identity and
paired settlement remain authoritative on successful or refused projection.
"""

from contextlib import contextmanager
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import sqlite3
import threading

import httpx
import openai
import pytest
from test_backend_registry import Gateway
from test_backend_registry import backend_binaries as _backend_binaries  # noqa: F401
from test_tracked_protocols import FAMILIES, evidence


@pytest.fixture(name="backend_binaries")
def native_source_binaries(request):
    return request.getfixturevalue("_backend_binaries")


def source_body(family):
    if family == "cohere_chat":
        return {
            "id": "native-source",
            "message": {
                "role": "assistant",
                "content": [{"type": "text", "text": "reply"}],
            },
            "finish_reason": "COMPLETE",
            "usage": {
                "tokens": {"input_tokens": 7, "output_tokens": 4},
                "billed_units": {"input_tokens": 2, "output_tokens": 1},
            },
        }
    return {
        "model": "fixture",
        "message": {"role": "assistant", "content": "reply"},
        "done": True,
        "done_reason": "stop",
        "prompt_eval_count": 7,
        "prompt_eval_cached_count": 3,
        "eval_count": 4,
    }


@contextmanager
def origin(family, scenario):
    calls = []

    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *_args):
            pass

        def do_POST(self):
            request = json.loads(self.rfile.read(int(self.headers["content-length"])))
            calls.append((self.path, request))
            expected = "/v2/chat" if family == "cohere_chat" else "/api/chat"
            assert self.path == expected
            body = source_body(family)
            if scenario == "tools" and len(calls) <= 2:
                tool_calls = []
                for index, name in enumerate(("beta", "alpha")):
                    args = {"n": len(calls) * 10 + index}
                    tool_calls.append(
                        {
                            "id": f"native-{len(calls)}-{index}",
                            "type": "function",
                            "function": {"name": name, "arguments": json.dumps(args)},
                        }
                        if family == "cohere_chat"
                        else {
                            "function": {
                                "index": index,
                                "name": name,
                                "arguments": args,
                            }
                        }
                    )
                body["message"]["tool_calls"] = tool_calls
                body["message"]["content"] = [] if family == "cohere_chat" else ""
                if family == "cohere_chat":
                    body["finish_reason"] = "TOOL_CALL"
            if scenario == "opaque":
                body["message"]["opaque_state"] = "must-not-disappear"
            if scenario == "missing":
                if family == "cohere_chat":
                    body["usage"].pop("tokens")
                else:
                    body.pop("eval_count")
            if request.get("stream"):
                if family == "cohere_chat":
                    frames = [
                        {
                            "type": "message-start",
                            "id": "native-source",
                            "delta": {"message": {"role": "assistant"}},
                        },
                        {
                            "type": "content-start",
                            "index": 0,
                            "delta": {
                                "message": {"content": {"type": "text", "text": ""}}
                            },
                        },
                        {
                            "type": "content-delta",
                            "index": 0,
                            "delta": {"message": {"content": {"text": "reply"}}},
                        },
                        {"type": "content-end", "index": 0},
                        {
                            "type": "message-end",
                            "delta": {
                                "finish_reason": "COMPLETE",
                                "usage": body["usage"],
                            },
                        },
                    ]
                    if body["message"].get("tool_calls"):
                        frames = [frames[0]]
                        for index, call in enumerate(body["message"]["tool_calls"]):
                            frames += [
                                {
                                    "type": "tool-call-start",
                                    "index": index,
                                    "delta": {
                                        "message": {
                                            "tool_calls": {
                                                "id": call["id"],
                                                "type": "function",
                                                "function": {
                                                    "name": call["function"]["name"],
                                                    "arguments": "",
                                                },
                                            }
                                        }
                                    },
                                },
                                {
                                    "type": "tool-call-delta",
                                    "index": index,
                                    "delta": {
                                        "message": {
                                            "tool_calls": {
                                                "function": {
                                                    "arguments": call["function"][
                                                        "arguments"
                                                    ]
                                                }
                                            }
                                        }
                                    },
                                },
                                {"type": "tool-call-end", "index": index},
                            ]
                        frames.append(
                            {
                                "type": "message-end",
                                "delta": {
                                    "finish_reason": "TOOL_CALL",
                                    "usage": body["usage"],
                                },
                            }
                        )
                    if scenario == "opaque":
                        frames[2]["delta"]["message"]["content"][
                            "opaque_state"
                        ] = "must-not-disappear"
                    wire = "".join(
                        "data: " + json.dumps(frame) + "\n\n" for frame in frames
                    )
                else:
                    wire = json.dumps(body) + "\n"
                encoded, content_type = wire.encode(), "text/event-stream"
            else:
                encoded, content_type = json.dumps(body).encode(), "application/json"
            if scenario == "ambiguous":
                if request.get("stream"):
                    # A duplicate required key in a complete data object cannot
                    # acquire source authority through last-value JSON parsing.
                    if family == "cohere_chat":
                        encoded = encoded.replace(
                            b'"type": "message-end"',
                            b'"type":"message-end","type":"message-end"',
                        )
                    else:
                        encoded = encoded.replace(
                            b'"eval_count": 4', b'"eval_count":4,"eval_count":4'
                        )
                else:
                    # Repeated equal values are still ambiguous wire input.
                    if family == "cohere_chat":
                        encoded = (
                            encoded[:-1]
                            + b',"usage":'
                            + json.dumps(body["usage"]).encode()
                            + b"}"
                        )
                    else:
                        encoded = encoded[:-1] + b',"eval_count":4}'
            self.send_response(200)
            self.send_header("content-type", content_type)
            self.send_header("content-length", str(len(encoded)))
            self.send_header("x-request-id", "synthetic-origin-request")
            self.end_headers()
            self.wfile.write(encoded)

    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        yield f"http://127.0.0.1:{server.server_port}", calls
    finally:
        server.shutdown()
        server.server_close()
        thread.join(timeout=2)


@contextmanager
def gateway(tmp_path, binaries, source, family):
    instance = Gateway(tmp_path, binaries)
    instance.config.write_text(
        json.dumps(
            {
                "streaming_deadlines": {
                    "ceiling_ms": 10000,
                    "default": {"setup_ms": 2000, "idle_ms": 2000, "body_ms": 5000},
                }
            }
        )
    )
    try:
        instance.start(
            SANDHI_SHUTDOWN_GRACE_SECS="1"
        )
        instance.provision(source, "synthetic-native-key")
        instance.cli(
            "set",
            "primary",
            "--provider",
            "native-label",
            "--credential-ref",
            "openai:default",
            "--endpoint-family",
            family,
            "--model",
            "fixture",
        )
        definition = {
            "id": "protocols",
            "models": [
                {"id": "agent", "targets": [{"backend": "primary", "model": "fixture"}]}
            ],
        }
        assert (
            instance.admin(
                "POST", "/admin/routes", json={"definition": definition}
            ).status_code
            == 201
        )
        for scope in ("group:protocol-team", "upstream:cohere", "upstream:ollama"):
            assert (
                instance.admin(
                    "POST",
                    "/admin/budget",
                    json={
                        "scope": scope,
                        "limit_tokens": 10000,
                        "window": "total",
                        "policy": "block",
                    },
                ).status_code
                == 200
            )
        minted = instance.admin(
            "POST",
            "/admin/keys/share",
            json={
                "upstream": "route/protocols",
                "models": ["agent"],
                "group": "protocol-team",
            },
        )
        assert minted.status_code == 200, minted.text
        yield instance, minted.json()["virtual_key"]
    finally:
        instance.stop()


def buffered_call(instance, token, ingress):
    headers = {"x-sandhi-session": "protocol-session"}
    if ingress in (FAMILIES[0], FAMILIES[2]):
        with openai.OpenAI(
            base_url=instance.base + "/v1", api_key=token, default_headers=headers
        ) as client:
            if ingress == FAMILIES[0]:
                return (
                    client.chat.completions.create(
                        model="agent",
                        max_tokens=64,
                        messages=[{"role": "user", "content": "hello"}],
                    )
                    .choices[0]
                    .message.content
                )
            return client.responses.create(
                model="agent", input="hello", max_output_tokens=64
            ).output_text
    if ingress == FAMILIES[1]:
        import anthropic

        with anthropic.Anthropic(
            base_url=instance.base, api_key=token, default_headers=headers
        ) as client:
            return (
                client.messages.create(
                    model="agent",
                    max_tokens=64,
                    messages=[{"role": "user", "content": "hello"}],
                )
                .content[0]
                .text
            )
    from google import genai
    from google.genai import types

    with genai.Client(
        api_key=token,
        http_options=types.HttpOptions(
            base_url=instance.base,
            headers=headers,
            retry_options=types.HttpRetryOptions(attempts=1),
        ),
    ) as client:
        return client.models.generate_content(
            model="agent",
            contents="hello",
            config=types.GenerateContentConfig(max_output_tokens=64),
        ).text


def stream_call(instance, token, ingress):
    headers = {"x-sandhi-session": "protocol-session"}
    if ingress in (FAMILIES[0], FAMILIES[2]):
        with openai.OpenAI(
            base_url=instance.base + "/v1", api_key=token, default_headers=headers
        ) as client:
            if ingress == FAMILIES[0]:
                return "".join(
                    e.choices[0].delta.content or ""
                    for e in client.chat.completions.create(
                        model="agent",
                        max_tokens=64,
                        messages=[{"role": "user", "content": "hello"}],
                        stream=True,
                    )
                    if e.choices
                )
            return "".join(
                e.delta
                for e in client.responses.create(
                    model="agent", input="hello", max_output_tokens=64, stream=True
                )
                if e.type == "response.output_text.delta"
            )
    if ingress == FAMILIES[1]:
        import anthropic

        with anthropic.Anthropic(
            base_url=instance.base, api_key=token, default_headers=headers
        ) as client:
            return "".join(
                e.delta.text
                for e in client.messages.create(
                    model="agent",
                    max_tokens=64,
                    messages=[{"role": "user", "content": "hello"}],
                    stream=True,
                )
                if e.type == "content_block_delta" and e.delta.type == "text_delta"
            )
    from google import genai
    from google.genai import types

    with genai.Client(
        api_key=token,
        http_options=types.HttpOptions(
            base_url=instance.base,
            headers=headers,
            retry_options=types.HttpRetryOptions(attempts=1),
        ),
    ) as client:
        return "".join(
            e.text or ""
            for e in client.models.generate_content_stream(
                model="agent",
                contents="hello",
                config=types.GenerateContentConfig(max_output_tokens=64),
            )
        )


@pytest.mark.parametrize("family", ["cohere_chat", "ollama_chat"])
@pytest.mark.parametrize("ingress", FAMILIES)
@pytest.mark.parametrize("streaming", [False, True])
def test_native_source_real_sdk_matrix_joins_original_source_attempt_and_paired_receipt(
    tmp_path,
    backend_binaries,
    family,
    ingress,
    streaming,
):
    with origin(family, "valid") as (source, calls), gateway(
        tmp_path, backend_binaries, source, family
    ) as (instance, token):
        call = stream_call if streaming else buffered_call
        assert call(instance, token, ingress) == "reply"
        assert len(calls) == 1
        # Restart uses persisted native endpoint binding and never replays the first call.
        instance.stop()
        instance.start(
            SANDHI_SHUTDOWN_GRACE_SECS="1"
        )
        assert len(calls) == 1
        row, context, usage, charge = evidence(instance.database)
        assert context["provider"] == "native-label"
        assert context["endpoint_family"] == family
        assert usage["tokens_in"] == (7 if family == "cohere_chat" else 4)
        assert usage["cache_read_tokens"] == (0 if family == "cohere_chat" else 3)
        assert usage["cache_creation_tokens"] == 0
        assert usage["tokens_out"] == 4
        assert usage.get("reasoning_tokens") is None
        assert usage["completeness"] == "final"
        assert charge == 11
        with sqlite3.connect(instance.database) as connection:
            assert connection.execute(
                "SELECT charged_tokens FROM budget_settlement_outbox"
            ).fetchall() == [(11,)]
            assert (
                connection.execute(
                    "SELECT settled,actual FROM budget_reservation"
                ).fetchall()
                == [(1, 11)] * 2
            )
            assert connection.execute(
                "SELECT count(*) FROM budget_physical_attempt"
            ).fetchone() == (1,)
            assert connection.execute(
                "SELECT execution_id FROM budget_terminal_observation"
            ).fetchone() == (row[0],)


@pytest.mark.parametrize("family", ["cohere_chat", "ollama_chat"])
@pytest.mark.parametrize("ingress", FAMILIES)
@pytest.mark.parametrize("streaming", [False, True])
@pytest.mark.parametrize("scenario", ["opaque", "missing", "ambiguous"])
def test_native_source_refusal_retains_source_accounting_without_another_inference(
    tmp_path,
    backend_binaries,
    family,
    ingress,
    streaming,
    scenario,
):
    with origin(family, scenario) as (source, calls), gateway(
        tmp_path, backend_binaries, source, family
    ) as (instance, token):
        call = stream_call if streaming else buffered_call
        import anthropic
        from google.genai import errors as gemini_errors
        from test_tracked_protocols import HTTP_PROTOCOL_ERRORS

        with pytest.raises(
            (openai.APIError, anthropic.APIError, gemini_errors.APIError)
            + HTTP_PROTOCOL_ERRORS
        ) as caught:
            call(instance, token, ingress)
        if not streaming:
            assert (
                getattr(
                    caught.value, "status_code", getattr(caught.value, "code", None)
                )
                == 502
            )
        assert len(calls) == 1, "default SDK retries must not replay paid inference"
        if scenario in ("missing", "ambiguous"):
            instance.process.terminate()
            assert instance.process.wait(timeout=6) == 124
        else:
            instance.stop()
        with sqlite3.connect(instance.database) as connection:
            assert connection.execute(
                "SELECT count(*) FROM budget_physical_attempt"
            ).fetchone() == (1,)
            rows = connection.execute(
                "SELECT settled,actual FROM budget_reservation"
            ).fetchall()
            if scenario == "opaque":
                assert rows == [(1, 11)] * 2
                observed = json.loads(
                    connection.execute(
                        "SELECT usage_json FROM budget_terminal_observation"
                    ).fetchone()[0]
                )
                assert (
                    observed["tokens_out"] == 4 and observed["completeness"] == "final"
                )
            else:
                assert rows == [(0, 0)] * 2
                assert connection.execute(
                    "SELECT count(*) FROM budget_settlement_outbox"
                ).fetchone() == (0,)


def tool_turn(client, ingress, streaming, history):
    schema = {
        "type": "object",
        "properties": {"n": {"type": "integer"}},
        "required": ["n"],
    }
    names = ("alpha", "beta")
    if ingress == FAMILIES[0]:
        tools = [
            {"type": "function", "function": {"name": name, "parameters": schema}}
            for name in names
        ]
        response = client.chat.completions.create(
            model="agent",
            max_tokens=64,
            messages=history,
            tools=tools,
            stream=streaming,
        )
        if streaming:
            assembled, reply = {}, ""
            for event in response:
                if not event.choices:
                    continue
                delta = event.choices[0].delta
                reply += delta.content or ""
                for call in delta.tool_calls or []:
                    item = assembled.setdefault(
                        call.index, {"id": "", "name": "", "arguments": ""}
                    )
                    item["id"] = call.id or item["id"]
                    if call.function:
                        item["name"] += call.function.name or ""
                        item["arguments"] += call.function.arguments or ""
            calls = list(assembled.values())
        else:
            reply = response.choices[0].message.content or ""
            calls = [
                {"id": c.id, "name": c.function.name, "arguments": c.function.arguments}
                for c in response.choices[0].message.tool_calls or []
            ]
        history.append(
            {
                "role": "assistant",
                "content": reply or None,
                "tool_calls": [
                    {
                        "id": c["id"],
                        "type": "function",
                        "function": {"name": c["name"], "arguments": c["arguments"]},
                    }
                    for c in calls
                ],
            }
        )
    elif ingress == FAMILIES[1]:
        tools = [{"name": name, "input_schema": schema} for name in names]
        if streaming:
            with client.messages.stream(
                model="agent", max_tokens=64, messages=history, tools=tools
            ) as stream:
                response = stream.get_final_message()
        else:
            response = client.messages.create(
                model="agent", max_tokens=64, messages=history, tools=tools
            )
        reply = "".join(b.text for b in response.content if b.type == "text")
        calls = [
            {"id": b.id, "name": b.name, "arguments": json.dumps(b.input)}
            for b in response.content
            if b.type == "tool_use"
        ]
        history.append(
            {
                "role": "assistant",
                "content": [b.model_dump(exclude_none=True) for b in response.content],
            }
        )
    elif ingress == FAMILIES[2]:
        tools = [
            {"type": "function", "name": name, "parameters": schema} for name in names
        ]
        response = client.responses.create(
            model="agent",
            max_output_tokens=64,
            input=history,
            tools=tools,
            stream=streaming,
        )
        if streaming:
            response = list(response)[-1].response
        reply = response.output_text
        calls = [
            {"id": c.call_id, "name": c.name, "arguments": c.arguments}
            for c in response.output
            if c.type == "function_call"
        ]
        history += [item.model_dump(exclude_none=True) for item in response.output]
    else:
        from google.genai import types

        config = types.GenerateContentConfig(
            max_output_tokens=64,
            tools=[
                {
                    "function_declarations": [
                        {"name": name, "parameters": schema} for name in names
                    ]
                }
            ],
        )
        responses = (
            list(
                client.models.generate_content_stream(
                    model="agent", contents=history, config=config
                )
            )
            if streaming
            else [
                client.models.generate_content(
                    model="agent", contents=history, config=config
                )
            ]
        )
        parts = [
            p
            for response in responses
            for candidate in response.candidates or []
            for p in (candidate.content.parts if candidate.content else [])
        ]
        calls = [
            {
                "id": p.function_call.id,
                "name": p.function_call.name,
                "arguments": json.dumps(p.function_call.args),
            }
            for p in parts
            if p.function_call
        ]
        reply = "".join(p.text or "" for p in parts)
        history.append(types.Content(role="model", parts=parts))
    return reply, calls


@pytest.mark.parametrize("family", ["cohere_chat", "ollama_chat"])
@pytest.mark.parametrize("ingress", FAMILIES)
@pytest.mark.parametrize("streaming", [False, True])
def test_native_three_turn_parallel_tools_preserve_reversed_results_and_one_side_effect(
    tmp_path,
    backend_binaries,
    family,
    ingress,
    streaming,
):
    import anthropic
    from google import genai
    from google.genai import types

    with origin(family, "tools") as (source, calls), gateway(
        tmp_path, backend_binaries, source, family
    ) as (instance, token):
        headers = {"x-sandhi-session": "native-tool-loop"}
        if ingress in (FAMILIES[0], FAMILIES[2]):
            client = openai.OpenAI(
                base_url=instance.base + "/v1", api_key=token, default_headers=headers
            )
        elif ingress == FAMILIES[1]:
            client = anthropic.Anthropic(
                base_url=instance.base, api_key=token, default_headers=headers
            )
        else:
            client = genai.Client(
                api_key=token,
                http_options=types.HttpOptions(
                    base_url=instance.base,
                    headers=headers,
                    retry_options=types.HttpRetryOptions(attempts=1),
                ),
            )
        history = (
            [types.Content(role="user", parts=[types.Part.from_text(text="hello")])]
            if ingress == FAMILIES[3]
            else [{"role": "user", "content": "hello"}]
        )
        executed = {}
        try:
            for turn in range(3):
                reply, requested = tool_turn(client, ingress, streaming, history)
                if turn == 2:
                    assert reply == "reply" and not requested
                    break
                assert [call["name"] for call in requested] == ["beta", "alpha"]
                assert len({call["id"] for call in requested}) == 2
                results = []
                for call in reversed(requested):
                    args = json.loads(call["arguments"])
                    expected = (turn + 1) * 10 + (0 if call["name"] == "beta" else 1)
                    assert args == {"n": expected}
                    identity = (call["name"], args["n"])
                    assert identity not in executed
                    executed[identity] = (
                        1  # the actual deterministic local tool side effect
                    )
                    results.append((call, f"result-{call['name']}-{args['n']}"))
                if ingress == FAMILIES[0]:
                    history += [
                        {"role": "tool", "tool_call_id": c["id"], "content": result}
                        for c, result in results
                    ]
                elif ingress == FAMILIES[1]:
                    history.append(
                        {
                            "role": "user",
                            "content": [
                                {
                                    "type": "tool_result",
                                    "tool_use_id": c["id"],
                                    "content": result,
                                }
                                for c, result in results
                            ],
                        }
                    )
                elif ingress == FAMILIES[2]:
                    history += [
                        {
                            "type": "function_call_output",
                            "call_id": c["id"],
                            "output": result,
                        }
                        for c, result in results
                    ]
                else:
                    history.append(
                        types.Content(
                            role="user",
                            parts=[
                                types.Part(
                                    function_response=types.FunctionResponse(
                                        name=c["name"],
                                        id=c["id"],
                                        response={"output": result},
                                    )
                                )
                                for c, result in results
                            ],
                        )
                    )
        finally:
            client.close()
        assert len(executed) == 4 and len(calls) == 3
        messages = calls[-1][1]["messages"]
        returned = [m for m in messages if m["role"] == "tool"]
        assert len(returned) == 4
        assert all(
            expected in json.dumps(returned[index])
            for index, expected in enumerate(
                (
                    "result-alpha-11",
                    "result-beta-10",
                    "result-alpha-21",
                    "result-beta-20",
                )
            )
        )
        if family == "ollama_chat":
            assert [m["tool_name"] for m in returned] == ["alpha", "beta"] * 2
        instance.stop()
        with sqlite3.connect(instance.database) as connection:
            assert (
                connection.execute(
                    "SELECT charged_tokens FROM budget_settlement_outbox"
                ).fetchall()
                == [(11,)] * 3
            )
            assert (
                connection.execute(
                    "SELECT settled,actual FROM budget_reservation"
                ).fetchall()
                == [(1, 11)] * 6
            )
            assert connection.execute(
                "SELECT count(*) FROM budget_physical_attempt"
            ).fetchone() == (3,)


@pytest.mark.parametrize("family", ["cohere_chat", "ollama_chat"])
@pytest.mark.parametrize("ingress", FAMILIES)
def test_native_source_cannot_accept_foreign_native_state_before_any_admission(
    tmp_path,
    backend_binaries,
    family,
    ingress,
):
    request = {
        "model": "agent",
        "max_tokens": 64,
        "messages": [{"role": "user", "content": "hello"}],
    }
    path = "/v1/chat/completions"
    if ingress == FAMILIES[0]:
        request["messages"].insert(
            0, {"role": "assistant", "content": "prior", "audio": {"id": "foreign"}}
        )
    elif ingress == FAMILIES[1]:
        path = "/v1/messages"
        request["messages"].insert(
            0,
            {
                "role": "assistant",
                "content": [
                    {"type": "thinking", "thinking": "prior", "signature": "foreign"}
                ],
            },
        )
    elif ingress == FAMILIES[2]:
        path = "/v1/responses"
        request = {
            "model": "agent",
            "max_output_tokens": 64,
            "input": "hello",
            "previous_response_id": "foreign",
        }
    else:
        path = "/v1beta/models/agent:generateContent"
        request = {
            "generationConfig": {"maxOutputTokens": 64},
            "contents": [
                {
                    "role": "model",
                    "parts": [{"text": "prior", "thoughtSignature": "foreign"}],
                },
                {"role": "user", "parts": [{"text": "hello"}]},
            ],
        }
    with origin(family, "valid") as (source, calls), gateway(
        tmp_path, backend_binaries, source, family
    ) as (instance, token):
        response = httpx.post(
            instance.base + path,
            headers={
                "authorization": "Bearer " + token,
                "x-sandhi-session": "new-native-state",
            },
            json=request,
        )
        assert 400 <= response.status_code < 500, response.text
        assert calls == []
        with sqlite3.connect(instance.database) as connection:
            assert connection.execute(
                "SELECT count(*) FROM budget_physical_attempt"
            ).fetchone() == (0,)
            assert connection.execute(
                "SELECT count(*) FROM budget_reservation"
            ).fetchone() == (0,)


@pytest.mark.parametrize("family", ["cohere_chat", "ollama_chat"])
@pytest.mark.parametrize("scenario", ["orphan", "duplicate_call", "duplicate_result"])
def test_native_portable_invalid_tool_history_refuses_before_reservation_or_dispatch(
    tmp_path,
    backend_binaries,
    family,
    scenario,
):
    call = {
        "id": "call",
        "type": "function",
        "function": {"name": "alpha", "arguments": "{}"},
    }
    result = {"role": "tool", "tool_call_id": "call", "content": "result"}
    history = (
        [result]
        if scenario == "orphan"
        else [
            {
                "role": "assistant",
                "tool_calls": [call] * (2 if scenario == "duplicate_call" else 1),
            },
            result,
        ]
        + ([result] if scenario == "duplicate_result" else [])
    )
    with origin(family, "valid") as (source, calls), gateway(
        tmp_path, backend_binaries, source, family
    ) as (instance, token):
        response = httpx.post(
            instance.base + "/v1/chat/completions",
            headers={"authorization": "Bearer " + token},
            json={
                "model": "agent",
                "max_tokens": 64,
                "messages": [{"role": "user", "content": "hello"}] + history,
            },
        )
        assert 400 <= response.status_code < 500, response.text
        assert calls == []
        with sqlite3.connect(instance.database) as connection:
            assert connection.execute(
                "SELECT count(*) FROM budget_physical_attempt"
            ).fetchone() == (0,)
            assert connection.execute(
                "SELECT count(*) FROM budget_reservation"
            ).fetchone() == (0,)
