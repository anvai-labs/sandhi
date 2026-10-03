"""Build a content-addressed worker zipapp and private local deployment configs."""

import hashlib
import io
import json
import os
from pathlib import Path
import secrets
import sys
import zipfile

ROOT = Path(__file__).resolve().parent
FILES = (
    "__main__.py",
    "evaluator_app/__init__.py",
    "evaluator_app/evaluate.py",
    "evaluator_app/protocol.py",
    "evaluator_app/worker.py",
    "evaluator_app/settings.py",
    "evaluator_app/supervisor.py",
    "evaluator_app/http_service.py",
)


def private_write(path, value):
    # Runtime files are created inside a verified private directory.
    temp = path.with_name(path.name + ".new")
    fd = os.open(temp, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
    with os.fdopen(fd, "w") as output:
        output.write(json.dumps(value, indent=2) + "\n")
    os.replace(temp, path)


def private_directory(path):
    if path.is_symlink():
        raise ValueError("symlink directory")
    path.mkdir(mode=0o700, exist_ok=True)
    info = path.stat()
    if info.st_uid != os.getuid() or info.st_mode & 0o077:
        raise ValueError("private directory required")


def main():
    build = ROOT / "build"
    runtime = ROOT / ".runtime"
    private_directory(build)
    private_directory(runtime)
    archive = io.BytesIO()
    with zipfile.ZipFile(archive, "w", zipfile.ZIP_DEFLATED) as bundle:
        for name in FILES:
            info = zipfile.ZipInfo(name, date_time=(2026, 1, 1, 0, 0, 0))
            info.compress_type = zipfile.ZIP_DEFLATED
            bundle.writestr(info, (ROOT / name).read_bytes())
    content = archive.getvalue()
    if len(content) > 262144:
        raise ValueError("bundle exceeds Sandhi worker limit")
    digest = hashlib.sha256(content).hexdigest()
    target = build / ("evaluator-" + digest + ".pyz")
    try:
        with target.open("xb") as output:
            output.write(content)
    except FileExistsError:
        if target.is_symlink() or target.read_bytes() != content:
            raise ValueError("bundle collision")
    artifact = ROOT / "artifact.json"
    artifact_digest = hashlib.sha256(artifact.read_bytes()).hexdigest()
    key = runtime / "service.key"
    if not key.exists() and not key.is_symlink():
        fd = os.open(key, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
        with os.fdopen(fd, "w") as output:
            output.write(secrets.token_urlsafe(32) + "\n")
    from evaluator_app.settings import read_auth_key

    read_auth_key(key)
    config = json.loads((ROOT / "evaluator.json").read_text())
    if not isinstance(config, dict) or set(config) != {"name"}:
        raise ValueError("evaluator configuration")
    name = config["name"]
    if (
        not isinstance(name, str)
        or not 1 <= len(name) <= 64
        or any(not c.isascii() or not (c.isalnum() or c in "._-") for c in name)
    ):
        raise ValueError("evaluator name")
    private_write(
        runtime / "sandhi-workers.json",
        {
            "version": 1,
            "workers": [
                {
                    "name": name,
                    "python": sys.executable,
                    "script": str(target),
                    "script_sha256": digest,
                    "artifact": str(artifact),
                    "artifact_sha256": artifact_digest,
                    "pool_size": 1,
                    "startup_timeout_ms": 5000,
                }
            ],
        },
    )
    private_write(
        runtime / "http.json",
        {
            "version": 1,
            "name": name,
            "python": sys.executable,
            "bundle": str(target),
            "bundle_sha256": digest,
            "artifact": str(artifact),
            "artifact_sha256": artifact_digest,
            "auth_file": str(key),
            "pool_size": 1,
            "startup_timeout_ms": 5000,
            "evaluation_timeout_ms": 200,
            "port": 9088,
        },
    )
    private_write(
        runtime / "service.json",
        {
            "version": 1,
            "port": 9088,
            "auth_file": str(key),
            "evaluators": [str(runtime / "http.json")],
        },
    )
    private_write(
        runtime / "sandhi-remote.json",
        {
            "version": 1,
            "evaluators": [
                {
                    "name": name,
                    "artifact_sha256": artifact_digest,
                    "code_sha256": digest,
                    "timeout_ms": 200,
                    "replicas": [
                        {"url": "http://127.0.0.1:9088", "auth_file": str(key)}
                    ],
                }
            ],
        },
    )
    private_write(
        runtime / "policy.json",
        {
            "schema_version": "1",
            "revision": 1,
            "deadline_ms": 200,
            "max_body_bytes": 65536,
            "rules": [
                {
                    "id": "example-evaluator",
                    "effect": "block",
                    "evaluator": {
                        "kind": "registered",
                        "name": name,
                        "configuration": {"at_least": 0.5},
                    },
                }
            ],
        },
    )
    print(
        json.dumps(
            {
                "bundle": str(target),
                "sandhi_manifest": str(runtime / "sandhi-workers.json"),
                "http_config": str(runtime / "http.json"),
                "service_config": str(runtime / "service.json"),
            }
        )
    )


if __name__ == "__main__":
    main()
