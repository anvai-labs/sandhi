"""ONNX export parity tests; optional development dependencies only."""

import importlib.util
import json
from pathlib import Path

import numpy as np
import pytest

from policy_text_worker import Model

ort = pytest.importorskip("onnxruntime")
pytest.importorskip("onnx")

ROOT = Path(__file__).resolve().parent
spec = importlib.util.spec_from_file_location(
    "exporter", ROOT / "policy_onnx_export.py"
)
exporter = importlib.util.module_from_spec(spec)
spec.loader.exec_module(exporter)

ARTIFACT = json.dumps(
    {
        "version": 1,
        "kind": "tfidf_reference",
        "references": ["restricted customer records", "private account credentials"],
        "max_features": 256,
    }
).encode()


def test_exported_onnx_matches_python_model_on_identical_text_views():
    model_bytes, descriptor = exporter.build(ARTIFACT)
    options = ort.SessionOptions()
    options.intra_op_num_threads = 1
    session = ort.InferenceSession(
        model_bytes, options, providers=["CPUExecutionProvider"]
    )
    model = Model(ARTIFACT)
    for text, joined in [
        ("restricted customer records", ""),
        ("weather forecast", ""),
        ("PRIVATE account credentials", ""),
        ("restr", "restricted customer records"),
        ("customer\nrecords", "customerrecords"),
        ("_private_ customer_42 7 a", ""),
        ("how to protect private account credentials", ""),
    ]:
        counts = exporter.counts(text, joined, descriptor)
        score = float(
            session.run(None, {"counts": np.asarray(counts, dtype=np.float32)})[0][0]
        )
        assert abs(score - model.score(text, joined)) < 1e-6


def test_export_profile_rejects_unknown_unicode_instead_of_silent_token_drift():
    _, descriptor = exporter.build(ARTIFACT)
    with pytest.raises(ValueError):
        exporter.counts("privaté", "", descriptor)
    with pytest.raises(ValueError):
        exporter.counts("x" * 262145, "", descriptor)
