"""Offline fake-process tests; no subprocess, network, providers or vault access."""

import contextlib
import importlib.util
import io
import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import Mock, patch

SPEC = importlib.util.spec_from_file_location(
    "release_binary_smoke", Path(__file__).resolve().parents[2] / "scripts" / "smoke-release-binaries.py")
smoke = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = smoke
SPEC.loader.exec_module(smoke)


class BinarySmokeTests(unittest.TestCase):
    def test_environment_is_allowlisted_and_does_not_inherit_secrets_or_loaders(self):
        inherited = {"SANDHI_STORE": "/personal/vault.db", "SANDHI_OPENAI_KEY": "secret",
                     "HOME": "/personal", "HTTP_PROXY": "secret", "HTTPS_PROXY": "secret",
                     "LD_PRELOAD": "/evil.so", "DYLD_INSERT_LIBRARIES": "/evil.dylib",
                     "SANDHI_OTEL_EXPORT": "otlp", "PATH": "/untrusted", "GH_TOKEN": "secret"}
        with patch.dict(os.environ, inherited, clear=True):
            environment = smoke.isolated_environment(Path("/isolated"), 1234)
        for key in inherited:
            if key == "PATH": self.assertEqual(environment[key], os.defpath)
            else: self.assertNotIn(key, environment)
        self.assertEqual(environment["SANDHI_BIND"], "127.0.0.1:1234")
        self.assertEqual(environment["SANDHI_CONFIG"], "/isolated/empty.json")
        self.assertNotIn("secret", str(environment))

    def test_subprocess_launch_has_no_shell_or_inherited_streams(self):
        with patch.object(smoke.subprocess, "Popen") as popen:
            smoke.start(Path("/bin/sandhi"), ["--help"], Path("/isolated"), {"LANG": "C"})
        args, kwargs = popen.call_args
        self.assertEqual(args[0], ["/bin/sandhi", "--help"])
        self.assertNotIn("shell", kwargs)
        self.assertTrue(kwargs["start_new_session"])
        for stream in ("stdin", "stdout", "stderr"):
            self.assertEqual(kwargs[stream], subprocess.DEVNULL)
        self.assertEqual(kwargs["env"], {"LANG": "C"})

    def test_cli_help_requires_zero_exit(self):
        for code in (0, 1, -signal.SIGTERM):
            process = Mock()
            process.wait.return_value = code
            process.poll.return_value = code
            with self.subTest(code=code), patch.object(smoke, "start", return_value=process):
                if code == 0: smoke.check_cli(Path("sandhi"), Path("tmp"), {})
                else:
                    with self.assertRaises(smoke.SmokeFailure):
                        smoke.check_cli(Path("sandhi"), Path("tmp"), {})

    def test_cli_timeout_forces_cleanup_and_never_passes(self):
        process = Mock()
        process.wait.side_effect = [subprocess.TimeoutExpired("secret", 5), -9]
        process.poll.return_value = None
        with patch.object(smoke, "start", return_value=process), self.assertRaisesRegex(smoke.SmokeFailure, "deadline"):
            smoke.check_cli(Path("sandhi"), Path("tmp"), {})
        process.kill.assert_called_once()
        self.assertEqual(process.wait.call_args_list[-1].kwargs, {"timeout": 2})

    def test_probe_exact_routes_bodies_and_readiness_cache_control(self):
        for path, body in (("/healthz", b"ok"), ("/readyz", b"ready")):
            connection = Mock()
            connection.__enter__ = Mock(return_value=connection)
            connection.__exit__ = Mock(return_value=False)
            connection.recv.side_effect = [b"HTTP/1.1 200 OK\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n" + body, b""]
            with patch.object(smoke.socket, "create_connection", return_value=connection) as factory:
                smoke.probe(1234, path, body, .5)
            factory.assert_called_once_with(("127.0.0.1", 1234), timeout=.5)
            self.assertIn(f"GET {path} HTTP/1.1".encode(), connection.sendall.call_args.args[0])
            connection.__exit__.assert_called_once()

    def test_wrong_probe_status_body_or_cache_policy_fails_closed(self):
        for status, body, cache in ((404, b"ready", "no-store"), (200, b"wrong", "no-store"),
                                    (200, b"ready", None), (200, b"ready", "public")):
            connection = Mock()
            connection.__enter__ = Mock(return_value=connection)
            connection.__exit__ = Mock(return_value=False)
            cache_header = "" if cache is None else f"Cache-Control: {cache}\r\n"
            connection.recv.side_effect = [f"HTTP/1.1 {status} Test\r\n{cache_header}\r\n".encode() + body, b""]
            with patch.object(smoke.socket, "create_connection", return_value=connection), self.assertRaises(smoke.SmokeFailure):
                smoke.probe(1234, "/readyz", b"ready", .5)
            connection.__exit__.assert_called_once()

    def test_probe_byte_limit_and_trickle_deadline(self):
        connection = Mock()
        connection.__enter__ = Mock(return_value=connection)
        connection.__exit__ = Mock(return_value=False)
        connection.recv.return_value = b"x" * 1024
        with patch.object(smoke.socket, "create_connection", return_value=connection), self.assertRaisesRegex(smoke.SmokeFailure, "byte limit"):
            smoke.probe(1234, "/healthz", b"ok", .5)
        with patch.object(smoke.socket, "create_connection", return_value=connection), \
                patch.object(smoke.time, "monotonic", side_effect=[0, 0, .1, .6]), self.assertRaises(TimeoutError):
            smoke.probe(1234, "/healthz", b"ok", .5)

    def test_readiness_retries_only_transport_and_checks_both_routes(self):
        process = Mock()
        process.poll.return_value = None
        now = [0]
        def sleep(seconds): now[0] += seconds
        with patch.object(smoke, "probe", side_effect=[ConnectionRefusedError(), None, None]) as probe:
            smoke.wait_ready(process, 1234, 1, clock=lambda: now[0], sleep=sleep)
        self.assertEqual([call.args[1] for call in probe.call_args_list], ["/healthz", "/healthz", "/readyz"])
        self.assertGreater(now[0], 0)

    def test_startup_deadline_and_unexpected_exit_do_not_pass(self):
        process = Mock()
        process.poll.return_value = None
        now = [0]
        def sleep(seconds): now[0] += seconds
        with patch.object(smoke, "probe", side_effect=ConnectionRefusedError()), self.assertRaisesRegex(smoke.SmokeFailure, "deadline"):
            smoke.wait_ready(process, 1234, .2, clock=lambda: now[0], sleep=sleep)
        process.poll.return_value = 1
        with patch.object(smoke, "probe") as probe, self.assertRaisesRegex(smoke.SmokeFailure, "exited"):
            smoke.wait_ready(process, 1234, 1)
        probe.assert_not_called()

    def test_exit_during_probe_is_rejected(self):
        process = Mock()
        process.poll.side_effect = [None, 1]
        with patch.object(smoke, "probe"), self.assertRaisesRegex(smoke.SmokeFailure, "exited"):
            smoke.wait_ready(process, 1234, 1)

    def test_graceful_shutdown_requires_sigterm_zero_exit(self):
        for code in (0, 124, -signal.SIGTERM):
            process = Mock()
            process.poll.return_value = None
            process.wait.return_value = code
            with self.subTest(code=code):
                if code == 0: smoke.graceful_shutdown(process, 8)
                else:
                    with self.assertRaises(smoke.SmokeFailure): smoke.graceful_shutdown(process, 8)
            process.send_signal.assert_called_once_with(signal.SIGTERM)
            process.wait.assert_called_once_with(timeout=8)

    def test_shutdown_timeout_or_preexisting_exit_fails(self):
        process = Mock()
        process.poll.return_value = None
        process.wait.side_effect = subprocess.TimeoutExpired("secret", 8)
        with self.assertRaisesRegex(smoke.SmokeFailure, "deadline"):
            smoke.graceful_shutdown(process, 8)
        process.poll.return_value = 0
        with self.assertRaisesRegex(smoke.SmokeFailure, "before SIGTERM"):
            smoke.graceful_shutdown(process, 8)

    def test_cleanup_is_bounded(self):
        process = Mock()
        process.poll.return_value = None
        process.wait.side_effect = subprocess.TimeoutExpired("secret", 2)
        with self.assertRaisesRegex(smoke.SmokeFailure, "reaped"):
            smoke.reap_failed_child(process)
        process.kill.assert_called_once()
        process.wait.assert_called_once_with(timeout=2)

    def test_orchestration_uses_empty_config_then_cleans_process_before_tempdir(self):
        process = Mock()
        with tempfile.TemporaryDirectory() as binary_dir:
            for name in ("sandhi", "sandhi-proxy"):
                binary = Path(binary_dir) / name
                binary.touch()
                binary.chmod(0o700)
            def help_check(binary, directory, environment):
                self.assertEqual((directory / "empty.json").read_text(), "{}\n")
                self.assertNotEqual(directory, Path(binary_dir))
            def cleanup(child):
                self.assertIs(child, process)
                self.assertTrue(spawn.call_args.args[2].is_dir())
            with patch.object(smoke, "unused_loopback_port", return_value=1234), \
                    patch.object(smoke, "check_cli", side_effect=help_check), \
                    patch.object(smoke, "start", return_value=process) as spawn, \
                    patch.object(smoke, "wait_ready") as ready, \
                    patch.object(smoke, "graceful_shutdown") as shutdown, \
                    patch.object(smoke, "reap_failed_child", side_effect=cleanup):
                smoke.run_smoke(Path(binary_dir))
            self.assertEqual(spawn.call_args.args[1], [])
            self.assertFalse(spawn.call_args.args[2].exists())
            ready.assert_called_once_with(process, 1234, 15)
            shutdown.assert_called_once_with(process, 8)

    def test_invalid_binary_directory_and_timeouts(self):
        with tempfile.TemporaryDirectory() as directory, self.assertRaises(smoke.SmokeFailure):
            smoke.run_smoke(Path(directory))
        for value in ("nan", "inf", "0", "-1", "61"):
            with contextlib.redirect_stderr(io.StringIO()), self.assertRaises(SystemExit) as error:
                smoke.main(["--binary-dir", ".", "--startup-timeout-seconds", value])
            self.assertEqual(error.exception.code, 2)

    def test_main_redacts_os_errors_and_returns_failure(self):
        output = io.StringIO()
        with patch.object(smoke, "run_smoke", side_effect=OSError("secret-path-token")), contextlib.redirect_stderr(output):
            self.assertEqual(smoke.main(["--binary-dir", "."]), 1)
        self.assertNotIn("secret-path-token", output.getvalue())


if __name__ == "__main__":
    unittest.main()
