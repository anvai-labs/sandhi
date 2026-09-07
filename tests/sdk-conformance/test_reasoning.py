"""Reasoning → provider parser → both planes → durable event, aggregate and lease."""

import json
import sqlite3
import time

import httpx
import pytest

import conftest
from test_dashboard import dashboard  # noqa: F401


@pytest.mark.parametrize("thoughts", [25, 40, 90])
@pytest.mark.parametrize("stream", [False, True])
@pytest.mark.parametrize("dialect", ["gemini", "openai", "responses", "anthropic"])
def test_reasoning_accounted_once_across_planes(dashboard, monkeypatch, thoughts, stream, dialect):
    def response(streaming):
        payload = {"candidates": [{"content": {"role": "model", "parts": [{"text": "pong"}]},
                                     "finishReason": "STOP", "index": 0}],
                   "modelVersion": "gemini-mock", "usageMetadata": {
                       "promptTokenCount": 100, "cachedContentTokenCount": 30,
                       "candidatesTokenCount": 40, "thoughtsTokenCount": thoughts,
                       "totalTokenCount": 140 + thoughts}}
        return f"data: {json.dumps(payload)}\r\n\r\n" if streaming else json.dumps(payload)
    monkeypatch.setattr(conftest, "_gemini_generate_body", response)
    with httpx.Client(base_url=dashboard.base, timeout=10) as client:
        key = client.post("/admin/keys/share", headers=dashboard.headers, json={
            "upstream": "gemini", "subject": "reason", "group": "reason",
            "budget_scope": "group:reason",
        })
        assert key.status_code == 200, key.text
        token = key.json()["virtual_key"]
        assert client.post("/admin/budget", headers=dashboard.headers, json={
            "scope": "group:reason", "limit_tokens": 10000,
        }).status_code == 200
        headers = {"Authorization": "Bearer " + token, "x-sandhi-run-id": "reasoning-run",
                   "x-sandhi-step-id": "reasoning-step"}
        if dialect == "gemini":
            action = "streamGenerateContent?alt=sse" if stream else "generateContent"
            url = "/v1beta/models/gemini-mock:" + action
            body = {"contents": [{"role": "user", "parts": [{"text": "hello"}]}],
                    "generationConfig": {"maxOutputTokens": 128}}
        elif dialect == "responses":
            url, body = "/v1/responses", {"model": "gemini-mock", "input": "hello",
                                           "max_output_tokens": 128, "stream": stream}
        else:
            url = "/v1/messages" if dialect == "anthropic" else "/v1/chat/completions"
            body = {"model": "gemini-mock", "messages": [{"role": "user", "content": "hello"}],
                    "max_tokens": 128, "stream": stream}
        result = client.post(url, headers=headers, json=body)
        assert result.status_code == 200, result.text
        documents = [result.json()] if not stream else [
            json.loads(line[5:].strip()) for line in result.text.splitlines()
            if line.startswith("data:") and line[5:].strip() != "[DONE]"
        ]
        # Responses nests the completed response; Anthropic's final delta owns terminal usage.
        documents = [doc.get("response", doc) for doc in documents]
        usage_key = "usageMetadata" if dialect == "gemini" else "usage"
        usages = [doc[usage_key] for doc in documents if doc.get(usage_key) is not None]
        assert usages, result.text
        usage = usages[-1]
        if dialect == "gemini":
            assert usage["candidatesTokenCount"] == 40
            assert usage["thoughtsTokenCount"] == thoughts
            assert usage["promptTokenCount"] == 100
            assert usage["totalTokenCount"] == 140 + thoughts
        else:
            count = "completion_tokens" if dialect == "openai" else "output_tokens"
            assert usage[count] == 40 + thoughts
        deadline = time.monotonic() + 10
        while True:
            tree = client.get("/admin/usage/run/reasoning-run", headers=dashboard.headers)
            if tree.status_code == 200:
                break
            assert time.monotonic() < deadline, tree.text
            time.sleep(0.05)
        assert tree.json()["run"]["total"]["billable_tokens"] == 140 + thoughts
        with sqlite3.connect(dashboard.database) as conn:
            assert conn.execute("SELECT tokens_out, reasoning_tokens, reasoning_included FROM usage_events "
                                "WHERE run_id='reasoning-run'").fetchall() == [(40, thoughts, 0)]
        usage = client.get("/admin/budget/usage?scope=group:reason", headers=dashboard.headers)
        assert usage.json()["spent"] == 140 + thoughts
