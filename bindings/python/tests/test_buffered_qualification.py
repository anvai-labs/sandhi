import asyncio
import copy
import json
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

import pytest
import sandhi_gateway as sg

CASES = [
    (
        "chat_completions",
        {
            "id": "r",
            "object": "chat.completion",
            "model": "fixture",
            "choices": [
                {
                    "index": 0,
                    "message": {"role": "assistant", "content": "ok"},
                    "finish_reason": "stop",
                }
            ],
            "usage": {
                "prompt_tokens": 5,
                "completion_tokens": 2,
                "prompt_tokens_details": {"cached_tokens": 9},
            },
        },
    ),
    (
        "responses",
        {
            "id": "r",
            "object": "response",
            "model": "fixture",
            "status": "completed",
            "output": [
                {
                    "type": "message",
                    "id": "msg",
                    "role": "assistant",
                    "content": [{"type": "output_text", "text": "ok"}],
                }
            ],
            "usage": {
                "input_tokens": 5,
                "output_tokens": 2,
                "input_tokens_details": {"cached_tokens": 9},
            },
        },
    ),
    (
        "anthropic_messages",
        {
            "id": "r",
            "type": "message",
            "model": "fixture",
            "role": "assistant",
            "content": [{"type": "text", "text": "ok"}],
            "stop_reason": "end_turn",
            "usage": {
                "input_tokens": 5,
                "output_tokens": 2,
                "cache_read_input_tokens": 0,
                "cache_creation_input_tokens": -1,
            },
        },
    ),
    (
        "gemini_generate_content",
        {
            "candidates": [
                {
                    "index": 0,
                    "content": {"role": "model", "parts": [{"text": "ok"}]},
                    "finishReason": "STOP",
                }
            ],
            "usageMetadata": {
                "promptTokenCount": 5,
                "cachedContentTokenCount": 9,
                "candidatesTokenCount": 2,
                "thoughtsTokenCount": 0,
                "totalTokenCount": 7,
            },
        },
    ),
    (
        "chatgpt_responses",
        {
            "id": "r",
            "object": "response",
            "model": "fixture",
            "status": "completed",
            "output": [
                {
                    "type": "message",
                    "id": "msg",
                    "role": "assistant",
                    "content": [{"type": "output_text", "text": "ok"}],
                }
            ],
            "usage": {
                "input_tokens": 5,
                "output_tokens": 2,
                "input_tokens_details": {"cached_tokens": 9},
            },
        },
    ),
]


@pytest.mark.parametrize("protocol,body", CASES)
@pytest.mark.parametrize("native", [False, True])
@pytest.mark.parametrize(
    "scenario",
    ["invalid", "missing", "zero", "valid", "invalid_both", "invalid_output"],
)
def test_buffered_source_usage_qualification(protocol, body, native, scenario):
    body = copy.deepcopy(body)
    family = "responses" if protocol == "chatgpt_responses" else protocol
    usage_key = "usageMetadata" if protocol == "gemini_generate_content" else "usage"
    if scenario == "missing":
        body.pop(usage_key)
    elif scenario in ("zero", "valid", "invalid_output"):
        n = int(scenario != "zero")
        body[usage_key] = {
            "chat_completions": {
                "prompt_tokens": 5 * n,
                "completion_tokens": 2 * n,
                "prompt_tokens_details": {
                    "cached_tokens": 2 * n,
                    "cache_write_tokens": n,
                    "audio_tokens": 0,
                },
                "completion_tokens_details": {
                    "reasoning_tokens": 0,
                    "audio_tokens": 0,
                    "accepted_prediction_tokens": 0,
                    "rejected_prediction_tokens": 0,
                },
            },
            "responses": {
                "input_tokens": 5 * n,
                "output_tokens": 2 * n,
                "input_tokens_details": {
                    "cached_tokens": 2 * n,
                    "cache_write_tokens": n,
                },
                "output_tokens_details": {"reasoning_tokens": 0},
            },
            "anthropic_messages": {
                "input_tokens": 5 * n,
                "output_tokens": 2 * n,
                "cache_read_input_tokens": 2 * n,
                "cache_creation_input_tokens": n,
                "output_tokens_details": {"thinking_tokens": 0},
            },
            "gemini_generate_content": {
                "promptTokenCount": 5 * n,
                "candidatesTokenCount": 2 * n,
                "cachedContentTokenCount": 2 * n,
                "toolUsePromptTokenCount": n,
                "thoughtsTokenCount": 0,
                "totalTokenCount": 8 * n,
            },
        }[family]
    if scenario in ("invalid_both", "invalid_output"):
        body.pop(
            {
                "chat_completions": "choices",
                "responses": "output",
                "anthropic_messages": "content",
                "gemini_generate_content": "candidates",
            }[family]
        )
    calls = []

    class Handler(BaseHTTPRequestHandler):
        def do_POST(self):
            calls.append(self.rfile.read(int(self.headers["content-length"])))
            raw = json.dumps(body).encode()
            content_type = "application/json"
            if protocol == "chatgpt_responses":
                raw = (
                    "data: "
                    + json.dumps({"type": "response.completed", "response": body})
                    + "\n\n"
                ).encode()
                content_type = "text/event-stream"
            self.send_response(200)
            self.send_header("content-type", content_type)
            self.send_header("content-length", str(len(raw)))
            self.end_headers()
            self.wfile.write(raw)

        def log_message(self, *args):
            pass

    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    threading.Thread(
        target=lambda: server.serve_forever(poll_interval=0.01), daemon=True
    ).start()
    try:
        handle = sg.ProviderRuntime().provider(
            "fixture",
            "fixture",
            "synthetic",
            base_url=f"http://127.0.0.1:{server.server_port}",
            protocol=protocol,
            max_retries=0,
        )
        request = json.dumps(
            {
                "model": "fixture",
                "max_output_tokens": 16,
                "include_native_response": native,
                "messages": [
                    {"role": "system", "content": "Answer precisely."},
                    {"role": "user", "content": "hello"},
                ],
            }
        )

        async def complete():
            return await handle.complete_json(request)

        if scenario in ("invalid", "missing", "invalid_both", "invalid_output"):
            with pytest.raises(sg.SandhiProviderError) as failure:
                asyncio.run(complete())
            error = json.loads(str(failure.value))
            assert error["code"] == "output_projection_error"
            assert error["retryable"] is False
        else:
            result = json.loads(asyncio.run(complete()))
            usage = result["usage"]
            expected = (
                {
                    "chat_completions": [2, 2, 1, 2],
                    "responses": [2, 2, 1, 2],
                    "anthropic_messages": [5, 2, 1, 2],
                    "gemini_generate_content": [4, 2, 0, 2],
                }[family]
                if scenario == "valid"
                else [0, 0, 0, 0]
            )
            assert [
                usage[k]
                for k in (
                    "tokens_in",
                    "cache_read_tokens",
                    "cache_creation_tokens",
                    "tokens_out",
                )
            ] == expected
            assert usage["completeness"] == "final"
            assert usage["reasoning_tokens"] == 0
            if family == "chat_completions":
                assert [
                    usage[key]
                    for key in (
                        "audio_input_tokens",
                        "audio_output_tokens",
                        "accepted_prediction_tokens",
                        "rejected_prediction_tokens",
                    )
                ] == [0, 0, 0, 0]
            assert usage["attempts"] == 1
            assert result["output"].get("content") == "ok", result
            assert bool(result.get("extensions")) == native
        assert len(calls) == 1
    finally:
        server.shutdown()
        server.server_close()


@pytest.mark.parametrize(
    "protocol",
    [
        "chat_completions",
        "responses",
        "chatgpt_responses",
        "anthropic_messages",
        "gemini_generate_content",
    ],
)
@pytest.mark.parametrize("native", [False, True])
@pytest.mark.parametrize(
    "mutation",
    [
        "bad_arguments",
        "invalid_id",
        "missing_id",
        "missing_name",
        "duplicate_id",
        "missing_args",
        "null_args",
        "valid",
    ],
)
def test_buffered_known_call_fields_cannot_be_invented(protocol, native, mutation):
    family = "responses" if protocol == "chatgpt_responses" else protocol
    body = copy.deepcopy(next(value for name, value in CASES if name == family))
    if family == "chat_completions":
        item = {
            "type": "function",
            "id": "call",
            "function": {"name": "inspect", "arguments": '{"value":7}'},
        }
        identity, payload, args_key, object_args = (
            item,
            item["function"],
            "arguments",
            False,
        )
        body["choices"][0]["message"] = {
            "role": "assistant",
            "content": None,
            "tool_calls": [item],
        }
        body["choices"][0]["finish_reason"] = "tool_calls"
        body["usage"] = {"prompt_tokens": 5, "completion_tokens": 2}
    elif family == "responses":
        item = {
            "type": "function_call",
            "id": "item",
            "call_id": "call",
            "name": "inspect",
            "arguments": '{"value":7}',
        }
        identity, payload, args_key, object_args = item, item, "arguments", False
        body["output"] = [item]
        body["usage"] = {"input_tokens": 5, "output_tokens": 2}
    elif family == "anthropic_messages":
        item = {
            "type": "tool_use",
            "id": "call",
            "name": "inspect",
            "input": {"value": 7},
        }
        identity, payload, args_key, object_args = item, item, "input", True
        body["content"] = [item]
        body["stop_reason"] = "tool_use"
        body["usage"] = {
            "input_tokens": 5,
            "output_tokens": 2,
            "cache_read_input_tokens": 0,
            "cache_creation_input_tokens": 0,
        }
    else:
        item = {"id": "call", "name": "inspect", "args": {"value": 7}}
        identity, payload, args_key, object_args = item, item, "args", True
        body["candidates"][0]["content"]["parts"] = [{"functionCall": item}]
        body["usageMetadata"] = {"promptTokenCount": 5, "candidatesTokenCount": 2}
    id_key = "call_id" if family == "responses" else "id"
    if mutation == "bad_arguments":
        payload[args_key] = [7] if object_args else {"value": 7}
    elif mutation == "invalid_id":
        identity[id_key] = 7
    elif mutation == "missing_id":
        identity.pop(id_key)
    elif mutation == "missing_name":
        payload.pop("name")
    elif mutation == "missing_args":
        payload.pop(args_key)
    elif mutation == "null_args":
        payload[args_key] = None
    if mutation == "duplicate_id":
        repeated = copy.deepcopy(item)
        if family == "chat_completions":
            body["choices"][0]["message"]["tool_calls"].append(repeated)
        elif family == "responses":
            body["output"].append(repeated)
        elif family == "anthropic_messages":
            body["content"].append(repeated)
        else:
            body["candidates"][0]["content"]["parts"].append({"functionCall": repeated})
    expected_success = mutation == "valid" or (
        family == "gemini_generate_content"
        and mutation in ("missing_id", "missing_args", "null_args")
    )
    raw = json.dumps(body).encode()
    content_type = "application/json"
    if protocol == "chatgpt_responses":
        raw = (
            "data: "
            + json.dumps({"type": "response.completed", "response": body})
            + "\n\n"
        ).encode()
        content_type = "text/event-stream"
    calls = []

    class Handler(BaseHTTPRequestHandler):
        def do_POST(self):
            calls.append(self.rfile.read(int(self.headers["content-length"])))
            self.send_response(200)
            self.send_header("content-type", content_type)
            self.send_header("content-length", str(len(raw)))
            self.end_headers()
            self.wfile.write(raw)

        def log_message(self, *_args):
            pass

    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    threading.Thread(
        target=lambda: server.serve_forever(poll_interval=0.01), daemon=True
    ).start()
    try:
        handle = sg.ProviderRuntime().provider(
            "fixture",
            "fixture",
            "synthetic",
            base_url=f"http://127.0.0.1:{server.server_port}",
            protocol=protocol,
            max_retries=0,
        )

        async def run():
            return await handle.complete_json(
                json.dumps(
                    {
                        "model": "fixture",
                        "max_output_tokens": 16,
                        "include_native_response": native,
                        "messages": [
                            {"role": "system", "content": "Use tools precisely."},
                            {"role": "user", "content": "hello"},
                        ],
                    }
                )
            )

        if expected_success:
            result = json.loads(asyncio.run(run()))
            (tool,) = result["output"]["tool_calls"]
            assert tool["name"] == "inspect"
            assert json.loads(tool["arguments"]) == (
                {} if mutation in ("missing_args", "null_args") else {"value": 7}
            )
            if mutation == "missing_id":
                assert tool["id"]
            else:
                assert tool["id"] == "call"
            assert result["finish_reason"] == "tool_calls"
            assert result["usage"]["completeness"] == "final"
            assert result["usage"]["attempts"] == 1
        else:
            with pytest.raises(sg.SandhiProviderError) as failure:
                asyncio.run(run())
            error = json.loads(str(failure.value))
            assert error["code"] == "output_projection_error"
            assert error["retryable"] is False
        assert len(calls) == 1
    finally:
        server.shutdown()
        server.server_close()
