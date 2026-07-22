"""Python binding gates for the typed provider runtime and metering facade."""

from __future__ import annotations

import asyncio
import json
import threading
from http.server import BaseHTTPRequestHandler, HTTPServer

import pytest
import sandhi_gateway as sg


def test_contract_discovery_and_schemas():
    assert sg.wire_contract_version() == "1"
    assert sg.provider_spec("kimi", "kimi-k3")["base_url"] == "https://api.moonshot.ai/v1"
    descriptor = json.loads(sg.provider_descriptor_json("claude"))
    assert descriptor["slug"] == "anthropic"
    assert descriptor["endpoint_family"] == "anthropic_messages"
    schema = json.loads(sg.chat_contract_schema_json("chat-request.v1"))
    assert schema["title"] == "ChatRequestV1"


def test_parse_usage_keeps_cache_split_single_sourced():
    openai = sg.parse_usage(
        "openai",
        json.dumps(
            {
                "usage": {
                    "prompt_tokens": 100,
                    "completion_tokens": 20,
                    "prompt_tokens_details": {"cached_tokens": 60},
                }
            }
        ),
    )
    assert openai == {
        "tokens_in": 40,
        "tokens_out": 20,
        "cache_creation_tokens": 0,
        "cache_read_tokens": 60,
    }


def test_persistent_typed_provider_complete_and_stream():
    complete_body = json.dumps(
        {
            "id": "r1",
            "model": "gpt-test",
            "choices": [{"message": {"content": "hello"}, "finish_reason": "stop"}],
            "usage": {
                "prompt_tokens": 10,
                "completion_tokens": 3,
                "prompt_tokens_details": {"cached_tokens": 4},
            },
        }
    ).encode()
    stream_body = (
        'data: {"id":"r2","model":"gpt-test","choices":[{"delta":{"content":"he"},"finish_reason":null}]}\n\n'
        'data: {"id":"r2","model":"gpt-test","choices":[{"delta":{},"finish_reason":"stop"}],'
        '"usage":{"prompt_tokens":10,"completion_tokens":3,"prompt_tokens_details":{"cached_tokens":4}}}\n\n'
        "data: [DONE]\n\n"
    ).encode()

    class Handler(BaseHTTPRequestHandler):
        calls = 0

        def do_POST(self):  # noqa: N802
            Handler.calls += 1
            body = complete_body if Handler.calls == 1 else stream_body
            content_type = "application/json" if Handler.calls == 1 else "text/event-stream"
            self.send_response(200)
            self.send_header("content-type", content_type)
            self.send_header("content-length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def log_message(self, *_args):
            pass

    server = HTTPServer(("127.0.0.1", 0), Handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    try:
        runtime = sg.ProviderRuntime()
        provider = runtime.openai_compat(
            "openai", f"http://127.0.0.1:{server.server_port}/v1", "key", max_retries=0
        )
        request = json.dumps(
            {
                "schema_version": "1",
                "model": "gpt-test",
                "messages": [{"role": "user", "content": "hi"}],
            }
        )

        async def run():
            response = json.loads(await provider.complete_json(request))
            events = [json.loads(event) async for event in provider.stream_json(request)]
            return response, events

        response, events = asyncio.run(run())
        assert provider.provider == "openai"
        assert response["output"]["content"] == "hello"
        assert response["usage"]["tokens_in"] == 6
        assert [event["event"] for event in events] == [
            "response_start",
            "text_delta",
            "usage",
            "finish",
        ]
        assert Handler.calls == 2
    finally:
        server.shutdown()


def test_typed_request_validation_fails_before_http():
    provider = sg.ProviderRuntime().openai_compat("openai", "http://127.0.0.1:1/v1", "key")
    with pytest.raises(ValueError, match="tool message requires"):
        provider.complete_json(
            json.dumps(
                {
                    "model": "m",
                    "messages": [{"role": "tool", "content": "missing", "tool_call_id": ""}],
                }
            )
        )


def test_anthropic_bearer_auth_crosses_typed_ffi_without_api_key_header():
    response_body = json.dumps(
        {
            "id": "msg_1",
            "type": "message",
            "role": "assistant",
            "model": "claude-test",
            "content": [{"type": "text", "text": "ok"}],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 2, "output_tokens": 1},
        }
    ).encode()

    class Handler(BaseHTTPRequestHandler):
        headers_seen = None

        def do_POST(self):  # noqa: N802
            Handler.headers_seen = self.headers
            self.send_response(200)
            self.send_header("content-type", "application/json")
            self.send_header("content-length", str(len(response_body)))
            self.end_headers()
            self.wfile.write(response_body)

        def log_message(self, *_args):
            pass

    server = HTTPServer(("127.0.0.1", 0), Handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    try:
        provider = sg.ProviderRuntime().provider(
            "anthropic",
            "claude-test",
            "oauth-token",
            base_url=f"http://127.0.0.1:{server.server_port}",
            max_retries=0,
            auth_scheme="bearer",
        )
        request = json.dumps(
            {
                "model": "claude-test",
                "messages": [{"role": "user", "content": "hi"}],
                "max_output_tokens": 16,
            }
        )
        async def complete():
            return await provider.complete_json(request)

        response = json.loads(asyncio.run(complete()))

        assert response["output"]["content"] == "ok"
        assert Handler.headers_seen["authorization"] == "Bearer oauth-token"
        assert Handler.headers_seen.get("x-api-key") is None
    finally:
        server.shutdown()


def test_responses_protocol_is_explicit_and_item_shaped_across_typed_ffi():
    response_body = json.dumps(
        {
            "id": "resp_1",
            "model": "gpt-test",
            "status": "completed",
            "output": [
                {
                    "type": "message",
                    "role": "assistant",
                    "content": [{"type": "output_text", "text": "ok"}],
                }
            ],
            "usage": {
                "input_tokens": 12,
                "output_tokens": 3,
                "input_tokens_details": {"cached_tokens": 2},
                "output_tokens_details": {"reasoning_tokens": 1},
            },
        }
    ).encode()

    class Handler(BaseHTTPRequestHandler):
        request_body = None
        path_seen = None
        headers_seen = None

        def do_POST(self):  # noqa: N802
            Handler.path_seen = self.path
            Handler.headers_seen = self.headers
            Handler.request_body = json.loads(
                self.rfile.read(int(self.headers["content-length"]))
            )
            self.send_response(200)
            self.send_header("content-type", "application/json")
            self.send_header("content-length", str(len(response_body)))
            self.end_headers()
            self.wfile.write(response_body)

        def log_message(self, *_args):
            pass

    server = HTTPServer(("127.0.0.1", 0), Handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    try:
        provider = sg.ProviderRuntime().provider(
            "openai",
            "gpt-test",
            "oauth-token",
            base_url=f"http://127.0.0.1:{server.server_port}/v1",
            protocol="responses",
            headers_json=json.dumps({"originator": "victor"}),
            max_retries=0,
        )
        request = json.dumps(
            {
                "model": "gpt-test",
                "messages": [{"role": "user", "content": "hi"}],
            }
        )

        async def complete():
            return await provider.complete_json(request)

        response = json.loads(asyncio.run(complete()))
        assert Handler.path_seen == "/v1/responses"
        assert Handler.headers_seen["authorization"] == "Bearer oauth-token"
        assert Handler.headers_seen["originator"] == "victor"
        assert Handler.request_body["input"][0]["type"] == "message"
        assert "messages" not in Handler.request_body
        assert response["output"]["content"] == "ok"
        assert response["usage"]["tokens_in"] == 10
        assert response["usage"]["cache_read_tokens"] == 2
        assert response["usage"]["reasoning_tokens"] == 1
    finally:
        server.shutdown()


def test_chatgpt_responses_profile_aggregates_required_upstream_stream():
    stream_body = (
        'data: {"type":"response.created","response":{"id":"resp_1","model":"gpt-test"}}\n\n'
        'data: {"type":"response.output_text.delta","delta":"ok"}\n\n'
        'data: {"type":"response.completed","response":{"status":"completed","output":[],"usage":{"input_tokens":4,"output_tokens":1}}}\n\n'
    ).encode()

    class Handler(BaseHTTPRequestHandler):
        request_body = None

        def do_POST(self):  # noqa: N802
            Handler.request_body = json.loads(
                self.rfile.read(int(self.headers["content-length"]))
            )
            self.send_response(200)
            self.send_header("content-type", "text/event-stream")
            self.send_header("content-length", str(len(stream_body)))
            self.end_headers()
            self.wfile.write(stream_body)

        def log_message(self, *_args):
            pass

    server = HTTPServer(("127.0.0.1", 0), Handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    try:
        provider = sg.ProviderRuntime().provider(
            "openai",
            "gpt-test",
            "oauth-token",
            base_url=f"http://127.0.0.1:{server.server_port}",
            protocol="chatgpt_responses",
            max_retries=0,
        )
        request = json.dumps(
            {
                "model": "gpt-test",
                "messages": [
                    {"role": "developer", "content": "be precise"},
                    {"role": "user", "content": "hi"},
                ],
                "temperature": 0.5,
                "max_output_tokens": 20,
            }
        )

        async def complete():
            return await provider.complete_json(request)

        response = json.loads(asyncio.run(complete()))
        assert response["output"]["content"] == "ok"
        assert Handler.request_body["instructions"] == "be precise"
        assert Handler.request_body["store"] is False
        assert Handler.request_body["stream"] is True
        assert "temperature" not in Handler.request_body
        assert "max_output_tokens" not in Handler.request_body
    finally:
        server.shutdown()


def test_raw_provider_transport_surface_is_not_exported():
    for obsolete in ("complete", "stream", "register_provider", "ByteStreamIter"):
        assert not hasattr(sg, obsolete)


def test_gateway_attributes_enforces_and_records_budget():
    gateway = sg.Gateway()
    gateway.add_virtual_key("vk_alice", subject="alice", group="platform", upstream="anthropic")
    gateway.set_budget("group:platform", 1000)
    event = gateway.meter(
        "vk_alice",
        "anthropic",
        "claude-x",
        json.dumps(
            {
                "usage": {
                    "input_tokens": 220,
                    "output_tokens": 80,
                    "cache_creation_input_tokens": 10,
                    "cache_read_input_tokens": 40,
                }
            }
        ),
        session_id="conv_7",
    )
    assert event["subject_id"] == "alice"
    assert event["group_id"] == "platform"
    assert event["session_id"] == "conv_7"
    assert gateway.spent("group:platform") == 300
    assert gateway.check_budget("group:platform", 701) is False
