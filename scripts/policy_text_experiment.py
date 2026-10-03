"""Offline synthetic/labeled-corpus evaluation. Tracking is explicitly opt-in."""

import argparse
import hashlib
import json
import math
import time
from pathlib import Path

from policy_text_worker import Model, strict_json


def evaluate(artifact, records, threshold):
    if (
        type(threshold) not in (int, float)
        or not math.isfinite(threshold)
        or not 0 <= threshold <= 1
    ):
        raise ValueError("threshold")
    if not isinstance(records, list) or not 1 <= len(records) <= 10_000:
        raise ValueError("corpus bound")
    model = Model(artifact)
    counts = {
        "true_positives": 0,
        "true_negatives": 0,
        "false_positives": 0,
        "false_negatives": 0,
    }
    latency = []
    for record in records:
        if (
            not isinstance(record, dict)
            or set(record) != {"text", "sensitive"}
            or type(record["sensitive"]) is not bool
        ):
            raise ValueError("corpus contract")
        start = time.perf_counter_ns()
        found = model.score(record["text"], "") >= threshold
        latency.append((time.perf_counter_ns() - start) / 1_000_000)
        label = ("true" if found == record["sensitive"] else "false") + (
            "_positives" if found else "_negatives"
        )
        counts[label] += 1
    latency.sort()
    metrics = {**counts, "samples": len(records)}
    for percentile in (50, 95, 99):
        metrics[f"latency_p{percentile}_ms"] = latency[
            math.ceil(percentile * len(latency) / 100) - 1
        ]
    import sklearn

    return {
        "version": 1,
        "evaluator": "sklearn.tfidf.v1",
        "sklearn_version": sklearn.__version__,
        "artifact_sha256": hashlib.sha256(artifact).hexdigest(),
        "corpus_sha256": hashlib.sha256(
            json.dumps(records, sort_keys=True, separators=(",", ":")).encode()
        ).hexdigest(),
        "threshold": threshold,
        "metrics": metrics,
    }


def export_mlflow(report, uri, client=None):
    # The first testbed exports only to an explicit local SQLite tracking store.
    # Remote tracking needs a separately reviewed destination/authentication setup.
    if (
        not isinstance(uri, str)
        or not uri.startswith("sqlite:////")
        or "?" in uri
        or "#" in uri
    ):
        raise ValueError("explicit absolute local SQLite URI required")
    if client is None:
        import mlflow as client
    client.set_tracking_uri(uri)
    client.set_experiment("sandhi-policy-evaluators")
    with client.start_run(run_name="offline-text-evaluation"):
        client.log_params({k: v for k, v in report.items() if k != "metrics"})
        client.log_metrics(report["metrics"])


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--artifact", required=True, type=Path)
    parser.add_argument("--corpus", required=True, type=Path)
    parser.add_argument("--threshold", type=float, default=0.5)
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument("--mlflow-uri")
    args = parser.parse_args()
    with args.artifact.open("rb") as source:
        artifact = source.read(1_048_577)
    with args.corpus.open("rb") as source:
        corpus = source.read(8_388_609)
    if len(corpus) > 8_388_608:
        raise ValueError("corpus byte bound")
    report = evaluate(artifact, strict_json(corpus), args.threshold)
    # Exclusive output avoids silently overwriting a previous experiment result.
    with args.output.open("x") as destination:
        json.dump(report, destination, indent=2, allow_nan=False)
        destination.write("\n")
    if args.mlflow_uri:
        export_mlflow(report, args.mlflow_uri)


if __name__ == "__main__":
    main()
