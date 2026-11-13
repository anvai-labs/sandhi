"""A received invalid body is a deterministic refusal, never retryable transport."""

import asyncio
import json
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

import pytest
import sandhi_gateway as sg


@pytest.mark.parametrize("native", [False, True])
@pytest.mark.parametrize(
    "protocol,wire",
    [
        (protocol, wire)
        for protocol in (
            "chat_completions",
            "responses",
            "anthropic_messages",
            "gemini_generate_content",
        )
        for wire in (b'{"output":[],"output":[]}', b"{invalid-json")
    ]
    + [
        (
            "chat_completions",
            json.dumps(
                {
                    "choices": [
                        {
                            "message": {
                                "role": "assistant",
                                "content": None,
                                "tool_calls": [
                                    {
                                        **(
                                            {}
                                            if kind == "__missing__"
                                            else {"type": kind}
                                        ),
                                        "id": "call",
                                        "function": {
                                            "name": "inspect",
                                            "arguments": "{}",
                                        },
                                    }
                                ],
                            },
                            "finish_reason": "tool_calls",
                        }
                    ],
                    "usage": {"prompt_tokens": 5, "completion_tokens": 2},
                }
            ).encode(),
        )
        for kind in (7, None, "", "__missing__")
    ],
)
def test_received_invalid_body_refuses_without_replay(protocol, wire, native):
    calls = []

    class Handler(BaseHTTPRequestHandler):
        def do_POST(self):
            calls.append(self.rfile.read(int(self.headers["content-length"])))
            self.send_response(200)
            self.send_header("content-type", "application/json")
            self.send_header("content-length", str(len(wire)))
            self.end_headers()
            self.wfile.write(wire)

        def log_message(self, *_args):
            pass

    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    threading.Thread(
        target=lambda: server.serve_forever(poll_interval=0.01), daemon=True
    ).start()
    try:
        # Retain default retry policy: the refusal category itself must prevent replay.
        handle = sg.ProviderRuntime().provider(
            "fixture",
            "fixture",
            "synthetic",
            base_url=f"http://127.0.0.1:{server.server_port}",
            protocol=protocol,
        )

        async def run():
            return await handle.complete_json(
                json.dumps(
                    {
                        "model": "fixture",
                        "max_output_tokens": 16,
                        "include_native_response": native,
                        "messages": [{"role": "user", "content": "hello"}],
                    }
                )
            )

        with pytest.raises(sg.SandhiProviderError) as failure:
            asyncio.run(run())
        error = json.loads(str(failure.value))
        assert error["code"] == "output_projection_error"
        assert error["retryable"] is False
        assert len(calls) == 1
    finally:
        server.shutdown()
        server.server_close()
