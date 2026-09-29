#!/usr/bin/env python3
"""Verify shipped synthetic ONNX bundle through the actual Rust policy adapter."""

import os
from pathlib import Path
import subprocess
import tempfile

import onnxruntime
from sensitive_policy import prepare

root = Path(__file__).resolve().parents[1]
libraries = sorted(
    (Path(onnxruntime.__file__).parent / "capi").glob("libonnxruntime.*")
)
libraries = [p for p in libraries if p.name.endswith(".dylib") or ".so." in p.name]
if len(libraries) != 1:
    raise RuntimeError("expected one approved ONNX Runtime library")
with tempfile.TemporaryDirectory(prefix="sandhi-sensitive-") as temporary:
    settings = prepare(
        root / "crates/sandhi-proxy/assets/sensitive-text-v1",
        libraries[0],
        Path(temporary) / "deployment",
    )
    subprocess.run(
        [
            "cargo",
            "test",
            "-p",
            "sandhi-proxy",
            "--features",
            "policy-onnx",
            "--test",
            "sensitive_onnx",
            "--",
            "--ignored",
        ],
        cwd=root,
        env={
            **os.environ,
            "SANDHI_SENSITIVE_TEST_MANIFEST": settings["SANDHI_POLICY_ONNX"],
        },
        check=True,
        timeout=600,
    )
