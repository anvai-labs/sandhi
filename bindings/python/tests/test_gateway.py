"""End-to-end tests for the `sandhi_gateway` in-process middleware.

Runnable with pytest OR directly (`python tests/test_gateway.py`). Requires the wheel to be
built/installed first (`maturin develop -m bindings/python/Cargo.toml`).
"""

import json

import sandhi_gateway as sg


def _serve(status=200, body="{}", content_type="application/json"):
    """Start a throwaway localhost server replying with a fixed status + body. Returns
    (base_url, shutdown). Used to drive transport error paths deterministically (no network)."""
    import threading
    from http.server import BaseHTTPRequestHandler, HTTPServer

    payload = body.encode() if isinstance(body, str) else body

    class _H(BaseHTTPRequestHandler):
        def do_POST(self):  # noqa: N802
            self.send_response(status)
            self.send_header("content-type", content_type)
            self.send_header("content-length", str(len(payload)))
            self.end_headers()
            self.wfile.write(payload)

        def log_message(self, *a):
            pass

    srv = HTTPServer(("127.0.0.1", 0), _H)
    port = srv.server_address[1]
    threading.Thread(target=srv.serve_forever, daemon=True).start()
    return f"http://127.0.0.1:{port}/v1", srv.shutdown


def test_wire_contract_version():
    assert sg.wire_contract_version() == "1"


def test_parse_usage_openai_cache_split():
    resp = json.dumps(
        {"usage": {"prompt_tokens": 100, "completion_tokens": 20,
                   "prompt_tokens_details": {"cached_tokens": 60}}}
    )
    u = sg.parse_usage("openai", resp)
    assert u["tokens_in"] == 40  # 100 total - 60 cached
    assert u["cache_read_tokens"] == 60
    assert u["tokens_out"] == 20
    assert u["cache_creation_tokens"] == 0


def test_parse_usage_anthropic_direct_split():
    resp = json.dumps(
        {"usage": {"input_tokens": 12, "output_tokens": 5,
                   "cache_creation_input_tokens": 3, "cache_read_input_tokens": 7}}
    )
    assert sg.parse_usage("anthropic", resp) == {
        "tokens_in": 12, "tokens_out": 5,
        "cache_creation_tokens": 3, "cache_read_tokens": 7,
    }


def test_complete_async_transport():
    """Step 3a (ADR-0047 D10): complete() forwards through sandhi's in-process transport and
    returns {status, body, usage} with usage parsed at the source. A local HTTP server stands in
    for the provider so no network is needed."""
    import asyncio
    import threading
    from http.server import BaseHTTPRequestHandler, HTTPServer

    resp = {
        "choices": [
            {"index": 0, "message": {"role": "assistant", "content": "hi"}, "finish_reason": "stop"}
        ],
        "usage": {
            "prompt_tokens": 100,
            "completion_tokens": 20,
            "prompt_tokens_details": {"cached_tokens": 60},
        },
    }

    class _H(BaseHTTPRequestHandler):
        def do_POST(self):  # noqa: N802
            body = json.dumps(resp).encode()
            self.send_response(200)
            self.send_header("content-type", "application/json")
            self.send_header("content-length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def log_message(self, *a):
            pass

    srv = HTTPServer(("127.0.0.1", 0), _H)
    port = srv.server_address[1]
    threading.Thread(target=srv.serve_forever, daemon=True).start()
    try:
        req = json.dumps({"model": "gpt-4o", "messages": [{"role": "user", "content": "hi"}]})

        async def _call():
            return await sg.complete(
                "openai", "gpt-4o", f"http://127.0.0.1:{port}/v1", "sk-test", req, "sess-1"
            )

        out = asyncio.run(_call())
        assert out["status"] == 200
        # usage parsed at the source → fresh-only tokens_in (100 − 60 cached).
        assert out["usage"]["tokens_in"] == 40
        assert out["usage"]["cache_read_tokens"] == 60
        assert out["usage"]["tokens_out"] == 20
        assert "hi" in json.loads(out["body"])["choices"][0]["message"]["content"]
    finally:
        srv.shutdown()


def test_stream_async_iterator():
    """Step 3b (ADR-0047 D10): stream() yields {data, usage} chunks via an async iterator; bytes
    are forwarded verbatim and usage is finalized on the terminal item. Local SSE server."""
    import asyncio
    import threading
    from http.server import BaseHTTPRequestHandler, HTTPServer

    sse = (
        'data: {"choices":[{"delta":{"content":"he"}}]}\n\n'
        'data: {"choices":[{"delta":{"content":"llo"}}]}\n\n'
        'data: {"choices":[],"usage":{"prompt_tokens":10,"completion_tokens":5,'
        '"prompt_tokens_details":{"cached_tokens":4}}}\n\n'
        "data: [DONE]\n\n"
    )

    class _H(BaseHTTPRequestHandler):
        def do_POST(self):  # noqa: N802
            b = sse.encode()
            self.send_response(200)
            self.send_header("content-type", "text/event-stream")
            self.send_header("content-length", str(len(b)))
            self.end_headers()
            self.wfile.write(b)

        def log_message(self, *a):
            pass

    srv = HTTPServer(("127.0.0.1", 0), _H)
    port = srv.server_address[1]
    threading.Thread(target=srv.serve_forever, daemon=True).start()
    try:
        req = json.dumps({"model": "gpt-4o", "messages": [{"role": "user", "content": "hi"}]})

        async def _collect():
            forwarded = b""
            usage = None
            async for chunk in sg.stream(
                "openai", "gpt-4o", f"http://127.0.0.1:{port}/v1", "sk", req, "s1"
            ):
                forwarded += chunk["data"]
                if chunk["usage"] is not None:
                    usage = chunk["usage"]
            return forwarded, usage

        forwarded, usage = asyncio.run(_collect())
        assert b"he" in forwarded and b"llo" in forwarded and b"[DONE]" in forwarded
        assert usage is not None
        assert usage["tokens_in"] == 6  # 10 − 4 cached
        assert usage["tokens_out"] == 5
        assert usage["cache_read_tokens"] == 4
    finally:
        srv.shutdown()


def test_register_custom_provider_escape_hatch():
    """Step 3d (ADR-0047 D10): a host-registered Python async provider serves complete() without a
    Rust adapter — the escape hatch for custom / air-gapped / community providers. The handler owns
    its own transport and reports usage; sandhi routes complete() to it and parses that usage."""
    import asyncio

    async def my_handler(model, body_json, session_id):
        req = json.loads(body_json)
        # A custom provider does its own 'transport'; here we just echo and self-report usage.
        return {
            "status": 200,
            "body": json.dumps({"model": model, "echoed": req, "session": session_id}),
            "usage": {
                "tokens_in": 7,
                "tokens_out": 3,
                "cache_creation_tokens": 0,
                "cache_read_tokens": 2,
            },
        }

    sg.register_provider("mycustom", my_handler)

    async def _call():
        body = json.dumps({"messages": [{"role": "user", "content": "hi"}]})
        return await sg.complete("mycustom", "custom-model-x", "http://unused", "k", body, "s9")

    out = asyncio.run(_call())
    assert out["status"] == 200
    assert out["usage"]["tokens_in"] == 7
    assert out["usage"]["tokens_out"] == 3
    assert out["usage"]["cache_read_tokens"] == 2
    body = json.loads(out["body"])
    assert body["model"] == "custom-model-x"
    assert body["session"] == "s9"


def test_gateway_meters_attributes_and_budgets():
    gw = sg.Gateway()
    gw.add_virtual_key("vk_alice", subject="alice", group="platform", upstream="anthropic")
    gw.set_budget("group:platform", 1000)

    resp = json.dumps(
        {"usage": {"input_tokens": 220, "output_tokens": 80,
                   "cache_creation_input_tokens": 0, "cache_read_input_tokens": 40}}
    )
    ev = gw.meter("vk_alice", "anthropic", "claude-x", resp, session_id="conv_7")

    assert ev["subject_id"] == "alice"
    assert ev["group_id"] == "platform"
    assert ev["virtual_key_id"] == "vk_alice"
    assert ev["session_id"] == "conv_7"
    assert ev["provider"] == "anthropic"
    assert ev["backend"] == "external"
    assert ev["schema_version"] == "1"
    assert ev["tokens_in"] == 220
    assert ev["cache_read_tokens"] == 40
    assert ev["gpu_seconds"] is None

    # billable = 220 + 80 = 300, recorded on the group scope
    assert gw.spent("group:platform") == 300
    assert len(gw.events()) == 1
    # a big next call is now over the 1000 budget (300 + 800 > 1000)
    assert gw.check_budget("group:platform", 800) is False
    assert gw.check_budget("group:platform", 700) is True


def test_unknown_virtual_key_raises_keyerror():
    gw = sg.Gateway()
    try:
        gw.meter("vk_nope", "openai", "m", json.dumps({"usage": {}}))
        raise AssertionError("expected KeyError")
    except KeyError:
        pass


def test_jsonl_sink_writes_events(tmp_path=None):
    import tempfile
    import os

    d = tempfile.mkdtemp()
    path = os.path.join(d, "usage.jsonl")
    gw = sg.Gateway(path)
    gw.add_virtual_key("vk", subject="s", group="g", upstream="openai")
    resp = json.dumps({"usage": {"prompt_tokens": 10, "completion_tokens": 5}})
    gw.meter("vk", "openai", "gpt-x", resp)
    gw.meter("vk", "openai", "gpt-x", resp)
    with open(path) as fh:
        lines = [json.loads(x) for x in fh if x.strip()]
    assert len(lines) == 2
    assert all(line["schema_version"] == "1" for line in lines)


def test_meter_tokens_bypasses_parsing():
    gw = sg.Gateway()
    gw.add_virtual_key("vk", subject="s", group="g", upstream="x")
    ev = gw.meter_tokens("vk", "custom-provider", "m", 11, 7, 0, 2, "sess")
    assert ev["tokens_in"] == 11
    assert ev["tokens_out"] == 7
    assert ev["cache_read_tokens"] == 2
    assert ev["provider"] == "custom-provider"
    assert ev["session_id"] == "sess"
    assert gw.spent("group:g") == 18  # 11 + 7


def test_register_parser_host_callback():
    gw = sg.Gateway()
    gw.add_virtual_key("vk", subject="s", group="g", upstream="x")
    calls = []

    def my_parser(response_json):
        d = json.loads(response_json)
        calls.append(d)
        return {"tokens_in": d["in"], "tokens_out": d["out"],
                "cache_creation_tokens": 0, "cache_read_tokens": 5}

    gw.register_parser("weirdprovider", my_parser)
    ev = gw.meter("vk", "weirdprovider", "m", json.dumps({"in": 30, "out": 12}))
    assert ev["tokens_in"] == 30
    assert ev["tokens_out"] == 12
    assert ev["cache_read_tokens"] == 5
    assert len(calls) == 1  # the host callback was invoked


# --------------------------------------------------------------------------------------------
# Error paths (quality control). The transport surface must fail loudly + with the right Python
# exception type: bad input → ValueError; provider/upstream/dispatch failure → RuntimeError.
# --------------------------------------------------------------------------------------------

_REQ = json.dumps({"model": "gpt-4o", "messages": [{"role": "user", "content": "hi"}]})


def _closed_port_base():
    """Bind then immediately release a localhost port so a connect there is refused — a
    deterministic transport (connection) failure with no network."""
    import socket

    s = socket.socket()
    s.bind(("127.0.0.1", 0))
    port = s.getsockname()[1]
    s.close()
    return f"http://127.0.0.1:{port}/v1"


def _expect(exc_type, make_awaitable):
    """Await `make_awaitable()` inside a running loop and assert it raises `exc_type`; return the
    message. The factory is invoked *inside* the loop (like a real caller) — invoking a transport
    coroutine before `asyncio.run` starts a loop would itself raise 'no running event loop'."""
    import asyncio

    async def _run():
        return await make_awaitable()

    try:
        asyncio.run(_run())
    except exc_type as e:
        return str(e)
    raise AssertionError(f"expected {exc_type.__name__}")


def test_complete_bad_body_json_raises_value_error():
    msg = _expect(
        ValueError,
        lambda: sg.complete("openai", "gpt-4o", "http://127.0.0.1:1/v1", "k", "not json", None),
    )
    assert "valid JSON" in msg


def test_complete_upstream_5xx_raises_runtime_error():
    base, shutdown = _serve(status=500, body='{"error": "boom"}')
    try:
        msg = _expect(
            RuntimeError, lambda: sg.complete("openai", "gpt-4o", base, "k", _REQ, "s1")
        )
        assert "sandhi transport" in msg  # upstream status surfaced, not swallowed
    finally:
        shutdown()


def test_complete_upstream_401_raises_runtime_error():
    base, shutdown = _serve(status=401, body='{"error": "unauthorized"}')
    try:
        _expect(RuntimeError, lambda: sg.complete("openai", "gpt-4o", base, "k", _REQ, "s1"))
    finally:
        shutdown()


def test_complete_upstream_429_raises_runtime_error():
    base, shutdown = _serve(status=429, body='{"error": "slow down"}')
    try:
        _expect(RuntimeError, lambda: sg.complete("openai", "gpt-4o", base, "k", _REQ, "s1"))
    finally:
        shutdown()


def test_complete_connection_refused_raises_runtime_error():
    msg = _expect(
        RuntimeError,
        lambda: sg.complete("openai", "gpt-4o", _closed_port_base(), "k", _REQ, "s1"),
    )
    assert "sandhi transport" in msg  # a Transport error, mapped to RuntimeError


def test_stream_bad_body_json_raises_value_error():
    # body_json is validated eagerly at the stream() call, before any iteration.
    import asyncio

    async def _call():
        return sg.stream("openai", "gpt-4o", "http://127.0.0.1:1/v1", "k", "{bad", None)

    try:
        asyncio.run(_call())
    except ValueError as e:
        assert "valid JSON" in str(e)
    else:
        raise AssertionError("expected ValueError")


def test_stream_upstream_5xx_raises_runtime_error_on_iteration():
    # The upstream failure surfaces on the first __anext__, not at stream() call time.
    base, shutdown = _serve(status=500, body='{"error": "boom"}')

    async def _drain():
        async for _ in sg.stream("openai", "gpt-4o", base, "k", _REQ, "s1"):
            pass

    try:
        msg = _expect(RuntimeError, lambda: _drain())
        assert "sandhi stream" in msg
    finally:
        shutdown()


def test_register_provider_handler_raises_surfaces_runtime_error():
    async def boom(model, body_json, session_id):
        raise ValueError("handler blew up")

    sg.register_provider("errprov", boom)
    msg = _expect(
        RuntimeError, lambda: sg.complete("errprov", "m", "http://unused", "k", _REQ, "s")
    )
    assert "custom provider" in msg


def test_register_provider_missing_status_raises_runtime_error():
    async def no_status(model, body_json, session_id):
        return {"body": "{}"}  # missing "status"

    sg.register_provider("nostatus", no_status)
    _expect(RuntimeError, lambda: sg.complete("nostatus", "m", "http://unused", "k", _REQ, "s"))


def test_register_provider_missing_body_raises_runtime_error():
    async def no_body(model, body_json, session_id):
        return {"status": 200}  # missing "body"

    sg.register_provider("nobody", no_body)
    _expect(RuntimeError, lambda: sg.complete("nobody", "m", "http://unused", "k", _REQ, "s"))


def test_register_provider_non_json_body_raises_runtime_error():
    async def bad_body(model, body_json, session_id):
        return {"status": 200, "body": "this is not json"}

    sg.register_provider("badbody", bad_body)
    msg = _expect(
        RuntimeError, lambda: sg.complete("badbody", "m", "http://unused", "k", _REQ, "s")
    )
    assert "not valid JSON" in msg


def test_register_provider_non_string_body_raises_runtime_error():
    async def obj_body(model, body_json, session_id):
        return {"status": 200, "body": {"not": "a string"}}  # body must be a JSON string

    sg.register_provider("objbody", obj_body)
    _expect(RuntimeError, lambda: sg.complete("objbody", "m", "http://unused", "k", _REQ, "s"))


def test_register_provider_missing_usage_defaults_to_zero():
    # usage is optional: a handler that omits it meters as all-zero, not an error.
    import asyncio

    async def no_usage(model, body_json, session_id):
        return {"status": 200, "body": json.dumps({"ok": True})}

    sg.register_provider("nousage", no_usage)

    async def _call():
        return await sg.complete("nousage", "m", "http://unused", "k", _REQ, "s")

    out = asyncio.run(_call())
    assert out["status"] == 200
    assert out["usage"] == {
        "tokens_in": 0,
        "tokens_out": 0,
        "cache_creation_tokens": 0,
        "cache_read_tokens": 0,
    }


def test_meter_bad_response_json_raises_value_error():
    gw = sg.Gateway()
    gw.add_virtual_key("vk", subject="s", group="g", upstream="openai")
    try:
        gw.meter("vk", "openai", "m", "not json at all")
    except ValueError as e:
        assert "valid JSON" in str(e)
    else:
        raise AssertionError("expected ValueError")


def test_register_parser_callback_raising_surfaces_value_error():
    gw = sg.Gateway()
    gw.add_virtual_key("vk", subject="s", group="g", upstream="x")

    def bad_parser(response_json):
        raise KeyError("missing field")

    gw.register_parser("weird", bad_parser)
    try:
        gw.meter("vk", "weird", "m", json.dumps({"anything": 1}))
    except ValueError as e:
        assert "custom parser" in str(e)
    else:
        raise AssertionError("expected ValueError")


if __name__ == "__main__":
    for _name, _fn in list(globals().items()):
        if _name.startswith("test_") and callable(_fn):
            _fn()
            print(f"ok {_name}")
    print("ALL PASS")
