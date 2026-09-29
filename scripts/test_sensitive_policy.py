import importlib.util
import json
from pathlib import Path
import shutil

import numpy as np
import onnxruntime as ort
import pytest

ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location(
    "sensitive_policy", ROOT / "scripts/sensitive_policy.py"
)
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)
BUNDLE = ROOT / "crates/sandhi-proxy/assets/sensitive-text-v1"


def test_preprocessing_is_bounded_and_preserves_utf8_bytes():
    assert module.counts("ABC") == module.counts("abc")
    assert sum(module.counts("é界")) == 3
    assert sum(module.counts("")) == 0
    with pytest.raises(ValueError):
        module.counts("a" * 262145)


def test_shipped_onnx_matches_offline_golden_scores():
    session = ort.InferenceSession(
        str(BUNDLE / "model.onnx"), providers=["CPUExecutionProvider"]
    )
    for case in json.loads((BUNDLE / "golden.json").read_text()):
        values = np.array(
            [module.counts(case["text"]), module.counts(case["joined"])],
            dtype=np.float32,
        )
        score = session.run(None, {"counts": values})[0].item()
        assert abs(score - case["score"]) < 1e-6


def test_preparation_pins_artifacts_and_refuses_overwrite(tmp_path):
    runtime = tmp_path / "runtime"
    runtime.write_bytes(b"synthetic runtime; loader separately verifies real ORT")
    output = tmp_path / "deployment"
    settings = module.prepare(BUNDLE, runtime, output)
    manifest = json.loads(Path(settings["SANDHI_POLICY_ONNX"]).read_text())
    assert manifest["models"][0]["name"] == module.NAME
    assert output.stat().st_mode & 0o777 == 0o700
    assert (output / "deployment.json").stat().st_mode & 0o777 == 0o600
    with pytest.raises(FileExistsError):
        module.prepare(BUNDLE, runtime, output)
    assert (
        json.loads((output / "policy.json").read_text())["rules"][-1]["effect"]
        == "audit"
    )


def test_tampered_bundle_is_rejected_before_creating_deployment(tmp_path):
    bundle = tmp_path / "bundle"
    shutil.copytree(BUNDLE, bundle)
    (bundle / "model.onnx").write_bytes(b"tampered")
    with pytest.raises(ValueError, match="digest"):
        module.prepare(bundle, tmp_path / "runtime", tmp_path / "output")
    assert not (tmp_path / "output").exists()


def test_audit_classifier_and_narrow_blocks_are_explicit():
    policy = json.loads((BUNDLE / "policy.json").read_text())
    assert policy == module.policy()
    assert [r["id"] for r in policy["rules"] if r["effect"] == "block"] == [
        "block-private-key",
        "block-provider-token",
    ]
