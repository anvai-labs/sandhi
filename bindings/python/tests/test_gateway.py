"""Python binding gates for the typed provider runtime and metering facade."""

from __future__ import annotations

import asyncio
import json
import os
import tempfile
import threading
from http.server import BaseHTTPRequestHandler, HTTPServer

import pytest
import sandhi_gateway as sg


def _start_server(handler_cls):
    """Start an HTTPServer on an ephemeral port with ``handler_cls`` (serves forever)."""
    server = HTTPServer(("127.0.0.1", 0), handler_cls)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    return server


def test_contract_discovery_and_schemas():
    assert sg.wire_contract_version() == "1"
    assert (
        sg.provider_spec("kimi", "kimi-k3")["base_url"] == "https://api.moonshot.ai/v1"
    )
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
            content_type = (
                "application/json" if Handler.calls == 1 else "text/event-stream"
            )
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
            events = [
                json.loads(event) async for event in provider.stream_json(request)
            ]
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
    provider = sg.ProviderRuntime().openai_compat(
        "openai", "http://127.0.0.1:1/v1", "key"
    )
    with pytest.raises(ValueError, match="tool message requires"):
        provider.complete_json(
            json.dumps(
                {
                    "model": "m",
                    "messages": [
                        {"role": "tool", "content": "missing", "tool_call_id": ""}
                    ],
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


def test_gemini_bearer_auth_crosses_typed_ffi_without_api_key_header():
    response_body = json.dumps(
        {
            "candidates": [{"content": {"parts": [{"text": "ok"}]}}],
            "usageMetadata": {"promptTokenCount": 2, "candidatesTokenCount": 1},
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
            "gemini",
            "gemini-test",
            "adc-token",
            base_url=f"http://127.0.0.1:{server.server_port}",
            max_retries=0,
            auth_scheme="bearer",
        )
        request = json.dumps(
            {
                "model": "gemini-test",
                "messages": [{"role": "user", "content": "hi"}],
            }
        )

        async def complete():
            return await provider.complete_json(request)

        response = json.loads(asyncio.run(complete()))

        assert response["output"]["content"] == "ok"
        assert Handler.headers_seen["authorization"] == "Bearer adc-token"
        assert Handler.headers_seen.get("x-goog-api-key") is None
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
    gateway.add_virtual_key(
        "vk_alice", subject="alice", group="platform", upstream="anthropic"
    )
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


# ---------------------------------------------------------------------------
# Provider dispatch + openai_responses factory + FFI-seam validation. Handle
# construction is pure (no network), so these exercise every branch offline.
# ---------------------------------------------------------------------------


def test_provider_factory_dispatches_every_named_backend():
    runtime = sg.ProviderRuntime()
    for provider, model in [
        ("anthropic", "claude-test"),
        ("claude", "claude-test"),
        ("gemini", "gemini-1.5-pro"),
        ("google", "gemini-1.5-pro"),
        ("cohere", "command-r"),
        ("ollama", "llama3"),
    ]:
        handle = runtime.provider(provider, model, "key", max_retries=0)
        assert isinstance(handle.provider, str)
        assert len(handle.provider) > 0

    # Default + explicit api_key auth scheme both valid for Anthropic.
    assert runtime.provider("anthropic", "claude-test", "k", max_retries=0).provider
    assert runtime.provider(
        "anthropic", "claude-test", "k", max_retries=0, auth_scheme="api_key"
    ).provider


def test_provider_routes_openai_compat_and_responses_escape_hatches():
    runtime = sg.ProviderRuntime()
    # Unknown provider WITH a base_url → openai_compat escape hatch (slug echoed back verbatim).
    custom = runtime.provider(
        "acme", "m", "key", base_url="https://example.test/v1", max_retries=0
    )
    assert custom.provider == "acme"

    # Known catalog provider WITHOUT a base_url → known_openai_compat resolves the spec.
    known = runtime.provider("deepseek", "deepseek-chat", "key", max_retries=0)
    assert known.provider == "deepseek"

    # openai_responses() direct factory.
    responses = runtime.openai_responses(
        "openai", "https://example.test/v1", "token", max_retries=0
    )
    assert responses.provider == "openai"

    # Responses protocol via provider() resolves a known catalog provider's base_url when omitted.
    via_provider = runtime.provider(
        "deepseek", "deepseek-chat", "key", max_retries=0, protocol="responses"
    )
    assert via_provider.provider == "deepseek"


def test_provider_rejects_invalid_dispatch_inputs_at_the_ffi_seam():
    runtime = sg.ProviderRuntime()
    # Unsupported auth_scheme value.
    with pytest.raises(ValueError, match="auth_scheme"):
        runtime.provider("anthropic", "m", "k", max_retries=0, auth_scheme="bogus")
    # auth_scheme supplied for a provider that does not support it.
    with pytest.raises(ValueError, match="Anthropic"):
        runtime.provider(
            "openai",
            "m",
            "k",
            base_url="https://e.test/v1",
            max_retries=0,
            auth_scheme="bearer",
        )
    # Unsupported protocol value.
    with pytest.raises(ValueError, match="protocol"):
        runtime.provider("openai", "m", "k", max_retries=0, protocol="bogus")
    # Responses protocol + unknown provider + no base_url.
    with pytest.raises(ValueError, match="base_url"):
        runtime.provider("acme", "m", "k", max_retries=0, protocol="responses")
    # Unknown catalog provider without a base_url under chat_completions.
    with pytest.raises(ValueError, match="unknown catalog provider"):
        runtime.provider("acme", "m", "k", max_retries=0)
    # Malformed headers JSON.
    with pytest.raises(ValueError, match="headers"):
        runtime.openai_compat(
            "openai", "https://e.test/v1", "k", headers_json="not-json"
        )


def test_contract_discovery_error_branches():
    # provider_spec: KeyError on an unknown provider; None model returns the default base_url.
    assert sg.provider_spec("kimi")["base_url"]  # None-model branch
    with pytest.raises(KeyError):
        sg.provider_spec("acme-unknown")
    # provider_descriptor_json: KeyError on unknown provider.
    with pytest.raises(KeyError):
        sg.provider_descriptor_json("acme-unknown")
    # chat_contract_schema_json: accepts the full filename and raises KeyError on unknown.
    by_full_name = json.loads(
        sg.chat_contract_schema_json("chat-request.v1.schema.json")
    )
    assert by_full_name["title"] == "ChatRequestV1"
    with pytest.raises(KeyError):
        sg.chat_contract_schema_json("nope.v1")


def _complete_error_handler():
    """A handler that always returns HTTP 500, capturing nothing."""
    body = json.dumps({"error": "boom"}).encode()

    class Handler(BaseHTTPRequestHandler):
        def do_POST(self):  # noqa: N802
            self.send_response(500)
            self.send_header("content-type", "application/json")
            self.send_header("content-length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def log_message(self, *_args):
            pass

    return Handler


def test_complete_json_surfaces_upstream_error_as_typed_descriptor():
    server = _start_server(_complete_error_handler())
    try:
        provider = sg.ProviderRuntime().openai_compat(
            "openai", f"http://127.0.0.1:{server.server_port}/v1", "key", max_retries=0
        )
        request = json.dumps(
            {"model": "gpt-test", "messages": [{"role": "user", "content": "hi"}]}
        )

        async def complete():
            return await provider.complete_json(request)

        with pytest.raises(Exception):  # typed ProviderError → RuntimeError JSON
            asyncio.run(complete())
    finally:
        server.shutdown()


def test_stream_json_surfaces_upstream_error_via_async_iteration():
    server = _start_server(_complete_error_handler())
    try:
        provider = sg.ProviderRuntime().openai_compat(
            "openai", f"http://127.0.0.1:{server.server_port}/v1", "key", max_retries=0
        )
        request = json.dumps(
            {"model": "gpt-test", "messages": [{"role": "user", "content": "hi"}]}
        )

        async def stream():
            return [json.loads(event) async for event in provider.stream_json(request)]

        with pytest.raises(Exception):
            asyncio.run(stream())
    finally:
        server.shutdown()


# ---------------------------------------------------------------------------
# Gateway: meter() (parse-driven), check_budget, events(), custom host parser
# (register_parser), meter_tokens, JSONL sink, group-less scope, and error paths.
# ---------------------------------------------------------------------------


def test_gateway_meter_parses_attributes_records_and_lists_events():
    gateway = sg.Gateway()
    gateway.add_virtual_key(
        "vk_alice", subject="alice", group="platform", upstream="anthropic"
    )
    gateway.set_budget("group:platform", 1000)

    event = gateway.meter(
        "vk_alice",
        "anthropic",
        "claude-x",
        json.dumps(
            {
                "usage": {
                    "input_tokens": 100,
                    "output_tokens": 20,
                    "cache_creation_input_tokens": 5,
                    "cache_read_input_tokens": 10,
                }
            }
        ),
        session_id="conv_1",
        route="router",
    )
    assert event["subject_id"] == "alice"
    assert event["group_id"] == "platform"
    assert event["session_id"] == "conv_1"
    assert event["route"] == "router"
    assert event["provider"] == "anthropic"
    assert event["usage_completeness"] == "final"
    assert event["backend"] == "external"
    assert gateway.spent("group:platform") == 120  # 100 + 20 billable

    # Within/over budget (120 spent of 1000 → 880 remaining).
    assert gateway.check_budget("group:platform", 879) is True
    assert gateway.check_budget("group:platform", 881) is False

    listed = gateway.events()
    assert len(listed) == 1
    assert listed[0]["request_id"] == event["request_id"]

    # Unknown virtual key → KeyError; bad JSON → ValueError.
    with pytest.raises(KeyError):
        gateway.meter("ghost", "openai", "m", json.dumps({}))
    with pytest.raises(ValueError):
        gateway.meter("vk_alice", "openai", "m", "{not json")


def test_gateway_custom_parser_overrides_builtin_and_supports_partial_dicts():
    gateway = sg.Gateway()
    gateway.add_virtual_key("vk_custom", subject="custom")

    calls = []

    def my_parser(response_json):
        calls.append(response_json)
        # Missing keys → parsed_from_pyobj defaults them to 0.
        return {"tokens_in": 50, "tokens_out": 7}

    gateway.register_parser("exotic", my_parser)
    event = gateway.meter("vk_custom", "exotic", "ex-1", json.dumps({"any": "shape"}))
    assert calls and calls[0] == json.dumps({"any": "shape"})
    assert event["tokens_in"] == 50
    assert event["tokens_out"] == 7
    assert event["cache_creation_tokens"] == 0  # missing-key default
    # Group-less key records against the vk:* scope.
    assert gateway.spent("vk:vk_custom") == 57

    # A custom parser that raises → ValueError.
    def bad_parser(_response_json):
        raise RuntimeError("nope")

    gateway.register_parser("broken", bad_parser)
    with pytest.raises(ValueError, match="custom parser"):
        gateway.meter("vk_custom", "broken", "x", "{}")


def test_gateway_meter_tokens_and_jsonl_sink_round_trip():
    sink = tempfile.NamedTemporaryFile(
        prefix="sandhi-sink-", suffix=".jsonl", delete=False
    )
    sink.close()
    os.unlink(sink.name)  # let the binding recreate it
    try:
        gateway = sg.Gateway(sink.name)
        gateway.add_virtual_key("vk_solo", subject="solo", upstream="openai")
        gateway.set_budget("vk:vk_solo", 1000)
        event = gateway.meter_tokens(
            "vk_solo", "openai", "gpt-test", 10, 5, session_id="s"
        )
        assert event["subject_id"] == "solo"
        assert event["group_id"] is None
        assert gateway.check_budget("vk:vk_solo", 1) is True
        assert gateway.check_budget("vk:vk_solo", 1000) is False

        with open(sink.name) as fh:
            lines = [ln for ln in fh.read().splitlines() if ln.strip()]
        assert len(lines) == 1
        assert json.loads(lines[0])["subject_id"] == "solo"
    finally:
        if os.path.exists(sink.name):
            os.unlink(sink.name)


def test_parse_usage_exercises_every_builtin_provider_parser():
    anthropic = sg.parse_usage(
        "anthropic", json.dumps({"usage": {"input_tokens": 7, "output_tokens": 3}})
    )
    assert anthropic == {
        "tokens_in": 7,
        "tokens_out": 3,
        "cache_creation_tokens": 0,
        "cache_read_tokens": 0,
    }
    # Remaining parsers are selected by slug; missing fields default to zero via unwrap_or_default,
    # so a minimal body still exercises each match arm.
    for provider in [
        "gemini",
        "cohere",
        "ollama",
        "bedrock",
        "openai_responses",
        "responses",
    ]:
        got = sg.parse_usage(provider, json.dumps({}))
        assert got == {
            "tokens_in": 0,
            "tokens_out": 0,
            "cache_creation_tokens": 0,
            "cache_read_tokens": 0,
        }
    with pytest.raises(ValueError):
        sg.parse_usage("openai", "{nope")
