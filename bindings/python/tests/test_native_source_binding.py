"""Real binding, synthetic native Cohere/Ollama sources, one physical call."""

import json
import threading
from contextlib import contextmanager
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

import pytest
import sandhi_gateway as sg


@contextmanager
def native_source(family, scenario):
    calls = []

    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *_args):
            pass

        def do_POST(self):
            request = json.loads(self.rfile.read(int(self.headers["content-length"])))
            calls.append(request)
            body = (
                {
                    "id": "native-binding",
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
                if family == "cohere_chat"
                else {
                    "model": "fixture",
                    "message": {"role": "assistant", "content": "reply"},
                    "done": True,
                    "done_reason": "stop",
                    "prompt_eval_count": 7,
                    "eval_count": 4,
                }
            )
            if family == "ollama_chat" and scenario in ("zero", "read", "invalid_read"):
                body["prompt_eval_cached_count"] = {
                    "zero": 0,
                    "read": 3,
                    "invalid_read": 8,
                }[scenario]
            if scenario == "missing":
                if family == "cohere_chat":
                    body["usage"].pop("tokens")
                else:
                    body.pop("eval_count")
            if scenario == "opaque":
                body["message"]["opaque"] = "state"
            if request.get("stream"):
                if family == "cohere_chat":
                    frames = [
                        {
                            "type": "message-start",
                            "id": "native-binding",
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
                    if scenario == "opaque":
                        frames[2]["delta"]["message"]["content"]["opaque"] = "state"
                    encoded = "".join(
                        "data: " + json.dumps(frame) + "\n\n" for frame in frames
                    ).encode()
                else:
                    encoded = (json.dumps(body) + "\n").encode()
            else:
                encoded = json.dumps(body).encode()
            self.send_response(200)
            self.send_header(
                "content-type",
                "text/event-stream" if request.get("stream") else "application/json",
            )
            self.send_header("content-length", str(len(encoded)))
            self.end_headers()
            self.wfile.write(encoded)

    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    worker = threading.Thread(
        target=server.serve_forever, kwargs={"poll_interval": 0.01}, daemon=True
    )
    worker.start()
    try:
        yield f"http://127.0.0.1:{server.server_port}", calls
    finally:
        server.shutdown()
        server.server_close()
        worker.join(timeout=2)


@pytest.mark.asyncio
@pytest.mark.parametrize(
    "family,scenario",
    [
        (f, s)
        for f in ("cohere_chat", "ollama_chat")
        for s in ("absent", "missing", "opaque")
    ]
    + [("ollama_chat", s) for s in ("zero", "read", "invalid_read")],
)
@pytest.mark.parametrize("streaming", [False, True])
async def test_native_binding_source_counts_unknown_and_refusal_conserve_one_attempt(
    family, scenario, streaming
):
    with native_source(family, scenario) as (url, calls):
        handle = sg.ProviderRuntime().provider(
            "native-label",
            "fixture",
            "synthetic",
            base_url=url,
            protocol=family,
            max_retries=0,
            timeout_secs=3,
        )
        request = json.dumps(
            {
                "model": "fixture",
                "max_output_tokens": 32,
                "include_native_response": False,
                "messages": [{"role": "user", "content": "hello"}],
            }
        )

        async def submit():
            if not streaming:
                return json.loads(await handle.complete_json(request))["usage"]
            events = [json.loads(item) async for item in handle.stream_json(request)]
            final = [
                e["usage"]
                for e in events
                if e["event"] == "usage" and e["usage"]["completeness"] == "final"
            ]
            assert len(final) == 1
            assert events[-1]["event"] == "finish"
            return final[0]

        if scenario in ("missing", "opaque", "invalid_read"):
            with pytest.raises(sg.SandhiProviderError) as caught:
                await submit()
            assert json.loads(str(caught.value))["retryable"] is False
        else:
            usage = await submit()
            read = 3 if scenario == "read" else 0
            assert (
                usage["tokens_in"],
                usage["cache_read_tokens"],
                usage["tokens_out"],
            ) == (7 - read, read, 4)
            assert usage["cache_creation_tokens"] == 0
            assert usage.get("reasoning_tokens") is None
            assert usage["completeness"] == "final" and usage["attempts"] == 1
            assert usage["cache_read_observation"]["status"] == (
                "reported" if scenario in ("zero", "read") else "absent"
            )
        assert len(calls) == 1
