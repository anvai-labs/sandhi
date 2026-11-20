"""Run the same stock SDK calls through HTTP and the built Python binding."""

import json
from urllib.parse import unquote

import anthropic
import conftest as fixture
import httpx2
import openai
import pytest
import sandhi_gateway as sg
from conftest import REAL_OPENAI_KEY, VK_OPENAI
from google import genai

DIALECTS = (
    "openai_chat",
    "openai_responses",
    "anthropic_messages",
    "gemini_generate_content",
)
MODEL = "gpt-mock"


class FfiStream(httpx2.AsyncByteStream):
    def __init__(self, events, first):
        self.events = events
        self.first = first

    async def __aiter__(self):
        if self.first is not None:
            yield self.frame(self.first)
        async for event in self.events:
            yield self.frame(event)

    @staticmethod
    def frame(event):
        item = json.loads(event)
        name = f"event: {item['event']}\n" if item["event"] else ""
        return f"{name}data: {json.dumps(item['data'])}\n\n".encode()

    async def aclose(self):
        await self.events.aclose()


def ffi_transport(handle, dialect):
    async def send(request):
        body = (await request.aread()).decode()
        context = {}
        if dialect == "gemini_generate_content":
            model, method = unquote(request.url.path).rsplit("/", 1)[1].split(":")
            context["model"] = model
            streaming = method == "streamGenerateContent"
        else:
            streaming = bool(json.loads(body).get("stream"))
        try:
            if streaming:
                events = handle.stream_sdk_json(dialect, body, **context)
                try:
                    first = await anext(events, None)
                except BaseException:
                    await events.aclose()
                    raise
                return httpx2.Response(
                    200,
                    stream=FfiStream(events, first),
                    headers={"content-type": "text/event-stream"},
                )
            response = await handle.complete_sdk_json(dialect, body, **context)
            return httpx2.Response(
                200,
                content=response.encode(),
                headers={"content-type": "application/json"},
            )
        except sg.SandhiSdkError as exc:
            error = json.loads(str(exc))
            headers = {} if error["error"]["retryable"] else {"x-should-retry": "false"}
            return httpx2.Response(error["status"], json=error["body"], headers=headers)

    return httpx2.MockTransport(send)


async def sdk_call(
    base, http, dialect, stream, *, model=MODEL, api_key=VK_OPENAI, **controls
):
    messages = [{"role": "user", "content": "ping"}]
    if dialect.startswith("openai"):
        async with openai.AsyncOpenAI(
            base_url=f"{base}/v1",
            api_key=api_key,
            max_retries=0,
            http_client=http,
        ) as client:
            if dialect == "openai_chat":
                result = await client.chat.completions.create(
                    model=model,
                    messages=messages,
                    max_completion_tokens=64,
                    stream=stream,
                    **controls,
                )
                if not stream:
                    return (
                        result.choices[0].message.content,
                        result.usage.prompt_tokens,
                        result.usage.completion_tokens,
                    )
                chunks = [chunk async for chunk in result]
                return [
                    (
                        (chunk.choices[0].delta.content, chunk.choices[0].finish_reason)
                        if chunk.choices
                        else (chunk.usage.prompt_tokens, chunk.usage.completion_tokens)
                    )
                    for chunk in chunks
                ]
            result = await client.responses.create(
                model=model,
                input=messages,
                max_output_tokens=64,
                stream=stream,
                **controls,
            )
            order = []
            if stream:
                async for event in result:
                    order.append(event.type)
                    if event.type == "response.completed":
                        result = event.response
            return (
                order,
                result.output_text,
                result.usage.input_tokens,
                result.usage.output_tokens,
            )
    if dialect == "anthropic_messages":
        async with anthropic.AsyncAnthropic(
            base_url=base,
            api_key=api_key,
            max_retries=0,
            http_client=http,
        ) as client:
            result = await client.messages.create(
                model=model,
                messages=messages,
                max_tokens=64,
                stream=stream,
                **controls,
            )
            if not stream:
                return (
                    result.content[0].text,
                    result.usage.input_tokens,
                    result.usage.output_tokens,
                )
            events = [event async for event in result]
            return [
                (
                    event.type,
                    event.delta.text if event.type == "content_block_delta" else None,
                    (
                        event.usage.output_tokens
                        if event.type == "message_delta"
                        else None
                    ),
                )
                for event in events
            ]
    options = genai.types.HttpOptions(base_url=base, httpx_async_client=http)
    async with genai.Client(api_key=api_key, http_options=options).aio as client:
        config = genai.types.GenerateContentConfig(max_output_tokens=64, **controls)
        if stream:
            result = await client.models.generate_content_stream(
                model=model,
                contents="ping",
                config=config,
            )
            return [
                (
                    chunk.text,
                    (
                        chunk.usage_metadata.candidates_token_count
                        if chunk.usage_metadata
                        else None
                    ),
                )
                async for chunk in result
            ]
        result = await client.models.generate_content(
            model=model, contents="ping", config=config
        )
        return (
            result.text,
            result.usage_metadata.prompt_token_count,
            result.usage_metadata.candidates_token_count,
        )


@pytest.fixture
def ffi_handle(upstream):
    return sg.ProviderRuntime().openai_compat(
        "openai",
        upstream.base_url,
        REAL_OPENAI_KEY,
        max_retries=0,
    )


@pytest.mark.parametrize("stream", [False, True])
@pytest.mark.parametrize(
    "dialect, provider, protocol, body, context",
    [
        (
            "openai_chat",
            "openai",
            "responses",
            {
                "model": MODEL,
                "messages": [{"role": "user", "content": "ping"}],
                "sandhi_extensions": {
                    "openai_responses": {"previous_response_id": "resp_fixture"}
                },
            },
            {},
        ),
        (
            "openai_responses",
            "openai",
            "responses",
            {"model": MODEL, "input": "ping", "prompt": {"id": "prompt_fixture"}},
            {},
        ),
        (
            "gemini_generate_content",
            "gemini",
            "gemini_generate_content",
            {
                "contents": [{"role": "user", "parts": [{"text": "ping"}]}],
                "cached_content": "cachedContents/fixture",
            },
            {"model": MODEL},
        ),
    ],
)
def test_direct_sdk_checks_references_in_the_effective_upstream_request(
    upstream,
    stream,
    dialect,
    provider,
    protocol,
    body,
    context,
):
    handle = sg.ProviderRuntime().provider(
        provider,
        MODEL,
        "synthetic",
        protocol=protocol,
        base_url=upstream.base_url,
        max_retries=0,
    )
    before = len(upstream.requests)
    call = handle.stream_sdk_json if stream else handle.complete_sdk_json
    with pytest.raises(sg.SandhiSdkError) as excinfo:
        call(dialect, json.dumps(body), **context)
    error = json.loads(str(excinfo.value))
    assert error["status"] == 400
    assert "gateway with reference ownership" in error["error"]["message"]
    assert error["error"]["retryable"] is False
    assert len(upstream.requests) == before


@pytest.mark.asyncio
@pytest.mark.parametrize("stream", [False, True])
async def test_sdk_gateway_peer_preserves_admitted_target_extensions(
    proxy, upstream, stream
):
    handle = sg.ProviderRuntime().provider(
        "openai",
        MODEL,
        fixture.VK_ANTHROPIC,
        protocol="chat_completions",
        peer_role="sandhi_gateway_v1",
        base_url=f"{proxy}/v1",
        max_retries=0,
    )
    kwargs = {
        "api_key": fixture.VK_ANTHROPIC,
        "extra_body": {"sandhi_extensions": {"anthropic": {"top_k": 7}}},
    }
    http = await sdk_call(proxy, None, "openai_chat", stream, **kwargs)
    http_origin = upstream.last()
    ffi = await sdk_call(
        "https://sandhi.invalid",
        httpx2.AsyncClient(transport=ffi_transport(handle, "openai_chat")),
        "openai_chat",
        stream,
        **kwargs,
    )
    assert upstream.last() is not http_origin
    assert upstream.last().body == http_origin.body
    assert upstream.last().body["top_k"] == 7
    assert "sandhi_extensions" not in upstream.last().body
    assert ffi == http


@pytest.mark.asyncio
async def test_gemini_proto_containers_follow_exact_model_admission(proxy, upstream):
    model = "gemini-3-flash-preview"
    body = {
        "contents": [{"role": "user", "parts": [{"text": "ping"}]}],
        "generation_config": {
            "max_output_tokens": 64,
            "thinking_config": {"thinking_budget": 1024},
        },
    }
    handle = sg.ProviderRuntime().provider(
        "gemini",
        model,
        fixture.REAL_GEMINI_KEY,
        base_url=upstream.base_url,
        max_retries=0,
    )
    before = len(upstream.requests)
    async with httpx2.AsyncClient() as client:
        http = await client.post(
            f"{proxy}/v1/models/{model}:generateContent",
            headers={"x-goog-api-key": fixture.VK_GEMINI},
            json=body,
        )
    assert http.status_code == 400
    with pytest.raises(sg.SandhiSdkError) as excinfo:
        await handle.complete_sdk_json(
            "gemini_generate_content", json.dumps(body), model=model
        )
    assert json.loads(str(excinfo.value))["status"] == 400
    assert len(upstream.requests) == before


@pytest.mark.asyncio
async def test_native_responses_failure_is_delivered_once(upstream, monkeypatch):
    def response(_streaming):
        def frame(kind, sequence, **fields):
            body = {
                "object": "response",
                "id": "resp_fixture",
                "model": MODEL,
                "output": [],
            }
            event = {
                "type": kind,
                "sequence_number": sequence,
                "response": {**body, **fields},
            }
            return f"event: {kind}\ndata: {json.dumps(event)}\n\n"

        return frame("response.created", 0, status="in_progress") + frame(
            "response.failed",
            1,
            status="failed",
            error={"code": "server_error", "message": "fixture failure"},
            usage={"input_tokens": 5, "output_tokens": 0, "total_tokens": 5},
        )

    monkeypatch.setattr(fixture, "_openai_chat_body", response)
    handle = sg.ProviderRuntime().provider(
        "openai",
        MODEL,
        REAL_OPENAI_KEY,
        base_url=upstream.base_url,
        protocol="responses",
        max_retries=0,
    )
    before = len(upstream.requests)
    stream = handle.stream_sdk_json(
        "openai_responses", json.dumps({"model": MODEL, "input": "ping"})
    )
    frames = [json.loads(frame) async for frame in stream]
    assert [frame["data"]["type"] for frame in frames] == [
        "response.created",
        "response.failed",
    ]
    assert len(upstream.requests) == before + 1


@pytest.mark.asyncio
@pytest.mark.parametrize(
    "dialect, provider, model, route, key, body, context",
    [
        (
            "anthropic_messages",
            "anthropic",
            "claude-sonnet-4-6",
            "/v1/messages",
            fixture.VK_ANTHROPIC,
            {
                "model": "claude-sonnet-4-6",
                "max_tokens": 64,
                "messages": [{"role": "user", "content": "ping"}],
                "container": None,
                "output_config": {"effort": "medium"},
                "temperature": 0.4,
            },
            {},
        ),
        (
            "gemini_generate_content",
            "gemini",
            "gemini-3-flash-preview",
            "/v1/models/gemini-3-flash-preview:generateContent",
            fixture.VK_GEMINI,
            {
                "contents": [{"role": "user", "parts": [{"text": "ping"}]}],
                "generationConfig": {"maxOutputTokens": 64},
                "cachedContent": None,
            },
            {"model": "gemini-3-flash-preview"},
        ),
    ],
)
async def test_native_sdk_default_modes_and_null_references_match_http(
    proxy,
    upstream,
    dialect,
    provider,
    model,
    route,
    key,
    body,
    context,
):
    handle = sg.ProviderRuntime().provider(
        provider,
        model,
        "synthetic",
        base_url=upstream.base_url,
        max_retries=0,
    )
    before = len(upstream.requests)
    async with httpx2.AsyncClient() as client:
        http = await client.post(
            f"{proxy}{route}",
            headers={"authorization": f"Bearer {key}"},
            json=body,
        )
    http.raise_for_status()
    ffi = json.loads(
        await handle.complete_sdk_json(dialect, json.dumps(body), **context)
    )
    assert ffi == http.json()
    assert len(upstream.requests) == before + 2
    if provider == "anthropic":
        for call in upstream.requests[before:]:
            assert "thinking" not in call.body
            assert call.body["output_config"]["effort"] == "medium"
            assert call.body["temperature"] == 0.4


@pytest.mark.asyncio
@pytest.mark.parametrize("stream", [False, True])
@pytest.mark.parametrize(
    "dialect, provider, model, upstream_key, virtual_key",
    [
        (
            "anthropic_messages",
            "anthropic",
            "claude-sonnet-4-6",
            fixture.REAL_ANTHROPIC_KEY,
            fixture.VK_ANTHROPIC,
        ),
        (
            "gemini_generate_content",
            "gemini",
            "gemini-3-flash-preview",
            fixture.REAL_GEMINI_KEY,
            fixture.VK_GEMINI,
        ),
    ],
)
async def test_sdk_native_thinking_uses_exact_model_admission(
    proxy,
    upstream,
    stream,
    dialect,
    provider,
    model,
    upstream_key,
    virtual_key,
):
    if provider == "anthropic":
        request = {
            "thinking": {"type": "adaptive"},
            "output_config": {"effort": "medium"},
        }
        denied = {**request, "output_config": {"effort": "xhigh"}}
        error_type = anthropic.BadRequestError
    else:
        request = {
            "thinking_config": genai.types.ThinkingConfig(thinking_level="MINIMAL")
        }
        denied = {"thinking_config": genai.types.ThinkingConfig(thinking_budget=1024)}
        error_type = genai.errors.ClientError

    handle = sg.ProviderRuntime().provider(
        provider,
        model,
        upstream_key,
        base_url=upstream.base_url,
        max_retries=0,
    )

    results = []
    for use_ffi in (False, True):

        def http_client():
            if use_ffi:
                return httpx2.AsyncClient(transport=ffi_transport(handle, dialect))
            return None

        base = "https://sandhi.invalid" if use_ffi else proxy
        results.append(
            await sdk_call(
                base,
                http_client(),
                dialect,
                stream,
                model=model,
                api_key=virtual_key,
                **request,
            )
        )
        sent = upstream.last().body
        if provider == "anthropic":
            assert sent["thinking"] == {"type": "adaptive"}
            assert sent["output_config"]["effort"] == "medium"
        else:
            # The native HTTP plane preserves SDK bytes. Typed FFI emits the
            # REST spelling from the same admitted semantic control.
            level_field = "thinkingLevel" if use_ffi else "thinking_level"
            assert sent["generationConfig"]["thinkingConfig"] == {
                level_field: "MINIMAL"
            }
        before = len(upstream.requests)
        with pytest.raises(error_type):
            await sdk_call(
                base,
                http_client(),
                dialect,
                stream,
                model=model,
                api_key=virtual_key,
                **denied,
            )
        assert len(upstream.requests) == before
    assert results[0] == results[1]


@pytest.mark.asyncio
@pytest.mark.parametrize("dialect", DIALECTS)
@pytest.mark.parametrize("stream", [False, True])
async def test_sdk_http_and_ffi_agree(proxy, upstream, ffi_handle, dialect, stream):
    http = await sdk_call(proxy, None, dialect, stream)
    http_origin = upstream.last()
    ffi = await sdk_call(
        "https://sandhi.invalid",
        httpx2.AsyncClient(transport=ffi_transport(ffi_handle, dialect)),
        dialect,
        stream,
    )
    ffi_origin = upstream.last()
    assert http_origin is not ffi_origin
    assert ffi_origin.path == http_origin.path
    assert ffi_origin.body == http_origin.body
    assert ffi == http
    assert "pong" in repr(ffi)


@pytest.mark.asyncio
@pytest.mark.parametrize("stream", [False, True])
async def test_sdk_denial_is_nonretryable_without_an_upstream_call(
    proxy, upstream, ffi_handle, stream
):
    before = len(upstream.requests)
    for base, http in (
        (proxy, None),
        (
            "https://sandhi.invalid",
            httpx2.AsyncClient(
                transport=ffi_transport(ffi_handle, "openai_chat"),
            ),
        ),
    ):
        with pytest.raises(openai.BadRequestError) as excinfo:
            await sdk_call(base, http, "openai_chat", stream, reasoning_effort="high")
        assert excinfo.value.response.json()["error"]["retryable"] is False
    assert len(upstream.requests) == before


@pytest.mark.asyncio
async def test_ffi_requires_unambiguous_json_and_explicit_gemini_model(
    ffi_handle, upstream
):
    before = len(upstream.requests)
    for dialect, body in (
        ("openai_chat", '{"model":"first","model":"second","messages":[]}'),
        (
            "gemini_generate_content",
            '{"contents":[{"role":"user","parts":[{"text":"ping"}]}]}',
        ),
    ):
        with pytest.raises(sg.SandhiSdkError) as excinfo:
            await ffi_handle.complete_sdk_json(dialect, body)
        error = json.loads(str(excinfo.value))
        assert error["status"] == 400
        assert error["error"]["retryable"] is False
    assert len(upstream.requests) == before


@pytest.mark.asyncio
async def test_sdk_tool_history_and_reasoning_usage_round_trip(
    proxy, upstream, ffi_handle, monkeypatch
):
    original = fixture._openai_chat_body

    def response(streaming):
        body = json.loads(original(streaming))
        if upstream.last().body["messages"][-1]["role"] == "user":
            body["choices"][0] = {
                "index": 0,
                "finish_reason": "tool_calls",
                "message": {
                    "role": "assistant",
                    "content": None,
                    "tool_calls": [
                        {
                            "id": "call_fixture",
                            "type": "function",
                            "function": {
                                "name": "echo",
                                "arguments": '{"value":"ping"}',
                            },
                        }
                    ],
                },
            }
        body["usage"]["completion_tokens_details"] = {"reasoning_tokens": 2}
        return json.dumps(body)

    monkeypatch.setattr(fixture, "_openai_chat_body", response)
    snapshots = []
    for base, http in (
        (proxy, None),
        (
            "https://sandhi.invalid",
            httpx2.AsyncClient(
                transport=ffi_transport(ffi_handle, "openai_chat"),
            ),
        ),
    ):
        async with openai.AsyncOpenAI(
            api_key=VK_OPENAI,
            base_url=f"{base}/v1",
            http_client=http,
            max_retries=0,
        ) as client:
            messages = [{"role": "user", "content": "ping"}]
            tools = [
                {
                    "type": "function",
                    "function": {"name": "echo", "parameters": {"type": "object"}},
                }
            ]
            first = await client.chat.completions.create(
                model=MODEL,
                messages=messages,
                tools=tools,
                max_completion_tokens=64,
            )
            call = first.choices[0].message.tool_calls[0]
            assert call.id == "call_fixture"
            messages.extend(
                [
                    {"role": "assistant", "tool_calls": [call.model_dump()]},
                    {"role": "tool", "tool_call_id": call.id, "content": "pong"},
                ]
            )
            final = await client.chat.completions.create(
                model=MODEL,
                messages=messages,
                tools=tools,
                max_completion_tokens=64,
            )
            assert final.choices[0].message.content == "pong"
            assert final.usage.completion_tokens_details.reasoning_tokens == 2
            assert final.usage.prompt_tokens_details.cached_tokens == 4
            snapshots.append(upstream.last().body)
    # A buffered typed dispatch supplies its upstream stream flag explicitly.
    for body in snapshots:
        assert body.get("stream", False) is False
        body.pop("stream", None)
    assert snapshots[0] == snapshots[1]


@pytest.mark.asyncio
async def test_ffi_stream_close_discards_buffered_frames(ffi_handle, upstream):
    before = len(upstream.requests)
    events = ffi_handle.stream_sdk_json(
        "openai_responses",
        json.dumps(
            {
                "model": MODEL,
                "input": "ping",
                "max_output_tokens": 64,
            }
        ),
    )
    assert json.loads(await anext(events))["event"] == "response.created"
    await events.aclose()
    assert await anext(events, None) is None
    assert len(upstream.requests) == before + 1


@pytest.mark.asyncio
async def test_sdk_midstream_failure_preserves_events_and_never_completes(
    proxy,
    upstream,
    ffi_handle,
    monkeypatch,
):
    original = fixture._openai_chat_body

    def interrupted(streaming):
        frames = original(streaming).split("\n\n")
        return "\n\n".join(frames[:2]) + "\n\n"

    monkeypatch.setattr(fixture, "_openai_chat_body", interrupted)
    before = len(upstream.requests)
    orders = []
    for base, http in (
        (proxy, None),
        (
            "https://sandhi.invalid",
            httpx2.AsyncClient(
                transport=ffi_transport(ffi_handle, "openai_responses"),
            ),
        ),
    ):
        async with openai.AsyncOpenAI(
            api_key=VK_OPENAI,
            base_url=f"{base}/v1",
            http_client=http,
            max_retries=0,
        ) as client:
            events = await client.responses.create(
                model=MODEL,
                input="ping",
                max_output_tokens=64,
                stream=True,
            )
            order = []
            try:
                async for event in events:
                    order.append(event.type)
            except openai.APIError:
                order.append("error")
            assert "response.output_text.delta" in order
            assert order[-1] == "error"
            assert "response.completed" not in order
            orders.append(order)
    assert len(upstream.requests) == before + 2
    assert orders[0] == orders[1]
