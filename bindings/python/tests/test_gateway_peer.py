"""Synthetic immediate-peer contract; backend cache/receipt evidence lives in Rust HTTP tests."""

import json
import threading
from contextlib import contextmanager
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

import pytest
import sandhi_gateway as sg

PROTOCOLS = {
    "chat_completions": "v1-chat-standard",
    "anthropic_messages": "v1-messages-standard",
    "responses": "v1-responses-standard",
    "gemini_generate_content": "v1-gemini-standard",
}


def body(protocol, implicit=False):
    if protocol == "chat_completions":
        return {
            "id": "r",
            "model": "fixture",
            "choices": [
                {
                    "index": 0,
                    "message": {"role": "assistant", "content": "reply"},
                    "finish_reason": "stop",
                }
            ],
            "usage": {
                "prompt_tokens": 10,
                "completion_tokens": 4,
                "prompt_tokens_details": {"cached_tokens": 6, "cache_write_tokens": 2},
            },
        }
    if protocol == "anthropic_messages":
        usage = {"input_tokens": 2, "output_tokens": 4, "cache_read_input_tokens": 6}
        if not implicit:
            usage["cache_creation_input_tokens"] = 2
        return {
            "id": "r",
            "type": "message",
            "model": "fixture",
            "role": "assistant",
            "content": [{"type": "text", "text": "reply"}],
            "stop_reason": "end_turn",
            "usage": usage,
        }
    if protocol == "responses":
        return {
            "id": "r",
            "object": "response",
            "model": "fixture",
            "status": "completed",
            "output": [
                {
                    "id": "msg",
                    "type": "message",
                    "role": "assistant",
                    "content": [{"type": "output_text", "text": "reply"}],
                }
            ],
            "usage": {
                "input_tokens": 10,
                "output_tokens": 4,
                "input_tokens_details": {"cached_tokens": 6, "cache_write_tokens": 2},
            },
        }
    return {
        "candidates": [
            {
                "index": 0,
                "content": {"role": "model", "parts": [{"text": "reply"}]},
                "finishReason": "STOP",
            }
        ],
        "usageMetadata": {
            "promptTokenCount": 8,
            "cachedContentTokenCount": 6,
            "candidatesTokenCount": 4,
            "thoughtsTokenCount": 0,
            "totalTokenCount": 12,
        },
    }


def stream(protocol, response):
    if protocol == "chat_completions":
        values = [
            {
                "id": "r",
                "object": "chat.completion.chunk",
                "model": "fixture",
                "choices": [
                    {"index": 0, "delta": {"content": "reply"}, "finish_reason": "stop"}
                ],
            },
            {
                "object": "chat.completion.chunk",
                "choices": [],
                "usage": response["usage"],
            },
        ]
    elif protocol == "anthropic_messages":
        start = dict(response, content=[], stop_reason=None)
        values = [
            {"type": "message_start", "message": start},
            {
                "type": "content_block_start",
                "index": 0,
                "content_block": {"type": "text", "text": ""},
            },
            {
                "type": "content_block_delta",
                "index": 0,
                "delta": {"type": "text_delta", "text": "reply"},
            },
            {"type": "content_block_stop", "index": 0},
            {
                "type": "message_delta",
                "delta": {"stop_reason": "end_turn"},
                "usage": {"output_tokens": 4},
            },
            {"type": "message_stop"},
        ]
    elif protocol == "responses":
        values = [{"type": "response.completed", "response": response}]
    else:
        values = [response]
    result = "".join(f"data: {json.dumps(value)}\n\n" for value in values)
    return result + ("data: [DONE]\n\n" if protocol == "chat_completions" else "")


@contextmanager
def peer(protocol):
    records = []
    control = {"convention": PROTOCOLS[protocol], "implicit": False}

    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *_args):
            pass

        def do_POST(self):
            request = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
            records.append((self.path, dict(self.headers), request))
            streaming = request.get("stream") or "streamGenerateContent" in self.path
            response = body(protocol, control["implicit"])
            status = control.get("status", 200)
            if status != 200 or control.get("stream_error"):
                response = {
                    "type": "error",
                    "error": {
                        "message": "synthetic gateway failure",
                        "retryable": control.get("retryable", False),
                        "http_status": 502,
                    },
                }
                if control.get("error_code") is not None:
                    response["error"]["code"] = control["error_code"]
                payload = (
                    f"data: {json.dumps(response)}\n\n"
                    if streaming and status == 200
                    else json.dumps(response)
                ).encode()
            else:
                payload = (
                    stream(protocol, response) if streaming else json.dumps(response)
                ).encode()
            self.send_response(status)
            self.send_header(
                "Content-Type", "text/event-stream" if streaming else "application/json"
            )
            if control["convention"] is not None:
                self.send_header("x-sandhi-usage-convention", control["convention"])
            self.send_header("x-sandhi-request-id", "synthetic-gateway-request")
            if status != 200:
                self.send_header("x-should-retry", "false")
            self.send_header("Content-Length", str(len(payload)))
            self.end_headers()
            self.wfile.write(payload)

    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    worker = threading.Thread(
        target=server.serve_forever, kwargs={"poll_interval": 0.01}, daemon=True
    )
    worker.start()
    try:
        yield f"http://127.0.0.1:{server.server_port}", records, control
    finally:
        server.shutdown()
        server.server_close()
        worker.join(timeout=2)


def handle(root, protocol):
    suffix = (
        ""
        if protocol == "anthropic_messages"
        else "/v1beta" if protocol == "gemini_generate_content" else "/v1"
    )
    return sg.ProviderRuntime().provider(
        "neutral-agent",
        "fixture",
        "virtual-credential",
        base_url=root + suffix,
        protocol=protocol,
        peer_role="sandhi_gateway_v1",
        max_retries=0,
        timeout_secs=5,
    )


def request(native=False):
    return {
        "model": "fixture",
        "max_output_tokens": 16,
        "messages": [
            {"role": "system", "content": "stable policy"},
            {"role": "user", "content": "hello"},
        ],
        "include_native_response": native,
        "include_native_stream": native,
        "metadata": {"session_id": "stable-session"},
        "cache_intent": {"mode": "auto_stable_prefix", "lifetime": "5m"},
    }


@pytest.mark.asyncio
@pytest.mark.parametrize("protocol", PROTOCOLS)
@pytest.mark.parametrize("native", [False, True])
@pytest.mark.parametrize("streaming", [False, True])
async def test_gateway_peer_binding_forwards_intent_without_backend_capabilities(
    protocol, native, streaming
):
    with peer(protocol) as (root, records, _):
        provider = handle(root, protocol)
        headers = json.dumps(
            {"Idempotency-Key": "obsolete", "ChatGPT-Account-ID": "upstream-only"}
        )
        if streaming:
            events = [
                json.loads(item)
                async for item in provider.stream_json(
                    json.dumps(request(native)), headers
                )
            ]
            final = [
                item["usage"]
                for item in events
                if item["event"] == "usage" and item["usage"]["completeness"] == "final"
            ]
            assert len(final) == 1
            usage = final[0]
        else:
            response = json.loads(
                await provider.complete_json(json.dumps(request(native)), headers)
            )
            assert response["output"]["content"] == "reply"
            usage = response["usage"]
        assert provider.provider == "neutral-agent"
        assert [
            usage[key]
            for key in (
                "tokens_in",
                "cache_read_tokens",
                "cache_creation_tokens",
                "tokens_out",
                "attempts",
            )
        ] == [2, 6, 0 if protocol == "gemini_generate_content" else 2, 4, 1]
        assert len(records) == 1
        _, sent_headers, sent = records[0]
        headers = {name.lower(): value for name, value in sent_headers.items()}
        assert headers["authorization"] == "Bearer virtual-credential"
        assert headers["x-sandhi-session"] == "stable-session"
        assert "idempotency-key" not in headers and "chatgpt-account-id" not in headers
        assert sent["sandhi_cache"] == request()["cache_intent"]
        for control in (
            "cache_control",
            "prompt_cache_breakpoint",
            "prompt_cache_key",
            "cachedContent",
        ):
            assert control not in json.dumps(sent)


@pytest.mark.asyncio
async def test_gateway_peer_binding_reuses_messages_handle_across_conventions():
    with peer("anthropic_messages") as (root, records, control):
        provider = handle(root, "anthropic_messages")
        for implicit in (False, True, False):
            control.update(
                implicit=implicit,
                convention=(
                    "v1-messages-implicit-cache" if implicit else "v1-messages-standard"
                ),
            )
            response = json.loads(await provider.complete_json(json.dumps(request())))
            assert response["usage"]["cache_creation_tokens"] == (0 if implicit else 2)
        assert len(records) == 3


@pytest.mark.asyncio
@pytest.mark.parametrize(
    "convention",
    [
        None,
        "chatgpt_codex",
        "v2-chat-standard",
        "v1-messages-standard",
        "v1-chat-standard, v1-chat-terminal",
    ],
)
@pytest.mark.parametrize("streaming", [False, True])
async def test_gateway_peer_binding_refuses_invalid_contract_without_replay(
    convention, streaming
):
    with peer("chat_completions") as (root, records, control):
        control["convention"] = convention
        provider = handle(root, "chat_completions")
        with pytest.raises(sg.SandhiProviderError) as failure:
            if streaming:
                _ = [item async for item in provider.stream_json(json.dumps(request()))]
            else:
                await provider.complete_json(json.dumps(request()))
        payload = json.loads(str(failure.value))
        assert (
            payload["code"] == "output_projection_error"
            and payload["retryable"] is False
        )
        assert len(records) == 1


@pytest.mark.asyncio
@pytest.mark.parametrize("protocol", PROTOCOLS)
@pytest.mark.parametrize("streaming", [False, True])
@pytest.mark.parametrize("code", ["output_projection_error", None])
async def test_gateway_peer_http_failure_never_advertises_inference_repair(
    protocol, streaming, code
):
    with peer(protocol) as (root, records, control):
        control.update(status=502, error_code=code, retryable=code is None)
        provider = handle(root, protocol)
        with pytest.raises(sg.SandhiProviderError) as failure:
            if streaming:
                _ = [item async for item in provider.stream_json(json.dumps(request()))]
            else:
                await provider.complete_json(json.dumps(request()))
        payload = json.loads(str(failure.value))
        assert payload["code"] == (code or "gateway_error")
        assert payload["retryable"] is False
        assert payload["http_status"] == 502
        assert payload["request_id"] == "synthetic-gateway-request"
        assert len(records) == 1


@pytest.mark.asyncio
@pytest.mark.parametrize("protocol", PROTOCOLS)
@pytest.mark.parametrize("native", [False, True])
async def test_gateway_peer_sse_failure_keeps_typed_error_terminal(protocol, native):
    with peer(protocol) as (root, records, control):
        control.update(stream_error=True, error_code="output_projection_error")
        provider = handle(root, protocol)
        events = []
        with pytest.raises(sg.SandhiProviderError) as failure:
            async for item in provider.stream_json(json.dumps(request(native))):
                events.append(json.loads(item))
        payload = json.loads(str(failure.value))
        assert payload["code"] == "output_projection_error"
        assert payload["retryable"] is False
        assert payload["request_id"] == "synthetic-gateway-request"
        assert not any(
            event["event"] == "finish"
            or event["event"] == "usage"
            and event["usage"]["completeness"] == "final"
            for event in events
        )
        assert len(records) == 1


@pytest.mark.asyncio
async def test_gateway_peer_preserves_explicit_wire_session_precedence():
    with peer("chat_completions") as (root, records, _):
        provider = handle(root, "chat_completions")
        await provider.complete_json(
            json.dumps(request()), json.dumps({"x-sandhi-session": "explicit-session"})
        )
        headers = {name.lower(): value for name, value in records[0][1].items()}
        assert headers["x-sandhi-session"] == "explicit-session"
        assert len(records) == 1
