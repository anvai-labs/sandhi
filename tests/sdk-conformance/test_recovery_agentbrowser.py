"""Real AgentBrowser reads only restored synthetic data; no live vault or browser profile."""

import copy
from contextlib import contextmanager
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import subprocess
import threading

import httpx
import pytest

from recovery_snapshot import restore, snapshot
from test_recovery import (  # noqa: F401 - reuse scoped fixture definitions
    SCOPES, gated_provider, public_evidence, recovery_gateway, seed,
)


@contextmanager
def altered_dashboard(gateway_origin, fault):
    """Fixed loopback target; mutate only disposable served assets, never source files."""
    class Handler(BaseHTTPRequestHandler):
        def do_GET(self):
            assert self.path.startswith(("/dashboard", "/admin/"))
            response = httpx.get(gateway_origin + self.path,
                                headers={"Authorization": self.headers.get("Authorization", "")},
                                timeout=3, trust_env=False)
            body = response.content
            if fault == "hidden" and self.path == "/dashboard/assets/dashboard.css":
                body += b"\n#tables table:nth-of-type(2) { display:none !important; }\n"
            if fault == "heading" and self.path == "/dashboard/assets/dashboard.js":
                body += b"""\nnew MutationObserver(() => {
                  const table = document.querySelector('#tables table:nth-of-type(2)');
                  if (table && !table.dataset.fixtureMoved) {
                    table.dataset.fixtureMoved = 'true';
                    table.after(table.previousElementSibling);
                  }
                }).observe(document.getElementById('usage'), {childList: true, subtree: true});\n"""
            self.send_response(response.status_code)
            for header in ("content-type", "content-security-policy", "cache-control", "x-content-type-options"):
                if header in response.headers:
                    self.send_header(header, response.headers[header])
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def log_message(self, *_args):
            pass

    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        yield f"http://127.0.0.1:{server.server_port}"
    finally:
        server.shutdown()
        server.server_close()
        thread.join(timeout=3)
        assert not thread.is_alive()


def test_agentbrowser_observes_restored_dashboard(recovery_gateway, tmp_path):
    sibling = os.environ.get("SANDHI_AGENTBROWSER_ROOT")
    if not sibling:
        pytest.skip("set SANDHI_AGENTBROWSER_ROOT to a built AgentBrowser checkout")
    sibling = Path(sibling).resolve()
    assert (sibling / "packages/api/dist/index.js").is_file(), "build AgentBrowser before running"
    source = tmp_path / "source" / "usage.db"
    with recovery_gateway(source) as original:
        seed(original)
        baseline = public_evidence(original.client)
        original.stop()
    archive = tmp_path / "snapshot"
    snapshot(source, archive, shards=1, metadata={"fixture": "restored-browser", "shutdown_exit": 0})
    restored = restore(archive, tmp_path / "restored", expected_shards=1)
    # No new model call is seeded after restore: the browser must observe the saved evidence.
    with recovery_gateway(restored) as gateway:
        assert public_evidence(gateway.client) == baseline
        expected = {key: baseline["usage"][key] for key in (
            "total", "by_subject", "by_group", "by_provider", "by_model",
        )}
        expected["budgets"] = baseline["budgets"]
        env = {k: v for k, v in os.environ.items()
               if not k.startswith(("SANDHI_", "SENTINELPASS_", "AGENTBROWSER_"))}
        env.update(SANDHI_AGENTBROWSER_ROOT=str(sibling),
                   SANDHI_SMOKE_ORIGIN=str(gateway.client.base_url),
                   SANDHI_SMOKE_ADMIN_TOKEN="recovery-test-admin", SANDHI_SMOKE_EXPECTED_SCOPE=SCOPES[0],
                   SANDHI_SMOKE_EXPECTED_EVIDENCE=json.dumps(expected))
        result = subprocess.run(["node", str(Path(__file__).with_name("agentbrowser-smoke.mjs"))],
                                cwd=tmp_path, env=env, capture_output=True, text=True, timeout=90)
        assert result.returncode == 0, result.stdout + result.stderr
        assert "AgentBrowser smoke passed" in result.stdout
        assert "recovery-test-admin" not in result.stdout + result.stderr
        # These values remain present elsewhere on the page. Incorrect card, keyed
        # attribution and budget associations must still fail, not match any text.
        for fault in ("card", "attribution", "budget"):
            wrong = copy.deepcopy(expected)
            if fault == "card":
                wrong["total"]["calls"] = wrong["total"]["billable_tokens"]
            elif fault == "attribution":
                wrong["by_group"][0]["calls"] = wrong["total"]["calls"]
            else:
                wrong["budgets"][0]["spent"] = wrong["total"]["billable_tokens"]
            env["SANDHI_SMOKE_EXPECTED_EVIDENCE"] = json.dumps(wrong)
            negative = subprocess.run(["node", str(Path(__file__).with_name("agentbrowser-smoke.mjs"))],
                                      cwd=tmp_path, env=env, capture_output=True, text=True, timeout=90)
            assert negative.returncode != 0, f"incorrect {fault} evidence was accepted"
            stage = {"card": "cards", "attribution": "attribution", "budget": "budgets"}[fault]
            assert f"restored dashboard numeric evidence mismatch: {stage}" in negative.stderr
            assert "AgentBrowser smoke passed" not in negative.stdout
            assert "recovery-test-admin" not in negative.stdout + negative.stderr
        env["SANDHI_SMOKE_EXPECTED_EVIDENCE"] = json.dumps(expected)
        for fault, stage in (("heading", "attribution"), ("hidden", "visibility")):
            with altered_dashboard(str(gateway.client.base_url).rstrip('/'), fault) as altered:
                env["SANDHI_SMOKE_ORIGIN"] = altered
                negative = subprocess.run(["node", str(Path(__file__).with_name("agentbrowser-smoke.mjs"))],
                                          cwd=tmp_path, env=env, capture_output=True, text=True, timeout=90)
                assert negative.returncode != 0, f"incorrect {fault} DOM was accepted"
                assert f"restored dashboard numeric evidence mismatch: {stage}" in negative.stderr
                assert "AgentBrowser smoke passed" not in negative.stdout
                assert "recovery-test-admin" not in negative.stdout + negative.stderr
        assert public_evidence(gateway.client) == baseline
        gateway.stop()
