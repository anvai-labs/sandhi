"""Synthetic protocol matrix for paired, single-attempt buffered accounting.

Real vendor SDKs and a copied gateway binary; no provider account or live cache claim.
"""
from contextlib import contextmanager
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import sqlite3
import threading
import time

import httpx
import openai
import pytest

from test_backend_registry import Gateway, backend_binaries  # noqa: F401

FAMILIES = ("openai_chat_completions", "anthropic_messages", "openai_responses", "gemini_generate_content")


def response_for(family):
    if family == "openai_chat_completions":
        return {"id": "chat_fixture", "object": "chat.completion", "created": 1, "model": "fixture",
                "choices": [{"index": 0, "finish_reason": "stop", "message": {"role": "assistant", "content": "reply"}}],
                "usage": {"prompt_tokens": 5, "completion_tokens": 2, "total_tokens": 7,
                          "prompt_tokens_details": {"cached_tokens": 2, "cache_write_tokens": 1},
                          "completion_tokens_details": {"reasoning_tokens": 1}}}
    if family == "openai_responses":
        return {"id": "resp_fixture", "object": "response", "created_at": 1, "model": "fixture", "status": "completed",
                "output": [{"id": "msg_fixture", "type": "message", "role": "assistant", "status": "completed",
                            "content": [{"type": "output_text", "text": "reply", "annotations": []}]}],
                "usage": {"input_tokens": 5, "output_tokens": 2, "total_tokens": 7,
                          "input_tokens_details": {"cached_tokens": 2, "cache_write_tokens": 1},
                          "output_tokens_details": {"reasoning_tokens": 1}}}
    if family == "anthropic_messages":
        return {"id": "msg_fixture", "type": "message", "role": "assistant", "model": "fixture",
                "content": [{"type": "text", "text": "reply"}], "stop_reason": "end_turn", "stop_sequence": None,
                "usage": {"input_tokens": 2, "output_tokens": 2, "cache_creation_input_tokens": 1,
                          "cache_read_input_tokens": 2, "output_tokens_details": {"thinking_tokens": 1}}}
    if family == "cohere_chat":
        return {"id": "cohere_fixture", "message": {"role": "assistant", "content": [{"type": "text", "text": "reply"}]},
                "finish_reason": "COMPLETE", "usage": {"billed_units": {"input_tokens": 5, "output_tokens": 2}}}
    return {"modelVersion": "fixture", "candidates": [{"index": 0, "content": {"role": "model", "parts": [{"text": "reply"}]},
                                                        "finishReason": "STOP"}],
            "usageMetadata": {"promptTokenCount": 5, "cachedContentTokenCount": 2, "candidatesTokenCount": 2,
                              "toolUsePromptTokenCount": 1, "thoughtsTokenCount": 1, "totalTokenCount": 9}}


@pytest.fixture
def qualified_origin():
    calls = []
    control = {"mode": "valid", "entered": threading.Event(), "release": threading.Event()}
    control["release"].set()

    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *_args):
            pass

        def do_POST(self):
            body = json.loads(self.rfile.read(int(self.headers["content-length"])))
            family = {"/chat/completions": FAMILIES[0], "/v1/messages": FAMILIES[1], "/responses": FAMILIES[2],
                      "/v1beta/models/fixture:generateContent": FAMILIES[3], "/models/fixture:generateContent": FAMILIES[3],
                      "/v1beta/models/fixture:streamGenerateContent?alt=sse": FAMILIES[3],
                      "/models/fixture:streamGenerateContent?alt=sse": FAMILIES[3],
                      "/v2/chat": "cohere_chat"}[self.path]
            calls.append((self.path, body))
            control["entered"].set()
            assert control["release"].wait(10), "test did not release origin"
            response = response_for(family)
            if family == FAMILIES[3] and control.get("tool_loop"):
                turn = len(calls)
                if turn <= 2:
                    call = {"name": "alpha" if turn == 1 else "beta", "args": {}}
                    if control.get("supplied_tool_id"):
                        call["id"] = "upstream-reused"
                    response["candidates"][0]["content"]["parts"] = [{"functionCall": call}]
                response["usageMetadata"] = {"promptTokenCount": 3, "cachedContentTokenCount": 2,
                    "toolUsePromptTokenCount": 1, "candidatesTokenCount": 2, "thoughtsTokenCount": 1, "totalTokenCount": 7}
            if family == FAMILIES[1] and control.get("implicit_stream_writes"):
                response["usage"].pop("cache_creation_input_tokens")
            if control["mode"] == "missing":
                response.pop("usageMetadata" if family == FAMILIES[3] else "usage")
            elif control["mode"] == "no_writes":
                response["usage"].pop("cache_creation_input_tokens")
            elif control["mode"] == "malformed":
                usage = response["usageMetadata" if family == FAMILIES[3] else "usage"]
                usage["candidatesTokenCount" if family == FAMILIES[3] else "output_tokens" if family != FAMILIES[0] else "completion_tokens"] = "bad"
            encoded = json.dumps(response).encode()
            if control["mode"] == "ambiguous":
                key = "usageMetadata" if family == FAMILIES[3] else "usage"
                encoded = encoded[:-1] + b',"' + key.encode() + b'":' + json.dumps(response[key]).encode() + b'}'
            streaming = (body.get("stream") is True and family in (FAMILIES[0], FAMILIES[1], FAMILIES[2])) or ":streamGenerateContent?alt=sse" in self.path
            if streaming and family == FAMILIES[0]:
                events = [{"id": "chat_fixture", "object": "chat.completion.chunk", "created": 1, "model": "fixture",
                    "choices": [{"index": 0, "delta": {"content": "reply"}, "finish_reason": "length" if control["mode"] == "incomplete" else "stop"}]}]
                if control.get("terminal_chunk_usage") and "usage" in response:
                    events[0]["usage"] = response["usage"]
                elif "usage" in response:
                    events.append({"id": "chat_fixture", "object": "chat.completion.chunk", "created": 1,
                        "model": "fixture", "choices": [], "usage": response["usage"]})
                encoded = ("".join("data: " + json.dumps(e) + "\n\n" for e in events) + "data: [DONE]\n\n").encode()
                if control["mode"] == "post_terminal":
                    encoded += b'data: {}\n\n'
            elif streaming and family == FAMILIES[1]:
                usage = dict(response.get("usage", {}))
                final_usage = {"output_tokens": usage["output_tokens"]} if "output_tokens" in usage else {}
                if "output_tokens" in usage:
                    usage["output_tokens"] = 1
                events = [
                    {"type": "message_start", "message": {"id": "msg_fixture", "type": "message",
                     "role": "assistant", "model": "fixture", "content": [], "usage": usage}},
                    {"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}},
                    {"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "reply"}},
                    {"type": "content_block_stop", "index": 0},
                ]
                if control.get("signed_output"):
                    for event in events:
                        if "index" in event:
                            event["index"] = 1
                    events[1:1] = [
                        {"type": "content_block_start", "index": 0, "content_block": {"type": "thinking", "thinking": "", "signature": ""}},
                        {"type": "content_block_delta", "index": 0, "delta": {"type": "signature_delta", "signature": "synthetic-nonportable"}},
                        {"type": "content_block_stop", "index": 0},
                    ]
                if control["mode"] != "progress_only":
                    events += [
                        {"type": "message_delta", "delta": {"stop_reason": "max_tokens" if control["mode"] == "incomplete" else "end_turn"}, "usage": final_usage},
                        {"type": "message_stop"},
                    ]
                encoded = "".join("event: " + e["type"] + "\ndata: " + json.dumps(e) + "\n\n" for e in events).encode()
                if control["mode"] == "post_terminal":
                    encoded += b'data: {}\n\n'
            elif streaming and family == FAMILIES[3]:
                # Fresh = 3 prompt - 2 read + 1 tool input. Thoughts are
                # separate from candidate output, unlike the other profiles.
                usage = {"promptTokenCount": 3, "cachedContentTokenCount": 2,
                         "toolUsePromptTokenCount": 1, "candidatesTokenCount": 2,
                         "thoughtsTokenCount": 1, "totalTokenCount": 7}
                events = [{"responseId": "gemini_fixture", "modelVersion": "fixture",
                           "candidates": [{"index": 0, "content": {"role": "model", "parts": [{"text": "reply"}]}}],
                           "usageMetadata": {"promptTokenCount": 3, "cachedContentTokenCount": 2,
                                             "toolUsePromptTokenCount": 1, "candidatesTokenCount": 1,
                                             "thoughtsTokenCount": 1, "totalTokenCount": 6}}]
                if control.get("tool_loop"):
                    events[0]["candidates"][0]["content"] = response["candidates"][0]["content"]
                if control["mode"] != "progress_only":
                    terminal = {"candidates": [{"index": 0, "finishReason": "MAX_TOKENS" if control["mode"] == "incomplete" else "STOP"}]}
                    if control["mode"] != "missing":
                        terminal["usageMetadata"] = usage
                    if control["mode"] == "malformed":
                        usage["candidatesTokenCount"] = "bad"
                    events.append(terminal)
                encoded = "".join("data: " + json.dumps(e) + "\n\n" for e in events).encode()
                if control["mode"] == "post_terminal":
                    encoded += b'data: {"candidates":[{"index":0}]}\n\n'
            elif streaming:
                progress = {"id": "resp_fixture", "status": "in_progress", "usage": {
                    "input_tokens": 5, "output_tokens": 1, "input_tokens_details": {"cached_tokens": 2}}}
                events = [
                    {"type": "response.in_progress", "response": progress},
                    {"type": "response.output_text.delta", "response_id": "resp_fixture", "output_index": 0,
                     "content_index": 0, "item_id": "msg_fixture", "delta": "reply"},
                ]
                if control["mode"] == "failed":
                    response["status"] = "failed"
                elif control["mode"] == "incomplete":
                    response["status"] = "incomplete"
                    response["incomplete_details"] = {"reason": "max_output_tokens"}
                if control["mode"] != "progress_only":
                    events.append({"type": "response." + response["status"], "response": response})
                encoded = "".join("event: " + e["type"] + "\ndata: " + json.dumps(e) + "\n\n" for e in events).encode()
                if control["mode"] == "post_terminal":
                    encoded += b'data: {}\n\n'
            self.send_response(200)
            self.send_header("content-type", "text/event-stream" if streaming else "application/json")
            self.send_header("content-length", str(len(encoded)))
            self.send_header("x-request-id", "synthetic-origin-request")
            self.end_headers()
            try:
                self.wfile.write(encoded)
            except (BrokenPipeError, ConnectionResetError):
                pass

    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        yield f"http://127.0.0.1:{server.server_port}", calls, control
    finally:
        control["release"].set()
        server.shutdown()
        server.server_close()
        thread.join(timeout=2)


@contextmanager
def tracked_gateway(tmp_path, binaries, origin, family, *, endpoint_profile="standard", streaming=False):
    instance = Gateway(tmp_path, binaries)
    if streaming:
        instance.config.write_text(json.dumps({"streaming_deadlines": {"ceiling_ms": 10000,
            "default": {"setup_ms": 2000, "idle_ms": 2000, "body_ms": 5000}}}))
    try:
        instance.start(SANDHI_BUFFERED_ACCOUNTING="tracked", SANDHI_CACHE_READ_WEIGHT="1000", SANDHI_SHUTDOWN_GRACE_SECS="1")
        instance.provision(origin, "synthetic-protocol-credential")
        policy = tmp_path / "cache.json"
        policy.write_text(json.dumps({"default_intent": {"mode": "auto"}, "models": {"fixture": {"profile": "implicit_only"}}}))
        instance.cli("set", "primary", "--provider", "models", "--credential-ref", "openai:default",
                     "--endpoint-family", family, "--endpoint-profile", endpoint_profile,
                     "--model", "fixture", "--cache-policy", str(policy))
        definition = {"id": "protocols", "models": [{"id": "agent", "targets": [{"backend": "primary", "model": "fixture"}]}]}
        assert instance.admin("POST", "/admin/routes", json={"definition": definition}).status_code == 201
        for scope in ("group:protocol-team", "upstream:models", "upstream:anthropic", "upstream:gemini"):
            assert instance.admin("POST", "/admin/budget", json={"scope": scope, "limit_tokens": 10000,
                "window": "total", "policy": "block"}).status_code == 200
        minted = instance.admin("POST", "/admin/keys/share", json={"upstream": "route/protocols", "models": ["agent"], "group": "protocol-team"})
        assert minted.status_code == 200, minted.text
        yield instance, minted.json()["virtual_key"]
    finally:
        instance.stop()


def sdk_call(instance, token, ingress):
    headers = {"x-sandhi-session": "protocol-session"}
    if ingress in FAMILIES[:1] + FAMILIES[2:3]:
        with openai.OpenAI(base_url=instance.base + "/v1", api_key=token, max_retries=0, default_headers=headers) as client:
            if ingress == FAMILIES[0]:
                return client.chat.completions.create(model="agent", max_tokens=64,
                    messages=[{"role": "user", "content": "hello"}]).choices[0].message.content
            return client.responses.create(model="agent", input="hello", max_output_tokens=64).output_text
    if ingress == FAMILIES[1]:
        import anthropic
        with anthropic.Anthropic(base_url=instance.base, api_key=token, max_retries=0, default_headers=headers) as client:
            return client.messages.create(model="agent", max_tokens=64,
                messages=[{"role": "user", "content": "hello"}]).content[0].text
    from google import genai
    from google.genai import types
    with genai.Client(api_key=token, http_options=types.HttpOptions(base_url=instance.base, headers=headers,
                     retry_options=types.HttpRetryOptions(attempts=1))) as client:
        return client.models.generate_content(model="agent", contents="hello",
            config=types.GenerateContentConfig(max_output_tokens=64)).text


def evidence(database, *, outcome="success"):
    with sqlite3.connect(database) as connection:
        row = connection.execute("""SELECT c.execution_id,c.context_json,a.attempt_id,a.authorized_at,a.evidence_json,o.usage_json,o.frozen_charge
            FROM budget_execution_context c JOIN budget_physical_attempt a USING(execution_id)
            JOIN budget_terminal_observation o USING(execution_id)""").fetchone()
        assert row is not None
        execution, context_json, identity, authorized, transport_json, usage_json, charge = row
        context, transport, usage = map(json.loads, (context_json, transport_json, usage_json))
        assert transport["attempt_id"] == identity and transport["attempt_ordinal"] == 1
        assert transport["dispatch_observed_at_ms"] is not None and transport["terminal"]["outcome"] == outcome
        assert transport["terminal"]["provider_status"] == 200
        assert transport["terminal"]["provider_request_id"] == "synthetic-origin-request"
        assert transport["dropped_observations"] == 0 and transport["invalid"] is False
        assert connection.execute("SELECT attempt_id,transitioned_at FROM budget_dispatch_fence").fetchone() == (identity, authorized)
        return row, context, usage, charge


@pytest.mark.parametrize("mode", ["valid", "incomplete", "failed", "progress_only", "missing", "malformed", "post_terminal"])
def test_tracked_responses_stream_preserves_canonical_usage_and_paired_settlement(
    tmp_path, backend_binaries, qualified_origin, mode,
):
    origin, calls, control = qualified_origin
    control["mode"] = mode
    with tracked_gateway(tmp_path, backend_binaries, origin, FAMILIES[2], streaming=True) as (instance, token):
        if mode in ("valid", "incomplete"):
            with instance.client(token) as client:
                events = list(client.responses.create(model="agent", input="hello", max_output_tokens=64, stream=True))
                assert "".join(e.delta for e in events if e.type == "response.output_text.delta") == "reply"
                assert events[-1].type == ("response.completed" if mode == "valid" else "response.incomplete")
        else:
            with httpx.Client(timeout=10) as client:
                # Hyper can observe an immediately failed body before it flushes
                # headers. Both failure positions must preserve source evidence;
                # neither is a successfully completed model response.
                with pytest.raises(httpx.RemoteProtocolError):
                    with client.stream("POST", instance.base + "/v1/responses", headers={"authorization": "Bearer " + token},
                        json={"model": "agent", "input": "hello", "max_output_tokens": 64, "stream": True}) as response:
                        assert response.status_code == 200
                        response.read()
        assert len(calls) == 1
        qualified = mode in ("valid", "incomplete", "failed")
        deadline = time.monotonic() + 5
        while True:
            with sqlite3.connect(instance.database) as connection:
                # Failed delivery is published before accounting finishes. A
                # durable observation alone is not a settlement receipt.
                table = "budget_settlement_outbox" if qualified else "budget_terminal_observation"
                ready = connection.execute(f"SELECT count(*) FROM {table}").fetchone()[0]
            if ready:
                break
            assert time.monotonic() < deadline
            time.sleep(.02)
        expected_outcome = "provider_rejected" if mode == "failed" else "success" if qualified else "incomplete_stream"
        _, context, usage, charge = evidence(instance.database, outcome=expected_outcome)
        assert context["ingress"] == FAMILIES[2] and context["endpoint_family"] == FAMILIES[2]
        assert context["delivery"] == "streaming" and context["plane"] == "transparent"
        assert usage["completeness"] == ("final" if qualified else "partial")
        assert usage["cache_read_tokens"] == 2 and usage["basis"] == "provider_reported"
        if qualified or mode == "post_terminal":
            assert (usage["tokens_in"], usage["cache_creation_tokens"], usage["tokens_out"], usage["reasoning_tokens"]) == (2, 1, 2, 1)
        else:
            assert (usage["tokens_in"], usage["cache_creation_tokens"], usage["tokens_out"]) == (3, 0, 1)
        with sqlite3.connect(instance.database) as connection:
            assert connection.execute("SELECT charged_tokens FROM budget_settlement_outbox").fetchall() == ([(5,)] if qualified else [])
            assert connection.execute("SELECT settled,actual FROM budget_reservation").fetchall() == ([(1, 5)] * 2 if qualified else [(0, 0)] * 2)
        if not qualified:
            instance.process.terminate()
            assert instance.process.wait(timeout=5) == 124
        instance.stop()
        with sqlite3.connect(instance.database) as connection:
            assert connection.execute("SELECT count(*) FROM usage_events").fetchone() == (1,)


@pytest.mark.parametrize("family", FAMILIES)
@pytest.mark.parametrize("ingress", FAMILIES)
def test_tracked_vendor_sdk_matrix_joins_route_cache_attempt_usage_and_paired_receipt(
    tmp_path, backend_binaries, qualified_origin, family, ingress,
):
    origin, calls, _ = qualified_origin
    with tracked_gateway(tmp_path, backend_binaries, origin, family) as (instance, token):
        assert sdk_call(instance, token, ingress) == "reply"
        assert len(calls) == 1
        row, context, usage, charge = evidence(instance.database)
        assert context["ingress"] == ingress and context["endpoint_family"] == family
        assert context["plane"] == ("transparent" if family == ingress else "translation")
        assert context["route"]["id"] == "protocols" and context["backend"]["id"] == "primary"
        assert context["requested_model"] == "agent" and context["actual_model"] == "fixture"
        assert context["cache"]["capability"] == "implicit_only"
        assert context["accounting_policy"]["cache_read_weight"] == 1000
        assert usage["completeness"] == "final" and usage["basis"] == "provider_reported"
        gemini = family == FAMILIES[3]
        assert (usage["tokens_in"], usage["cache_read_tokens"], usage["cache_creation_tokens"], usage["tokens_out"],
                usage["reasoning_tokens"], usage["reasoning_included"]) == ((4, 2, 0, 2, 1, False) if gemini else (2, 2, 1, 2, 1, True))
        assert charge == (7 if gemini else 5)
        instance.stop()
        with sqlite3.connect(instance.database) as connection:
            assert connection.execute("SELECT charged_tokens FROM budget_settlement_outbox").fetchall() == [(charge,)]
            assert connection.execute("SELECT scope,actual,settled FROM budget_reservation ORDER BY scope").fetchall() == sorted([
                ("group:protocol-team", charge, 1), (context["upstream_budget_scope"], charge, 1)])
            assert connection.execute("SELECT count(*) FROM usage_events").fetchone() == (1,)
            assert connection.execute("SELECT context_json FROM budget_execution_context").fetchone() == (row[1],)


@pytest.mark.parametrize("client_flavor", ["openai", "anthropic"])
@pytest.mark.parametrize("failure", ["missing_usage", "receipt_write"])
def test_tracked_ambiguous_accounting_suppresses_default_sdk_redispatch(
    tmp_path, backend_binaries, qualified_origin, client_flavor, failure,
):
    origin, calls, control = qualified_origin
    if failure == "missing_usage":
        control["mode"] = "missing"
    with tracked_gateway(tmp_path, backend_binaries, origin, FAMILIES[0]) as (instance, token):
        if failure == "receipt_write":
            with sqlite3.connect(instance.database) as connection:
                connection.execute("CREATE TRIGGER fail_sdk_receipt BEFORE INSERT ON budget_settlement_outbox BEGIN SELECT RAISE(ABORT,'synthetic failure'); END")
        # Keep each SDK's normal retry defaults. Explicitly disabling retries
        # here would hide duplicate model calls caused by a 502 status.
        if client_flavor == "openai":
            client = openai.OpenAI(base_url=instance.base + "/v1", api_key=token)
            error_type = openai.APIStatusError
            submit = lambda: client.chat.completions.create(model="agent", max_tokens=64,
                messages=[{"role": "user", "content": "hello"}])
        else:
            import anthropic
            client = anthropic.Anthropic(base_url=instance.base, api_key=token)
            error_type = anthropic.APIStatusError
            submit = lambda: client.messages.create(model="agent", max_tokens=64,
                messages=[{"role": "user", "content": "hello"}])
        try:
            with pytest.raises(error_type) as caught:
                submit()
            assert caught.value.status_code == 502
            assert caught.value.response.headers["x-should-retry"] == "false"
        finally:
            client.close()
        assert len(calls) == 1, "default SDK retries must not create another model request"
        with sqlite3.connect(instance.database) as connection:
            assert connection.execute("SELECT count(*) FROM budget_execution_intent").fetchone() == (1,)
            assert connection.execute("SELECT count(*) FROM budget_physical_attempt").fetchone() == (1,)
            assert connection.execute("SELECT count(*) FROM budget_settlement_outbox").fetchone() == (0,)
            assert connection.execute("SELECT settled FROM budget_reservation").fetchall() == [(0,), (0,)]
        if failure == "missing_usage":
            instance.process.terminate()
            assert instance.process.wait(timeout=5) == 124
        else:
            instance.process.kill()
            assert instance.process.wait(timeout=3) < 0
            instance.stop()
            with sqlite3.connect(instance.database) as connection:
                connection.execute("DROP TRIGGER fail_sdk_receipt")
            instance.start(SANDHI_BUFFERED_ACCOUNTING="tracked", SANDHI_CACHE_READ_WEIGHT="1000")
            deadline = time.monotonic() + 5
            while True:
                with sqlite3.connect(instance.database) as connection:
                    receipts = connection.execute("SELECT charged_tokens FROM budget_settlement_outbox").fetchall()
                if receipts == [(5,)]:
                    break
                assert time.monotonic() < deadline
                time.sleep(.02)
            assert len(calls) == 1, "restart retries accounting only"


@pytest.mark.parametrize("family", FAMILIES[1:])
def test_tracked_protocol_receipt_failure_recovers_original_attempt_and_weight_without_redispatch(
    tmp_path, backend_binaries, qualified_origin, family,
):
    origin, calls, _ = qualified_origin
    with tracked_gateway(tmp_path, backend_binaries, origin, family) as (instance, token):
        with sqlite3.connect(instance.database) as connection:
            connection.execute("CREATE TRIGGER fail_receipt BEFORE INSERT ON budget_settlement_outbox BEGIN SELECT RAISE(ABORT,'synthetic failure'); END")
        response = httpx.post(instance.base + "/v1/chat/completions", headers={"authorization": "Bearer " + token},
            json={"model": "agent", "max_tokens": 64, "messages": [{"role": "user", "content": "hello"}]})
        assert response.status_code == 502, response.text
        original, context, _, charge = evidence(instance.database)
        with sqlite3.connect(instance.database) as connection:
            assert connection.execute("SELECT settled,actual FROM budget_reservation").fetchall() == [(0, 0), (0, 0)]
        instance.process.kill()
        assert instance.process.wait(timeout=3) < 0
        instance.stop()
        with sqlite3.connect(instance.database) as connection:
            connection.execute("DROP TRIGGER fail_receipt")
        instance.start(SANDHI_BUFFERED_ACCOUNTING="tracked", SANDHI_CACHE_READ_WEIGHT="10000")
        deadline = time.monotonic() + 5
        while True:
            with sqlite3.connect(instance.database) as connection:
                receipts = connection.execute("SELECT charged_tokens FROM budget_settlement_outbox").fetchall()
            if receipts == [(charge,)]:
                break
            assert time.monotonic() < deadline
            time.sleep(.02)
        assert len(calls) == 1
        assert evidence(instance.database)[0] == original
        instance.stop()
        with sqlite3.connect(instance.database) as connection:
            assert connection.execute("SELECT scope,actual,settled FROM budget_reservation ORDER BY scope").fetchall() == sorted([
                ("group:protocol-team", charge, 1), (context["upstream_budget_scope"], charge, 1)])


@pytest.mark.parametrize("family", FAMILIES[1:])
@pytest.mark.parametrize("mode", ["missing", "malformed", "ambiguous"])
def test_tracked_protocol_unqualified_usage_retains_both_liabilities(tmp_path, backend_binaries, qualified_origin, family, mode):
    origin, calls, control = qualified_origin
    control["mode"] = mode
    with tracked_gateway(tmp_path, backend_binaries, origin, family) as (instance, token):
        response = httpx.post(instance.base + "/v1/chat/completions", headers={"authorization": "Bearer " + token},
            json={"model": "agent", "max_tokens": 64, "messages": [{"role": "user", "content": "hello"}]})
        assert response.status_code == 502
        _, _, usage, charge = evidence(instance.database, outcome="transport_error" if mode == "ambiguous" else "success")
        assert usage["completeness"] == "unavailable" and charge is None
        with sqlite3.connect(instance.database) as connection:
            assert connection.execute("SELECT count(*) FROM budget_settlement_outbox").fetchone() == (0,)
            assert connection.execute("SELECT settled,actual FROM budget_reservation").fetchall() == [(0, 0), (0, 0)]
        assert len(calls) == 1
        instance.process.terminate()
        assert instance.process.wait(timeout=5) == 124


@pytest.mark.parametrize("family", FAMILIES[1:])
def test_tracked_protocol_sigkill_after_acceptance_preserves_unknown_attempt_and_both_leases(
    tmp_path, backend_binaries, qualified_origin, family,
):
    origin, calls, control = qualified_origin
    control["release"].clear()
    outcomes = []
    with tracked_gateway(tmp_path, backend_binaries, origin, family) as (instance, token):
        def send():
            try:
                outcomes.append(httpx.post(instance.base + "/v1/chat/completions", timeout=10,
                    headers={"authorization": "Bearer " + token},
                    json={"model": "agent", "max_tokens": 64, "messages": [{"role": "user", "content": "hello"}]}))
            except httpx.TransportError as error:
                outcomes.append(error)
        thread = threading.Thread(target=send, daemon=True)
        thread.start()
        try:
            assert control["entered"].wait(5)
            with sqlite3.connect(instance.database) as connection:
                original = connection.execute("SELECT * FROM budget_physical_attempt").fetchall()
                context_json, = connection.execute("SELECT context_json FROM budget_execution_context").fetchone()
                original_leases = connection.execute("SELECT * FROM budget_reservation ORDER BY rowid").fetchall()
                assert len(original) == 1 and original[0][3] is None
                assert connection.execute("SELECT count(*) FROM budget_terminal_observation").fetchone() == (0,)
                assert connection.execute("SELECT actual,settled FROM budget_reservation").fetchall() == [(0, 0), (0, 0)]
            instance.process.kill()
            assert instance.process.wait(timeout=3) < 0
        finally:
            control["release"].set()
            thread.join(timeout=5)
            assert not thread.is_alive()
        assert len(outcomes) == 1 and isinstance(outcomes[0], httpx.TransportError)
        instance.stop()
        instance.start(SANDHI_BUFFERED_ACCOUNTING="tracked", SANDHI_SHUTDOWN_GRACE_SECS="1")
        context = json.loads(context_json)
        for scope in ("group:protocol-team", context["upstream_budget_scope"]):
            assert instance.admin("POST", "/admin/budget", json={"scope": scope, "limit_tokens": 1,
                "window": "total", "policy": "block"}).status_code == 200
        denied = httpx.post(instance.base + "/v1/chat/completions", headers={"authorization": "Bearer " + token},
            json={"model": "agent", "max_tokens": 64, "messages": [{"role": "user", "content": "held capacity"}]})
        assert denied.status_code == 429
        instance.process.terminate()
        assert instance.process.wait(timeout=5) == 124
        with sqlite3.connect(instance.database) as connection:
            assert connection.execute("SELECT * FROM budget_physical_attempt").fetchall() == original
            assert connection.execute("SELECT context_json FROM budget_execution_context").fetchone() == (context_json,)
            assert connection.execute("SELECT * FROM budget_reservation ORDER BY rowid").fetchall() == original_leases
            assert connection.execute("SELECT count(*) FROM budget_settlement_outbox").fetchone() == (0,)
        assert len(calls) == 1


def test_tracked_async_responses_refuses_before_admission(tmp_path, backend_binaries, qualified_origin):
    origin, calls, _ = qualified_origin
    with tracked_gateway(tmp_path, backend_binaries, origin, "openai_responses") as (instance, token):
        token = instance.key("backend/primary")
        response = httpx.post(instance.base + "/v1/responses", headers={"authorization": "Bearer " + token},
            json={"model": "fixture", "max_output_tokens": 64, "input": "hello", "background": True})
        assert response.status_code == 400, response.text
        assert calls == []
        with sqlite3.connect(instance.database) as connection:
            for table in ("budget_reservation", "budget_execution_intent", "budget_physical_attempt"):
                assert connection.execute(f"SELECT count(*) FROM {table}").fetchone() == (0,)


def test_tracked_route_selects_accounting_eligible_target_before_pinning(tmp_path, backend_binaries, qualified_origin):
    origin, calls, _ = qualified_origin
    with tracked_gateway(tmp_path, backend_binaries, origin, "openai_responses") as (instance, token):
        instance.cli("set", "unqualified", "--provider", "other-models", "--credential-ref", "openai:default",
                     "--endpoint-family", "cohere_chat", "--model", "fixture")
        definition = {"id": "protocols", "models": [{"id": "agent", "targets": [
            {"backend": "unqualified", "model": "fixture"}, {"backend": "primary", "model": "fixture"}]}]}
        assert instance.admin("POST", "/admin/routes", json={"definition": definition, "expected_revision": 1}).status_code == 201
        assert sdk_call(instance, token, FAMILIES[0]) == "reply"
        assert len(calls) == 1 and calls[0][0] == "/responses"
        assert evidence(instance.database)[1]["backend"]["id"] == "primary"


@pytest.mark.parametrize("mode", ["select_translation", "existing_pin", "unbounded"])
def test_tracked_responses_stream_preflights_delivery_before_pinning(
    tmp_path, backend_binaries, qualified_origin, mode,
):
    origin, calls, _ = qualified_origin
    with tracked_gateway(tmp_path, backend_binaries, origin, FAMILIES[2], streaming=mode != "unbounded") as (instance, token):
        instance.cli("set", "chat", "--provider", "other-models", "--credential-ref", "openai:default",
                     "--endpoint-family", FAMILIES[0], "--model", "fixture")
        definition = {"id": "protocols", "models": [{"id": "agent", "targets": [
            {"backend": "chat", "model": "fixture"}, {"backend": "primary", "model": "fixture"}]}]}
        assert instance.admin("POST", "/admin/routes", json={"definition": definition, "expected_revision": 1}).status_code == 201
        if mode == "existing_pin":
            assert sdk_call(instance, token, FAMILIES[2]) == "reply"
            assert len(calls) == 1 and calls[0][0] == "/chat/completions"
        with sqlite3.connect(instance.database) as connection:
            pins = connection.execute("SELECT * FROM route_affinity").fetchall()
            leases = connection.execute("SELECT * FROM budget_reservation ORDER BY rowid").fetchall()
        with instance.client(token) as client:
            def submit():
                return list(client.responses.create(model="agent", input="hello", max_output_tokens=64, stream=True,
                    extra_headers={"x-sandhi-session": "protocol-session"}))
            if mode != "unbounded":
                events = submit()
                assert events[-1].type == "response.completed"
                assert len(calls) == (2 if mode == "existing_pin" else 1)
                assert all(call[0] == "/chat/completions" for call in calls)
                with sqlite3.connect(instance.database) as connection:
                    contexts = [json.loads(row[0]) for row in connection.execute("SELECT context_json FROM budget_execution_context")]
                    streamed = [context for context in contexts if context["delivery"] == "streaming"]
                    assert len(streamed) == 1
                    assert streamed[0]["backend"]["id"] == "chat"
                    assert streamed[0]["plane"] == "translation"
                    assert connection.execute("SELECT count(*) FROM budget_settlement_outbox").fetchone() == (len(calls),)
            else:
                with pytest.raises(openai.APIStatusError) as refused:
                    submit()
                assert refused.value.status_code == 409
                assert calls == []
                with sqlite3.connect(instance.database) as connection:
                    assert connection.execute("SELECT * FROM route_affinity").fetchall() == pins
                    assert connection.execute("SELECT * FROM budget_reservation ORDER BY rowid").fetchall() == leases


def test_tracked_async_route_refuses_without_creating_a_pin_or_lease(tmp_path, backend_binaries, qualified_origin):
    origin, calls, _ = qualified_origin
    with tracked_gateway(tmp_path, backend_binaries, origin, "openai_responses") as (instance, token):
        response = httpx.post(instance.base + "/v1/responses", headers={"authorization": "Bearer " + token},
            json={"model": "agent", "max_output_tokens": 64, "input": "hello", "background": True})
        assert response.status_code == 409, response.text
        assert calls == []
        with sqlite3.connect(instance.database) as connection:
            for table in ("route_affinity", "budget_reservation", "budget_execution_intent", "budget_physical_attempt"):
                assert connection.execute(f"SELECT count(*) FROM {table}").fetchone() == (0,)


def test_existing_unqualified_pin_refuses_after_accounting_mode_change_without_switching(tmp_path, backend_binaries, qualified_origin):
    origin, calls, _ = qualified_origin
    with tracked_gateway(tmp_path, backend_binaries, origin, "openai_responses") as (instance, token):
        instance.cli("set", "unqualified", "--provider", "other-models", "--credential-ref", "openai:default",
                     "--endpoint-family", "cohere_chat", "--model", "fixture")
        definition = {"id": "protocols", "models": [{"id": "agent", "targets": [
            {"backend": "unqualified", "model": "fixture"}, {"backend": "primary", "model": "fixture"}]}]}
        assert instance.admin("POST", "/admin/routes", json={"definition": definition, "expected_revision": 1}).status_code == 201
        instance.stop()
        instance.start()  # Older/default accounting can use the Cohere target.
        assert sdk_call(instance, token, FAMILIES[0]) == "reply"
        assert len(calls) == 1 and calls[0][0] == "/v2/chat"
        instance.stop()
        with sqlite3.connect(instance.database) as connection:
            pin = connection.execute("SELECT * FROM route_affinity").fetchall()
            leases = connection.execute("SELECT * FROM budget_reservation ORDER BY rowid").fetchall()
            assert len(pin) == 1
        instance.start(SANDHI_BUFFERED_ACCOUNTING="tracked")
        with pytest.raises(openai.APIStatusError) as refused:
            sdk_call(instance, token, FAMILIES[0])
        assert refused.value.status_code == 409
        assert len(calls) == 1, "an existing session must not switch to a different backend"
        with sqlite3.connect(instance.database) as connection:
            assert connection.execute("SELECT * FROM route_affinity").fetchall() == pin
            assert connection.execute("SELECT * FROM budget_reservation ORDER BY rowid").fetchall() == leases
            assert connection.execute("SELECT count(*) FROM budget_execution_intent").fetchone() == (0,)


@pytest.mark.parametrize("family", FAMILIES[:3])
def test_live_report_verifier_checks_actual_sql_join_and_detects_corrupted_evidence(
    tmp_path, backend_binaries, qualified_origin, family,
):
    import copy
    import importlib.util
    from conftest import REPO_ROOT
    spec = importlib.util.spec_from_file_location("qualification_harness", REPO_ROOT / "scripts/qualify_zai_router.py")
    harness = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(harness)
    origin, calls, _ = qualified_origin
    with tracked_gateway(tmp_path, backend_binaries, origin, family) as (instance, token):
        response = httpx.post(instance.base + "/v1/chat/completions", headers={"authorization": "Bearer " + token,
            "x-sandhi-step-id": "qualification-cell"}, json={"model": "agent", "max_tokens": 64,
            "messages": [{"role": "user", "content": "hello"}]})
        assert response.status_code == 200 and len(calls) == 1
        instance.stop()
        with sqlite3.connect(instance.database) as connection:
            connection.row_factory = sqlite3.Row
            executions, inventory = harness.tracked_evidence(connection)
            events = [dict(row) for row in connection.execute("SELECT * FROM usage_events")]
        report = {"accounting": "tracked", "delivery": "buffered", "model": "fixture", "logical_route": False,
            "cache_policy": "implicit_only / auto", "gateway_exit_code": 0,
            "cells": [{"step_id": "qualification-cell", "backend": {FAMILIES[0]: "chat", FAMILIES[1]: "messages", FAMILIES[2]: "responses"}[family],
                       "client": "chat", "status": "ok", "stream": False, "usage": [response.json()["usage"]]}],
            "usage_events": events, "executions": executions, "accounting_inventory": inventory}
        assert harness.verify_report(report)["errors"] == []
        assert harness.verify_report(report)["tracked_evidence_matches"] == 1
        assert executions[0]["context"].get("transport_binding_sha256") is None
        assert "credential_ref" not in executions[0]["context"]["backend"]
        for field, value in [("charged_tokens", 99), ("upstream_actual", 0), ("upstream_settled", 0),
                             ("upstream_scope", "wrong-scope"), ("attempt_id", "different"), ("usage", None)]:
            broken = copy.deepcopy(report)
            broken["executions"][0][field] = value
            assert harness.verify_report(broken)["errors"], field
        broken = copy.deepcopy(report)
        broken["executions"][0]["context"]["plane"] = "invalid-plane"
        assert harness.verify_report(broken)["errors"]
        broken = copy.deepcopy(report)
        broken["accounting_inventory"].pop("budget_physical_attempt")
        assert harness.verify_report(broken)["errors"]


@pytest.mark.parametrize("profile", ["standard", "anthropic_implicit_cache"])
@pytest.mark.parametrize("ingress", FAMILIES)
def test_messages_implicit_write_profile_is_explicit_on_both_planes(tmp_path, backend_binaries, qualified_origin, profile, ingress):
    origin, calls, control = qualified_origin
    control["mode"] = "no_writes"
    with tracked_gateway(tmp_path, backend_binaries, origin, "anthropic_messages", endpoint_profile=profile) as (instance, token):
        if profile == "standard":
            with pytest.raises(Exception) as refused:
                sdk_call(instance, token, ingress)
            assert getattr(refused.value, "status_code", getattr(refused.value, "code", None)) == 502
            _, context, usage, charge = evidence(instance.database)
            assert usage["completeness"] == "unavailable" and charge is None
            assert usage["outcome"] == 'buffered_qualification:InvalidField("cache_creation_input_tokens")'
            instance.process.terminate()
            assert instance.process.wait(timeout=5) == 124
        else:
            assert sdk_call(instance, token, ingress) == "reply"
            _, context, usage, charge = evidence(instance.database)
            assert (usage["tokens_in"], usage["cache_read_tokens"], usage["cache_creation_tokens"], usage["tokens_out"]) == (2,2,0,2)
            assert charge == 4
        assert context["backend"]["endpoint_profile"] == profile
        assert len(calls) == 1


def test_messages_usage_profile_survives_restart_and_revision_change_fences_the_pin(tmp_path, backend_binaries, qualified_origin):
    origin, calls, control = qualified_origin
    control["mode"] = "no_writes"
    with tracked_gateway(tmp_path, backend_binaries, origin, "anthropic_messages", endpoint_profile="anthropic_implicit_cache") as (instance, token):
        assert sdk_call(instance, token, FAMILIES[0]) == "reply"
        first = evidence(instance.database)[0]
        instance.stop()
        instance.start(SANDHI_BUFFERED_ACCOUNTING="tracked", SANDHI_CACHE_READ_WEIGHT="1000")
        assert sdk_call(instance, token, FAMILIES[0]) == "reply"
        instance.cli("set", "primary", "--provider", "models", "--credential-ref", "openai:default",
            "--endpoint-family", "anthropic_messages", "--endpoint-profile", "standard", "--model", "fixture", "--revision", "1")
        with pytest.raises(openai.APIStatusError) as refused:
            sdk_call(instance, token, FAMILIES[0])
        assert refused.value.status_code == 409
        assert len(calls) == 2
        assert evidence(instance.database)[0] == first


@pytest.mark.parametrize("mode", ["valid", "incomplete"])
@pytest.mark.parametrize("family,profile", [(FAMILIES[0], "standard"), (FAMILIES[0], "openai_terminal_chunk_usage"), (FAMILIES[2], "standard"),
                                         (FAMILIES[1], "standard"), (FAMILIES[1], "anthropic_implicit_cache"), (FAMILIES[3], "standard")])
@pytest.mark.parametrize("ingress", FAMILIES)
def test_tracked_stream_real_sdks_translate_and_settle_source_counts(
    tmp_path, backend_binaries, qualified_origin, family, profile, ingress, mode,
):
    origin, calls, control = qualified_origin
    control["mode"] = mode
    control["terminal_chunk_usage"] = profile == "openai_terminal_chunk_usage"
    control["implicit_stream_writes"] = profile == "anthropic_implicit_cache"
    written = 0 if profile == "anthropic_implicit_cache" or family == FAMILIES[3] else 1
    inclusive_input = 4 + written
    charged = 4 + written + (1 if family == FAMILIES[3] else 0)
    inclusive_output = 3 if family == FAMILIES[3] else 2
    with tracked_gateway(tmp_path, backend_binaries, origin, family, endpoint_profile=profile, streaming=True) as (instance, token):
        headers = {"x-sandhi-session": "stream-protocol-session"}
        if ingress in (FAMILIES[0], FAMILIES[2]):
            with openai.OpenAI(base_url=instance.base + "/v1", api_key=token, max_retries=0,
                               default_headers=headers) as client:
                if ingress == FAMILIES[0]:
                    events = list(client.chat.completions.create(model="agent", max_tokens=64,
                        messages=[{"role": "user", "content": "hello"}], stream=True))
                    text = "".join(event.choices[0].delta.content or "" for event in events if event.choices)
                    assert events[-1].usage is not None
                    assert events[-1].usage.prompt_tokens == inclusive_input
                    assert events[-1].usage.completion_tokens == inclusive_output
                    assert any(event.choices[0].finish_reason == ("length" if mode == "incomplete" else "stop") for event in events if event.choices)
                else:
                    events = list(client.responses.create(model="agent", max_output_tokens=64,
                        input="hello", stream=True))
                    text = "".join(event.delta for event in events if event.type == "response.output_text.delta")
                    assert events[-1].type == ("response.incomplete" if mode == "incomplete" else "response.completed")
                    assert events[-1].response.status == ("incomplete" if mode == "incomplete" else "completed")
                    assert events[-1].response.usage.input_tokens == inclusive_input
                    assert events[-1].response.usage.output_tokens == inclusive_output
        elif ingress == FAMILIES[1]:
            import anthropic
            with anthropic.Anthropic(base_url=instance.base, api_key=token, max_retries=0,
                                     default_headers=headers) as client:
                with client.messages.stream(model="agent", max_tokens=64,
                        messages=[{"role": "user", "content": "hello"}]) as stream:
                    text = "".join(stream.text_stream)
                    final = stream.get_final_message()
                    assert final.stop_reason == ("max_tokens" if mode == "incomplete" else "end_turn")
                    assert final.usage.output_tokens == inclusive_output
                    assert final.usage.input_tokens == 2
                    assert final.usage.cache_read_input_tokens == 2
                    assert final.usage.cache_creation_input_tokens == (None if family == FAMILIES[1] and not written else written)
        else:
            from google import genai
            from google.genai import types
            with genai.Client(api_key=token, http_options=types.HttpOptions(base_url=instance.base,
                    headers=headers, retry_options=types.HttpRetryOptions(attempts=1))) as client:
                events = list(client.models.generate_content_stream(
                    model="agent", contents="hello", config=types.GenerateContentConfig(max_output_tokens=64)))
                text = "".join(event.text or "" for event in events)
                assert events[-1].candidates[0].finish_reason == ("MAX_TOKENS" if mode == "incomplete" else "STOP")
                assert events[-1].usage_metadata.total_token_count == inclusive_input + inclusive_output
                assert events[-1].usage_metadata.thoughts_token_count == 1
        assert text == "reply"
        # Chat SDKs stop at [DONE], before the body owner's bounded settlement
        # wait necessarily finishes. Join the actual receipt rather than racing it.
        deadline = time.monotonic() + 5
        while True:
            with sqlite3.connect(instance.database) as connection:
                receipts = connection.execute("SELECT count(*) FROM budget_settlement_outbox").fetchone()[0]
            if receipts == 1:
                break
            assert time.monotonic() < deadline, "stream owner did not commit the paired receipt"
            time.sleep(.02)
        assert len(calls) == 1
        _, context, usage, charge = evidence(instance.database)
        assert context["ingress"] == ingress and context["endpoint_family"] == family
        assert context["delivery"] == "streaming"
        assert context["backend"]["endpoint_profile"] == profile
        assert context["plane"] == ("transparent" if ingress == family else "translation")
        assert usage["completeness"] == "final" and usage["reasoning_tokens"] == 1
        assert (usage["tokens_in"], usage["cache_read_tokens"], usage["cache_creation_tokens"], usage["tokens_out"]) == (2, 2, written, 2)
        assert charge == charged
        with sqlite3.connect(instance.database) as connection:
            assert connection.execute("SELECT charged_tokens FROM budget_settlement_outbox").fetchall() == [(charged,)]
            assert connection.execute("SELECT settled,actual FROM budget_reservation").fetchall() == [(1, charged)] * 2


def test_terminal_chat_profile_survives_restart_and_profile_change_fences_pin(
    tmp_path, backend_binaries, qualified_origin,
):
    origin, calls, control = qualified_origin
    control["terminal_chunk_usage"] = True
    with tracked_gateway(tmp_path, backend_binaries, origin, FAMILIES[0],
            endpoint_profile="openai_terminal_chunk_usage", streaming=True) as (instance, token):
        def submit():
            with instance.client(token) as client:
                events = list(client.responses.create(model="agent", input="hello", max_output_tokens=64,
                    stream=True, extra_headers={"x-sandhi-session": "terminal-profile-session"}))
                assert events[-1].type == "response.completed"
                assert events[-1].response.usage.input_tokens == 5
                assert events[-1].response.usage.output_tokens == 2
        submit()
        first = evidence(instance.database)[0]
        instance.stop()
        instance.start(SANDHI_BUFFERED_ACCOUNTING="tracked", SANDHI_CACHE_READ_WEIGHT="1000")
        submit()
        instance.cli("set", "primary", "--provider", "models", "--credential-ref", "openai:default",
            "--endpoint-family", FAMILIES[0], "--endpoint-profile", "standard", "--model", "fixture", "--revision", "1")
        with pytest.raises(openai.APIStatusError) as refused:
            submit()
        assert refused.value.status_code == 409
        assert len(calls) == 2
        assert evidence(instance.database)[0] == first


def test_messages_stream_profile_survives_restart_and_profile_change_fences_pin(
    tmp_path, backend_binaries, qualified_origin,
):
    origin, calls, control = qualified_origin
    control["implicit_stream_writes"] = True
    with tracked_gateway(tmp_path, backend_binaries, origin, FAMILIES[1],
            endpoint_profile="anthropic_implicit_cache", streaming=True) as (instance, token):
        def submit():
            with instance.client(token) as client:
                events = list(client.responses.create(model="agent", input="hello", max_output_tokens=64,
                    stream=True, extra_headers={"x-sandhi-session": "messages-stream-profile-session"}))
                assert events[-1].type == "response.completed"
                assert events[-1].response.usage.input_tokens == 4
                assert events[-1].response.usage.output_tokens == 2
        submit()
        first = evidence(instance.database)[0]
        instance.stop()
        instance.start(SANDHI_BUFFERED_ACCOUNTING="tracked", SANDHI_CACHE_READ_WEIGHT="1000")
        submit()
        instance.cli("set", "primary", "--provider", "models", "--credential-ref", "openai:default",
            "--endpoint-family", FAMILIES[1], "--endpoint-profile", "standard", "--model", "fixture", "--revision", "1")
        with pytest.raises(openai.APIStatusError) as refused:
            submit()
        assert refused.value.status_code == 409
        assert len(calls) == 2
        assert evidence(instance.database)[0] == first


@pytest.mark.parametrize("ingress", ["chat", "messages", "responses", "gemini"])
def test_explicit_thinking_disable_reaches_messages_from_each_real_sdk(
    tmp_path, backend_binaries, qualified_origin, ingress,
):
    import importlib.util
    from conftest import REPO_ROOT
    spec = importlib.util.spec_from_file_location("thinking_harness", REPO_ROOT / "scripts/qualify_zai_router.py")
    harness = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(harness)
    origin, calls, _ = qualified_origin
    with tracked_gateway(tmp_path, backend_binaries, origin, FAMILIES[1], streaming=True) as (instance, token):
        result = harness.infer(ingress, instance.base, token, "agent", "hello", True, "thinking-intent", "disabled")
        assert result["text"] == "reply"
        assert len(calls) == 1
        assert calls[0][1]["thinking"] == {"type": "disabled"}


def test_gemini_default_sdk_stream_refusal_has_no_type_error_or_inference_replay(
    tmp_path, backend_binaries, qualified_origin,
):
    from google import genai
    from google.genai import types, errors
    origin, calls, control = qualified_origin
    control["signed_output"] = True
    with tracked_gateway(tmp_path, backend_binaries, origin, FAMILIES[1], streaming=True) as (instance, token):
        with genai.Client(api_key=token, http_options=types.HttpOptions(base_url=instance.base)) as client:
            with pytest.raises((errors.APIError, httpx.RemoteProtocolError)) as failed:
                list(client.models.generate_content_stream(model="agent", contents="hello",
                    config=types.GenerateContentConfig(max_output_tokens=64)))
        if isinstance(failed.value, errors.APIError):
            assert failed.value.code == 502
        assert len(calls) == 1, "default SDK must not replay a dispatched inference on a stream projection error"


@pytest.mark.parametrize("mode", ["missing", "progress_only"])
def test_gemini_source_partial_usage_survives_restart_without_model_replay(
    tmp_path, backend_binaries, qualified_origin, mode,
):
    from google import genai
    from google.genai import types, errors
    origin, calls, control = qualified_origin
    control["mode"] = mode
    with tracked_gateway(tmp_path, backend_binaries, origin, FAMILIES[3], streaming=True) as (instance, token):
        with genai.Client(api_key=token, http_options=types.HttpOptions(base_url=instance.base,
                headers={"x-sandhi-session": "gemini-recovery-session"})) as client:
            with pytest.raises((errors.APIError, httpx.RemoteProtocolError)):
                list(client.models.generate_content_stream(model="agent", contents="hello",
                    config=types.GenerateContentConfig(max_output_tokens=64)))
        assert len(calls) == 1
        deadline = time.monotonic() + 5
        while True:
            with sqlite3.connect(instance.database) as connection:
                count = connection.execute("SELECT count(*) FROM budget_terminal_observation").fetchone()[0]
            if count:
                break
            assert time.monotonic() < deadline
            time.sleep(.02)
        original, context, usage, frozen_charge = evidence(instance.database, outcome="incomplete_stream")
        assert context["endpoint_family"] == FAMILIES[3]
        # A policy valuation is frozen for reconciliation even for partial
        # evidence. Only a committed receipt/settled lease authorizes a debit.
        assert usage["completeness"] == "partial" and frozen_charge == 4
        assert (usage["tokens_in"], usage["cache_read_tokens"], usage["tokens_out"], usage["reasoning_tokens"]) == (2, 2, 1, 1)
        assert usage["reasoning_included"] is False
        instance.process.terminate()
        assert instance.process.wait(timeout=5) == 124
        instance.stop()
        instance.start(SANDHI_BUFFERED_ACCOUNTING="tracked", SANDHI_CACHE_READ_WEIGHT="1000", SANDHI_SHUTDOWN_GRACE_SECS="1")
        assert evidence(instance.database, outcome="incomplete_stream")[0] == original
        assert len(calls) == 1, "restart/reconciliation cannot execute an inference"
        with sqlite3.connect(instance.database) as connection:
            assert connection.execute("SELECT count(*) FROM budget_settlement_outbox").fetchone() == (0,)
            assert connection.execute("SELECT settled,actual FROM budget_reservation").fetchall() == [(0, 0)] * 2
            assert connection.execute("SELECT count(*) FROM usage_events").fetchone() == (1,)
        instance.process.terminate()
        assert instance.process.wait(timeout=5) == 124
