"""Real standalone shared starter; synthetic upstream and disposable durable state."""
from contextlib import contextmanager
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import sqlite3
import subprocess
import threading
import time

import httpx
import pytest

from conftest import REAL_OPENAI_KEY, VK_OPENAI, _free_port
from test_shutdown import BODY, gated_provider  # noqa: F401
from test_backend_registry import backend_origin  # noqa: F401

SCOPE = "starter:shared:v1"


@pytest.fixture(scope="module")
def starter_binary(request):
    # Explicit immutable older artifact supports the before/after causal probe.
    override = os.environ.get("SANDHI_STARTER_TEST_BINARY")
    return Path(override).resolve(strict=True) if override else request.getfixturevalue("proxy_binary")


@pytest.fixture
def starter_gateway(starter_binary, gated_provider, tmp_path):
    @contextmanager
    def launch(*, mode=None, admin=None, extra=None, database=None, expected_status=0):
        credentials = tmp_path / "credentials"
        credentials.mkdir(mode=0o700, exist_ok=True)
        database = database or tmp_path / "starter.db"
        env = {k: v for k, v in os.environ.items() if not k.startswith(("SANDHI_", "SENTINELPASS_"))}
        port = _free_port()
        env.update(SANDHI_STORE=str(database), SANDHI_BIND=f"127.0.0.1:{port}",
                   SANDHI_VAULT_BACKEND="credstore", SANDHI_CREDENTIALS_DIR=str(credentials),
                   SANDHI_OPENAI_KEY=REAL_OPENAI_KEY, SANDHI_OPENAI_BASE=gated_provider.base,
                   SANDHI_SHUTDOWN_QUIESCE_MS="0")
        if mode is not None:
            env["SANDHI_AUTH_MODE"] = mode
        if admin is not None:
            env["SANDHI_ADMIN_TOKEN"] = admin
        env.update(extra or {})
        with (tmp_path / f"starter-{port}.log").open("wb") as log:
            process = subprocess.Popen([str(starter_binary)], env=env, cwd=tmp_path,
                                       stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=log)
            try:
                with httpx.Client(base_url=f"http://127.0.0.1:{port}", trust_env=False, timeout=4) as client:
                    deadline = time.monotonic() + 10
                    while True:
                        assert process.poll() is None, "starter exited before readiness; private log retained"
                        try:
                            if client.get("/readyz").status_code == 200:
                                break
                        except httpx.TransportError:
                            pass
                        assert time.monotonic() < deadline, "starter never became ready"
                        time.sleep(.05)
                    yield process, client, database
            finally:
                if process.poll() is None:
                    process.terminate()
                    assert process.wait(timeout=8) == expected_status
    return launch


def test_default_starter_requires_no_inference_or_operator_credential(starter_gateway):
    with starter_gateway() as (_, client, _):
        session = client.get("/auth/session").json()
        assert session["mode"] == session["operator_mode"] == "none"
        assert session["authenticated"] is False
        assert set(session["permissions"]) == {"read", "operate", "admin", "diagnostics"}
        assert session["starter"]["budget_scope"] == SCOPE
        assert client.get("/admin/version").status_code == 200
        assert client.get("/v1/models").status_code == 200
        assert client.post("/v1/chat/completions", json=BODY).status_code == 200


@pytest.mark.parametrize("path,body,headers", [
    ("/v1/chat/completions", BODY, {}),
    ("/v1/messages", {**BODY, "max_tokens": 8}, {"anthropic-version": "2023-06-01"}),
    ("/v1/responses", {"model": "gpt-mock", "input": "ping", "max_output_tokens": 8}, {}),
    ("/v1beta/models/gpt-mock:generateContent", {"contents": [{"role": "user", "parts": [{"text": "ping"}]}]}, {}),
])
def test_all_four_ingress_dialects_use_the_same_selected_starter(starter_gateway, gated_provider, path, body, headers):
    with starter_gateway() as (_, client, _):
        response = client.post(path, json=body, headers=headers)
        assert response.status_code == 200, response.text
    assert len(gated_provider.requests) == 1


def test_count_tokens_needs_no_key_and_never_dispatches(starter_gateway, gated_provider):
    with starter_gateway() as (_, client, _):
        assert client.post("/v1/messages/count_tokens", json={**BODY, "max_tokens": 8}).status_code == 200
    assert gated_provider.requests == []


def test_dummy_credentials_cannot_select_identity_and_spoofed_attribution_refuses(starter_gateway, gated_provider):
    with starter_gateway() as (_, client, database):
        for headers in [{}, {"Authorization": "Bearer unrelated"}, {"x-api-key": "different"}]:
            assert client.post("/v1/chat/completions", json=BODY, headers=headers).status_code == 200
        for headers in [{"x-sandhi-subject-id": "another-person"}, {"x-sandhi-group-id": "another-group"},
                        {"x-sandhi-upstream": "anthropic"}, {"x-sandhi-key": "someone-else"}]:
            assert client.post("/v1/chat/completions", json=BODY, headers=headers).status_code == 403
        assert len(gated_provider.requests) == 3
    with sqlite3.connect(database) as db:
        identities = db.execute("SELECT DISTINCT virtual_key_id,subject_id,group_id FROM usage_events").fetchall()
    assert identities == [("starter_shared_v1", "builtin_starter", "builtin_starter")]


def test_configured_operator_token_does_not_protect_starter_inference(starter_gateway):
    with starter_gateway(admin="starter-admin") as (_, client, _):
        session = client.get("/auth/session").json()
        assert session["mode"] == "none" and session["operator_mode"] == "tokens"
        assert session["authenticated"] is False and session["permissions"] == []
        assert client.get("/admin/version").status_code == 401
        assert client.get("/admin/version", headers={"Authorization": "Bearer starter-admin"}).status_code == 200
        assert client.post("/v1/chat/completions", json=BODY).status_code == 200


def test_none_control_plane_rejects_cross_origin_and_dns_rebinding(starter_gateway):
    with starter_gateway() as (_, client, _):
        body = {"scope": SCOPE, "limit_tokens": 1000}
        for headers in [{"Origin": "http://attacker.invalid"}, {"Origin": "null"},
                        {"Host": "attacker.invalid", "Origin": "http://attacker.invalid"},
                        {"Sec-Fetch-Site": "cross-site"}, {"Referer": "http://attacker.invalid/page"}]:
            assert client.post("/admin/budget", json=body, headers=headers).status_code == 403
        assert client.post("/admin/budget", json=body).status_code == 200
        assert client.post("/admin/budget", json=body, headers={"Origin": str(client.base_url).rstrip('/')}).status_code == 200


@pytest.mark.parametrize("method,path,body", [
    ("POST", "/v1/chat/completions", BODY),
    ("POST", "/v1/messages", {**BODY, "max_tokens": 8}),
    ("POST", "/v1/responses", {"model": "gpt-mock", "input": "ping", "max_output_tokens": 8}),
    ("POST", "/v1beta/models/gpt-mock:generateContent", {"contents": [{"parts": [{"text": "ping"}]}]}),
    ("POST", "/v1/models/gpt-mock:generateContent", {"contents": [{"parts": [{"text": "ping"}]}]}),
    ("POST", "/v1beta/models/gpt-mock:streamGenerateContent", {"contents": [{"parts": [{"text": "ping"}]}]}),
    ("POST", "/v1/models/gpt-mock:streamGenerateContent", {"contents": [{"parts": [{"text": "ping"}]}]}),
    ("POST", "/v1/messages/count_tokens", {**BODY, "max_tokens": 8}),
    ("POST", "/v1beta/models/gpt-mock:countTokens", {"contents": [{"parts": [{"text": "ping"}]}]}),
    ("POST", "/v1/models/gpt-mock:countTokens", {"contents": [{"parts": [{"text": "ping"}]}]}),
    ("POST", "/v1/completions", {"model": "gpt-mock", "prompt": "ping", "max_tokens": 8}),
    ("POST", "/v1/embeddings", {"model": "gpt-mock", "input": "ping"}),
    ("GET", "/v1/models", None),
    ("GET", "/v1beta/models", None),
])
def test_starter_inference_rejects_browser_origin_and_rebinding_before_dispatch(
    starter_gateway, gated_provider, method, path, body
):
    with starter_gateway() as (_, client, _):
        attacks = [
            {"Host": "attacker.invalid", "Origin": "http://attacker.invalid", "Sec-Fetch-Site": "same-origin"},
            {"Host": "attacker.invalid", "Origin": "http://attacker.invalid", "Authorization": "Bearer not-needed"},
            {"Origin": "http://attacker.invalid"},
            {"Origin": "null"},
            {"Sec-Fetch-Site": "cross-site"},
            {"Referer": "http://attacker.invalid/page"},
        ]
        for headers in attacks:
            # A simple browser request needs no CORS preflight. Dummy credentials
            # must not disable the starter's listener/Origin boundary either.
            response = client.request(method, path, content=None if body is None else json.dumps(body),
                headers={"Content-Type": "text/plain", **headers})
            assert response.status_code == 403, (path, headers, response.status_code)
            assert gated_provider.requests == [], "browser refusal must precede physical model dispatch"


def test_starter_accepts_neutral_agents_and_exact_same_origin(starter_gateway, gated_provider):
    with starter_gateway() as (_, client, _):
        for headers in ({}, {"Origin": str(client.base_url).rstrip('/'), "Sec-Fetch-Site": "same-origin"}):
            assert client.post("/v1/chat/completions", json=BODY, headers=headers).status_code == 200
        assert len(gated_provider.requests) == 2


def test_tokens_remains_explicit_and_cannot_fall_back_to_starter(starter_gateway):
    with starter_gateway(mode="tokens") as (_, client, _):
        assert client.get("/auth/session").json() == {"mode": "tokens"}
        assert client.post("/v1/chat/completions", json=BODY).status_code == 401
        assert client.post("/v1/chat/completions", json=BODY,
                           headers={"Authorization": f"Bearer {VK_OPENAI}"}).status_code == 200


def test_starter_budget_survives_restart_and_dummy_key_rotation(starter_gateway, gated_provider, tmp_path):
    database = tmp_path / "persistent.db"
    with starter_gateway(database=database) as (_, client, _):
        assert client.post("/admin/budget", json={"scope": SCOPE, "limit_tokens": 1000}).status_code == 200
        assert client.post("/v1/chat/completions", json={**BODY, "max_tokens": 8}).status_code == 200
        # The fixture reports14 neutral tokens (including its cache-read split).
        assert client.post("/admin/budget", json={"scope": SCOPE, "limit_tokens": 14}).status_code == 200
    with starter_gateway(database=database) as (_, client, _):
        denied = client.post("/v1/chat/completions", json={**BODY, "max_tokens": 8},
                             headers={"Authorization": "Bearer a-new-dummy"})
        assert denied.status_code == 429
    assert len(gated_provider.requests) == 1
    with sqlite3.connect(database) as db:
        scopes = {row[0] for row in db.execute("SELECT DISTINCT scope FROM budget_reservation")}
    assert SCOPE in scopes and len(scopes) == 2, "both existing budget legs remain on the same durable owner"


def test_starter_rate_limit_uses_one_bucket_despite_dummy_credentials(starter_gateway, gated_provider):
    with starter_gateway(extra={"SANDHI_STARTER_RATE_LIMIT_PER_MIN": "1"}) as (_, client, _):
        assert client.post("/v1/chat/completions", json=BODY).status_code == 200
        assert client.post("/v1/chat/completions", json=BODY,
                           headers={"Authorization": "Bearer new-dummy"}).status_code == 429
    assert len(gated_provider.requests) == 1


def test_unknown_source_usage_keeps_both_starter_holds_through_restart(starter_gateway, gated_provider, tmp_path):
    database = tmp_path / "unknown.db"
    gated_provider.omit_usage = True
    extra = {"SANDHI_SHUTDOWN_GRACE_SECS": "2"}
    with starter_gateway(database=database, extra=extra, expected_status=124) as (_, client, _):
        failed = client.post("/v1/chat/completions", json={**BODY, "max_tokens": 8})
        assert failed.status_code == 502
        assert failed.headers["x-should-retry"] == "false"
    with sqlite3.connect(database) as db:
        held = db.execute("SELECT scope,ceiling,actual,settled FROM budget_reservation ORDER BY scope").fetchall()
        assert len(held) == 2 and all(actual == settled == 0 and ceiling > 0 for _, ceiling, actual, settled in held)
        assert db.execute("SELECT count(*) FROM budget_settlement_outbox").fetchone() == (0,)
    ceiling = next(amount for scope, amount, _, _ in held if scope == SCOPE)
    with starter_gateway(database=database, extra=extra, expected_status=124) as (_, client, _):
        assert client.post("/admin/budget", json={"scope": SCOPE, "limit_tokens": ceiling}).status_code == 200
        assert client.post("/v1/chat/completions", json={**BODY, "max_tokens": 8},
                           headers={"Authorization": "Bearer cannot-replace-the-owner"}).status_code == 429
    with sqlite3.connect(database) as db:
        assert db.execute("SELECT scope,ceiling,actual,settled FROM budget_reservation ORDER BY scope").fetchall() == held
    assert len(gated_provider.requests) == 1


def test_starter_native_owner_survives_restart_and_is_not_adopted_by_token_mode(
    starter_gateway, backend_origin, tmp_path,
):
    origin, received = backend_origin
    database = tmp_path / "native.db"
    extra = {"SANDHI_STARTER_UPSTREAM": "backend/native"}
    admin = {"Authorization": "Bearer starter-admin"}
    with starter_gateway(database=database, extra=extra, admin="starter-admin") as (_, client, _):
        secret = tmp_path / "credentials/openai__default"
        secret.write_text("synthetic-native-source"); secret.chmod(0o600)
        assert client.post("/admin/keys/reference", headers=admin,
                           json={"provider": "openai", "base_url": origin}).status_code == 201
        backend = {"id": "native", "provider": "models", "credential_ref": "openai:default",
                   "endpoint": {"endpoint_family": "openai_responses"}, "models": [{"id": "fixture"}]}
        response = client.post("/admin/backends", headers=admin, json={"definition": backend})
        assert response.status_code == 201, response.text
        token = client.post("/admin/keys/share", headers=admin, json={"upstream": "backend/native",
                            "subject": "separate-person", "group": "separate-group", "models": ["fixture"]}).json()["virtual_key"]
        first = client.post("/v1/responses", json={"model": "fixture", "input": "first", "max_output_tokens": 8})
        assert first.status_code == 200, first.text
        reference = first.json()["id"]
    with starter_gateway(database=database, extra=extra, admin="starter-admin") as (_, client, _):
        continued = client.post("/v1/responses", headers={"Authorization": "Bearer " + token},
                                json={"model": "fixture", "input": "next", "previous_response_id": reference})
        assert continued.status_code == 200, continued.text
    assert len(received) == 2
    with starter_gateway(database=database, mode="tokens", extra=extra, admin="starter-admin") as (_, client, _):
        headers = {"Authorization": "Bearer " + token}
        assert client.get("/v1/models", headers=headers).status_code == 200
        foreign = client.post("/v1/responses", headers=headers,
                              json={"model": "fixture", "input": "cannot-adopt", "previous_response_id": reference})
        assert foreign.status_code == 403, foreign.text
        assert "unverified for this caller" in foreign.json()["error"]["message"]
    assert len(received) == 2, "a different authenticated owner cannot adopt a shared starter native reference"


def test_native_text_and_embeddings_keep_the_shared_principal(starter_gateway, gated_provider):
    calls = []
    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *_):
            pass
        def do_POST(self):
            calls.append((self.path, json.loads(self.rfile.read(int(self.headers["content-length"])))))
            if self.path.endswith("/embeddings"):
                body = {"object": "list", "data": [{"object": "embedding", "index": 0, "embedding": [0.25]}],
                        "model": "gpt-mock", "usage": {"prompt_tokens": 4, "total_tokens": 4}}
            else:
                assert self.path.endswith("/completions")
                body = {"id": "cmpl-starter", "object": "text_completion", "created": 1, "model": "gpt-mock",
                        "choices": [{"text": "pong", "index": 0, "finish_reason": "stop"}],
                        "usage": {"prompt_tokens": 4, "completion_tokens": 1, "total_tokens": 5}}
            data = json.dumps(body).encode()
            self.send_response(200); self.send_header("content-type", "application/json")
            self.send_header("content-length", str(len(data))); self.end_headers(); self.wfile.write(data)
    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    thread = threading.Thread(target=server.serve_forever, daemon=True); thread.start()
    try:
        with starter_gateway(extra={"SANDHI_OPENAI_BASE": f"http://127.0.0.1:{server.server_port}"}) as (_, client, database):
            text = client.post("/v1/completions", json={"model": "gpt-mock", "prompt": "ping", "max_tokens": 8})
            assert text.status_code == 200, text.text
            assert text.json()["choices"][0]["text"] == "pong"
            embedding = client.post("/v1/embeddings", json={"model": "gpt-mock", "input": "ping"})
            assert embedding.status_code == 200, embedding.text
            assert embedding.json()["data"][0]["embedding"] == [0.25]
        with sqlite3.connect(database) as db:
            assert db.execute("SELECT DISTINCT virtual_key_id FROM usage_events").fetchall() == [("starter_shared_v1",)]
        assert len(calls) == 2 and gated_provider.requests == []
    finally:
        server.shutdown(); server.server_close(); thread.join(timeout=2)


def test_cli_without_admin_token_reaches_none_but_protected_operator_denies(starter_gateway, request):
    override = os.environ.get("SANDHI_STARTER_TEST_CLI")
    cli = Path(override).resolve(strict=True) if override else request.getfixturevalue("cli_binary")
    env = {k: v for k, v in os.environ.items() if not k.startswith("SANDHI_")}
    with starter_gateway() as (_, client, _):
        result = subprocess.run([str(cli), "--admin-url", str(client.base_url), "usage"], env=env,
                                capture_output=True, text=True, timeout=5)
        assert result.returncode == 0, result.stderr
    with starter_gateway(admin="protected-admin") as (_, client, _):
        result = subprocess.run([str(cli), "--admin-url", str(client.base_url), "usage"], env=env,
                                capture_output=True, text=True, timeout=5)
        assert result.returncode != 0
