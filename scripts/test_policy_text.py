"""Golden, protocol, and experiment tests; no live prompts or network."""

import hashlib
import importlib.util
import io
import json
from pathlib import Path

import pytest

ROOT = Path(__file__).resolve().parent


def load(name):
    spec = importlib.util.spec_from_file_location(name, ROOT / (name + ".py"))
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


worker = load("policy_text_worker")
experiment = load("policy_text_experiment")


def artifact():
    return json.dumps(
        {
            "version": 1,
            "kind": "tfidf_reference",
            "references": [
                "restricted customer records",
                "private account credentials",
            ],
            "max_features": 256,
        }
    ).encode()


def test_model_warms_and_matches_without_remote_dependencies():
    model = worker.Model(artifact())
    assert model.score("restricted customer records", "") > 0.99
    assert model.score("weather forecast", "") == 0
    assert model.score("restr", "restricted customer records") > 0.99


@pytest.mark.parametrize(
    "payload",
    [
        b"{}",
        b'{"version":1,"version":1}',
        b'{"version":NaN}',
        json.dumps(
            {"version": 1, "kind": "pickle", "references": ["x"], "max_features": 256}
        ).encode(),
    ],
)
def test_invalid_model_is_rejected(payload):
    with pytest.raises((ValueError, TypeError)):
        worker.Model(payload)


def test_worker_protocol_has_only_normalized_output(tmp_path):
    path = tmp_path / "artifact.json"
    path.write_bytes(artifact())
    digest = hashlib.sha256(artifact()).hexdigest()
    request = {
        "version": 1,
        "id": "1",
        "text": "restricted customer records",
        "joined": "",
    }
    output = io.StringIO()
    worker.serve(
        path, digest, io.BytesIO((json.dumps(request) + "\n").encode()), output
    )
    ready, result = [json.loads(line) for line in output.getvalue().splitlines()]
    assert ready == {"version": 1, "kind": "ready", "artifact_sha256": digest}
    assert set(result) == {"version", "id", "score"}
    assert result["score"] > 0.99
    assert "restricted" not in output.getvalue()


@pytest.mark.parametrize(
    "payload",
    [
        b'{"version":1,"id":"1","text":"x","joined":"","url":"https://invalid"}\n',
        b'{"version":1,"id":"1","text":NaN,"joined":""}\n',
        b"x" * 2097153,
    ],
)
def test_worker_rejects_invalid_input(tmp_path, payload):
    path = tmp_path / "artifact.json"
    path.write_bytes(artifact())
    with pytest.raises((ValueError, TypeError)):
        worker.serve(
            path,
            hashlib.sha256(artifact()).hexdigest(),
            io.BytesIO(payload),
            io.StringIO(),
        )


def test_experiment_reports_metrics_and_digests_without_text():
    records = [
        {"text": "restricted customer records", "sensitive": True},
        {"text": "weather forecast", "sensitive": False},
    ]
    report = experiment.evaluate(artifact(), records, 0.5)
    assert report["metrics"]["false_negatives"] == 0
    assert report["metrics"]["false_positives"] == 0
    assert report["metrics"]["samples"] == 2
    assert "restricted" not in json.dumps(report)
    assert len(report["artifact_sha256"]) == 64


def test_experiment_requires_explicit_local_tracking_and_exports_metadata_only():
    report = experiment.evaluate(
        artifact(), [{"text": "weather", "sensitive": False}], 0.5
    )

    class Run:
        def __enter__(self):
            return self

        def __exit__(self, *args):
            pass

    class Fake:
        def __init__(self):
            self.calls = []

        def set_tracking_uri(self, x):
            self.calls.append(("uri", x))

        def set_experiment(self, x):
            self.calls.append(("experiment", x))

        def start_run(self, **x):
            return Run()

        def log_params(self, x):
            self.calls.append(("params", x))

        def log_metrics(self, x):
            self.calls.append(("metrics", x))

    fake = Fake()
    experiment.export_mlflow(report, "sqlite:////tmp/experiments.db", fake)
    assert any(k == "metrics" for k, v in fake.calls)
    assert "weather" not in json.dumps(fake.calls)
    with pytest.raises(ValueError):
        experiment.export_mlflow(report, "https://external.invalid", fake)


def test_runtime_model_does_not_depend_on_mlflow(monkeypatch):
    import builtins

    original = builtins.__import__

    def unavailable(name, *args, **kwargs):
        if name == "mlflow" or name.startswith("mlflow."):
            raise RuntimeError("tracking unavailable")
        return original(name, *args, **kwargs)

    monkeypatch.setattr(builtins, "__import__", unavailable)
    model = worker.Model(artifact())
    assert model.score("restricted customer records", "") > 0.99
