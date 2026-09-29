"""Authenticated HTTP wrapper; policy and user authorization stay in Sandhi."""

import atexit
import hmac
import time
import ssl

from .protocol import MAX_FRAME, request_view, strict_json
from .settings import ServiceSettings, Settings, read_auth_key
from .supervisor import Supervisor, Unavailable


class Evaluators:
    def __init__(self, models):
        self.pools = {}
        try:
            for name, model in models.items():
                self.pools[name] = Supervisor(model)
        except BaseException:
            self.close()
            raise

    @property
    def ready(self):
        return all(pool.ready for pool in self.pools.values())

    @property
    def pids(self):
        return tuple(pid for pool in self.pools.values() for pid in pool.pids)

    def close(self):
        for pool in self.pools.values():
            pool.close()


def create_app(settings):
    from flask import Flask, jsonify, request
    from werkzeug.exceptions import HTTPException

    if isinstance(settings, Settings):
        settings = ServiceSettings(
            settings.auth_file, settings.port, {settings.name: settings}
        )
    key = read_auth_key(settings.auth_file)
    supervisor = Evaluators(settings.evaluators)
    atexit.register(supervisor.close)
    app = Flask(__name__, static_folder=None)
    app.config.update(MAX_CONTENT_LENGTH=MAX_FRAME, PROPAGATE_EXCEPTIONS=False)
    app.extensions["evaluator"] = supervisor

    def failure(code, status):
        return jsonify({"error": {"code": code}}), status

    def authenticated():
        value = request.headers.get("Authorization", "").encode()
        return len(value) <= 256 and hmac.compare_digest(value, b"Bearer " + key)

    @app.after_request
    def private_response(response):
        response.headers["Cache-Control"] = "no-store"
        response.headers["X-Content-Type-Options"] = "nosniff"
        return response

    @app.errorhandler(Exception)
    def error(error):
        # Never log or echo exception strings, request payloads or detected content.
        if isinstance(error, HTTPException):
            return failure("invalid_http_request", error.code)
        return failure("evaluation_unavailable", 503)

    @app.get("/healthz")
    def health():
        return jsonify({"alive": True})

    @app.get("/readyz")
    def ready():
        if not authenticated():
            return failure("unauthorized", 401)
        if not supervisor.ready:
            return failure("evaluation_unavailable", 503)
        return jsonify({"ready": True})

    @app.get("/v1/evaluators/<name>/readyz")
    def model_ready(name):
        if not authenticated():
            return failure("unauthorized", 401)
        model = settings.evaluators.get(name)
        if model is None:
            return failure("unknown_evaluator", 404)
        if not supervisor.pools[name].ready:
            return failure("evaluation_unavailable", 503)
        return jsonify(
            {
                "ready": True,
                "evaluator": name,
                "artifact_sha256": model.artifact_sha256,
                "code_sha256": model.bundle_sha256,
            }
        )

    @app.get("/v1/evaluators/<name>/status")
    def model_status(name):
        if not authenticated():
            return failure("unauthorized", 401)
        model = settings.evaluators.get(name)
        if model is None:
            return failure("unknown_evaluator", 404)
        return jsonify(
            {
                "version": 1,
                "evaluator": name,
                "artifact_sha256": model.artifact_sha256,
                "code_sha256": model.bundle_sha256,
                **supervisor.pools[name].status(),
            }
        )

    @app.post("/v1/evaluators/<name>/evaluate")
    def evaluate(name):
        started = time.monotonic()
        if not authenticated():
            return failure("unauthorized", 401)
        model = settings.evaluators.get(name)
        if model is None:
            return failure("unknown_evaluator", 404)
        if (
            not request.is_json
            or request.headers.get("Content-Encoding", "identity") != "identity"
        ):
            return failure("unsupported_media_type", 415)
        raw = request.get_data(cache=False)
        try:
            value = strict_json(raw)
            if not isinstance(value, dict) or set(value) != {
                "version",
                "id",
                "text",
                "joined",
                "timeout_ms",
                "evaluator",
                "artifact_sha256",
                "code_sha256",
            }:
                raise ValueError("request contract")
            timeout = value["timeout_ms"]
            if (
                type(timeout) is not int
                or not 1 <= timeout <= model.evaluation_timeout_ms
            ):
                raise ValueError("deadline")
            view = request_view(
                {k: value[k] for k in ("version", "id", "text", "joined")}
            )
        except (ValueError, TypeError, KeyError, UnicodeError):
            return failure("invalid_request", 400)
        if (
            value["evaluator"] != model.name
            or value["artifact_sha256"] != model.artifact_sha256
            or value["code_sha256"] != model.bundle_sha256
        ):
            return failure("deployment_mismatch", 409)
        try:
            answer = supervisor.pools[name].evaluate(view, started + timeout / 1000)
        except Unavailable:
            return failure("evaluation_unavailable", 503)
        answer.update(
            evaluator=model.name,
            artifact_sha256=model.artifact_sha256,
            code_sha256=model.bundle_sha256,
        )
        return jsonify(answer)

    return app


def run(path):
    # No Flask development server, debug mode, auto-reloader or --preload.
    # Gunicorn loads one app/pool inside each serving process after fork.
    from gunicorn.app.base import BaseApplication

    settings = ServiceSettings.load(path)

    def worker_exit(server, worker):
        app = getattr(worker, "wsgi", None)
        if app is not None:
            app.extensions["evaluator"].close()

    class Application(BaseApplication):
        def load_config(self):
            for key, value in {
                "bind": (
                    f"[{settings.bind_host}]:{settings.port}"
                    if ":" in settings.bind_host
                    else f"{settings.bind_host}:{settings.port}"
                ),
                "certfile": settings.tls_certfile,
                "keyfile": settings.tls_keyfile,
                "ca_certs": settings.tls_cafile,
                "cert_reqs": (
                    ssl.CERT_REQUIRED if settings.tls_cafile else ssl.CERT_NONE
                ),
                "workers": 1,
                "worker_class": "gthread",
                "threads": 4,
                "worker_connections": 16,
                "backlog": 16,
                "timeout": 20,
                "graceful_timeout": 15,
                "keepalive": 2,
                "preload_app": False,
                "accesslog": None,
                "errorlog": "-",
                "loglevel": "warning",
                "worker_exit": worker_exit,
                "limit_request_line": 2048,
                "limit_request_fields": 32,
                "limit_request_field_size": 2048,
            }.items():
                self.cfg.set(key, value)
            # Gunicorn 26 introduced an optional management socket. This service
            # has one deliberate HTTP listener and no runtime management endpoint.
            if "control_socket_disable" in self.cfg.settings:
                self.cfg.set("control_socket_disable", True)

        def load(self):
            return create_app(settings)

    Application().run()
