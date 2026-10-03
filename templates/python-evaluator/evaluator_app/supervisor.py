"""Bounded local process supervision for the HTTP template, independent of Flask."""

import json
import queue
import socket
import subprocess
import threading
import time

from .protocol import MAX_FRAME, checked_bytes, request_view, result, strict_json


class Unavailable(Exception):
    """Safe public failure; never attach model diagnostics."""


def remaining(deadline):
    value = deadline - time.monotonic()
    if value <= 0:
        raise Unavailable
    return value


class Process:
    def __init__(self, settings):
        self.child = None
        self.socket = None
        checked_bytes(settings.bundle, settings.bundle_sha256, 4194304)
        checked_bytes(settings.artifact, settings.artifact_sha256, 1048576)
        parent, child = socket.socketpair()
        self.socket = parent
        try:
            self.child = subprocess.Popen(
                [
                    settings.python,
                    "-I",
                    "-B",
                    "-u",
                    str(settings.bundle),
                    str(settings.artifact),
                    settings.artifact_sha256,
                ],
                stdin=child,
                stdout=child,
                stderr=subprocess.DEVNULL,
                cwd="/",
                env={
                    "OMP_NUM_THREADS": "1",
                    "OPENBLAS_NUM_THREADS": "1",
                    "MKL_NUM_THREADS": "1",
                    "VECLIB_MAXIMUM_THREADS": "1",
                },
                close_fds=True,
            )
            child.close()
            ready = self.read(time.monotonic() + settings.startup_timeout_ms / 1000)
            if (
                ready
                != {
                    "version": 1,
                    "kind": "ready",
                    "artifact_sha256": settings.artifact_sha256,
                }
                or type(ready.get("version")) is not int
            ):
                raise Unavailable
        except BaseException:
            child.close()
            self.close()
            raise

    def read(self, deadline):
        data = bytearray()
        while True:
            self.socket.settimeout(remaining(deadline))
            chunk = self.socket.recv(min(512, 4097 - len(data)))
            if not chunk:
                raise Unavailable
            data.extend(chunk)
            if len(data) > 4096:
                raise Unavailable
            if b"\n" in data:
                if data.index(b"\n") != len(data) - 1:
                    raise Unavailable
                return strict_json(data[:-1])

    def evaluate(self, value, deadline):
        data = (
            json.dumps(value, allow_nan=False, separators=(",", ":")).encode() + b"\n"
        )
        if len(data) > MAX_FRAME:
            raise Unavailable
        self.socket.settimeout(remaining(deadline))
        self.socket.sendall(data)
        reply = result(self.read(deadline), value["id"])
        remaining(deadline)
        return reply

    def close(self):
        if self.socket is not None:
            self.socket.close()
            self.socket = None
        if self.child is not None:
            if self.child.poll() is None:
                self.child.kill()
            self.child.wait(timeout=2)


COUNTER_LIMIT = (1 << 63) - 1


class Slot:
    def __init__(self, settings, stop):
        self.settings = settings
        self.stop = stop
        self.stats_lock = threading.Lock()
        self.state = "ready"
        self.counts = {"completed_total": 0, "failed_total": 0, "restarts_total": 0}
        self.ready = threading.Event()
        self.busy = threading.Lock()
        self.jobs = queue.Queue(maxsize=1)
        self.process = Process(settings)
        self.ready.set()
        self.thread = threading.Thread(
            target=self.run, name="evaluator-supervisor", daemon=True
        )
        try:
            self.thread.start()
        except BaseException:
            self.process.close()
            raise

    def record(self, counter, state=None):
        with self.stats_lock:
            self.counts[counter] = min(COUNTER_LIMIT, self.counts[counter] + 1)
            if state is not None:
                self.state = state

    def snapshot(self):
        with self.stats_lock:
            return dict(
                self.counts,
                state=self.state,
                busy=self.state == "ready" and self.busy.locked(),
            )

    def run(self):
        restarts = 0
        try:
            while not self.stop.is_set():
                try:
                    value, deadline, reply = self.jobs.get(timeout=0.05)
                except queue.Empty:
                    continue
                try:
                    remaining(deadline)
                    answer = self.process.evaluate(value, deadline)
                except (
                    Exception
                ):  # noqa: BLE001 - only safe unavailable crosses this boundary
                    del value  # Do not retain the last prompt while warming a replacement.
                    self.ready.clear()
                    self.record("failed_total", "recovering")
                    reply.put(None)
                    try:
                        self.process.close()
                        if self.stop.is_set() or restarts >= 3:
                            return
                        restarts += 1
                        self.record("restarts_total")
                        self.process = Process(self.settings)
                        with self.stats_lock:
                            self.state = "ready"
                        self.ready.set()
                    except (
                        Exception
                    ):  # noqa: BLE001 - failed replacement leaves the slot closed
                        return
                    finally:
                        self.busy.release()
                else:
                    del value  # The idle supervisor retains metadata only.
                    self.record("completed_total")
                    self.busy.release()
                    reply.put(answer)
        finally:
            self.ready.clear()
            self.process.close()
            with self.stats_lock:
                self.state = "closed" if self.stop.is_set() else "unavailable"


class Supervisor:
    def __init__(self, settings):
        self.stop = threading.Event()
        self.slots = []
        self.closed = False
        self.stats_lock = threading.Lock()
        self.rejected_total = 0
        try:
            for _ in range(settings.pool_size):
                self.slots.append(Slot(settings, self.stop))
        except BaseException:
            self.close()
            raise

    @property
    def ready(self):
        return not self.stop.is_set() and any(
            slot.ready.is_set() for slot in self.slots
        )

    @property
    def pids(self):
        return tuple(
            slot.process.child.pid for slot in self.slots if slot.ready.is_set()
        )

    def status(self):
        snapshots = [slot.snapshot() for slot in self.slots]
        ready = sum(value["state"] == "ready" for value in snapshots)
        busy = sum(value["busy"] for value in snapshots)
        recovering = sum(value["state"] == "recovering" for value in snapshots)
        if self.stop.is_set():
            state = "closed"
        elif ready > busy:
            state = "ready"
        elif ready:
            state = "saturated"
        elif recovering:
            state = "recovering"
        else:
            state = "unavailable"
        with self.stats_lock:
            rejected = self.rejected_total
        return {
            "state": state,
            "capacity": len(snapshots),
            "ready_slots": ready,
            "busy_slots": busy,
            "recovering_slots": recovering,
            "unavailable_slots": sum(
                value["state"] in {"unavailable", "closed"} for value in snapshots
            ),
            "rejected_total": rejected,
            **{
                key: min(COUNTER_LIMIT, sum(value[key] for value in snapshots))
                for key in ("completed_total", "failed_total", "restarts_total")
            },
        }

    def reject(self):
        with self.stats_lock:
            self.rejected_total = min(COUNTER_LIMIT, self.rejected_total + 1)
        raise Unavailable

    def evaluate(self, value, deadline):
        request_view(value)
        try:
            remaining(deadline)
        except Unavailable:
            self.reject()
        if self.stop.is_set():
            self.reject()
        for slot in self.slots:
            if not slot.ready.is_set() or not slot.busy.acquire(blocking=False):
                continue
            reply = queue.Queue(maxsize=1)
            try:
                slot.jobs.put_nowait((value, deadline, reply))
            except queue.Full:
                slot.busy.release()
                continue
            try:
                answer = reply.get(timeout=remaining(deadline))
            except queue.Empty:
                raise Unavailable from None
            remaining(deadline)
            if answer is None:
                raise Unavailable
            return answer
        self.reject()

    def close(self):
        if self.closed:
            return
        self.closed = True
        self.stop.set()
        for slot in self.slots:
            slot.thread.join(timeout=slot.settings.startup_timeout_ms / 1000 + 5)
            if slot.thread.is_alive():
                # Host/service-manager cleanup remains the final safety net for
                # uninterruptible OS I/O; never reuse this slot.
                slot.process.close()
                slot.thread.join(timeout=2)
