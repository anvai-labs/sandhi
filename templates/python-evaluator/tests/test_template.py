"""Conformance tests carried into every generated evaluator project."""

import hashlib
import json
import os
import subprocess
import sys
import threading
import time
from pathlib import Path

import pytest

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT))


@pytest.fixture
def deployment(tmp_path):
    import shutil

    for name in (
        "evaluator_app",
        "__main__.py",
        "build_bundle.py",
        "artifact.json",
        "evaluator.json",
    ):
        source = ROOT / name
        target = tmp_path / name
        if source.is_dir():
            shutil.copytree(
                source, target, ignore=shutil.ignore_patterns("__pycache__")
            )
        else:
            shutil.copyfile(source, target)
    subprocess.run(
        [sys.executable, str(tmp_path / "build_bundle.py")],
        check=True,
        capture_output=True,
    )
    return tmp_path


def config(root):
    from evaluator_app.settings import Settings

    return Settings.load(root / ".runtime/http.json")


def test_bundle_matches_existing_worker_protocol(deployment):
    c = config(deployment)
    payload = {"version": 1, "id": "1", "text": "SYNTHETIC_RESTRICTED", "joined": ""}
    result = subprocess.run(
        [c.python, "-I", "-B", str(c.bundle), str(c.artifact), c.artifact_sha256],
        input=(json.dumps(payload) + "\n").encode(),
        capture_output=True,
        timeout=5,
    )
    assert result.returncode == 0
    ready, reply = [json.loads(line) for line in result.stdout.splitlines()]
    assert ready == {
        "version": 1,
        "kind": "ready",
        "artifact_sha256": c.artifact_sha256,
    }
    assert reply == {"version": 1, "id": "1", "score": 1.0}
    assert b"SYNTHETIC_RESTRICTED" not in result.stdout + result.stderr


def test_modified_artifact_is_rejected_before_readiness(deployment):
    c = config(deployment)
    c.artifact.write_text("{}")
    result = subprocess.run(
        [c.python, "-I", "-B", str(c.bundle), str(c.artifact), c.artifact_sha256],
        capture_output=True,
        timeout=5,
    )
    assert result.returncode != 0
    assert result.stdout == b""


def test_auth_file_must_be_private_and_not_symlink(deployment):
    from evaluator_app.settings import read_auth_key

    c = config(deployment)
    assert len(read_auth_key(c.auth_file)) >= 32
    c.auth_file.chmod(0o644)
    with pytest.raises(ValueError):
        read_auth_key(c.auth_file)
    c.auth_file.chmod(0o600)
    link = c.auth_file.with_name("link")
    link.symlink_to(c.auth_file)
    with pytest.raises((ValueError, OSError)):
        read_auth_key(link)


def payload(c, text="SYNTHETIC_RESTRICTED", **changes):
    value = {
        "version": 1,
        "id": "1",
        "text": text,
        "joined": "",
        "timeout_ms": 200,
        "evaluator": c.name,
        "artifact_sha256": c.artifact_sha256,
        "code_sha256": c.bundle_sha256,
    }
    value.update(changes)
    return value


def test_http_auth_provenance_validation_and_score_parity(deployment):
    pytest.importorskip("flask")
    from evaluator_app.http_service import create_app

    c = config(deployment)
    app = create_app(c)
    try:
        client = app.test_client()
        headers = {"Authorization": "Bearer " + c.auth_file.read_text().strip()}
        assert client.get("/healthz").status_code == 200
        assert (
            client.post(
                f"/v1/evaluators/{c.name}/evaluate", json=payload(c)
            ).status_code
            == 401
        )
        assert (
            client.post(
                f"/v1/evaluators/{c.name}/evaluate",
                headers={"Authorization": "Bearer wrong"},
                json=payload(c),
            ).status_code
            == 401
        )
        assert client.get("/readyz", headers=headers).status_code == 200
        response = client.post(
            f"/v1/evaluators/{c.name}/evaluate",
            headers=headers,
            json=payload(c),
        )
        assert response.status_code == 200
        assert response.json == {
            "version": 1,
            "id": "1",
            "score": 1.0,
            "evaluator": c.name,
            "artifact_sha256": c.artifact_sha256,
            "code_sha256": c.bundle_sha256,
        }
        assert response.headers["Cache-Control"] == "no-store"
        assert (
            client.post(
                f"/v1/evaluators/{c.name}/evaluate",
                headers=headers,
                json=payload(c, "weather"),
            ).json["score"]
            == 0.0
        )
        assert (
            client.post(
                f"/v1/evaluators/{c.name}/evaluate",
                headers=headers,
                json=payload(c, artifact_sha256="0" * 64),
            ).status_code
            == 409
        )
        assert (
            client.post(
                f"/v1/evaluators/{c.name}/evaluate",
                headers=headers,
                json=payload(c, groups=["admin"]),
            ).status_code
            == 400
        )
        assert (
            client.post(
                f"/v1/evaluators/{c.name}/evaluate",
                headers=headers,
                json=payload(c, timeout_ms=True),
            ).status_code
            == 400
        )
        assert (
            client.post(
                f"/v1/evaluators/{c.name}/evaluate",
                headers=headers,
                data='{"version":1,"version":1}',
                content_type="application/json",
            ).status_code
            == 400
        )
        assert (
            client.post(
                f"/v1/evaluators/{c.name}/evaluate",
                headers=headers,
                data=b"x" * 2097153,
                content_type="application/json",
            ).status_code
            == 413
        )
        assert (
            client.post(
                f"/v1/evaluators/{c.name}/evaluate",
                headers=headers,
                data="text",
            ).status_code
            == 415
        )
    finally:
        app.extensions["evaluator"].close()


@pytest.mark.parametrize("fault", ["hang", "crash", "nan"])
def test_http_model_failures_are_redacted_and_processes_recover(deployment, fault):
    pytest.importorskip("flask")
    from evaluator_app.http_service import create_app

    source = deployment / "evaluator_app/evaluate.py"
    code = source.read_text().replace(
        "def score(self, text, joined):",
        """def score(self, text, joined):
        if text == "hang":
            import time
            time.sleep(10)
        if text == "crash":
            import os
            os._exit(2)
        if text == "nan":
            return float("nan")""",
    )
    source.write_text(code)
    subprocess.run(
        [sys.executable, str(deployment / "build_bundle.py")],
        check=True,
        capture_output=True,
    )
    c = config(deployment)
    app = create_app(c)
    supervisor = app.extensions["evaluator"]
    try:
        before = supervisor.pids
        headers = {"Authorization": "Bearer " + c.auth_file.read_text().strip()}
        start = time.monotonic()
        response = app.test_client().post(
            f"/v1/evaluators/{c.name}/evaluate",
            headers=headers,
            json=payload(c, fault, timeout_ms=50),
        )
        assert response.status_code == 503
        assert response.json == {"error": {"code": "evaluation_unavailable"}}
        assert time.monotonic() - start < 1
        end = time.monotonic() + 5
        while not supervisor.ready or supervisor.pids == before:
            assert time.monotonic() < end
            time.sleep(0.01)
        assert (
            app.test_client()
            .post(
                f"/v1/evaluators/{c.name}/evaluate",
                headers=headers,
                json=payload(c, "weather"),
            )
            .status_code
            == 200
        )
        for pid in before:
            with pytest.raises(ProcessLookupError):
                os.kill(pid, 0)
    finally:
        supervisor.close()


def test_supervisor_saturation_has_no_queue_and_close_reaps(deployment):
    from evaluator_app.supervisor import Supervisor, Unavailable

    source = deployment / "evaluator_app/evaluate.py"
    source.write_text(
        source.read_text().replace(
            "def score(self, text, joined):",
            """def score(self, text, joined):
        if text == "hang":
            import time
            time.sleep(10)""",
        )
    )
    subprocess.run(
        [sys.executable, str(deployment / "build_bundle.py")],
        check=True,
        capture_output=True,
    )
    s = Supervisor(config(deployment))
    pids = s.pids
    failures = []

    def run():
        try:
            s.evaluate(
                {"version": 1, "id": "1", "text": "hang", "joined": ""},
                time.monotonic() + 0.3,
            )
        except Unavailable:
            failures.append(True)

    thread = threading.Thread(target=run)
    thread.start()
    time.sleep(0.03)
    assert s.status()["state"] == "saturated"
    assert s.status()["busy_slots"] == 1
    start = time.monotonic()
    with pytest.raises(Unavailable):
        s.evaluate(
            {"version": 1, "id": "2", "text": "weather", "joined": ""},
            time.monotonic() + 1,
        )
    assert time.monotonic() - start < 0.1
    assert s.status()["rejected_total"] == 1
    thread.join(timeout=2)
    assert failures
    s.close()
    for pid in pids:
        with pytest.raises(ProcessLookupError):
            os.kill(pid, 0)


def test_business_errors_never_echo_inputs(deployment):
    c = config(deployment)
    bad = b'{"version":1,"id":"1","text":NaN,"joined":""}\n'
    result = subprocess.run(
        [c.python, "-I", "-B", str(c.bundle), str(c.artifact), c.artifact_sha256],
        input=bad,
        capture_output=True,
        timeout=5,
    )
    assert result.returncode != 0
    assert b"NaN" not in result.stdout + result.stderr


def test_rebuild_covers_business_code_and_keeps_service_key(deployment):
    c = config(deployment)
    key = c.auth_file.read_bytes()
    p = deployment / "evaluator_app/evaluate.py"
    p.write_text(p.read_text() + "\n# changed deployment\n")
    subprocess.run(
        [sys.executable, str(deployment / "build_bundle.py")],
        check=True,
        capture_output=True,
    )
    updated = config(deployment)
    assert updated.bundle_sha256 != c.bundle_sha256
    assert (
        hashlib.sha256(updated.bundle.read_bytes()).hexdigest() == updated.bundle_sha256
    )
    assert updated.auth_file.read_bytes() == key


def test_service_routes_multiple_models_on_one_listener(deployment):
    from evaluator_app.http_service import create_app
    from evaluator_app.settings import ServiceSettings

    base = json.loads((deployment / ".runtime/http.json").read_text())
    second = dict(base, name="example.second.v1")
    second_artifact = deployment / "second.json"
    second_artifact.write_text('{"markers":["OTHER_RESTRICTED"]}')
    second["artifact"] = str(second_artifact)
    second["artifact_sha256"] = hashlib.sha256(second_artifact.read_bytes()).hexdigest()
    path = deployment / ".runtime/second.json"
    path.write_text(json.dumps(second))
    service = {
        "version": 1,
        "port": 9088,
        "auth_file": base["auth_file"],
        "evaluators": [str(deployment / ".runtime/http.json"), str(path)],
    }
    service_path = deployment / ".runtime/service.json"
    service_path.write_text(json.dumps(service))
    settings = ServiceSettings.load(service_path)
    app = create_app(settings)
    try:
        headers = {
            "Authorization": "Bearer " + Path(base["auth_file"]).read_text().strip()
        }
        client = app.test_client()
        for name, marker, expected in [
            (base["name"], "SYNTHETIC_RESTRICTED", 1),
            ("example.second.v1", "SYNTHETIC_RESTRICTED", 0),
            ("example.second.v1", "OTHER_RESTRICTED", 1),
        ]:
            c = settings.evaluators[name]
            response = client.post(
                "/v1/evaluators/" + name + "/evaluate",
                headers=headers,
                json=payload(c, marker),
            )
            assert response.status_code == 200
            assert response.json["score"] == expected
        assert (
            client.post(
                "/v1/evaluators/missing/evaluate", headers=headers, json={}
            ).status_code
            == 404
        )
        assert (
            client.post("/v1/evaluators/missing/evaluate", json={}).status_code == 401
        )
        assert (
            client.get(
                "/v1/evaluators/example.second.v1/readyz", headers=headers
            ).status_code
            == 200
        )
        c = settings.evaluators[base["name"]]
        assert (
            client.post(
                "/v1/evaluators/example.second.v1/evaluate",
                headers=headers,
                json=payload(c),
            ).status_code
            == 409
        )
        service["evaluators"] = [str(deployment / ".runtime/http.json")] * 2
        service_path.write_text(json.dumps(service))
        with pytest.raises(ValueError):
            ServiceSettings.load(service_path)
    finally:
        app.extensions["evaluator"].close()


def test_service_enforces_global_worker_cap_and_strict_version(deployment):
    from evaluator_app.settings import ServiceSettings, Settings

    path = deployment / ".runtime/http.json"
    base = json.loads(path.read_text())
    base["version"] = True
    path.write_text(json.dumps(base))
    with pytest.raises(ValueError):
        Settings.load(path)
    base["version"] = 1
    base["pool_size"] = 4
    path.write_text(json.dumps(base))
    second = deployment / ".runtime/second.json"
    second.write_text(json.dumps(dict(base, name="second")))
    service_path = deployment / ".runtime/service.json"
    service_path.write_text(
        json.dumps(
            {
                "version": 1,
                "port": 9088,
                "auth_file": base["auth_file"],
                "evaluators": [str(path), str(second)],
            }
        )
    )
    with pytest.raises(ValueError):
        ServiceSettings.load(service_path)


def test_remote_listener_requires_tls_and_private_key(deployment):
    from evaluator_app.settings import ServiceSettings

    p = deployment / ".runtime/service.json"
    value = json.loads(p.read_text())
    value["bind_host"] = "0.0.0.0"
    p.write_text(json.dumps(value))
    with pytest.raises(ValueError):
        ServiceSettings.load(p)
    value["tls_certfile"] = str(deployment / "cert.pem")
    value["tls_keyfile"] = str(deployment / "key.pem")
    Path(value["tls_certfile"]).write_text("fixture-cert")
    Path(value["tls_keyfile"]).write_text("fixture-key")
    Path(value["tls_keyfile"]).chmod(0o644)
    p.write_text(json.dumps(value))
    with pytest.raises(ValueError):
        ServiceSettings.load(p)
    Path(value["tls_keyfile"]).chmod(0o600)
    assert ServiceSettings.load(p).bind_host == "0.0.0.0"


def test_modified_bundle_never_starts_and_environment_is_cleared(
    deployment, monkeypatch
):
    from evaluator_app.supervisor import Supervisor

    c = config(deployment)
    c.bundle.write_bytes(c.bundle.read_bytes() + b"tampered")
    with pytest.raises(ValueError):
        Supervisor(c)
    source = deployment / "evaluator_app/evaluate.py"
    source.write_text(
        source.read_text().replace(
            "def score(self, text, joined):",
            """def score(self, text, joined):
        import os
        if 'TEMPLATE_SYNTHETIC_SECRET' in os.environ:
            raise ValueError('should never inherit')""",
        )
    )
    # A changed business module produces a new address, independently of the corrupt old archive.
    subprocess.run(
        [sys.executable, str(deployment / "build_bundle.py")],
        check=True,
        capture_output=True,
    )
    monkeypatch.setenv("TEMPLATE_SYNTHETIC_SECRET", "synthetic")
    supervisor = Supervisor(config(deployment))
    try:
        answer = supervisor.evaluate(
            {"version": 1, "id": "1", "text": "weather", "joined": ""},
            time.monotonic() + 1,
        )
        assert answer["score"] == 0
    finally:
        supervisor.close()


def test_authenticated_status_tracks_results_without_payloads(deployment):
    from evaluator_app.http_service import create_app

    c = config(deployment)
    app = create_app(c)
    path = "/v1/evaluators/" + c.name + "/status"
    headers = {"Authorization": "Bearer " + c.auth_file.read_text().strip()}
    try:
        client = app.test_client()
        assert client.get(path).status_code == 401
        assert (
            client.get("/v1/evaluators/missing/status", headers=headers).status_code
            == 404
        )
        value = client.get(path, headers=headers).json
        assert value["version"] == 1 and value["state"] == "ready"
        assert value["capacity"] == value["ready_slots"] == 1
        assert value["busy_slots"] == value["restarts_total"] == 0
        for text in ("weather", "SYNTHETIC_RESTRICTED"):
            assert (
                client.post(
                    "/v1/evaluators/" + c.name + "/evaluate",
                    headers=headers,
                    json=payload(c, text),
                ).status_code
                == 200
            )
        value = client.get(path, headers=headers).json
        assert value["completed_total"] == 2
        assert value["failed_total"] == value["rejected_total"] == 0
        raw = json.dumps(value)
        for hidden in (
            "weather",
            "SYNTHETIC_RESTRICTED",
            str(c.artifact),
            str(c.python),
            c.auth_file.read_text().strip(),
        ):
            assert hidden not in raw
    finally:
        app.extensions["evaluator"].close()


def test_exhausted_restart_allowance_is_visible_and_does_not_affect_other_models(
    deployment,
):
    from evaluator_app.http_service import create_app
    from evaluator_app.settings import ServiceSettings

    source = deployment / "evaluator_app/evaluate.py"
    source.write_text(
        source.read_text().replace(
            "def score(self, text, joined):",
            """def score(self, text, joined):
        if text == 'crash':
            import os
            os._exit(2)""",
        )
    )
    subprocess.run(
        [sys.executable, str(deployment / "build_bundle.py")],
        check=True,
        capture_output=True,
    )
    c = config(deployment)
    other_path = deployment / ".runtime/other.json"
    other = json.loads((deployment / ".runtime/http.json").read_text())
    other["name"] = "healthy.model"
    other_path.write_text(json.dumps(other))
    service = deployment / ".runtime/service.json"
    service.write_text(
        json.dumps(
            {
                "version": 1,
                "port": 9088,
                "auth_file": str(c.auth_file),
                "evaluators": [str(deployment / ".runtime/http.json"), str(other_path)],
            }
        )
    )
    app = create_app(ServiceSettings.load(service))
    headers = {"Authorization": "Bearer " + c.auth_file.read_text().strip()}
    try:
        client = app.test_client()
        for attempt in range(4):
            assert (
                client.post(
                    "/v1/evaluators/" + c.name + "/evaluate",
                    headers=headers,
                    json=payload(c, "crash"),
                ).status_code
                == 503
            )
            end = time.monotonic() + 5
            while True:
                value = client.get(
                    "/v1/evaluators/" + c.name + "/status", headers=headers
                ).json
                if value["state"] == ("unavailable" if attempt == 3 else "ready"):
                    break
                assert time.monotonic() < end
                time.sleep(0.01)
        assert value["failed_total"] == 4
        assert value["restarts_total"] == 3
        assert value["ready_slots"] == value["busy_slots"] == 0
        assert client.get("/readyz", headers=headers).status_code == 503
        assert (
            client.get(
                "/v1/evaluators/healthy.model/readyz", headers=headers
            ).status_code
            == 200
        )
        from evaluator_app.settings import Settings

        healthy = Settings.load(other_path)
        assert (
            client.post(
                "/v1/evaluators/healthy.model/evaluate",
                headers=headers,
                json=payload(healthy, "weather"),
            ).status_code
            == 200
        )
        before = value["rejected_total"]
        assert (
            client.post(
                "/v1/evaluators/" + c.name + "/evaluate",
                headers=headers,
                json=payload(c, "weather"),
            ).status_code
            == 503
        )
        assert (
            client.get("/v1/evaluators/" + c.name + "/status", headers=headers).json[
                "rejected_total"
            ]
            == before + 1
        )
    finally:
        app.extensions["evaluator"].close()


@pytest.fixture
def container_material(deployment):
    # TLS syntax and key matching are exercised against real material in the Docker smoke.
    files = {}
    for name in ("cert.pem", "key.pem", "ca.pem"):
        path = deployment / name
        path.write_text("synthetic fixture")
        path.chmod(0o600)
        files[name] = path
    return files


def test_container_snapshot_pins_models_and_does_not_follow_rebuilds(
    deployment, container_material, monkeypatch, tmp_path
):
    import containerize

    monkeypatch.setattr(containerize, "validate_tls", lambda *args: None)
    target = tmp_path / "release"
    c = config(deployment)
    containerize.prepare(
        target,
        [deployment / ".runtime/http.json"],
        "sha256:" + "a" * 64,
        c.auth_file,
        container_material["cert.pem"],
        container_material["key.pem"],
        container_material["ca.pem"],
    )
    compose = json.loads((target / "compose.json").read_text())
    service = compose["services"]["evaluator"]
    assert service["user"] == f"{os.getuid()}:{os.getgid()}"
    assert service["read_only"] and service["cap_drop"] == ["ALL"]
    assert service["security_opt"] == ["no-new-privileges:true"]
    assert service["mem_limit"] == service["memswap_limit"] == 512 * 1024 * 1024
    assert service["cpus"] == 1 and service["pids_limit"] == 64
    assert "ports" not in service
    assert compose["networks"]["evaluation"]["internal"] is True
    assert all(v["read_only"] for v in service["volumes"])
    before = {
        p.relative_to(target): p.read_bytes() for p in target.rglob("*") if p.is_file()
    }
    c.artifact.write_text('{"markers":["CHANGED"]}')
    subprocess.run(
        [sys.executable, str(deployment / "build_bundle.py")],
        check=True,
        capture_output=True,
    )
    assert before == {
        p.relative_to(target): p.read_bytes() for p in target.rglob("*") if p.is_file()
    }
    with pytest.raises(FileExistsError):
        containerize.prepare(
            target,
            [deployment / ".runtime/http.json"],
            "sha256:" + "a" * 64,
            c.auth_file,
            container_material["cert.pem"],
            container_material["key.pem"],
            container_material["ca.pem"],
        )
    assert (target / "secrets/service.key").stat().st_mode & 0o077 == 0
    assert c.auth_file.read_text().strip() not in (target / "compose.json").read_text()


@pytest.mark.parametrize(
    "bad",
    [
        "mutable_image",
        "root",
        "memory",
        "cpu",
        "pids",
        "tamper",
        "duplicate",
        "public_secret",
        "symlink_secret",
    ],
)
def test_container_snapshot_rejects_unsafe_configuration(
    deployment, container_material, monkeypatch, tmp_path, bad
):
    import containerize

    monkeypatch.setattr(containerize, "validate_tls", lambda *args: None)
    c = config(deployment)
    image = "sha256:" + "a" * 64
    models = [deployment / ".runtime/http.json"]
    kwargs = {}
    if bad == "mutable_image":
        image = "python:latest"
    if bad == "root":
        monkeypatch.setattr(containerize.os, "getuid", lambda: 0)
    if bad == "memory":
        kwargs["memory_mb"] = 0
    if bad == "cpu":
        kwargs["cpus"] = float("nan")
    if bad == "pids":
        kwargs["pids"] = 0
    if bad == "tamper":
        c.bundle.write_bytes(b"changed")
    if bad == "duplicate":
        models *= 2
    if bad == "public_secret":
        container_material["key.pem"].chmod(0o644)
    if bad == "symlink_secret":
        link = deployment / "linked.key"
        link.symlink_to(container_material["key.pem"])
        container_material["key.pem"] = link
    target = tmp_path / "unsafe"
    with pytest.raises((ValueError, OSError)):
        containerize.prepare(
            target,
            models,
            image,
            c.auth_file,
            container_material["cert.pem"],
            container_material["key.pem"],
            container_material["ca.pem"],
            **kwargs,
        )
    assert not target.exists()


def test_published_container_profile_is_explicit(
    deployment, container_material, monkeypatch, tmp_path
):
    import containerize

    monkeypatch.setattr(containerize, "validate_tls", lambda *args: None)
    c = config(deployment)
    target = tmp_path / "published"
    containerize.prepare(
        target,
        [deployment / ".runtime/http.json"],
        "sha256:" + "a" * 64,
        c.auth_file,
        container_material["cert.pem"],
        container_material["key.pem"],
        container_material["ca.pem"],
        network="published",
    )
    value = json.loads((target / "compose.json").read_text())
    assert len(value["services"]["evaluator"]["ports"]) == 1
    assert value["services"]["evaluator"]["ports"][0]["host_ip"] == "127.0.0.1"
    assert value["networks"]["evaluation"]["internal"] is False
