"""Pinned scikit-learn text evaluator over private stdin/stdout IPC.

Trusted deployment code only. No model download, pickle, MLflow, identity, or
provider credentials. The proxy enforces the wall deadline and kills failures.
"""

import hashlib
import json
import math
import sys
from pathlib import Path

MAX_FRAME = 2_097_152
MAX_TEXT = 262_144


def strict_json(data):
    def pairs(items):
        result = {}
        for key, value in items:
            if key in result:
                raise ValueError("duplicate field")
            result[key] = value
        return result

    def invalid(_):
        raise ValueError("nonfinite number")

    return json.loads(data, object_pairs_hook=pairs, parse_constant=invalid)


class Model:
    def __init__(self, data):
        if len(data) > 1_048_576:
            raise ValueError("artifact too large")
        spec = strict_json(data)
        if not isinstance(spec, dict) or set(spec) != {
            "version",
            "kind",
            "references",
            "max_features",
        }:
            raise ValueError("artifact contract")
        references = spec["references"]
        if type(spec["version"]) is not int or spec["version"] != 1:
            raise ValueError("artifact version")
        if spec["kind"] != "tfidf_reference":
            raise ValueError("unsupported model")
        if (
            type(spec["max_features"]) is not int
            or not 1 <= spec["max_features"] <= 4096
        ):
            raise ValueError("feature bound")
        if not isinstance(references, list) or not 1 <= len(references) <= 256:
            raise ValueError("reference count")
        if any(
            not isinstance(s, str) or not s or len(s.encode()) > 4096
            for s in references
        ):
            raise ValueError("reference bound")
        # Imported once during readiness; never initialized/downloaded per request.
        from sklearn.feature_extraction.text import TfidfVectorizer
        from sklearn.metrics.pairwise import cosine_similarity

        self.vectorizer = TfidfVectorizer(max_features=spec["max_features"], norm="l2")
        self.references = self.vectorizer.fit_transform(references)
        self.cosine = cosine_similarity
        # Exercise both known and unknown terms before announcing readiness.
        self.score(references[0], "")
        self.score("", "")

    def score(self, text, joined):
        if any(
            not isinstance(s, str) or len(s.encode()) > MAX_TEXT for s in (text, joined)
        ):
            raise ValueError("text bound")
        vectors = self.vectorizer.transform([text, joined])
        score = float(self.cosine(vectors, self.references).max())
        if not math.isfinite(score):
            raise ValueError("nonfinite score")
        return max(0.0, min(1.0, score))


def serve(path, expected_digest, source, destination):
    with Path(path).open("rb") as file:
        data = file.read(1_048_577)
    digest = hashlib.sha256(data).hexdigest()
    if digest != expected_digest:
        raise ValueError("artifact digest")
    model = Model(data)

    def emit(value):
        destination.write(
            json.dumps(value, allow_nan=False, separators=(",", ":")) + "\n"
        )
        destination.flush()

    emit({"version": 1, "kind": "ready", "artifact_sha256": digest})
    while True:
        line = source.readline(MAX_FRAME + 1)
        if not line:
            return
        if len(line) > MAX_FRAME or not line.endswith(b"\n"):
            raise ValueError("frame bound")
        request = strict_json(line)
        if not isinstance(request, dict) or set(request) != {
            "version",
            "id",
            "text",
            "joined",
        }:
            raise ValueError("request contract")
        if type(request["version"]) is not int or request["version"] != 1:
            raise ValueError("request version")
        identifier = request["id"]
        if (
            not isinstance(identifier, str)
            or not identifier.isascii()
            or not identifier.isdigit()
            or len(identifier) > 20
        ):
            raise ValueError("request id")
        emit(
            {
                "version": 1,
                "id": identifier,
                "score": model.score(request["text"], request["joined"]),
            }
        )


if __name__ == "__main__":
    try:
        if len(sys.argv) != 3:
            raise ValueError("usage")
        serve(sys.argv[1], sys.argv[2], sys.stdin.buffer, sys.stdout)
    except Exception:  # noqa: BLE001 - redact every third-party model failure
        # No exception text, model data or prompt in stdout/stderr.
        raise SystemExit(2) from None
