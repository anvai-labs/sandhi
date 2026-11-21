"""Exercise SDK-shaped Python binding calls without vendor SDK dependencies."""

import json
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from types import SimpleNamespace

import pytest
import sandhi_gateway as sg

REQUEST = {
    "model": "fixture",
    "messages": [{"role": "user", "content": "ping"}],
}
RESPONSES_REQUEST = {"model": "fixture", "input": "ping"}
USAGE = {"prompt_tokens": 5, "completion_tokens": 2, "total_tokens": 7}


@pytest.fixture
def origin():
    state = SimpleNamespace(requests=[], status=200, interrupt=False, response=None)

    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *_args):
            pass

        def do_POST(self):
            body = json.loads(self.rfile.read(int(self.headers["content-length"])))
            state.requests.append(
                (body, {name.lower(): value for name, value in self.headers.items()})
            )
            streaming = body.get("stream", False)
            if state.status != 200:
                payload = json.dumps({"error": {"message": "fixture unavailable"}})
            elif state.response is not None:
                payload = json.dumps(state.response)
            elif streaming:
                chunks = [
                    {
                        "choices": [
                            {
                                "index": 0,
                                "delta": {"role": "assistant", "content": "pong"},
                            }
                        ]
                    },
                    {"choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]},
                    {"choices": [], "usage": USAGE},
                ]
                if state.interrupt:
                    chunks = chunks[:1]
                payload = "".join(
                    "data: "
                    + json.dumps(
                        {
                            "id": "fixture",
                            "object": "chat.completion.chunk",
                            "model": "fixture",
                            **chunk,
                        }
                    )
                    + "\n\n"
                    for chunk in chunks
                )
                if not state.interrupt:
                    payload += "data: [DONE]\n\n"
            else:
                payload = json.dumps(
                    {
                        "id": "fixture",
                        "object": "chat.completion",
                        "model": "fixture",
                        "choices": [
                            {
                                "index": 0,
                                "message": {"role": "assistant", "content": "pong"},
                                "finish_reason": "stop",
                            }
                        ],
                        "usage": USAGE,
                    }
                )
            encoded = payload.encode()
            self.send_response(state.status)
            self.send_header(
                "content-type",
                (
                    "text/event-stream"
                    if streaming and state.status == 200
                    else "application/json"
                ),
            )
            self.send_header("content-length", str(len(encoded)))
            self.end_headers()
            self.wfile.write(encoded)

    with ThreadingHTTPServer(("127.0.0.1", 0), Handler) as server:
        worker = threading.Thread(target=server.serve_forever, daemon=True)
        worker.start()
        state.url = f"http://127.0.0.1:{server.server_port}"
        state.handle = sg.ProviderRuntime().openai_compat(
            "openai",
            state.url,
            "synthetic",
            max_retries=0,
        )
        try:
            yield state
        finally:
            server.shutdown()
            worker.join()


@pytest.mark.asyncio
async def test_neutral_openai_o_series_uses_model_defaults_and_sdk_keeps_native_options(origin):
    request = {**REQUEST, "model": "o3", "temperature": 0.7, "max_output_tokens": 64}
    await origin.handle.complete_json(json.dumps(request))
    events = [json.loads(event) async for event in origin.handle.stream_json(json.dumps(request))]
    assert any(event["event"] == "finish" for event in events)
    assert len(origin.requests) == 2
    for body, _ in origin.requests:
        assert "temperature" not in body
        assert "max_tokens" not in body
        assert body["max_completion_tokens"] == 64

    await origin.handle.complete_sdk_json(
        "openai_chat",
        json.dumps({**REQUEST, "model": "o3", "temperature": 1.0, "max_completion_tokens": 64}),
    )
    assert origin.requests[-1][0]["temperature"] == 1.0
    await origin.handle.complete_json(json.dumps({**request, "model": "o3-like-fixture"}))
    assert origin.requests[-1][0]["temperature"] == 0.7
    assert origin.requests[-1][0]["max_tokens"] == 64


@pytest.mark.asyncio
async def test_neutral_ollama_thinking_toggle_reaches_only_qualified_models(origin):
    origin.response = {
        "message": {"role": "assistant", "content": "pong"},
        "done": True,
        "done_reason": "stop",
        "prompt_eval_count": 5,
        "eval_count": 2,
    }
    runtime = sg.ProviderRuntime()
    handle = runtime.provider("ollama", "qwen3:8b", "synthetic", base_url=origin.url)
    for enabled in (False, True):
        request = {**REQUEST, "model": "qwen3:8b", "thinking": {"enabled": enabled}}
        reply = json.loads(await handle.complete_json(json.dumps(request)))
        assert reply["output"]["content"] == "pong"
        assert origin.requests[-1][0]["think"] is enabled
    with pytest.raises(sg.SandhiProviderError, match="does not support configurable thinking"):
        await handle.complete_json(json.dumps({**request, "model": "qwen3:unqualified"}))
    with pytest.raises(sg.SandhiProviderError, match="does not support thinking effort"):
        await handle.complete_json(json.dumps({**request, "thinking": {"enabled": True, "effort": "low"}}))
    native = {**REQUEST, "model": "qwen3:8b", "extensions": {"ollama": {"think": False}}}
    await handle.complete_json(json.dumps(native))
    assert origin.requests[-1][0]["think"] is False
    with pytest.raises(sg.SandhiProviderError, match="does not support configurable thinking"):
        await handle.complete_json(json.dumps({**native, "model": "qwen3:unqualified"}))
    with pytest.raises(sg.SandhiProviderError, match="conflicts with admitted thinking"):
        await handle.complete_json(json.dumps({**native, "thinking": {"enabled": True}}))
    assert len(origin.requests) == 3


@pytest.mark.asyncio
async def test_neutral_anthropic_stable_tool_prefix_survives_stub_changes(origin):
    origin.response = {
        "id": "fixture",
        "type": "message",
        "role": "assistant",
        "model": "claude-sonnet-4-6",
        "content": [{"type": "text", "text": "pong"}],
        "stop_reason": "end_turn",
        "usage": {
            "input_tokens": 5,
            "output_tokens": 2,
            "cache_creation_input_tokens": 0,
            "cache_read_input_tokens": 0,
        },
    }
    handle = sg.ProviderRuntime().provider(
        "anthropic", "claude-sonnet-4-6", "synthetic", base_url=origin.url
    )
    for stub in ("stub_a", "stub_b"):
        request = {
            **REQUEST,
            "model": "claude-sonnet-4-6",
            "max_output_tokens": 64,
            "stable_tool_prefix_count": 1,
            "tools": [{"name": name, "parameters": {"type": "object"}} for name in ("stable", stub)],
        }
        await handle.complete_json(json.dumps(request))
        cached, tail = origin.requests[-1][0]["tools"]
        assert cached["name"] == "stable"
        assert cached["cache_control"] == {"type": "ephemeral"}
        assert tail["name"] == stub
        assert "cache_control" not in tail
    with pytest.raises(ValueError, match="stable tool prefix must be within"):
        await handle.complete_json(json.dumps({**request, "stable_tool_prefix_count": 3}))
    assert len(origin.requests) == 2


@pytest.mark.asyncio
async def test_sdk_buffered_native_and_translated_responses(origin):
    native = json.loads(
        await origin.handle.complete_sdk_json(
            "openai_chat",
            json.dumps(REQUEST),
            metadata_json=json.dumps({"session_id": "fixture"}),
            wire_headers_json=json.dumps({"x-fixture": "sdk"}),
        )
    )
    assert native["choices"][0]["message"]["content"] == "pong"
    assert origin.requests[0][1]["x-fixture"] == "sdk"
    translated = json.loads(
        await origin.handle.complete_sdk_json(
            "openai_responses",
            json.dumps(RESPONSES_REQUEST),
        )
    )
    assert translated["output"][0]["content"][0]["text"] == "pong"


@pytest.mark.asyncio
@pytest.mark.parametrize(
    "dialect,request_body",
    [
        ("openai_chat", REQUEST),
        ("openai_responses", RESPONSES_REQUEST),
    ],
)
async def test_sdk_stream_rendering_and_close(origin, dialect, request_body):
    events = origin.handle.stream_sdk_json(dialect, json.dumps(request_body))
    frames = [json.loads(event) async for event in events]
    assert "pong" in json.dumps(frames)
    await events.aclose()
    assert await anext(events, None) is None
    assert len(origin.requests) == 1


@pytest.mark.asyncio
async def test_sdk_interrupted_translated_stream_has_one_error(origin):
    origin.interrupt = True
    events = origin.handle.stream_sdk_json(
        "openai_responses", json.dumps(RESPONSES_REQUEST)
    )
    names = [json.loads(event)["event"] async for event in events]
    assert names[-1] == "error"
    assert names.count("error") == 1
    assert "response.completed" not in names


@pytest.mark.asyncio
@pytest.mark.parametrize("stream", [False, True])
async def test_sdk_source_failure_raises_a_typed_error(origin, stream):
    origin.status = 401
    with pytest.raises(sg.SandhiSdkError) as excinfo:
        if stream:
            await anext(
                origin.handle.stream_sdk_json("openai_chat", json.dumps(REQUEST))
            )
        else:
            await origin.handle.complete_sdk_json("openai_chat", json.dumps(REQUEST))
    assert json.loads(str(excinfo.value))["error"]["retryable"] is False
    assert len(origin.requests) == 1


@pytest.mark.parametrize(
    "dialect,body,options,error,message",
    [
        ("unknown", "{}", {}, ValueError, "unknown SDK chat dialect"),
        ("openai_chat", "{", {}, sg.SandhiSdkError, "body is not valid JSON"),
        (
            "openai_chat",
            json.dumps(REQUEST),
            {"metadata_json": "{"},
            sg.SandhiSdkError,
            "metadata is not valid JSON",
        ),
        (
            "openai_chat",
            json.dumps(REQUEST),
            {"metadata_json": "[]"},
            sg.SandhiSdkError,
            "metadata must be RequestMetadataV1",
        ),
        (
            "openai_chat",
            json.dumps(REQUEST),
            {"model": "extra"},
            sg.SandhiSdkError,
            "model context is only used by Gemini",
        ),
        (
            "openai_chat",
            json.dumps({**REQUEST, "stream": True}),
            {},
            sg.SandhiSdkError,
            "stream field disagrees",
        ),
        ("openai_chat", "{}", {}, sg.SandhiSdkError, "model"),
        (
            "gemini_generate_content",
            '{"contents":[{"role":"user","parts":[{"text":"ping"}]}]}',
            {},
            sg.SandhiSdkError,
            "Gemini requires model context",
        ),
        (
            "openai_responses",
            json.dumps({**RESPONSES_REQUEST, "previous_response_id": "resp_fixture"}),
            {},
            sg.SandhiSdkError,
            "cannot be translated",
        ),
    ],
)
def test_sdk_invalid_requests_are_rejected_before_dispatch(
    origin, dialect, body, options, error, message
):
    with pytest.raises(error, match=message):
        origin.handle.complete_sdk_json(dialect, body, **options)
    assert not origin.requests
