"""Real binding profile selection, independent of provider labels and credentials."""

import asyncio
import json
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

import pytest
import sandhi_gateway as sg


def wire(protocol, profile="standard"):
    if protocol == "chat_completions":
        values = [
            {
                "object": "chat.completion.chunk",
                "id": "chat",
                "model": "fixture",
                "choices": [
                    {"index": 0, "delta": {"content": "ok"}, "finish_reason": "stop"}
                ],
                "usage": {
                    "prompt_tokens": 11,
                    "completion_tokens": 3,
                    "prompt_tokens_details": {
                        "cached_tokens": 4,
                        "cache_write_tokens": 1,
                    },
                    "completion_tokens_details": {"reasoning_tokens": 0},
                },
            }
        ]
        if profile == "deepseek_final_choice_usage":
            terminal = values[0]
            progress = {**terminal, "choices": [{"index": 0, "delta": {"content": "ok"}, "finish_reason": None}], "usage": None}
            terminal["choices"][0]["delta"] = {}
            values.insert(0, progress)
        tail = "data: [DONE]\n\n"
    elif protocol == "responses":
        values = [
            {
                "type": "response.created",
                "response": {
                    "id": "resp",
                    "model": "fixture",
                    "status": "in_progress",
                    "output": [],
                },
            },
            {
                "type": "response.completed",
                "response": {
                    "id": "resp",
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
                        "input_tokens": 10,
                        "output_tokens": 3,
                        "input_tokens_details": {"cached_tokens": 4},
                        "output_tokens_details": {"reasoning_tokens": 0},
                    },
                },
            },
        ]
        tail = ""
    elif protocol == "gemini_generate_content":
        values = [
            {
                "responseId": "gemini",
                "modelVersion": "fixture",
                "candidates": [
                    {
                        "index": 0,
                        "content": {"role": "model", "parts": [{"text": "ok"}]},
                        "finishReason": "STOP",
                    }
                ],
                "usageMetadata": {
                    "promptTokenCount": 10,
                    "cachedContentTokenCount": 4,
                    "candidatesTokenCount": 3,
                    "thoughtsTokenCount": 1,
                    "totalTokenCount": 14,
                },
            }
        ]
        tail = ""
    else:
        values = [
            {
                "type": "message_start",
                "message": {
                    "id": "msg",
                    "type": "message",
                    "model": "fixture",
                    "role": "assistant",
                    "content": [],
                    "usage": {
                        "input_tokens": 6,
                        "output_tokens": 0,
                        "cache_read_input_tokens": 4,
                    },
                },
            },
            {
                "type": "content_block_start",
                "index": 0,
                "content_block": {"type": "text", "text": "ok"},
            },
            {"type": "content_block_stop", "index": 0},
            {
                "type": "message_delta",
                "delta": {"stop_reason": "end_turn"},
                "usage": {"output_tokens": 3},
            },
            {"type": "message_stop"},
        ]
        tail = ""
    return (
        "".join(f"data: {json.dumps(value)}\n\n" for value in values) + tail
    ).encode()


@pytest.mark.parametrize(
    "protocol,profile,provider,auth_scheme",
    [
        ("chat_completions", "openai_terminal_chunk_usage", "anthropic", "bearer"),
        ("chat_completions", "zai_final_choice_usage", "custom-zai", "bearer"),
        ("chat_completions", "deepseek_final_choice_usage", "custom-deepseek", "bearer"),
        (
            "anthropic_messages",
            "anthropic_implicit_cache",
            "custom-messages",
            "api_key",
        ),
        ("responses", "standard", "anthropic", "bearer"),
        ("gemini_generate_content", "standard", "custom-gemini", "api_key"),
    ],
)
def test_explicit_profile_uses_actual_protocol_and_retains_disjoint_counts(
    protocol, profile, provider, auth_scheme
):
    seen = []
    body = wire(protocol, profile)

    class Handler(BaseHTTPRequestHandler):
        def do_POST(self):
            request = json.loads(self.rfile.read(int(self.headers["content-length"])))
            seen.append((self.path, dict(self.headers), request))
            self.send_response(200)
            self.send_header("content-type", "text/event-stream")
            self.send_header("content-length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def log_message(self, *_args):
            pass

    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    try:
        runtime = sg.ProviderRuntime()
        handle = runtime.provider(
            provider,
            "fixture",
            "synthetic",
            max_retries=0,
            base_url=f"http://127.0.0.1:{server.server_port}",
            protocol=protocol,
            auth_scheme=auth_scheme,
            endpoint_profile=profile,
        )
        assert handle.provider == provider
        request = json.dumps(
            {
                "model": "fixture",
                "max_output_tokens": 64,
                "messages": [{"role": "user", "content": "hello"}],
            }
        )

        async def collect():
            return [json.loads(event) async for event in handle.stream_json(request)]

        events = asyncio.run(collect())
        assert (
            "".join(
                event["delta"] for event in events if event["event"] == "text_delta"
            )
            == "ok"
        )
        final = [
            event["usage"]
            for event in events
            if event["event"] == "usage" and event["usage"]["completeness"] == "final"
        ]
        assert len(final) == 1
        assert [
            final[0][field]
            for field in (
                "tokens_in",
                "cache_read_tokens",
                "cache_creation_tokens",
                "tokens_out",
            )
        ] == [6, 4, int(protocol == "chat_completions"), 3]
        assert events[-1]["event"] == "finish"
        assert len(seen) == 1
        assert "endpoint_profile" not in seen[0][2]
        headers = {key.lower(): value for key, value in seen[0][1].items()}
        expected = (
            ("Bearer synthetic", None, None)
            if auth_scheme == "bearer"
            else (
                (None, "synthetic", None)
                if protocol == "anthropic_messages"
                else (None, None, "synthetic")
            )
        )
        assert (
            headers.get("authorization"),
            headers.get("x-api-key"),
            headers.get("x-goog-api-key"),
        ) == expected
        if profile != "standard":
            handle = runtime.provider(
                provider,
                "fixture",
                "synthetic",
                max_retries=0,
                base_url=f"http://127.0.0.1:{server.server_port}",
                protocol=protocol,
                auth_scheme=auth_scheme,
                endpoint_profile="standard",
            )
            with pytest.raises(sg.SandhiProviderError) as failure:
                asyncio.run(collect())
            error = json.loads(str(failure.value))
            assert error["code"] == "output_projection_error"
            assert error["retryable"] is False
            assert len(seen) == 2
    finally:
        server.shutdown()
        server.server_close()


@pytest.mark.parametrize(
    "protocol,profile",
    [
        ("responses", "openai_terminal_chunk_usage"),
        ("gemini_generate_content", "anthropic_implicit_cache"),
        ("anthropic_messages", "chatgpt_codex"),
        ("chatgpt_responses", "standard"),
        ("chat_completions", "unsupported"),
    ],
)
def test_incompatible_profiles_fail_during_construction(protocol, profile):
    with pytest.raises(ValueError):
        sg.ProviderRuntime().provider(
            "fixture",
            "fixture",
            "synthetic",
            base_url="http://127.0.0.1:9",
            protocol=protocol,
            endpoint_profile=profile,
        )
