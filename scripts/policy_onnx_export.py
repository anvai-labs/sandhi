"""Export the bounded TF-IDF reference evaluator to an embedded ONNX deployment."""

import argparse
import hashlib
import json
import re
from pathlib import Path

from policy_text_worker import Model, strict_json


def counts(text, joined, descriptor):
    words = {word: i for i, word in enumerate(descriptor["vocabulary"])}
    result = []
    for value in (text, joined):
        if not isinstance(value, str) or not value.isascii() or len(value) > 262144:
            raise ValueError("ASCII profile coverage")
        row = [0.0] * len(words)
        for word in re.findall(r"[a-z0-9_]{2,}", value.lower()):
            if word in words:
                row[words[word]] += 1.0
        result.append(row)
    return result


def build(artifact):
    import numpy as np
    import onnx
    from onnx import TensorProto, helper, numpy_helper

    model = Model(artifact)
    vocabulary = list(model.vectorizer.get_feature_names_out())
    if any(not re.fullmatch(r"[a-z0-9_]{2,256}", word) for word in vocabulary):
        raise ValueError("export requires ASCII vocabulary")
    descriptor = {
        "version": 1,
        "profile": "ascii_word_counts_v1",
        "vocabulary": vocabulary,
    }
    initializers = [
        numpy_helper.from_array(
            np.asarray(model.vectorizer.idf_, dtype=np.float32), "idf"
        ),
        numpy_helper.from_array(
            model.references.toarray().astype(np.float32).T, "references"
        ),
        numpy_helper.from_array(np.asarray([1], dtype=np.int64), "axis"),
        numpy_helper.from_array(np.asarray([1], dtype=np.int64), "output_shape"),
        numpy_helper.from_array(np.asarray(1e-12, dtype=np.float32), "epsilon"),
        numpy_helper.from_array(np.asarray(0.0, dtype=np.float32), "zero"),
        numpy_helper.from_array(np.asarray(1.0, dtype=np.float32), "one"),
    ]
    nodes = [
        helper.make_node("Mul", ["counts", "idf"], ["weighted"]),
        helper.make_node("Mul", ["weighted", "weighted"], ["squares"]),
        helper.make_node("ReduceSum", ["squares", "axis"], ["sums"], keepdims=1),
        helper.make_node("Sqrt", ["sums"], ["lengths"]),
        helper.make_node("Max", ["lengths", "epsilon"], ["denominator"]),
        helper.make_node("Div", ["weighted", "denominator"], ["normalized"]),
        helper.make_node("MatMul", ["normalized", "references"], ["similarities"]),
        helper.make_node("ReduceMax", ["similarities"], ["maximum"], keepdims=0),
        helper.make_node("Clip", ["maximum", "zero", "one"], ["bounded"]),
        helper.make_node("Reshape", ["bounded", "output_shape"], ["score"]),
    ]
    graph = helper.make_graph(
        nodes,
        "sandhi_tfidf_reference",
        [
            helper.make_tensor_value_info(
                "counts", TensorProto.FLOAT, [2, len(vocabulary)]
            )
        ],
        [helper.make_tensor_value_info("score", TensorProto.FLOAT, [1])],
        initializers,
    )
    exported = helper.make_model(
        graph, opset_imports=[helper.make_opsetid("", 13)], ir_version=8
    )
    onnx.checker.check_model(exported, full_check=True)
    result = exported.SerializeToString()
    if len(result) > 8388608:
        raise ValueError("model byte bound")
    return result, descriptor


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--artifact", required=True, type=Path)
    parser.add_argument("--runtime-library", required=True, type=Path)
    parser.add_argument("--output-dir", required=True, type=Path)
    parser.add_argument("--corpus", type=Path)
    args = parser.parse_args()
    with args.artifact.open("rb") as source:
        artifact = source.read(1048577)
    model, descriptor = build(artifact)
    cases = [
        ("restricted customer records", ""),
        ("weather forecast", ""),
        ("PRIVATE account credentials", ""),
        ("restr", "restricted customer records"),
        ("customer\nrecords", "customerrecords"),
        ("_private_ customer_42 7 a", ""),
    ]
    if args.corpus:
        with args.corpus.open("rb") as source:
            raw = source.read(8388609)
        if len(raw) > 8388608:
            raise ValueError("corpus byte bound")
        records = strict_json(raw)
        if not isinstance(records, list) or len(records) > 10000:
            raise ValueError("corpus bound")
        cases += [(record["text"], "") for record in records]
    reference = Model(artifact)
    golden = []
    for text, joined in cases:
        counts(text, joined, descriptor)
        golden.append(
            {"text": text, "joined": joined, "score": reference.score(text, joined)}
        )
    runtime = args.runtime_library.resolve(strict=True)
    with runtime.open("rb") as source:
        runtime_digest = hashlib.file_digest(source, "sha256").hexdigest()
    root = args.output_dir.absolute()
    root.mkdir(mode=0o700)
    model_path = root / "model.onnx"
    preprocessing = root / "preprocessing.json"
    model_path.write_bytes(model)
    preprocessing.write_text(json.dumps(descriptor, indent=2) + "\n")
    (root / "golden.json").write_text(json.dumps(golden, indent=2) + "\n")
    manifest = {
        "version": 1,
        "runtime_library": str(runtime),
        "runtime_sha256": runtime_digest,
        "models": [
            {
                "name": "onnx.tfidf.v1",
                "model": str(model_path),
                "model_sha256": hashlib.sha256(model).hexdigest(),
                "preprocessing": str(preprocessing),
                "preprocessing_sha256": hashlib.sha256(
                    preprocessing.read_bytes()
                ).hexdigest(),
            }
        ],
    }
    (root / "deployment.json").write_text(json.dumps(manifest, indent=2) + "\n")
    # Golden vectors contain corpus text: the whole export directory stays private.
    print(
        json.dumps(
            {"deployment": str(root / "deployment.json"), "golden_cases": len(golden)}
        )
    )


if __name__ == "__main__":
    main()
