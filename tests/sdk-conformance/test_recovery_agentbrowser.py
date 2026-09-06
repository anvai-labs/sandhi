"""Real AgentBrowser reads only restored synthetic data; no live vault or browser profile."""

import os
from pathlib import Path
import subprocess

import pytest

from recovery_snapshot import restore, snapshot
from test_recovery import (  # noqa: F401 - reuse scoped fixture definitions
    SCOPES, gated_provider, public_evidence, recovery_gateway, seed,
)


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
        env = {k: v for k, v in os.environ.items()
               if not k.startswith(("SANDHI_", "SENTINELPASS_", "AGENTBROWSER_"))}
        env.update(SANDHI_AGENTBROWSER_ROOT=str(sibling),
                   SANDHI_SMOKE_ORIGIN=str(gateway.client.base_url),
                   SANDHI_SMOKE_ADMIN_TOKEN="recovery-test-admin", SANDHI_SMOKE_EXPECTED_SCOPE=SCOPES[0])
        result = subprocess.run(["node", str(Path(__file__).with_name("agentbrowser-smoke.mjs"))],
                                cwd=tmp_path, env=env, capture_output=True, text=True, timeout=90)
        assert result.returncode == 0, result.stdout + result.stderr
        assert "AgentBrowser smoke passed" in result.stdout
        assert "recovery-test-admin" not in result.stdout + result.stderr
        assert public_evidence(gateway.client) == baseline
        gateway.stop()
