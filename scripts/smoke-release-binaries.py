#!/usr/bin/env python3
"""Smoke native release binaries without providers, credentials, or durable state.

Run with --binary-dir PATH on Linux/macOS. This checks `sandhi --help`, actual
/healthz and /readyz responses from sandhi-proxy, and a zero-exit SIGTERM shutdown.
It does not assert artifact version, provider connectivity, persistence, or load.
The proxy has no --help/--version switch. Child output is discarded, never logged.
No caller environment is inherited. The only HTTP destination is IPv4 loopback;
HTTP proxy environment settings are not consulted. Use a CI job timeout as an
additional outer bound for OS-level stalls. No binaries are downloaded here.
"""

from __future__ import annotations

import argparse
import http.client
import io
import math
import os
from pathlib import Path
import signal
import socket
import subprocess
import sys
import tempfile
import time


class SmokeFailure(Exception):
    """Only fixed, non-sensitive diagnostics are emitted to the caller."""


def isolated_environment(directory, port):
    # Deliberate allowlist, NOT an inherited environment with selected keys removed.
    # HOME, loader hooks, proxy variables, provider keys and vault/telemetry settings
    # are absent. No store means the proxy never initializes a vault backend.
    return {
        "PATH": os.defpath,
        "LANG": "C",
        "LC_ALL": "C",
        "TMPDIR": str(directory),
        "XDG_CONFIG_HOME": str(directory / "config"),
        "XDG_CACHE_HOME": str(directory / "cache"),
        "SANDHI_CONFIG": str(directory / "empty.json"),
        "SANDHI_BIND": f"127.0.0.1:{port}",
        "SANDHI_LOG": "off",
        "SANDHI_SHUTDOWN_GRACE_SECS": "3",
        "SANDHI_SHUTDOWN_QUIESCE_MS": "0",
    }


def unused_loopback_port():
    # The binary does not support inherited listeners. Release the reservation just
    # before spawning; a bind collision is a smoke failure, not a successful skip.
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as listener:
        listener.bind(("127.0.0.1", 0))
        return listener.getsockname()[1]


def start(binary, arguments, directory, environment):
    return subprocess.Popen(
        [str(binary), *arguments], cwd=directory, env=environment,
        stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
        start_new_session=True,
    )


def reap_failed_child(process):
    if process.poll() is None:
        process.kill()
        try:
            process.wait(timeout=2)
        except subprocess.TimeoutExpired:
            raise SmokeFailure("child could not be reaped after forced cleanup") from None


def check_cli(binary, directory, environment, timeout=5):
    process = start(binary, ["--help"], directory, environment)
    try:
        try:
            code = process.wait(timeout=timeout)
        except subprocess.TimeoutExpired:
            raise SmokeFailure("sandhi --help exceeded its deadline") from None
        if code != 0:
            raise SmokeFailure("sandhi --help returned a nonzero exit status")
    finally:
        reap_failed_child(process)


def probe(port, path, expected_body, timeout):
    # Socket-level absolute deadline covers header AND body trickles. Parse only
    # after bounded bytes are in memory; HTTPResponse can then perform no network IO.
    deadline = time.monotonic() + timeout
    received = bytearray()
    with socket.create_connection(("127.0.0.1", port), timeout=timeout) as connection:
        connection.settimeout(max(.001, deadline - time.monotonic()))
        connection.sendall(f"GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n".encode("ascii"))
        while True:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise TimeoutError("probe deadline")
            connection.settimeout(remaining)
            chunk = connection.recv(min(1024, 4097 - len(received)))
            if not chunk:
                break
            received.extend(chunk)
            if len(received) > 4096:
                raise SmokeFailure("proxy probe response exceeds its byte limit")

    class BufferedSocket:
        def makefile(self, *_args, **_kwargs):
            return io.BytesIO(received)

    with http.client.HTTPResponse(BufferedSocket()) as response:
        response.begin()
        body = response.read(65)
        if response.status != 200 or body != expected_body:
            raise SmokeFailure("proxy probe returned an unexpected status or body")
        if path == "/readyz" and response.getheader("Cache-Control") != "no-store":
            raise SmokeFailure("readiness response lacks its no-store contract")


def wait_ready(process, port, timeout, *, clock=time.monotonic, sleep=time.sleep):
    deadline = clock() + timeout
    while clock() < deadline:
        if process.poll() is not None:
            raise SmokeFailure("proxy exited before becoming ready")
        try:
            for path, body in (("/healthz", b"ok"), ("/readyz", b"ready")):
                remaining = deadline - clock()
                if remaining <= 0:
                    raise SmokeFailure("proxy startup exceeded its deadline")
                probe(port, path, body, min(.5, remaining))
            if process.poll() is not None:
                raise SmokeFailure("proxy exited while probes completed")
            return
        except (OSError, http.client.HTTPException):
            sleep(min(.05, max(0, deadline - clock())))
    raise SmokeFailure("proxy startup exceeded its deadline")


def graceful_shutdown(process, timeout):
    if process.poll() is not None:
        raise SmokeFailure("proxy exited before SIGTERM")
    process.send_signal(signal.SIGTERM)
    try:
        code = process.wait(timeout=timeout)
    except subprocess.TimeoutExpired:
        raise SmokeFailure("proxy graceful shutdown exceeded its deadline") from None
    if code != 0:
        raise SmokeFailure("proxy SIGTERM shutdown returned a nonzero exit status")


def run_smoke(binary_directory, startup_timeout=15, shutdown_timeout=8):
    directory = Path(binary_directory).resolve()
    binaries = [directory / name for name in ("sandhi", "sandhi-proxy")]
    if any(not binary.is_file() or not os.access(binary, os.X_OK) for binary in binaries):
        raise SmokeFailure("binary directory must contain executable sandhi and sandhi-proxy")
    with tempfile.TemporaryDirectory(prefix="sandhi-binary-smoke-") as temporary:
        isolated = Path(temporary)
        (isolated / "empty.json").write_text("{}\n", encoding="utf-8")
        environment = isolated_environment(isolated, 0)
        check_cli(binaries[0], isolated, environment)
        environment["SANDHI_BIND"] = f"127.0.0.1:{unused_loopback_port()}"
        process = start(binaries[1], [], isolated, environment)
        try:
            port = int(environment["SANDHI_BIND"].rsplit(":", 1)[1])
            wait_ready(process, port, startup_timeout)
            graceful_shutdown(process, shutdown_timeout)
        finally:
            reap_failed_child(process)


def bounded_timeout(raw):
    value = float(raw)
    if not math.isfinite(value) or not .1 <= value <= 60:
        raise argparse.ArgumentTypeError("timeout must be finite in 0.1..60 seconds")
    return value


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary-dir", required=True, type=Path)
    parser.add_argument("--startup-timeout-seconds", type=bounded_timeout, default=15)
    parser.add_argument("--shutdown-timeout-seconds", type=bounded_timeout, default=8)
    args = parser.parse_args(argv)
    if os.name != "posix":
        parser.error("this native-binary smoke requires Linux or macOS SIGTERM semantics")
    try:
        run_smoke(args.binary_dir, args.startup_timeout_seconds, args.shutdown_timeout_seconds)
    except SmokeFailure as error:
        print(f"FAIL: {error}", file=sys.stderr)
        return 1
    except (OSError, ValueError, subprocess.SubprocessError):
        # Never print child output, paths, caller environment, or OS exception text.
        print("FAIL: binary smoke OS/process operation failed", file=sys.stderr)
        return 1
    print("PASS: sandhi --help; proxy /healthz + /readyz; zero-exit graceful SIGTERM (isolated volatile mode)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
