"""Zipapp entrypoint. HTTP dependencies are only imported in HTTP mode."""

import sys

try:
    if len(sys.argv) == 3 and sys.argv[1] == "--http":
        from evaluator_app.http_service import run

        run(sys.argv[2])
    else:
        from evaluator_app.worker import main

        main(sys.argv[1:])
except Exception:  # noqa: BLE001 - do not disclose model/configuration data
    raise SystemExit(2) from None
