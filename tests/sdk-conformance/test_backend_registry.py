"""Explicit backends through the real CLI/API, disk state and process restarts.

Credentials, stores and upstreams are disposable synthetic fixture data.
"""

from contextlib import contextmanager
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import shutil
import sqlite3
import subprocess
import threading
import time

import httpx
import openai
import pytest

from conftest import REPO_ROOT, _free_port

ADMIN = "backend-fixture-admin"


def test_cache_policy_all_vendor_sdks_both_modes_and_namespace_restart_rotation(tmp_path, backend_binaries, backend_origin):
    import anthropic
    from google import genai
    from google.genai import types

    origin, received = backend_origin
    policy = {"default_intent": {"mode": "auto"}, "namespace": "scoped_key", "models": {
        "fixture": {"profile": "openai_explicit", "lifetimes": ["30m", "12h"], "max_breakpoints": 4}}}
    policy_file = tmp_path / "cache-policy.json"
    policy_file.write_text(json.dumps(policy))
    headers = {"x-sandhi-session": "sdk-cache-fixture"}
    directive = {"sandhi_cache": {"mode": "auto", "lifetime": "12h"}}
    with gateway(tmp_path, backend_binaries) as instance:
        instance.provision(origin, "cache-original-fixture")
        record = instance.cli("set", "cached", "--provider", "custom-models", "--credential-ref", "openai:default",
                              "--endpoint-family", "openai_responses", "--model", "fixture",
                              "--cache-policy", str(policy_file))
        assert record["definition"]["cache_policy"] == dict(policy, allow_request_override=True)
        token = instance.key("backend/cached")
        for stream in (False, True):
            with instance.client(token) as client:
                result = client.chat.completions.create(model="fixture", max_tokens=64,
                    messages=[{"role": "user", "content": "hello"}], stream=stream,
                    extra_headers=headers, extra_body=directive)
                text = "".join(chunk.choices[0].delta.content or "" for chunk in result if chunk.choices) if stream else result.choices[0].message.content
                assert text == "reply"
                result = client.responses.create(model="fixture", input="hello", max_output_tokens=64,
                    stream=stream, extra_headers=headers, extra_body=directive)
                text = "".join(event.delta for event in result if event.type == "response.output_text.delta") if stream else result.output[0].content[0].text
                assert text == "reply"
            with anthropic.Anthropic(base_url=instance.base, api_key=token, max_retries=0, default_headers=headers) as client:
                result = client.messages.create(model="fixture", max_tokens=64,
                    messages=[{"role": "user", "content": "hello"}], stream=stream, extra_body=directive)
                text = "".join(event.delta.text for event in result if event.type == "content_block_delta" and event.delta.type == "text_delta") if stream else result.content[0].text
                assert text == "reply"
            with genai.Client(api_key=token, http_options=types.HttpOptions(base_url=instance.base, headers=headers)) as client:
                config = types.GenerateContentConfig(max_output_tokens=64, http_options=types.HttpOptions(extra_body=directive))
                result = (client.models.generate_content_stream if stream else client.models.generate_content)(
                    model="fixture", contents="hello", config=config)
                text = "".join(chunk.text or "" for chunk in result) if stream else result.text
                assert text == "reply"
        assert len(received) == 8
        keys = [body["prompt_cache_key"] for _, _, body in received]
        assert len(set(keys)) == 1 and keys[0].startswith("sandhi:")
        for _, sent_headers, body in received:
            assert sent_headers["authorization"] == "Bearer cache-original-fixture"
            assert body["prompt_cache_options"] == {"mode": "implicit", "ttl": "12h"}
            assert "sandhi_cache" not in body and "compiled_cache" not in body
        assert token not in json.dumps(received)
        instance.stop()
        instance.start()
        with instance.client(token) as client:
            assert client.responses.create(model="fixture", input="hello", extra_headers=headers).output[0].content[0].text == "reply"
        assert received[-1][2]["prompt_cache_key"] == keys[0]
        instance.provision(origin, "cache-rotated-fixture")
        with instance.client(token) as client:
            assert client.responses.create(model="fixture", input="hello", extra_headers=headers).output[0].content[0].text == "reply"
        assert received[-1][2]["prompt_cache_key"] != keys[0]
        assert received[-1][1]["authorization"] == "Bearer cache-rotated-fixture"
        assert instance.admin("GET", "/admin/keys").json()["keys"][0]["generation"] == 2
        instance.stop()  # drain the bounded usage writer before inspecting disk evidence
        with sqlite3.connect(instance.database) as connection:
            rows = connection.execute("SELECT tokens_in,tokens_out,cache_read_tokens,cache_creation_tokens FROM usage_events").fetchall()
        assert len(rows) == 10 and all(row == (2, 2, 2, 1) for row in rows)


def test_gemini_sdk_uses_explicit_responses_backend(tmp_path, backend_binaries, backend_origin):
    genai = pytest.importorskip("google.genai", reason="google-genai SDK not installed")
    origin, received = backend_origin
    with gateway(tmp_path, backend_binaries) as instance:
        instance.provision(origin, "gemini-client-synthetic-key")
        instance.cli("set", "responses", "--provider", "custom-models",
                     "--credential-ref", "openai:default", "--endpoint-family", "openai_responses",
                     "--model", "fixture")
        token = instance.key("backend/responses")
        with genai.Client(api_key=token, http_options=genai.types.HttpOptions(base_url=instance.base)) as client:
            response = client.models.generate_content(model="fixture", contents="hello")
        assert response.text == "reply"
        assert response.usage_metadata.prompt_token_count == 5
        assert response.usage_metadata.cached_content_token_count == 2
        assert response.usage_metadata.candidates_token_count == 2
        assert response.usage_metadata.total_token_count == 7
        assert len(received) == 1 and received[0][0] == "/responses"
        assert received[0][1]["authorization"] == "Bearer gemini-client-synthetic-key"
        assert any(item["role"] == "user" and "hello" in json.dumps(item)
                   for item in received[0][2]["input"])
        assert token not in json.dumps(received), "virtual credentials must not reach the origin"



@pytest.fixture(scope="module")
def backend_binaries(tmp_path_factory):
    subprocess.run(["cargo", "build", "--locked", "-p", "sandhi-proxy", "--bins"],
                   cwd=REPO_ROOT, check=True)
    directory = tmp_path_factory.mktemp("backend-binaries")
    for name in ("sandhi", "sandhi-proxy"):
        shutil.copy2(REPO_ROOT / "target/debug" / name, directory / name)
    return directory


@pytest.fixture
def backend_origin():
    received = []

    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *_args):
            pass

        def do_POST(self):
            request = json.loads(self.rfile.read(int(self.headers.get("content-length", "0"))))
            received.append((self.path, {k.lower(): v for k, v in self.headers.items()}, request))
            if self.path == "/responses":
                response = {"id": "resp_fixture", "object": "response", "created_at": 1,
                            "model": "fixture", "status": "completed",
                            "output": [{"id": "msg_fixture", "type": "message", "role": "assistant",
                                        "status": "completed", "content": [
                                            {"type": "output_text", "text": "reply", "annotations": []}]}],
                            "usage": {"input_tokens": 5, "output_tokens": 2, "total_tokens": 7,
                                      "input_tokens_details": {"cached_tokens": 2, "cache_write_tokens": 1}}}
            else:
                assert self.path == "/chat/completions"
                response = {"id": "chat_fixture", "object": "chat.completion", "created": 1,
                            "model": "fixture", "choices": [{"index": 0, "finish_reason": "stop",
                                "message": {"role": "assistant", "content": "reply"}}],
                            "usage": {"prompt_tokens": 5, "completion_tokens": 2, "total_tokens": 7,
                                      "prompt_tokens_details": {"cached_tokens": 2, "cache_write_tokens": 1}}}
            if request.get("stream"):
                assert self.path == "/responses"
                started = dict(response, status="in_progress", output=[], usage=None)
                frames = [{"type": "response.created", "sequence_number": 0, "response": started},
                          {"type": "response.output_text.delta", "sequence_number": 1,
                           "item_id": "msg_fixture", "output_index": 0, "content_index": 0, "delta": "reply"},
                          {"type": "response.completed", "sequence_number": 2, "response": response}]
                encoded = "".join(f"event: {frame['type']}\ndata: {json.dumps(frame)}\n\n" for frame in frames).encode()
                content_type = "text/event-stream"
            else:
                encoded = json.dumps(response).encode()
                content_type = "application/json"
            self.send_response(200)
            self.send_header("content-type", content_type)
            self.send_header("content-length", str(len(encoded)))
            self.end_headers()
            self.wfile.write(encoded)

    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        yield f"http://127.0.0.1:{server.server_port}", received
    finally:
        server.shutdown()
        server.server_close()
        thread.join(timeout=2)


class Gateway:
    def __init__(self, directory: Path, binaries: Path):
        self.directory, self.binaries = directory, binaries
        self.database = directory / "gateway.db"
        self.config = directory / "config.json"
        self.config.write_text("{}")
        self.credentials = directory / "credentials"
        self.credentials.mkdir(mode=0o700)
        self.process = None

    def start(self, **overrides):
        env = {key: value for key, value in os.environ.items()
               if not key.startswith(("SANDHI_", "SENTINELPASS_"))}
        env.update(SANDHI_AUTH_MODE="tokens", SANDHI_BIND=f"127.0.0.1:{_free_port()}",
                   SANDHI_ADMIN_TOKEN=ADMIN, SANDHI_STORE=str(self.database),
                   SANDHI_CONFIG=str(self.config), SANDHI_VAULT_BACKEND="credstore",
                   SANDHI_CREDENTIALS_DIR=str(self.credentials), SANDHI_SHUTDOWN_QUIESCE_MS="0")
        env.update(overrides)
        self.base = "http://" + env["SANDHI_BIND"]
        self.process = subprocess.Popen([str(self.binaries / "sandhi-proxy")], cwd=self.directory,
                                        env=env, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
        deadline = time.monotonic() + 10
        while time.monotonic() < deadline:
            if self.process.poll() is not None:
                raise AssertionError(self.process.stderr.read().decode())
            try:
                if httpx.get(self.base + "/readyz", timeout=1).status_code == 200:
                    return self
            except httpx.TransportError:
                pass
            time.sleep(0.02)
        raise AssertionError("gateway did not become ready")

    def stop(self):
        if self.process and self.process.poll() is None:
            self.process.terminate()
            try:
                self.process.wait(timeout=10)
            except subprocess.TimeoutExpired:
                self.process.kill()
                self.process.wait(timeout=2)
                raise AssertionError("gateway failed to stop cleanly")
            assert self.process.returncode == 0, self.process.stderr.read().decode()
        if self.process and self.process.stderr:
            self.process.stderr.close()

    def admin(self, method, route, **kwargs):
        return httpx.request(method, self.base + route,
                             headers={"authorization": "Bearer " + ADMIN}, timeout=10, **kwargs)

    def provision(self, origin, secret):
        # Simulate external provisioning into the disposable, read-only credstore.
        credential = self.credentials / "openai__default"
        credential.write_text(secret)
        credential.chmod(0o600)
        response = self.admin("POST", "/admin/keys/reference", json={
            "provider": "openai", "base_url": origin})
        assert response.status_code == 201, response.text

    def cli(self, *args, success=True):
        result = subprocess.run([str(self.binaries / "sandhi"), "--admin-url", self.base,
                                 "--admin-token", ADMIN, "backends", *args],
                                cwd=self.directory, capture_output=True, text=True, timeout=10)
        if not success:
            assert result.returncode != 0
            return result
        assert result.returncode == 0, result.stderr
        return json.loads(result.stdout)

    def key(self, upstream):
        result = self.admin("POST", "/admin/keys/share", json={"upstream": upstream, "models": ["fixture"]})
        assert result.status_code == 200, result.text
        return result.json()["virtual_key"]

    def client(self, token):
        return openai.OpenAI(base_url=self.base + "/v1", api_key=token, max_retries=0)


@contextmanager
def gateway(directory, binaries):
    instance = Gateway(directory, binaries)
    try:
        yield instance.start()
    finally:
        instance.stop()


def test_cli_backend_rotation_real_restart_and_independent_revocation(tmp_path, backend_binaries, backend_origin):
    origin, received = backend_origin
    with gateway(tmp_path, backend_binaries) as instance:
        instance.provision(origin, "first-synthetic-key")
        arguments = ["set", "primary", "--provider", "custom-openai", "--credential-ref", "openai:default",
                     "--endpoint-family", "openai_responses", "--base-url", origin, "--model", "fixture"]
        record = instance.cli(*arguments)
        assert record["revision"] == 1 and record["ready"] is True
        assert record["upstream_ref"] == "backend/primary"
        token = instance.key("backend/primary")
        with instance.client(token) as client:
            result = client.chat.completions.create(model="fixture", max_tokens=64, messages=[
                {"role": "system", "content": "Be precise"}, {"role": "user", "content": "hello"}])
            assert result.choices[0].message.content == "reply"
        assert received[-1][0] == "/responses"
        assert received[-1][1]["authorization"] == "Bearer first-synthetic-key"
        instance.provision(origin, "rotated-synthetic-key")
        instance.stop()
        instance.start()
        with instance.client(token) as client:
            response = client.responses.create(model="fixture", instructions="Be precise",
                                               input=[{"role": "user", "content": "hello again"}])
            assert response.output[0].content[0].text == "reply"
        assert received[-1][1]["authorization"] == "Bearer rotated-synthetic-key"
        assert instance.cli("list")["backends"][0]["ready"] is True
        instance.cli(*arguments, "--user-agent", "changed", success=False)
        assert instance.cli("list")["backends"][0]["revision"] == 1
        instance.cli("revoke", "primary", "--revision", "0", success=False)
        instance.cli("revoke", "primary", "--revision", "1")
        assert instance.admin("GET", "/admin/keys").json()["keys"][0]["status"] == "active"
        instance.stop()
        instance.start()
        with instance.client(token) as client, pytest.raises(openai.APIStatusError) as error:
            client.responses.create(model="fixture", input="must not dispatch")
        assert error.value.status_code == 502
        inventory = instance.cli("list")
        assert inventory["backends"][0]["status"] == "revoked"
        assert inventory["backends"][0]["ready"] is False
        encoded = json.dumps(inventory)
        assert "first-synthetic-key" not in encoded and "rotated-synthetic-key" not in encoded
        with sqlite3.connect(instance.database) as connection:
            rows = connection.execute("SELECT definition_json FROM backends").fetchall()
        assert all("synthetic-key" not in row[0] for row in rows)
        assert len(received) == 2, "revoked routes must not replay or fall back"


def test_config_backend_persists_without_reapply_and_credential_revoke_disables_it(tmp_path, backend_binaries, backend_origin):
    origin, received = backend_origin
    with gateway(tmp_path, backend_binaries) as instance:
        instance.provision(origin, "config-synthetic-key")
        instance.config.write_text(json.dumps({
            "backends": [{"id": "configured", "provider": "custom-openai", "credential_ref": "openai:default",
                          "endpoint": {"endpoint_family": "openai_responses"},
                          "cache_policy": {"default_intent": {"mode": "auto"}, "namespace": "scoped_key",
                                           "models": {"fixture": {"profile": "openai_explicit",
                                               "lifetimes": ["30m"], "max_breakpoints": 4}}},
                          "models": [{"id": "fixture", "capabilities": {"tools": True}}]}],
            "vkeys": [{"upstream": "backend/configured", "models": ["fixture"], "subject": "synthetic-agent"}]
        }))
        preview = instance.admin("GET", "/admin/config")
        assert preview.status_code == 200, preview.text
        assert preview.json()["backends"][0]["action"] == "create"
        applied = instance.admin("POST", "/admin/config/apply")
        assert applied.status_code == 200, applied.text
        token = applied.json()["vkeys"]["minted"][0]["virtual_key"]
        repeated = instance.admin("POST", "/admin/config/apply")
        assert repeated.status_code == 200, repeated.text
        assert not repeated.json()["vkeys"]["minted"]
        assert instance.cli("list")["backends"][0]["revision"] == 1
        instance.config.write_text("{}")
        assert instance.admin("POST", "/admin/config/apply").status_code == 200
        instance.stop()
        instance.start()
        with instance.client(token) as client:
            assert client.responses.create(model="fixture", input="hello").output[0].content[0].text == "reply"
        assert received[-1][2]["prompt_cache_options"] == {"mode": "implicit"}
        assert received[-1][2]["prompt_cache_key"].startswith("sandhi:")
        assert instance.admin("DELETE", "/admin/keys/openai/default").status_code == 200
        assert instance.cli("list")["backends"][0]["ready"] is False
        instance.stop()
        instance.start()
        with instance.client(token) as client, pytest.raises(openai.APIStatusError) as error:
            client.responses.create(model="fixture", input="must not dispatch")
        assert error.value.status_code == 502
        assert len(received) == 1


def test_failed_credential_update_cannot_grant_through_existing_backend(tmp_path, backend_binaries, backend_origin):
    origin, received = backend_origin
    with gateway(tmp_path, backend_binaries) as instance:
        instance.provision(origin, "unchanged-synthetic-key")
        instance.cli("set", "primary", "--provider", "custom-openai",
                     "--credential-ref", "openai:default", "--endpoint-family", "openai_responses",
                     "--model", "fixture")
        instance.config.write_text(json.dumps({
            # Applying provider intent is a write; credstore must reject it.
            "providers": [{"provider": "openai", "base_url": origin}],
            "backends": [{"id": "primary", "provider": "custom-openai", "credential_ref": "openai:default",
                          "endpoint": {"endpoint_family": "openai_responses"}, "models": [{"id": "fixture"}]}],
            "vkeys": [{"upstream": "backend/primary", "subject": "must-not-mint", "models": ["fixture"]}],
            "profiles": [{"group": "must-not-grant", "upstream": "backend/primary", "models": ["fixture"]}]
        }))
        result = instance.admin("POST", "/admin/config/apply")
        assert result.status_code == 503, result.text
        payload = result.json()
        assert {failure["component"] for failure in payload["failures"]} == {
            "provider", "backend", "vkey", "profile"}
        assert payload["vkeys"]["minted"] == [] and payload["profiles"]["applied"] == []
        assert instance.cli("list")["backends"][0]["revision"] == 1
        assert instance.cli("list")["backends"][0]["ready"] is True
        assert received == [], "management failures must not trigger inference"
