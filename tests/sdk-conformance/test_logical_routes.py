"""Logical routes through real SDKs, CLI, durable config and process restart."""
import json
import sqlite3
import subprocess
import time

import anthropic
from google import genai
from google.genai import types
import openai
import pytest

from test_backend_registry import ADMIN, backend_binaries, backend_origin, gateway  # noqa: F401


def route_cli(instance, *arguments):
    result = subprocess.run([str(instance.binaries / "sandhi"), "--admin-url", instance.base,
                             "--admin-token", ADMIN, "routes", *arguments],
                            capture_output=True, text=True, timeout=15)
    assert result.returncode == 0, result.stderr
    return json.loads(result.stdout)


def test_sdk_aliases_persisted_affinity_operator_changes_and_rotation(tmp_path, backend_binaries, backend_origin):
    origin, received = backend_origin
    definition = {"id": "coding", "models": [{"id": "agent", "targets": [
        {"backend": "primary", "model": "fixture"}, {"backend": "secondary", "model": "fixture"}]}]}
    path = tmp_path / "route.json"
    path.write_text(json.dumps(definition))
    with gateway(tmp_path, backend_binaries) as instance:
        instance.provision(origin, "route-original-fixture")
        for backend in ("primary", "secondary"):
            instance.cli("set", backend, "--provider", "models", "--credential-ref", "openai:default",
                         "--endpoint-family", "openai_responses", "--model", "fixture",
                         "--user-agent", "logical-" + backend)
        assert route_cli(instance, "set", str(path))["revision"] == 1
        minted = instance.admin("POST", "/admin/keys/share", json={"upstream": "route/coding", "models": ["agent"]})
        assert minted.status_code == 200, minted.text
        token = minted.json()["virtual_key"]
        headers = {"x-sandhi-session": "sdk-route-session"}
        for stream in (False, True):
            with openai.OpenAI(base_url=instance.base + "/v1", api_key=token, max_retries=0, default_headers=headers) as client:
                result = client.chat.completions.create(model="agent", max_tokens=64,
                    messages=[{"role": "user", "content": "hello"}], stream=stream)
                text = "".join(chunk.choices[0].delta.content or "" for chunk in result if chunk.choices) if stream else result.choices[0].message.content
                assert text == "reply"
                result = client.responses.create(model="agent", input="hello", max_output_tokens=64, stream=stream)
                text = "".join(event.delta for event in result if event.type == "response.output_text.delta") if stream else result.output_text
                assert text == "reply"
                assert [model.id for model in client.models.list().data] == ["agent"]
            with anthropic.Anthropic(base_url=instance.base, api_key=token, max_retries=0, default_headers=headers) as client:
                result = client.messages.create(model="agent", max_tokens=64,
                    messages=[{"role": "user", "content": "hello"}], stream=stream)
                text = "".join(event.delta.text for event in result if event.type == "content_block_delta" and event.delta.type == "text_delta") if stream else result.content[0].text
                assert text == "reply"
            with genai.Client(api_key=token, http_options=types.HttpOptions(base_url=instance.base, headers=headers)) as client:
                result = (client.models.generate_content_stream if stream else client.models.generate_content)(
                    model="agent", contents="hello", config=types.GenerateContentConfig(max_output_tokens=64))
                text = "".join(chunk.text or "" for chunk in result) if stream else result.text
                assert text == "reply"
                assert [model.name for model in client.models.list()] == ["models/agent"]
        assert len(received) == 8
        assert all(body["model"] == "fixture" and sent_headers["user-agent"] == "logical-primary"
                   and sent_headers["authorization"] == "Bearer route-original-fixture"
                   for _, sent_headers, body in received)
        assert token not in json.dumps(received)
        definition["models"][0]["targets"].reverse()
        path.write_text(json.dumps(definition))
        assert route_cli(instance, "set", str(path), "--revision", "1")["revision"] == 2
        instance.stop()
        instance.start()
        with openai.OpenAI(base_url=instance.base + "/v1", api_key=token, max_retries=0) as client:
            # The active session keeps its original compatible backend despite reordered priority.
            assert client.responses.create(model="agent", input="next turn", extra_headers=headers).output_text == "reply"
            assert received[-1][1]["user-agent"] == "logical-primary"
            assert client.responses.create(model="agent", input="new session",
                extra_headers={"x-sandhi-session": "new-route-session"}).output_text == "reply"
            assert received[-1][1]["user-agent"] == "logical-secondary"
            denied = instance.admin("POST", "/v1/responses", json={"model": "fixture", "input": "forbidden"})
            assert denied.status_code in (401, 403)  # admin authority is not an inference key
        instance.provision(origin, "route-rotated-fixture")
        before = len(received)
        import httpx
        response = httpx.post(instance.base + "/v1/responses", headers={"authorization": "Bearer " + token, **headers},
                              json={"model": "agent", "input": "old session"})
        assert response.status_code == 409, response.text
        assert len(received) == before
        with openai.OpenAI(base_url=instance.base + "/v1", api_key=token, max_retries=0) as client:
            assert client.responses.create(model="agent", input="new after rotation",
                extra_headers={"x-sandhi-session": "rotated-session"}).output_text == "reply"
        assert received[-1][1]["authorization"] == "Bearer route-rotated-fixture"
        assert route_cli(instance, "revoke", "coding", "--revision", "2")["status"] == "revoked"
        response = httpx.post(instance.base + "/v1/responses", headers={"authorization": "Bearer " + token},
                              json={"model": "agent", "input": "revoked"})
        assert response.status_code == 409
        assert len(received) == 11
        instance.stop()
        with sqlite3.connect(instance.database) as connection:
            rows = connection.execute("SELECT model,tokens_in,tokens_out,cache_read_tokens,cache_creation_tokens FROM usage_events").fetchall()
            assert len(rows) == 11 and all(row == ("fixture", 2, 2, 2, 1) for row in rows)
            assert connection.execute("SELECT COUNT(*) FROM route_affinity").fetchone()[0] == 3


def test_route_config_preflight_and_additive_apply(tmp_path, backend_binaries, backend_origin):
    origin, _ = backend_origin
    definition = {"id": "coding", "models": [{"id": "agent", "targets": [{"backend": "primary", "model": "fixture"}]}]}
    with gateway(tmp_path, backend_binaries) as instance:
        instance.provision(origin, "config-route-fixture")
        backend = {"id": "primary", "provider": "models", "credential_ref": "openai:default",
                   "endpoint": {"endpoint_family": "openai_responses"}, "models": [{"id": "fixture"}]}
        invalid = dict(definition, models=[{"id": "agent", "targets": [{"backend": "unknown", "model": "fixture"}]}])
        instance.config.write_text(json.dumps({"backends": [backend], "routes": [invalid]}))
        assert instance.admin("POST", "/admin/config/apply").status_code == 400
        assert instance.admin("GET", "/admin/backends").json()["backends"] == []
        instance.config.write_text(json.dumps({"backends": [backend], "routes": [definition],
            "vkeys": [{"upstream": "route/coding", "subject": "route-user", "models": ["agent"]}]}))
        preview = instance.admin("GET", "/admin/config").json()
        assert preview["routes"][0]["upstream_ref"] == "route/coding"
        applied = instance.admin("POST", "/admin/config/apply")
        assert applied.status_code == 200, applied.text
        token = applied.json()["vkeys"]["minted"][0]["virtual_key"]
        instance.config.write_text("{}")
        assert instance.admin("POST", "/admin/config/apply").status_code == 200
        instance.stop()
        instance.start()
        with openai.OpenAI(base_url=instance.base + "/v1", api_key=token, max_retries=0) as client:
            assert client.responses.create(model="agent", input="persisted").output_text == "reply"


def test_unreported_external_secret_change_cannot_reuse_a_session_after_restart(tmp_path, backend_binaries, backend_origin):
    import httpx
    origin, received = backend_origin
    with gateway(tmp_path, backend_binaries) as instance:
        instance.provision(origin, "unreported-original-fixture")
        instance.cli("set", "primary", "--provider", "models", "--credential-ref", "openai:default",
                     "--endpoint-family", "openai_responses", "--model", "fixture")
        definition = {"id": "coding", "models": [{"id": "agent", "targets": [{"backend": "primary", "model": "fixture"}]}]}
        assert instance.admin("POST", "/admin/routes", json={"definition": definition}).status_code == 201
        token = instance.admin("POST", "/admin/keys/share", json={"upstream": "route/coding", "models": ["agent"]}).json()["virtual_key"]
        headers = {"authorization": "Bearer " + token, "x-sandhi-session": "opaque-session"}
        response = httpx.post(instance.base + "/v1/responses", headers=headers, json={"model": "agent", "input": "first turn"})
        assert response.status_code == 200
        instance.stop()
        # External vault owner changed the secret without metadata registration.
        (instance.credentials / "openai__default").write_text("unreported-replacement-fixture")
        instance.start()
        assert instance.admin("GET", "/admin/keys").json()["keys"][0]["generation"] == 1
        before = len(received)
        response = httpx.post(instance.base + "/v1/responses", headers=headers,
            json={"model": "agent", "input": "continue", "previous_response_id": "resp_fixture"})
        assert response.status_code == 409, response.text
        assert len(received) == before
        response = httpx.post(instance.base + "/v1/responses", headers=dict(headers, **{"x-sandhi-session": "new-materialized-session"}),
                              json={"model": "agent", "input": "fresh turn"})
        assert response.status_code == 200, response.text
        assert received[-1][1]["authorization"] == "Bearer unreported-replacement-fixture"


def test_tracked_route_context_and_original_charge_survive_restart_and_rotation(tmp_path, backend_binaries, backend_origin):
    origin, received = backend_origin
    with gateway(tmp_path, backend_binaries) as instance:
        instance.stop()
        instance.start(SANDHI_BUFFERED_ACCOUNTING="tracked", SANDHI_CACHE_READ_WEIGHT="1000")
        instance.provision(origin, "context-original-fixture")
        policy = tmp_path / "cache.json"
        policy.write_text(json.dumps({"default_intent": {"mode": "auto"},
            "models": {"fixture": {"profile": "implicit_only"}}}))
        instance.cli("set", "primary", "--provider", "models", "--credential-ref", "openai:default",
                     "--endpoint-family", "openai_chat_completions", "--model", "fixture", "--cache-policy", str(policy))
        definition = {"id": "coding", "models": [{"id": "agent", "targets": [{"backend": "primary", "model": "fixture"}]}]}
        assert instance.admin("POST", "/admin/routes", json={"definition": definition}).status_code == 201
        for scope in ("group:context-team", "upstream:models"):
            assert instance.admin("POST", "/admin/budget", json={"scope": scope, "limit_tokens": 10000,
                "window": "total", "policy": "block"}).status_code == 200
        minted = instance.admin("POST", "/admin/keys/share", json={"upstream": "route/coding", "models": ["agent"], "group": "context-team"})
        assert minted.status_code == 200
        token = minted.json()["virtual_key"]
        with sqlite3.connect(instance.database) as connection:
            connection.execute("CREATE TRIGGER block_receipt BEFORE INSERT ON budget_settlement_outbox BEGIN SELECT RAISE(ABORT,'synthetic receipt failure'); END")
        with openai.OpenAI(base_url=instance.base + "/v1", api_key=token, max_retries=0) as client:
            with pytest.raises(openai.APIStatusError) as failed:
                client.chat.completions.create(model="agent", max_tokens=64, messages=[{"role": "user", "content": "first"}],
                    extra_headers={"x-sandhi-session": "original-context-session"})
            assert failed.value.status_code == 502
        assert len(received) == 1, "an accounting error must never replay inference"
        with sqlite3.connect(instance.database) as connection:
            execution, original = connection.execute("SELECT execution_id,context_json FROM budget_execution_context").fetchone()
            original_attempt = connection.execute("SELECT attempt_id,authorized_at,evidence_json FROM budget_physical_attempt WHERE execution_id=?", (execution,)).fetchone()
            identity, authorized_at, encoded = original_attempt
            evidence = json.loads(encoded)
            assert evidence["attempt_id"] == identity and evidence["attempt_ordinal"] == 1
            assert evidence["dispatch_observed_at_ms"] is not None
            assert evidence["terminal"]["outcome"] == "success"
            assert evidence["terminal"]["provider_status"] == 200
            assert evidence["dropped_observations"] == 0 and evidence["invalid"] is False
            assert connection.execute("SELECT attempt_version,attempt_id,transitioned_at FROM budget_dispatch_fence WHERE execution_id=?", (execution,)).fetchone() == (1, identity, authorized_at)
            snapshot = json.loads(original)
            assert snapshot["route"]["id"] == "coding" and snapshot["route"]["revision"] == 1
            assert snapshot["backend"]["credential_generation"] == 1
            assert snapshot["requested_model"] == "agent" and snapshot["actual_model"] == "fixture"
            assert snapshot["accounting_policy"] == {"version": 1, "cache_read_weight": 1000}
            assert snapshot["cache"]["intent"] == {"mode": "auto"}
            assert connection.execute("SELECT frozen_charge FROM budget_terminal_observation").fetchall() == [(5,)]
            assert connection.execute("SELECT count(*) FROM budget_settlement_outbox").fetchone() == (0,)
            assert connection.execute("SELECT scope,actual,settled FROM budget_reservation ORDER BY scope").fetchall() == [
                ("group:context-team", 0, 0), ("upstream:models", 0, 0)]
            assert connection.execute("SELECT execution_id,version FROM budget_execution_back_leg").fetchall() == [(execution, 1)]
        instance.process.kill()
        assert instance.process.wait(timeout=5) < 0
        instance.stop()  # close the reaped process's private log pipe
        with sqlite3.connect(instance.database) as connection:
            connection.execute("DROP TRIGGER block_receipt")
        instance.start(SANDHI_BUFFERED_ACCOUNTING="tracked", SANDHI_CACHE_READ_WEIGHT="10000")
        instance.provision(origin, "context-replacement-fixture")
        deadline = time.monotonic() + 10
        while True:
            with sqlite3.connect(instance.database) as connection:
                receipts = connection.execute("SELECT charged_tokens FROM budget_settlement_outbox").fetchall()
            if receipts == [(5,)]:
                break
            assert time.monotonic() < deadline, "original terminal charge was not recovered"
            time.sleep(.02)
        assert len(received) == 1, "restart recovery must only settle accounting"
        with sqlite3.connect(instance.database) as connection:
            assert connection.execute("SELECT context_json FROM budget_execution_context WHERE execution_id=?", (execution,)).fetchone() == (original,)
            assert connection.execute("SELECT attempt_id,authorized_at,evidence_json FROM budget_physical_attempt WHERE execution_id=?", (execution,)).fetchone() == original_attempt
        with openai.OpenAI(base_url=instance.base + "/v1", api_key=token, max_retries=0) as client:
            assert client.chat.completions.create(model="agent", max_tokens=64, messages=[{"role": "user", "content": "new"}],
                extra_headers={"x-sandhi-session": "replacement-context-session"}).choices[0].message.content == "reply"
        assert len(received) == 2
        assert received[-1][1]["authorization"] == "Bearer context-replacement-fixture"
        instance.stop()
        with sqlite3.connect(instance.database) as connection:
            snapshots = [json.loads(row[0]) for row in connection.execute("SELECT context_json FROM budget_execution_context ORDER BY rowid")]
            assert snapshots[0] == snapshot
            assert snapshots[1]["accounting_policy"]["cache_read_weight"] == 10000
            assert snapshots[1]["backend"]["credential_generation"] == 2
            assert snapshots[1]["transport_binding_sha256"] != snapshot["transport_binding_sha256"]
            assert connection.execute("SELECT charged_tokens FROM budget_settlement_outbox ORDER BY rowid").fetchall() == [(5,), (7,)]
            assert connection.execute("SELECT scope,sum(actual) FROM budget_reservation WHERE settled=1 GROUP BY scope ORDER BY scope").fetchall() == [("group:context-team", 12), ("upstream:models", 12)]
