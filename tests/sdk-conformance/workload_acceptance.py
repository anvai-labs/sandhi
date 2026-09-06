"""Bounded, synthetic M1 workload acceptance; not the full TD-0015 benchmark.

Run with --binary /path/to/sandhi-proxy --output /tmp/workload.json. The binary is
copied into a disposable directory before execution. No live service, vault or
credential is used. Timing reports are observations, not performance SLO gates.
"""

from __future__ import annotations

import argparse
import asyncio
from collections import Counter
from contextlib import ExitStack, closing
from dataclasses import asdict, dataclass
import hashlib
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import math
import os
from pathlib import Path
import platform
import random
import shutil
import socket
import sqlite3
import statistics
import subprocess
import sys
import tempfile
import threading
import time

import httpx


ROOT = Path(__file__).resolve().parents[2]
ADMIN = "synthetic-workload-admin"
UPSTREAM_KEY = "synthetic-workload-provider"
LANES = ("transparent-unary", "translation-unary", "transparent-sse", "translation-sse")


@dataclass(frozen=True)
class Config:
    requests: int = 128
    tenants: int = 32
    repeats: int = 3
    concurrency: int = 8
    arrival_rate: float = 50.0
    max_pending: int = 128
    frames: int = 16
    frame_delay_ms: float = 2.0
    timeout_seconds: float = 10.0
    total_seconds: float = 180.0
    seed: int = 17

    def validate(self):
        if not __debug__:
            raise ValueError("acceptance requires unoptimized Python so proof assertions remain enabled")
        for name, maximum in [("requests", 4096), ("tenants", 128), ("repeats", 10),
                              ("concurrency", 128), ("max_pending", 512), ("frames", 128)]:
            value = getattr(self, name)
            if type(value) is not int or not 1 <= value <= maximum:
                raise ValueError(f"{name} must be an integer in 1..{maximum}")
        for name, minimum, maximum in [("arrival_rate", 0.1, 10000),
                                       ("frame_delay_ms", 0, 100),
                                       ("timeout_seconds", 0.1, 60),
                                       ("total_seconds", 1, 600)]:
            value = getattr(self, name)
            if not math.isfinite(value) or not minimum <= value <= maximum:
                raise ValueError(f"{name} must be finite in {minimum}..{maximum}")
        if self.requests < self.tenants:
            raise ValueError("requests must cover every synthetic tenant")
        if self.max_pending < self.concurrency:
            raise ValueError("max_pending must be at least concurrency")


def components(index):
    """Fresh/cache/output/reasoning/inclusion: provider facts, never byte estimates."""
    variant = index % 3
    cached = (0, 30, 30)[variant]
    return (100 - cached, cached, 40, (0, 25, 90)[variant], 0)


def charge(index):
    fresh, cached, output, reasoning, _ = components(index)
    return fresh + cached + output + reasoning


def percentile(values, quantile):
    if not math.isfinite(quantile) or not 0 < quantile <= 1:
        raise ValueError("quantile must be finite in (0, 1]")
    if not values or not all(math.isfinite(value) and value >= 0 for value in values):
        raise ValueError("latency samples must be nonempty, finite and nonnegative")
    ordered = sorted(values)
    return ordered[max(0, math.ceil(len(ordered) * quantile) - 1)]


def distribution(values):
    quantiles = {name: percentile(values, q) for name, q in
                 [("p50", .5), ("p95", .95), ("p99", .99)]}
    return {"samples": len(values), "mean": statistics.mean(values), **quantiles}


def request_shape(lane, index):
    model = f"gemini-workload-v{index % 3}"
    stream = lane.endswith("sse")
    # Deterministic 10:1 request-size skew; contains no attribution or real content.
    text = "x" * (1280 if index % 11 == 0 else 128)
    if lane.startswith("translation"):
        return "/v1/chat/completions", {"model": model, "stream": stream,
            "messages": [{"role": "user", "content": text}], "max_tokens": 512}
    action = "streamGenerateContent?alt=sse" if stream else "generateContent"
    return f"/v1beta/models/{model}:{action}", {
        "contents": [{"role": "user", "parts": [{"text": text}]}],
        "generationConfig": {"maxOutputTokens": 512}}


def mock_worker(port, frames, delay_ms):
    counts = Counter()
    lock = threading.Lock()

    class Handler(BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.1"

        def log_message(self, *_args):
            pass

        def do_GET(self):  # noqa: N802
            with lock:
                data = json.dumps(dict(counts)).encode()
            self.send_response(200)
            self.send_header("Content-Length", str(len(data)))
            self.end_headers()
            self.wfile.write(data)

        def do_POST(self):  # noqa: N802
            length = int(self.headers.get("Content-Length", "0"))
            if not 0 < length <= 65536:
                self.send_error(400)
                return
            self.rfile.read(length)
            if self.headers.get("x-goog-api-key") != UPSTREAM_KEY:
                self.send_error(401)
                return
            variant = next((i for i in range(3) if f"gemini-workload-v{i}:" in self.path), None)
            if variant is None:
                self.send_error(404)
                return
            streaming = ":streamGenerateContent" in self.path
            fresh, cached, output, reasoning, _ = components(variant)
            usage = {"promptTokenCount": fresh + cached, "cachedContentTokenCount": cached,
                     "candidatesTokenCount": output, "thoughtsTokenCount": reasoning,
                     "totalTokenCount": fresh + cached + output + reasoning}
            terminal = {"candidates": [{"content": {"role": "model", "parts": []},
                          "finishReason": "STOP", "index": 0}], "usageMetadata": usage,
                        "modelVersion": f"gemini-workload-v{variant}"}
            if streaming:
                payloads = [{"candidates": [{"content": {"role": "model",
                             "parts": [{"text": "pong"}]}, "index": 0}]} for _ in range(frames)]
                chunks = [f"data: {json.dumps(payload)}\n\n".encode()
                          for payload in [*payloads, terminal]]
            else:
                terminal["candidates"][0]["content"]["parts"] = [{"text": "pong" * frames}]
                chunks = [json.dumps(terminal).encode()]
            with lock:
                counts["requests"] += 1
                counts["sse" if streaming else "unary"] += 1
                counts[f"variant_{variant}"] += 1
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream" if streaming else "application/json")
            self.send_header("Content-Length", str(sum(map(len, chunks))))
            self.end_headers()
            try:
                for chunk in chunks:
                    time.sleep(delay_ms / 1000)
                    self.wfile.write(chunk)
                    self.wfile.flush()
            except (BrokenPipeError, ConnectionResetError):
                with lock:
                    counts["disconnects"] += 1

    server = ThreadingHTTPServer(("127.0.0.1", port), Handler)
    server.daemon_threads = True
    server.serve_forever()


def clean_environment():
    # Never inherit gateway/vault endpoints, tracing exporters or SDK proxy settings.
    return {key: value for key, value in os.environ.items()
            if not key.startswith(("SANDHI_", "SENTINELPASS_", "OTEL_"))
            and key.upper() not in {"HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY"}}


def free_port():
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def wait_ready(process, url, deadline):
    with httpx.Client(timeout=.5, trust_env=False) as client:
        while time.monotonic() < deadline:
            if process.poll() is not None:
                raise RuntimeError(f"test process exited before readiness ({process.returncode})")
            try:
                if client.get(url).status_code == 200:
                    return
            except httpx.TransportError:
                pass
            time.sleep(.02)
    raise TimeoutError("loopback process readiness deadline")


def stop_process(process):
    if process.poll() is None:
        process.terminate()
        try:
            process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait(timeout=5)


def proc_sample(pid):
    """Linux process counters; missing/non-Linux is unavailable, never a fake zero."""
    try:
        base = Path(f"/proc/{pid}")
        fields = (base / "stat").read_text().rsplit(")", 1)[1].split()
        status = dict(line.split(":", 1) for line in (base / "status").read_text().splitlines()
                      if ":" in line)
        return {"cpu_seconds": (int(fields[11]) + int(fields[12])) / os.sysconf("SC_CLK_TCK"),
                "rss_bytes": int(status["VmRSS"].split()[0]) * 1024,
                "lifetime_rss_hwm_bytes": int(status["VmHWM"].split()[0]) * 1024,
                "fds": len(list((base / "fd").iterdir()))}
    except (OSError, ValueError, KeyError, IndexError):
        return None


class Resources:
    def __init__(self, pids):
        self.pids = pids
        self.before = {name: proc_sample(pid) for name, pid in pids.items()}
        self.samples = {name: [] for name in pids}
        self.done = threading.Event()
        self.thread = threading.Thread(target=self.poll, daemon=True)

    def poll(self):
        while not self.done.is_set():
            for name, pid in self.pids.items():
                if sample := proc_sample(pid):
                    self.samples[name].append(sample)
            self.done.wait(.02)

    def start(self):
        self.thread.start()

    def finish(self, completed):
        self.done.set()
        self.thread.join(timeout=1)
        result = {}
        for name, pid in self.pids.items():
            before, after, samples = self.before[name], proc_sample(pid), self.samples[name]
            if before is None or after is None or not samples:
                result[name] = {"available": False, "reason": "Linux /proc counters unavailable"}
                continue
            cpu = max(0, after["cpu_seconds"] - before["cpu_seconds"])
            result[name] = {"available": True, "cpu_seconds": cpu,
                "cpu_ms_per_completed_request": cpu * 1000 / completed if completed else None,
                "sampled_peak_rss_bytes": max(s["rss_bytes"] for s in samples),
                "lifetime_rss_hwm_bytes": after["lifetime_rss_hwm_bytes"],
                "sampled_peak_fds": max(s["fds"] for s in samples),
                "fd_before": before["fds"], "fd_after": after["fds"],
                "sample_count": len(samples), "sampling_interval_ms": 20}
        return result


def metrics(text):
    values = {}
    for line in text.splitlines():
        if line and not line.startswith("#"):
            key, value = line.rsplit(" ", 1)
            values[key] = float(value)
    return values


def assert_quiet(snapshot):
    for buffer in ("usage", "alerts"):
        for kind in ("queued", "in_flight", "dropped_total"):
            key = f'sandhi_buffer_{kind}{{buffer="{buffer}"}}'
            if key not in snapshot or snapshot[key] != 0:
                raise AssertionError(f"missing or nonzero {key}")
    if snapshot.get("sandhi_shutdown_active_operations") != 0:
        raise AssertionError("active operation metric missing or nonzero")


def validate_payload(lane, index, documents, frames):
    if lane.startswith("translation"):
        usages = [doc["usage"] for doc in documents if doc.get("usage")]
        texts = [choice.get("delta", choice.get("message", {})).get("content", "")
                 for doc in documents for choice in doc.get("choices", [])]
        expected_out = 40 + components(index)[3]
        assert usages and usages[-1]["prompt_tokens"] == 100
        assert usages[-1]["completion_tokens"] == expected_out
        assert usages[-1]["total_tokens"] == charge(index)
        assert usages[-1].get("prompt_tokens_details", {}).get("cached_tokens", 0) == components(index)[1]
        assert usages[-1]["completion_tokens_details"]["reasoning_tokens"] in (
            components(index)[3], None if components(index)[3] == 0 else components(index)[3])
        assert any(choice.get("finish_reason") == "stop"
                   for doc in documents for choice in doc.get("choices", []))
    else:
        usages = [doc["usageMetadata"] for doc in documents if "usageMetadata" in doc]
        texts = [part.get("text", "") for doc in documents for candidate in doc.get("candidates", [])
                 for part in candidate.get("content", {}).get("parts", [])]
        assert usages and usages[-1]["totalTokenCount"] == charge(index)
        assert usages[-1]["candidatesTokenCount"] == 40
        assert usages[-1]["cachedContentTokenCount"] == components(index)[1]
        assert usages[-1]["thoughtsTokenCount"] == components(index)[3]
        assert any(candidate.get("finishReason") == "STOP"
                   for doc in documents for candidate in doc.get("candidates", []))
    assert "".join(texts) == "pong" * frames


async def one_request(client, base, lane, index, token, run_id, scheduled, config):
    started = time.monotonic()
    record = {"index": index, "schedule_lag_ms": max(0, started - scheduled) * 1000,
              "status": None, "error": None}
    path, body = request_shape(lane, index)
    headers = {"Authorization": "Bearer " + token, "x-sandhi-run-id": run_id,
               "x-sandhi-step-id": str(index)} if token else {"x-goog-api-key": UPSTREAM_KEY}
    documents, pending = [], b""
    stream_done = False
    first_byte = first_content = None
    try:
        async with client.stream("POST", base + path, headers=headers, json=body) as response:
            record["status"] = response.status_code
            async for chunk in response.aiter_bytes():
                if chunk and first_byte is None:
                    first_byte = time.monotonic()
                pending += chunk
                if len(pending) > 262144:
                    raise ValueError("bounded synthetic response exceeded")
                if lane.endswith("sse"):
                    while b"\n" in pending:
                        line, pending = pending.split(b"\n", 1)
                        if line.startswith(b"data:") and line[5:].strip() == b"[DONE]":
                            stream_done = True
                        elif line.startswith(b"data:"):
                            if stream_done:
                                raise ValueError("content after terminal sentinel")
                            doc = json.loads(line[5:])
                            documents.append(doc)
                            if len(documents) > 512:
                                raise ValueError("bounded synthetic frame count exceeded")
                            content = any(choice.get("delta", {}).get("content")
                                          for choice in doc.get("choices", [])) or any(
                                part.get("text") for candidate in doc.get("candidates", [])
                                for part in candidate.get("content", {}).get("parts", []))
                            if content and first_content is None:
                                first_content = time.monotonic()
            if not lane.endswith("sse"):
                documents = [json.loads(pending)]
        if record["status"] != 200:
            raise ValueError("unexpected HTTP status")
        validate_payload(lane, index, documents, config.frames)
        if lane == "translation-sse" and not stream_done:
            raise ValueError("missing translated stream terminal sentinel")
        if first_byte is None or (lane.endswith("sse") and first_content is None):
            raise ValueError("missing response byte/content timing")
    except (httpx.HTTPError, ValueError, KeyError, TypeError, AttributeError, AssertionError) as error:
        # No bodies, URLs, keys or arbitrary provider messages in durable artifacts.
        record["error"] = type(error).__name__
    finished = time.monotonic()
    record.update({"latency_ms": (finished - started) * 1000,
                   "scheduled_latency_ms": (finished - scheduled) * 1000,
                   "ttfb_ms": (first_byte - started) * 1000 if first_byte else None,
                   "first_content_ms": (first_content - started) * 1000 if first_content else None})
    return record


async def drive(base, lane, mode, tokens, run_id, config, pids):
    samples, rejected, pending = [], [], set()
    resource = Resources(pids)
    resource.start()
    started = time.monotonic()
    try:
        async with httpx.AsyncClient(timeout=config.timeout_seconds, trust_env=False,
                limits=httpx.Limits(max_connections=config.max_pending,
                                   max_keepalive_connections=config.max_pending)) as client:
            async def execute(index, scheduled):
                token = tokens[index % len(tokens)] if tokens else None
                samples.append(await one_request(client, base, lane, index, token, run_id,
                                                 scheduled, config))

            if mode == "closed":
                next_index = iter(range(config.requests))

                async def worker():
                    for index in next_index:
                        await execute(index, time.monotonic())

                await asyncio.gather(*(worker() for _ in range(config.concurrency)))
            else:
                for index in range(config.requests):
                    scheduled = started + index / config.arrival_rate
                    await asyncio.sleep(max(0, scheduled - time.monotonic()))
                    # Do NOT await a semaphore here: that would turn fixed arrivals into a
                    # hidden closed-loop test. Shed excess generator work explicitly instead.
                    finished = {task for task in pending if task.done()}
                    for task in finished:
                        task.result()
                    pending -= finished
                    if len(pending) >= config.max_pending:
                        rejected.append(index)
                    else:
                        pending.add(asyncio.create_task(execute(index, scheduled)))
                if pending:
                    await asyncio.gather(*pending)
    finally:
        for task in pending:
            task.cancel()
        if pending:
            await asyncio.gather(*pending, return_exceptions=True)
        resource_report = resource.finish(len(samples))
    elapsed = time.monotonic() - started
    good = [sample for sample in samples if sample["error"] is None and sample["status"] == 200]
    result = {"lane": lane, "mode": mode, "offered": config.requests, "started": len(samples),
        "completed": len(samples), "successful": len(good), "generator_rejected": rejected,
        "statuses": dict(Counter(str(s["status"]) for s in samples)),
        "errors": dict(Counter(s["error"] for s in samples if s["error"])),
        "elapsed_seconds": elapsed, "completed_per_second": len(samples) / elapsed,
        "fixed_arrival_rate": config.arrival_rate if mode == "fixed" else None,
        "resources": resource_report, "samples": sorted(samples, key=lambda s: s["index"])}
    # Empty or failed runs must never generate a pretty all-zero latency summary/pass.
    if len(good) != config.requests or rejected:
        result["passed"] = False
        return result
    for field in ("latency_ms", "scheduled_latency_ms", "schedule_lag_ms", "ttfb_ms"):
        result[field] = distribution([sample[field] for sample in good])
    if lane.endswith("sse"):
        result["first_content_ms"] = distribution([sample["first_content_ms"] for sample in good])
    result["passed"] = True
    return result


def accounting(client, base, database, run_id, config, before_spend, deadline):
    expected = [(str(i), f"subject-{i % config.tenants}", f"tenant-{i % config.tenants}",
                 *components(i)) for i in range(config.requests)]
    while time.monotonic() < deadline:
        with closing(sqlite3.connect(database, timeout=1)) as connection:
            rows = connection.execute("SELECT step_id,subject_id,group_id,tokens_in,cache_read_tokens,"
                "tokens_out,COALESCE(reasoning_tokens,0),reasoning_included FROM usage_events "
                "WHERE run_id=?", (run_id,)).fetchall()
            remaining = connection.execute("SELECT COUNT(*) FROM budget_reservation WHERE settled=0").fetchone()[0]
            totals = dict(connection.execute("SELECT scope,SUM(actual) FROM budget_reservation "
                                            "WHERE settled=1 GROUP BY scope"))
        if len(rows) >= config.requests and remaining == 0:
            break
        time.sleep(.02)
    else:
        raise AssertionError("accounting convergence deadline")
    assert sorted(rows) == sorted(expected), "missing, duplicate or misattributed usage categories"
    expected_spend = Counter()
    for i in range(config.requests):
        expected_spend[f"group:tenant-{i % config.tenants}"] += charge(i)
    assert set(totals) <= set(expected_spend), "unexpected ledger scope"
    for scope, value in expected_spend.items():
        if time.monotonic() >= deadline:
            raise TimeoutError("accounting verification deadline")
        assert totals.get(scope, 0) - before_spend.get(scope, 0) == value
        body = client.get(base + "/admin/budget/usage", params={"scope": scope}).json()
        assert body["spent"] == totals[scope]
    tree = client.get(base + "/admin/usage/run/" + run_id).json()
    assert tree["run"]["total"]["billable_tokens"] == sum(expected_spend.values())
    assert_quiet(metrics(client.get(base + "/metrics").text))
    return {"events": len(rows), "charged_tokens": sum(expected_spend.values()),
            "unsettled_leases": remaining, "tenant_count": config.tenants}, totals


def host_metadata(binary, binary_revision):
    def optional_text(path):
        try:
            return Path(path).read_text().strip()
        except OSError:
            return None

    try:
        revision = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT, text=True, timeout=5).strip()
        dirty = bool(subprocess.check_output(["git", "status", "--porcelain"], cwd=ROOT, text=True, timeout=5))
    except (OSError, subprocess.SubprocessError):
        revision, dirty = None, None
    cpu = optional_text("/proc/cpuinfo")
    digest = hashlib.sha256()
    with binary.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(chunk)
    return {"platform": platform.system(), "kernel": platform.release(), "machine": platform.machine(),
        "cpu_model": next((line.split(":", 1)[1].strip() for line in (cpu or "").splitlines()
                           if line.startswith("model name")), None),
        "logical_cpus": os.cpu_count(), "affinity_cpus": len(os.sched_getaffinity(0))
        if hasattr(os, "sched_getaffinity") else None,
        "cgroup_cpu_max": optional_text("/sys/fs/cgroup/cpu.max"),
        "cgroup_memory_max": optional_text("/sys/fs/cgroup/memory.max"),
        "python_version": platform.python_version(), "harness_revision": revision,
        "harness_dirty": dirty, "binary_sha256": digest.hexdigest(),
        "binary_revision": binary_revision, "binary_revision_verified": False,
        "binary_profile_and_features": "caller-supplied binary; not inferred from filename",
        "host_exclusivity": "not established; results may include shared-host contention"}


def run_acceptance(binary: Path, config: Config, binary_revision=None):
    config.validate()
    binary = binary.resolve(strict=True)
    report = {"artifact_version": 1, "scope": "initial M1 loopback workload acceptance",
        "operator_acceptance": "pending actual-user review", "milestone_complete": False,
        "config": asdict(config), "phases": [],
        "limitations": ["Synthetic local provider, not production capacity or an SLO",
            "Sampled RSS/FD peaks may miss spikes; RSS high-water includes startup",
            "CPU counters have kernel tick granularity; no allocation measurement",
            "Direct-provider baseline is not an earlier Sandhi revision or pure proxy-overhead subtraction",
            "No warmup; fresh driver client each phase, provider pool and ledger history carry over",
            "Request timeout is HTTPX I/O inactivity; the remaining total deadline bounds each phase",
            "Fresh stores with accumulating short history; no long-history or full TD-0015 fault certification",
            "p99 from small samples is descriptive only; no performance regression gate"]}
    started = time.monotonic()
    deadline = started + config.total_seconds
    stage = "setup"
    try:
        with tempfile.TemporaryDirectory(prefix="sandhi-workload-") as temp, ExitStack() as cleanup:
            directory = Path(temp)
            executable = directory / "sandhi-proxy"
            shutil.copy2(binary, executable)
            report["host"] = host_metadata(executable, binary_revision)
            database = directory / "usage.db"
            mock_port, proxy_port = free_port(), free_port()
            while proxy_port == mock_port:
                proxy_port = free_port()
            mock_url, base = f"http://127.0.0.1:{mock_port}", f"http://127.0.0.1:{proxy_port}"
            with (directory / "mock.log").open("wb") as mock_log, (directory / "proxy.log").open("wb") as proxy_log:
                mock = subprocess.Popen([sys.executable, str(Path(__file__).resolve()), "--mock-port",
                    str(mock_port), "--frames", str(config.frames), "--frame-delay-ms", str(config.frame_delay_ms)],
                    env=clean_environment(), stdout=subprocess.DEVNULL, stderr=mock_log)
                cleanup.callback(stop_process, mock)
                wait_ready(mock, mock_url, min(deadline, time.monotonic() + 10))
                environment = clean_environment()
                environment.update({"SANDHI_BIND": f"127.0.0.1:{proxy_port}", "SANDHI_STORE": str(database),
                    "SANDHI_GEMINI_KEY": UPSTREAM_KEY, "SANDHI_GEMINI_BASE": mock_url,
                    "SANDHI_ADMIN_TOKEN": ADMIN, "SANDHI_LOG": "error",
                    "SANDHI_SHUTDOWN_GRACE_SECS": "3", "SANDHI_SHUTDOWN_QUIESCE_MS": "0",
                    "SANDHI_VAULT_BACKEND": "workload-disabled", "SANDHI_USAGE_BUFFER_CAPACITY": "4096"})
                proxy = subprocess.Popen([str(executable)], env=environment,
                                         stdout=subprocess.DEVNULL, stderr=proxy_log)
                cleanup.callback(stop_process, proxy)
                wait_ready(proxy, base + "/readyz", min(deadline, time.monotonic() + 10))
                with httpx.Client(headers={"Authorization": "Bearer " + ADMIN}, timeout=3, trust_env=False) as admin:
                    tokens = []
                    for tenant in range(config.tenants):
                        if time.monotonic() >= deadline:
                            raise TimeoutError("tenant setup deadline")
                        scope = f"group:tenant-{tenant}"
                        response = admin.post(base + "/admin/budget", json={"scope": scope,
                            "limit_tokens": 100000000, "policy": "block", "window": "total"})
                        assert response.status_code == 200
                        response = admin.post(base + "/admin/keys/share", json={"upstream": "gemini",
                            "subject": f"subject-{tenant}", "group": f"tenant-{tenant}", "budget_scope": scope})
                        assert response.status_code == 200
                        tokens.append(response.json()["virtual_key"])
                    before_spend = {}
                    # Baseline uses the same provider family/content/pacing, not translated input.
                    lanes = [*LANES, "provider-unary", "provider-sse"]
                    for repeat in range(config.repeats):
                        cases = [(lane, mode) for lane in lanes for mode in ("closed", "fixed")]
                        random.Random(config.seed + repeat).shuffle(cases)
                        for lane, mode in cases:
                            stage = f"request:{repeat}:{lane}:{mode}"
                            remaining = deadline - time.monotonic()
                            if remaining <= 0:
                                raise TimeoutError("total workload deadline")
                            direct = lane.startswith("provider")
                            wire_lane = lane.replace("provider", "transparent")
                            run_id = f"workload-{repeat}-{lane}-{mode}"
                            mock_before = admin.get(mock_url).json().get("requests", 0)
                            before_metrics = metrics(admin.get(base + "/metrics").text)
                            result = asyncio.run(asyncio.wait_for(drive(mock_url if direct else base,
                                wire_lane, mode, [] if direct else tokens, run_id, config,
                                {"proxy": proxy.pid, "provider_mock": mock.pid, "driver": os.getpid()}),
                                timeout=remaining))
                            result.update({"lane": lane, "repeat": repeat})
                            report["phases"].append(result)
                            if not result["passed"]:
                                raise AssertionError("workload request failure or generator overload")
                            # A phase is accepted only after independent counters/accounting,
                            # not merely after valid HTTP responses.
                            result["passed"] = False
                            assert admin.get(mock_url).json().get("requests", 0) - mock_before == config.requests
                            if not direct:
                                stage = f"accounting:{repeat}:{lane}:{mode}"
                                result["accounting"], before_spend = accounting(admin, base, database,
                                    run_id, config, before_spend, min(deadline, time.monotonic() + 10))
                                after_metrics = metrics(admin.get(base + "/metrics").text)
                                plane = "translation" if lane.startswith("translation") else "transparent"
                                plane_count = sum(value - before_metrics.get(key, 0)
                                    for key, value in after_metrics.items()
                                    if key.startswith("sandhi_requests_total{") and f'plane="{plane}"' in key)
                                assert plane_count == config.requests, "wrong forwarding plane or logical count"
                            result["passed"] = True
                    # Untimed denied paths must neither dispatch nor produce accounting rows.
                    stage = "denied_paths"
                    if time.monotonic() >= deadline:
                        raise TimeoutError("total workload deadline")
                    count_before = admin.get(mock_url).json().get("requests", 0)
                    with closing(sqlite3.connect(database)) as connection:
                        rows_before = connection.execute("SELECT COUNT(*) FROM usage_events").fetchone()[0]
                    path, body = request_shape("transparent-unary", 0)
                    assert admin.post(base + path, headers={"Authorization": "Bearer invalid-synthetic"}, json=body).status_code == 401
                    assert admin.post(base + "/admin/budget", json={"scope": "group:tenant-0", "limit_tokens": 0}).status_code == 200
                    assert admin.post(base + path, headers={"Authorization": "Bearer " + tokens[0]}, json=body).status_code == 429
                    assert admin.get(mock_url).json().get("requests", 0) == count_before
                    with closing(sqlite3.connect(database)) as connection:
                        assert connection.execute("SELECT COUNT(*) FROM usage_events").fetchone()[0] == rows_before
                        assert connection.execute("SELECT COUNT(*) FROM budget_reservation WHERE settled=0").fetchone()[0] == 0
                    report["denied_path_checks"] = "passed: unauthorized and exhausted budget did not dispatch"
                stage = "shutdown"
                stop_process(proxy)
                assert proxy.returncode == 0, "proxy did not shut down cleanly"
                report["proxy_shutdown_exit_code"] = proxy.returncode
            if time.monotonic() >= deadline:
                raise TimeoutError("total workload deadline")
            report["passed"] = True
    except (AssertionError, OSError, ValueError, RuntimeError, TimeoutError, httpx.HTTPError,
            sqlite3.Error, KeyError, TypeError, AttributeError) as error:
        report["passed"] = False
        report["failure"] = type(error).__name__  # never arbitrary provider/config text
        report["failure_stage"] = stage
    report["elapsed_seconds"] = time.monotonic() - started
    groups = {}
    for phase in report["phases"]:
        if phase["passed"]:
            groups.setdefault((phase["lane"], phase["mode"]), []).append(phase["latency_ms"]["p99"])
    report["repeat_variance"] = [{"lane": lane, "mode": mode, "repeats": len(values),
        "p99_ms_mean": statistics.mean(values), "p99_ms_stdev": statistics.stdev(values)
        if len(values) > 1 else None, "p99_relative_range": (max(values) - min(values)) / statistics.mean(values)
        if statistics.mean(values) > 0 else None} for (lane, mode), values in sorted(groups.items())]
    return report


def compact_summary(report):
    """Deterministic view for reviewed evidence; retain the full artifact separately."""
    canonical = json.dumps(report, sort_keys=True, allow_nan=False).encode()
    return {**{key: value for key, value in report.items() if key != "phases"},
            "full_report_canonical_sha256": hashlib.sha256(canonical).hexdigest(),
            "request_samples_omitted": True,
            "phases": [{key: value for key, value in phase.items() if key != "samples"}
                       for phase in report["phases"]]}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path)
    parser.add_argument("--binary-revision", help="Caller-reported, not inferred or verified")
    parser.add_argument("--output", type=Path)
    parser.add_argument("--summary-output", type=Path,
                        help="Optional compact evidence JSON without individual request samples")
    parser.add_argument("--mock-port", type=int, help=argparse.SUPPRESS)
    for name, field in Config.__dataclass_fields__.items():
        parser.add_argument("--" + name.replace("_", "-"), type=type(field.default), default=field.default)
    args = parser.parse_args()
    config = Config(**{name: getattr(args, name) for name in Config.__dataclass_fields__})
    config.validate()
    if args.mock_port:
        mock_worker(args.mock_port, config.frames, config.frame_delay_ms)
        return
    if args.binary is None or args.output is None:
        parser.error("--binary and --output are required")
    if args.summary_output and (
            args.output.resolve() == args.summary_output.resolve()
            or (args.output.exists() and args.summary_output.exists()
                and args.output.samefile(args.summary_output))):
        parser.error("full and summary outputs must be different files")
    report = run_acceptance(args.binary, config, args.binary_revision)
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(report, indent=2, allow_nan=False) + "\n")
    if args.summary_output:
        args.summary_output.parent.mkdir(parents=True, exist_ok=True)
        args.summary_output.write_text(json.dumps(compact_summary(report), indent=2,
                                                 sort_keys=True, allow_nan=False) + "\n")
    print(json.dumps({"passed": report["passed"], "phases": len(report["phases"]),
                      "operator_acceptance": report["operator_acceptance"]}))
    raise SystemExit(0 if report["passed"] else 1)


if __name__ == "__main__":
    main()
