"""Real vendor SDK inline-image calls, with Rust-owned source/receipt assertions."""

import hashlib
import os
from pathlib import Path
import shutil
import subprocess
import sys

import pytest

from conftest import REPO_ROOT
from source_provenance import freeze_cargo_build

PROFILES = ('chat', 'messages', 'responses', 'gemini', 'codex', 'cohere', 'ollama')


@pytest.fixture(scope='module')
def media_executable(tmp_path_factory):
    directory = tmp_path_factory.mktemp('media-executable')
    source = REPO_ROOT / 'tests/sdk-conformance/media_sdk_client.py'
    helper_hash = hashlib.sha256(source.read_bytes()).hexdigest()
    frozen = freeze_cargo_build(
        REPO_ROOT,
        ['cargo', 'test', '--locked', '-p', 'sandhi-proxy', '--test',
         'inline_image_sdk', '--no-run'],
        [('inline_image_sdk', 'test')],
        directory,
    )
    helper = directory / source.name
    shutil.copy2(source, helper)
    helper.chmod(0o444)
    assert hashlib.sha256(helper.read_bytes()).hexdigest() == helper_hash
    assert hashlib.sha256(source.read_bytes()).hexdigest() == helper_hash
    return frozen['inline_image_sdk'], helper


@pytest.mark.parametrize('profile', PROFILES)
def test_actual_sdk_inline_images_join_source_usage_and_receipts(
    media_executable, tmp_path, profile,
):
    executable, helper = media_executable
    result = subprocess.run(
        [str(executable), '--ignored', '--exact',
         f'real_sdk_inline_images_{profile}', '--nocapture'],
        cwd=REPO_ROOT,
        env={
            'PATH': os.defpath,
            'LANG': 'C.UTF-8',
            'SANDHI_MEDIA_SDK_PYTHON': sys.executable,
            'SANDHI_MEDIA_SDK_HELPER': str(helper),
            'SANDHI_MEDIA_EVIDENCE_DIR': str(tmp_path),
        },
        capture_output=True, text=True, timeout=120,
    )
    assert result.returncode == 0, result.stdout + result.stderr
    assert result.stdout.count(f'media-cell {profile}/') == 8
    assert '1 passed; 0 failed' in result.stdout
