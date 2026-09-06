"""Opt-in cross-repository smoke; missing explicitly requested dependencies fail."""

import os
import subprocess
from pathlib import Path

import pytest

from test_dashboard import dashboard  # noqa: F401 — reuse the disposable real gateway fixture


def test_agentbrowser_dashboard(dashboard, tmp_path):
    root = os.environ.get("SANDHI_AGENTBROWSER_ROOT")
    if not root:
        pytest.skip("set SANDHI_AGENTBROWSER_ROOT to a built AgentBrowser checkout")
    root = Path(root).resolve()
    assert (root / "packages/api/dist/index.js").is_file(), "build AgentBrowser before running"
    env = {k: v for k, v in os.environ.items() if not k.startswith(("SANDHI_", "SENTINELPASS_", "AGENTBROWSER_"))}
    env.update(SANDHI_AGENTBROWSER_ROOT=str(root), SANDHI_SMOKE_ORIGIN=dashboard.base)
    result = subprocess.run(
        ["node", str(Path(__file__).with_name("agentbrowser-smoke.mjs"))],
        cwd=tmp_path, env=env, capture_output=True, text=True, timeout=90,
    )
    assert result.returncode == 0, result.stdout + result.stderr
    assert "AgentBrowser smoke passed" in result.stdout
