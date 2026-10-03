"""Disposable TLS Flask replicas, multi-model routing and real Sandhi adapter smoke.

Run with the template environment (Flask, Gunicorn, pytest), --sandhi WORKTREE.
Only synthetic text and temporary credentials are used. No production changes.
"""

import argparse
from concurrent.futures import ThreadPoolExecutor
import threading
import hashlib
import json
import os
import signal
import socket
import ssl
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request
from pathlib import Path


def command(args, **kwargs):
    return subprocess.run(args, check=True, capture_output=True, **kwargs)


def write(path, value):
    path.write_text(json.dumps(value, indent=2) + "\n")
    path.chmod(0o600)


def descendants(root):
    rows = command(["ps", "-Ao", "pid=,ppid="]).stdout.decode().splitlines()
    parents = {int(row.split()[0]): int(row.split()[1]) for row in rows}
    found = {root}
    while True:
        expanded = found | {pid for pid, parent in parents.items() if parent in found}
        if expanded == found:
            return found - {root}
        found = expanded


def resources(processes):
    pids = {process.pid for process in processes}
    for process in processes:
        pids.update(descendants(process.pid))
    output = command(
        [
            "ps",
            "-o",
            "pid=,rss=,time=",
            "-p",
            ",".join(str(pid) for pid in sorted(pids)),
        ]
    )
    rows = {}
    for line in output.stdout.decode().splitlines():
        pid, rss, clock = line.split()
        days = 0
        if "-" in clock:
            day, clock = clock.split("-", 1)
            days = int(day)
        seconds = 0.0
        for field in clock.split(":"):
            seconds = seconds * 60 + float(field)
        rows[int(pid)] = {"rss_kib": int(rss), "cpu_seconds": seconds + days * 86400}
    return rows


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--sandhi", required=True, type=Path)
    args = parser.parse_args()
    root = Path(tempfile.mkdtemp(prefix="sandhi-evaluator-service-"))
    project = root / "example"
    command(
        [
            sys.executable,
            str(args.sandhi / "scripts/new_policy_evaluator.py"),
            str(project),
            "--name",
            "test.remote.v1",
        ]
    )
    duplicate = subprocess.run(
        [
            sys.executable,
            str(args.sandhi / "scripts/new_policy_evaluator.py"),
            str(project),
        ],
        capture_output=True,
    )
    assert duplicate.returncode != 0
    tests = command(
        [
            sys.executable,
            "-m",
            "pytest",
            str(project / "tests"),
            "-q",
            "-p",
            "no:cacheprovider",
        ]
    )
    (root / "template-tests.log").write_bytes(tests.stdout + tests.stderr)
    model_path = project / ".runtime/http.json"
    model = json.loads(model_path.read_text())
    # A second named model shares each replica's single listener and service key.
    other_artifact = root / "other-artifact.json"
    write(other_artifact, {"markers": ["OTHER_RESTRICTED"]})
    other = dict(
        model,
        name="test.other.v1",
        artifact=str(other_artifact),
        artifact_sha256=hashlib.sha256(other_artifact.read_bytes()).hexdigest(),
    )
    other_path = root / "other.json"
    write(other_path, other)
    ca, ca_key = root / "ca.pem", root / "ca-key.pem"
    cert, key = root / "cert.pem", root / "key.pem"
    command(
        [
            "openssl",
            "req",
            "-x509",
            "-newkey",
            "rsa:2048",
            "-nodes",
            "-days",
            "1",
            "-subj",
            "/CN=synthetic-test-ca",
            "-keyout",
            str(ca_key),
            "-out",
            str(ca),
        ]
    )
    csr, extensions = root / "leaf.csr", root / "leaf.ext"
    extensions.write_text(
        "basicConstraints=critical,CA:FALSE\nkeyUsage=critical,digitalSignature,keyEncipherment\n"
        "extendedKeyUsage=serverAuth,clientAuth\nsubjectAltName=DNS:localhost,IP:127.0.0.1\n"
    )
    command(
        [
            "openssl",
            "req",
            "-new",
            "-newkey",
            "rsa:2048",
            "-nodes",
            "-subj",
            "/CN=localhost",
            "-keyout",
            str(key),
            "-out",
            str(csr),
        ]
    )
    command(
        [
            "openssl",
            "x509",
            "-req",
            "-in",
            str(csr),
            "-CA",
            str(ca),
            "-CAkey",
            str(ca_key),
            "-CAcreateserial",
            "-days",
            "1",
            "-extfile",
            str(extensions),
            "-out",
            str(cert),
        ]
    )
    key.chmod(0o600)
    identity = root / "client-identity.pem"
    identity.write_bytes(cert.read_bytes() + key.read_bytes())
    identity.chmod(0o600)
    context = ssl.create_default_context(cafile=str(ca))
    context.load_cert_chain(cert, key)
    opener = urllib.request.build_opener(
        urllib.request.ProxyHandler({}), urllib.request.HTTPSHandler(context=context)
    )
    token = Path(model["auth_file"]).read_text().strip()
    processes, servers, children = [], [], set()

    def http(url, body=None, authenticated=True):
        headers = {"Content-Type": "application/json"}
        if authenticated:
            headers["Authorization"] = "Bearer " + token
        request = urllib.request.Request(
            url,
            data=None if body is None else json.dumps(body).encode(),
            headers=headers,
        )
        try:
            with opener.open(request, timeout=2) as response:
                return response.status, json.load(response)
        except urllib.error.HTTPError as error:
            return error.code, json.load(error)

    try:
        for index in range(2):
            with socket.socket() as reservation:
                reservation.bind(("127.0.0.1", 0))
                port = reservation.getsockname()[1]
            service = root / f"service-{index}.json"
            service_config = {
                "version": 1,
                "port": port,
                "auth_file": model["auth_file"],
                "evaluators": [str(model_path), str(other_path)],
                "tls_certfile": str(cert),
                "tls_keyfile": str(key),
            }
            if index == 1:
                service_config["tls_cafile"] = str(ca)
            write(service, service_config)
            log = open(root / f"service-{index}.log", "wb")
            process = subprocess.Popen(
                [sys.executable, "-I", "-B", model["bundle"], "--http", str(service)],
                stdout=log,
                stderr=log,
                start_new_session=True,
            )
            log.close()
            processes.append(process)
            base = f"https://127.0.0.1:{port}"
            servers.append(base)
            end = time.monotonic() + 10
            while True:
                assert process.poll() is None, f"service failed; see {root}"
                try:
                    if http(base + "/readyz")[0] == 200:
                        break
                except (OSError, urllib.error.URLError):
                    pass
                assert time.monotonic() < end
                time.sleep(0.05)
            children.update(descendants(process.pid))
            assert http(base + "/readyz", authenticated=False)[0] == 401
            for deployment, text, score in [
                (model, "SYNTHETIC_RESTRICTED", 1),
                (other, "SYNTHETIC_RESTRICTED", 0),
                (other, "OTHER_RESTRICTED", 1),
            ]:
                body = {
                    "version": 1,
                    "id": "1",
                    "text": text,
                    "joined": "",
                    "timeout_ms": 200,
                    "evaluator": deployment["name"],
                    "artifact_sha256": deployment["artifact_sha256"],
                    "code_sha256": deployment["bundle_sha256"],
                }
                status, response = http(
                    base + "/v1/evaluators/" + deployment["name"] + "/evaluate", body
                )
                assert status == 200 and response["score"] == score
        # Fixed-size, synthetic burst. Capture aggregate process resources only;
        # no prompt, token, path or process command is put in the report.
        before = resources(processes)
        observed = dict(before)
        peak_rss = sum(value["rss_kib"] for value in before.values())
        peak_processes = len(before)
        stop_sampling = threading.Event()
        sampling_errors = []

        def sample():
            nonlocal peak_rss, peak_processes
            try:
                while not stop_sampling.wait(0.02):
                    current = resources(processes)
                    peak_rss = max(
                        peak_rss, sum(value["rss_kib"] for value in current.values())
                    )
                    peak_processes = max(peak_processes, len(current))
                    for pid, value in current.items():
                        if value["cpu_seconds"] >= observed.get(pid, {}).get(
                            "cpu_seconds", 0
                        ):
                            observed[pid] = value
            except Exception:
                sampling_errors.append(True)

        sampler = threading.Thread(target=sample, daemon=True)
        sampler.start()

        def scan(index):
            deployment = model if index % 2 == 0 else other
            expected = index % 4 < 2
            text = (
                ("SYNTHETIC_RESTRICTED" if deployment is model else "OTHER_RESTRICTED")
                if expected
                else "weather"
            )
            body = {
                "version": 1,
                "id": str(index + 1),
                "text": text,
                "joined": "",
                "timeout_ms": 200,
                "evaluator": deployment["name"],
                "artifact_sha256": deployment["artifact_sha256"],
                "code_sha256": deployment["bundle_sha256"],
            }
            start = time.monotonic()
            status, response = http(
                servers[(index // 2) % 2]
                + "/v1/evaluators/"
                + deployment["name"]
                + "/evaluate",
                body,
            )
            assert status in {200, 503}
            if status == 200:
                assert response["score"] == int(expected)
            else:
                assert response == {"error": {"code": "evaluation_unavailable"}}
            return status, round((time.monotonic() - start) * 1000000)

        started = time.monotonic()
        try:
            with ThreadPoolExecutor(max_workers=8) as executor:
                observations = list(executor.map(scan, range(64)))
        finally:
            stop_sampling.set()
            sampler.join(timeout=5)
        assert not sampler.is_alive() and not sampling_errors
        elapsed = time.monotonic() - started
        after = resources(processes)
        children.update(
            pid for pid in after if pid not in {process.pid for process in processes}
        )
        assert len(after) == len(before) == 8
        assert peak_processes <= 8
        statuses = {}
        for base in servers:
            for deployment in (model, other):
                url = base + "/v1/evaluators/" + deployment["name"] + "/status"
                assert http(url, authenticated=False)[0] == 401
                code, value = http(url)
                assert code == 200 and value["state"] == "ready"
                assert value["capacity"] == value["ready_slots"] == 1
                assert value["busy_slots"] == 0
                assert value["failed_total"] == value["restarts_total"] == 0
                statuses[str(len(statuses))] = {
                    k: value[k]
                    for k in (
                        "state",
                        "completed_total",
                        "failed_total",
                        "restarts_total",
                        "rejected_total",
                    )
                }
        times = sorted(item[1] for item in observations)
        load = {
            "requests": len(observations),
            "concurrency": 8,
            "successful": sum(code == 200 for code, _ in observations),
            "unavailable": sum(code == 503 for code, _ in observations),
            "elapsed_seconds": round(elapsed, 4),
            "offered_requests_per_second": round(len(observations) / elapsed, 2),
            "successful_requests_per_second": round(
                sum(code == 200 for code, _ in observations) / elapsed, 2
            ),
            "p50_us": times[31],
            "p95_us": times[60],
            "p99_us": times[63],
            "process_count_before": len(before),
            "process_count_after": len(after),
            "max_sampled_processes": peak_processes,
            "rss_before_kib": sum(v["rss_kib"] for v in before.values()),
            "rss_after_kib": sum(v["rss_kib"] for v in after.values()),
            "max_sampled_rss_kib": max(
                peak_rss, sum(v["rss_kib"] for v in after.values())
            ),
            "sampled_cpu_seconds": round(
                sum(
                    max(v["cpu_seconds"], after.get(pid, {}).get("cpu_seconds", 0))
                    - before.get(pid, {}).get("cpu_seconds", 0)
                    for pid, v in observed.items()
                ),
                3,
            ),
            "models": statuses,
            "scope": "Two TLS replicas on one development host; fresh HTTP connections; marker fixture only. ps samples exclude the caller and are not hard resource limits.",
        }
        assert load["successful"] > 0
        remote = root / "remote.json"
        write(
            remote,
            {
                "version": 1,
                "evaluators": [
                    {
                        "name": model["name"],
                        "artifact_sha256": model["artifact_sha256"],
                        "code_sha256": model["bundle_sha256"],
                        "timeout_ms": 200,
                        "replicas": [
                            {
                                "url": base,
                                "auth_file": model["auth_file"],
                                "ca_file": str(ca),
                                **(
                                    {"identity_file": str(identity)}
                                    if index == 1
                                    else {}
                                ),
                            }
                            for index, base in enumerate(servers)
                        ],
                    }
                ],
            },
        )
        env = dict(
            os.environ,
            SANDHI_REMOTE_TEST_MANIFEST=str(remote),
            SANDHI_WORKER_TEST_MANIFEST=str(project / ".runtime/sandhi-workers.json"),
        )
        result = subprocess.run(
            [
                "cargo",
                "test",
                "-p",
                "sandhi-proxy",
                "--test",
                "policy_remote",
                "remote_real_flask_and_local_zipapp_share_policy_results",
                "--",
                "--ignored",
                "--nocapture",
            ],
            cwd=args.sandhi,
            env=env,
            capture_output=True,
            timeout=120,
        )
        (root / "rust-live.log").write_bytes(result.stdout + result.stderr)
        assert result.returncode == 0, f"Rust acceptance failed; see {root}"
    finally:
        for process in processes:
            if process.poll() is None:
                process.terminate()
        for process in processes:
            try:
                process.wait(timeout=20)
            except subprocess.TimeoutExpired:
                os.killpg(process.pid, signal.SIGKILL)
                process.wait(timeout=5)
    for pid in children:
        try:
            os.kill(pid, 0)
        except ProcessLookupError:
            continue
        raise AssertionError(f"owned child survived shutdown: {pid}")
    output = {
        "status": "passed",
        "tls_replicas": 2,
        "models_per_listener": 2,
        "load": load,
        "checks": [
            "generator_refuses_overwrite",
            "generated_project_tests",
            "tls",
            "mutual_tls",
            "service_authentication",
            "named_model_routes",
            "shared_listener",
            "rust_remote_replica_policy",
            "rust_local_zipapp_policy",
            "shutdown_reaps_children",
            "bounded_concurrent_load",
            "authenticated_model_status",
            "fixed_process_count",
        ],
        "evidence_directory": str(root),
    }
    write(root / "result.json", output)
    print(json.dumps(output, indent=2))


if __name__ == "__main__":
    main()
