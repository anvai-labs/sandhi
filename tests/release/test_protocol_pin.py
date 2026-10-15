"""Scheduled protocol updates preserve the declared dependency pin shape."""
from pathlib import Path
import subprocess
import sys

import pytest

ROOT = Path(__file__).resolve().parents[2]
SCRIPT = ROOT / '.github/scripts/bump_protocol_pin.py'


@pytest.mark.parametrize('attribute,prefix', [('version', '='), ('version', ''), ('tag', 'v')])
def test_read_and_update_preserve_pin_shape(tmp_path, attribute, prefix):
    manifest = tmp_path / 'crates/sandhi-store/Cargo.toml'
    manifest.parent.mkdir(parents=True)
    decoys = (f'# sentinelpass-protocol = {{ {attribute} = "{prefix}0.17.0" }}\n'
              f'[dev-dependencies]\nsentinelpass-protocol = {{ {attribute} = "{prefix}0.17.0" }}\n')
    live = f'[dependencies]\nsentinelpass-protocol = {{ {attribute} = "{prefix}0.17.0", optional = true }}\n'
    original = decoys + live
    manifest.write_text(original)
    read = subprocess.run([sys.executable, str(SCRIPT), '--current'], cwd=tmp_path,
                          capture_output=True, text=True)
    assert read.returncode == 0, read.stderr
    assert read.stdout.strip() == '0.17.0'
    same = subprocess.run([sys.executable, str(SCRIPT), read.stdout.strip(), '0.17.0'],
                          cwd=tmp_path, capture_output=True, text=True)
    assert same.returncode == 0, same.stderr
    assert manifest.read_text() == original
    update = subprocess.run([sys.executable, str(SCRIPT), '0.17.0', '0.18.0'],
                            cwd=tmp_path, capture_output=True, text=True)
    assert update.returncode == 0, update.stderr
    assert manifest.read_text() == decoys + live.replace('0.17.0', '0.18.0')


@pytest.mark.parametrize('current,latest', [('0.16.0', '0.18.0'), ('0.17.0', 'invalid')])
def test_invalid_or_stale_update_preserves_manifest(tmp_path, current, latest):
    manifest = tmp_path / 'crates/sandhi-store/Cargo.toml'
    manifest.parent.mkdir(parents=True)
    original = '[dependencies]\nsentinelpass-protocol = { version = "=0.17.0", optional = true }\n'
    manifest.write_text(original)
    result = subprocess.run([sys.executable, str(SCRIPT), current, latest], cwd=tmp_path,
                            capture_output=True, text=True)
    assert result.returncode != 0
    assert manifest.read_text() == original
