#!/usr/bin/env python3
"""Build or install Sandhi's optional synthetic-trained, audit-only text model.

Training/export is offline Python. Serving uses the Rust/ONNX CPU adapter only.
"""

import argparse
import hashlib
import json
from pathlib import Path

PROFILE = "utf8_byte_trigrams_v1"
NAME = "onnx.sensitive_text.v1"
BINS = 512


def counts(text):
    data = text.encode("utf-8")
    if len(data) > 262144:
        raise ValueError("input exceeds byte bound")
    data = data.lower()  # bytes.lower: ASCII only; UTF-8 bytes remain unchanged.
    result = [0.0] * BINS
    for i in range(max(0, len(data) - 2)):
        value = 2166136261
        for byte in data[i : i + 3]:
            value = ((value ^ byte) * 16777619) & 0xFFFFFFFF
        result[value % BINS] += 1.0
    return result


def policy():
    return {
        "schema_version": "1",
        "revision": 1,
        "deadline_ms": 200,
        "max_body_bytes": 262144,
        "rules": [
            {
                "id": "block-private-key",
                "effect": "block",
                "evaluator": {
                    "kind": "regex",
                    "pattern": "-----BEGIN (?:RSA |EC |OPENSSH |ENCRYPTED )?PRIVATE KEY-----",
                },
            },
            {
                "id": "block-provider-token",
                "effect": "block",
                "evaluator": {
                    "kind": "regex",
                    "pattern": r"\b(?:sk-(?:proj-|svcacct-)[A-Za-z0-9_-]{32,}|gh[pousr]_[A-Za-z0-9]{36,})\b",
                },
            },
            {
                "id": "audit-email",
                "effect": "audit",
                "evaluator": {
                    "kind": "regex",
                    "pattern": r"(?i)\b[A-Z0-9._%+-]+@[A-Z0-9.-]+\.[A-Z]{2,}\b",
                },
            },
            {
                "id": "audit-sensitive-text",
                "effect": "audit",
                "evaluator": {
                    "kind": "registered",
                    "name": NAME,
                    "configuration": {"at_least": 0.75},
                },
            },
        ],
    }


def corpus():
    """Synthetic examples, no customer data. Templates are training scaffolding."""
    sensitive = [
        "Customer {name} social security number is 123-45-{n:04d}.",
        "Bank account for {name}: {n:012d}, routing number 021000021.",
        "Patient {name} diagnosis diabetes, medical record {n}.",
        "Employee {name} salary is {n} dollars, payroll and home address attached.",
        "Private credentials for {name}: password ExampleSecret{n}!",
        "Confidential customer records: {name}, date of birth 1985-04-12, passport {n}.",
        "Access token and recovery code for {name}: secret-{n}-do-not-share.",
        "Credit card owner {name}, number 4111111111111111, security code {n:03d}.",
    ]
    benign = [
        "Explain how {name} can protect a password without showing any credentials.",
        "Write a tutorial on bank account validation and privacy for {name}.",
        "Summarize public medical research about diabetes for {name}.",
        "Generate a weather forecast for day {n} for {name}.",
        "Review this Python sorting function for {name}, complexity {n}.",
        "How do I redact confidential records before sending them to an API?",
        "Document the OAuth token format and use placeholders in examples.",
        "Translate this greeting for {name}: Bonjour, नमस्ते, 你好, مرحبا.",
    ]
    rows = []
    for label, templates in [(1, sensitive), (0, benign)]:
        for template in templates:
            for i, name in enumerate(
                ["Alex Chen", "Casey Morgan", "Robin Lee", "Jamie Patel"]
            ):
                for n in [137 + i, 284 + i, 593 + i, 826 + i]:
                    rows.append((template.format(name=name, n=n), label))
    return rows


HOLDOUT = [
    ("Dana Park social security number: 987-65-4321", 1),
    ("Patient Dana Park has diabetes. Medical record number 445566.", 1),
    ("Dana Park bank account 098765432100 and routing number 021000021", 1),
    ("Private credentials: password ZebraExample777! for Dana Park", 1),
    ("Explain OAuth without using real tokens", 0),
    ("How can I redact a social security number safely?", 0),
    ("Write a sorting algorithm in Rust", 0),
    ("你好，今天的天气如何？", 0),
    ("Bonjour, quelle heure est-il?", 0),
    ("What does confidential mean in a document?", 0),
]


def build(output):
    import numpy as np
    import onnx
    from onnx import TensorProto, helper, numpy_helper
    from sklearn.linear_model import LogisticRegression

    rows = corpus()
    matrix = np.asarray([counts(text) for text, _ in rows], dtype=np.float32)
    matrix /= np.maximum(np.linalg.norm(matrix, axis=1, keepdims=True), 1e-12)
    classifier = LogisticRegression(
        C=8, solver="liblinear", random_state=0, max_iter=1000
    )
    classifier.fit(matrix, [label for _, label in rows])
    weights = classifier.coef_.astype(np.float32).T
    bias = classifier.intercept_.astype(np.float32)
    nodes = [
        helper.make_node("ReduceL2", ["counts"], ["norm"], axes=[1], keepdims=1),
        helper.make_node("Max", ["norm", "epsilon"], ["denominator"]),
        helper.make_node("Div", ["counts", "denominator"], ["normalized"]),
        helper.make_node("MatMul", ["normalized", "weights"], ["linear"]),
        helper.make_node("Add", ["linear", "bias"], ["logits"]),
        helper.make_node("Sigmoid", ["logits"], ["scores"]),
        helper.make_node("ReduceMax", ["scores"], ["score"], axes=[0], keepdims=0),
    ]
    graph = helper.make_graph(
        nodes,
        NAME,
        [helper.make_tensor_value_info("counts", TensorProto.FLOAT, [2, BINS])],
        [helper.make_tensor_value_info("score", TensorProto.FLOAT, [1])],
        [
            numpy_helper.from_array(weights, "weights"),
            numpy_helper.from_array(bias, "bias"),
            numpy_helper.from_array(np.array([1e-12], dtype=np.float32), "epsilon"),
        ],
    )
    model = helper.make_model(
        graph, opset_imports=[helper.make_opsetid("", 13)], ir_version=8
    )
    onnx.checker.check_model(model, full_check=True)
    output.mkdir(mode=0o700)

    def write(name, value):
        (output / name).write_text(
            json.dumps(value, indent=2, ensure_ascii=False) + "\n"
        )

    (output / "model.onnx").write_bytes(model.SerializeToString())
    write(
        "preprocessing.json",
        {
            "version": 1,
            "profile": PROFILE,
            "vocabulary": [f"bin{i:03d}" for i in range(BINS)],
        },
    )
    write("policy.json", policy())
    golden = []
    for text, label in HOLDOUT + [("", 0), ("ABC", 0), ("é界", 0)]:
        x = np.asarray(counts(text), dtype=np.float32)
        x /= max(float(np.linalg.norm(x)), 1e-12)
        score = float(1 / (1 + np.exp(-(x @ weights[:, 0] + bias[0]))))
        golden.append({"text": text, "joined": text, "label": label, "score": score})
    write("golden.json", golden)
    write(
        "model-card.json",
        {
            "name": NAME,
            "profile": PROFILE,
            "training_examples": len(rows),
            "source": "repository-owned synthetic templates; no private data",
            "seed": 0,
            "action": "audit",
            "threshold": 0.75,
            "model_bytes": len(model.SerializeToString()),
            "limits": [
                "Unvalidated on real user traffic; audit-only classifier",
                "No guarantee for paraphrases, obfuscation, long mixed text or non-English content",
                "A score is not a calibrated probability; synthetic holdout is a smoke test",
            ],
        },
    )
    write(
        "bundle.json",
        {
            "version": 1,
            "files": {
                name: hashlib.sha256((output / name).read_bytes()).hexdigest()
                for name in [
                    "model.onnx",
                    "preprocessing.json",
                    "policy.json",
                    "golden.json",
                    "model-card.json",
                ]
            },
        },
    )


def prepare(bundle, runtime, output):
    metadata = json.loads((bundle / "bundle.json").read_text())
    names = {
        "model.onnx",
        "preprocessing.json",
        "policy.json",
        "golden.json",
        "model-card.json",
    }
    if metadata.get("version") != 1 or set(metadata.get("files", {})) != names:
        raise ValueError("invalid bundle inventory")
    verified = {}
    for name in names:
        path = bundle / name
        if path.is_symlink() or path.stat().st_size > 8388608:
            raise ValueError("invalid bundle artifact")
        data = path.read_bytes()
        if hashlib.sha256(data).hexdigest() != metadata["files"][name]:
            raise ValueError("bundle digest mismatch")
        verified[name] = data
    runtime = runtime.resolve(strict=True)
    if not runtime.is_file() or runtime.stat().st_size > 268435456:
        raise ValueError("invalid runtime")
    with runtime.open("rb") as source:
        runtime_digest = hashlib.file_digest(source, "sha256").hexdigest()
    output = output.absolute()
    output.mkdir(mode=0o700)  # existing paths are refused, including symlinks.
    for name, data in verified.items():
        (output / name).write_bytes(data)
        (output / name).chmod(0o600)
    manifest = {
        "version": 1,
        "runtime_library": str(runtime),
        "runtime_sha256": runtime_digest,
        "models": [
            {
                "name": NAME,
                "model": str(output / "model.onnx"),
                "model_sha256": metadata["files"]["model.onnx"],
                "preprocessing": str(output / "preprocessing.json"),
                "preprocessing_sha256": metadata["files"]["preprocessing.json"],
            }
        ],
    }
    (output / "deployment.json").write_text(json.dumps(manifest, indent=2) + "\n")
    (output / "deployment.json").chmod(0o600)
    return {
        "SANDHI_POLICY_CONFIG": str(output / "policy.json"),
        "SANDHI_POLICY_ONNX": str(output / "deployment.json"),
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="command", required=True)
    builder = sub.add_parser("build")
    builder.add_argument("--output", type=Path, required=True)
    installer = sub.add_parser("prepare")
    installer.add_argument(
        "--bundle",
        type=Path,
        default=Path(__file__).resolve().parents[1]
        / "crates/sandhi-proxy/assets/sensitive-text-v1",
    )
    installer.add_argument("--runtime-library", type=Path, required=True)
    installer.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    if args.command == "build":
        build(args.output)
    else:
        print(json.dumps(prepare(args.bundle, args.runtime_library, args.output)))


if __name__ == "__main__":
    main()
