"""Local worker compatible with Sandhi's existing supervised Python adapter."""

import contextlib
import json
import os
import sys

from .protocol import MAX_FRAME, checked_bytes, request_view, score, strict_json


def serve(artifact, digest, source, destination):
    data = checked_bytes(artifact, digest, 1048576)
    # Libraries and business logic may print; never let that corrupt the wire or
    # expose prompts in service logs. Native writes still cause fail-closed IPC.
    with open(os.devnull, "w") as sink, contextlib.redirect_stdout(
        sink
    ), contextlib.redirect_stderr(sink):
        from .evaluate import Evaluator

        evaluator = Evaluator(strict_json(data))
        evaluator.self_test()

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
            request = request_view(strict_json(line))
            value = score(evaluator.score(request["text"], request["joined"]))
            emit({"version": 1, "id": request["id"], "score": value})
            del (
                line,
                request,
                value,
            )  # Avoid retaining the previous request during idle reads.


def main(args):
    if len(args) != 2:
        raise ValueError("worker arguments")
    serve(args[0], args[1], sys.stdin.buffer, sys.stdout)
