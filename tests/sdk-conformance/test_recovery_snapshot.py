"""Adversarial tests for the disposable offline snapshot helper; no proxy/vault/network."""

from contextlib import closing
import hashlib
import json
from pathlib import Path
import shutil
import sqlite3

import pytest

from recovery_snapshot import SnapshotError, restore, snapshot


def database(path, value="before"):
    with closing(sqlite3.connect(path)) as connection:
        connection.execute("CREATE TABLE future_unknown_table (id INTEGER PRIMARY KEY, value TEXT)")
        connection.execute("INSERT INTO future_unknown_table(value) VALUES (?)", (value,))
        connection.commit()


def values(path):
    with closing(sqlite3.connect(path)) as connection:
        return connection.execute("SELECT value FROM future_unknown_table ORDER BY id").fetchall()


def edit_manifest(path, edit):
    document = json.loads(path.read_text())
    edit(document)
    path.write_text(json.dumps(document))


@pytest.mark.parametrize("shards", [1, 2])
def test_fixed_topology_round_trip_preserves_unknown_tables_and_source(tmp_path, shards):
    source = tmp_path / "custom.sqlite"
    database(source)
    inputs = [source]
    if shards > 1:
        for i in range(shards):
            path = source.with_name(source.name + f"-ledger-shard-{i}.db")
            database(path, f"shard-{i}")
            inputs.append(path)
    before = {path.name: path.read_bytes() for path in inputs}
    manifest = snapshot(source, tmp_path / "snapshot", shards=shards, metadata={"drain": "exit-0"})
    assert manifest.name == "manifest.json"
    assert json.loads(manifest.read_text())["metadata"] == {"drain": "exit-0"}
    restored = restore(manifest.parent, tmp_path / "restored", expected_shards=shards)
    assert restored.name == "usage.db"
    assert values(restored) == [("before",)]
    for i in range(shards if shards > 1 else 0):
        assert values(restored.with_name(restored.name + f"-ledger-shard-{i}.db")) == [(f"shard-{i}",)]
    assert {path.name: path.read_bytes() for path in inputs} == before
    with closing(sqlite3.connect(restored)) as connection:
        connection.execute("INSERT INTO future_unknown_table(value) VALUES ('after')")
        connection.commit()
    assert values(source) == [("before",)]


def test_backup_includes_uncheckpointed_wal_without_modifying_source(tmp_path):
    source = tmp_path / "source.db"
    database(source)
    with closing(sqlite3.connect(source)) as connection:
        connection.execute("PRAGMA journal_mode=WAL")
        connection.execute("PRAGMA wal_autocheckpoint=0")
        connection.execute("INSERT INTO future_unknown_table(value) VALUES ('wal-only')")
        connection.commit()
        artifacts = [source, Path(str(source) + "-wal"), Path(str(source) + "-shm")]
        before = {path.name: path.read_bytes() for path in artifacts}
        naive = tmp_path / "naive.db"
        shutil.copyfile(source, naive)
        assert values(naive) == [("before",)], "main-file copy alone loses committed WAL rows"
        # The fixture keeps an idle connection solely to retain the WAL; no writer runs.
        manifest = snapshot(source, tmp_path / "snapshot", shards=1, metadata={})
        assert {path.name: path.read_bytes() for path in artifacts} == before
    restored = restore(manifest.parent, tmp_path / "restored", expected_shards=1)
    assert values(restored) == [("before",), ("wal-only",)]
    assert {p.name for p in manifest.parent.iterdir()} == {"usage.db", "manifest.json"}


@pytest.mark.parametrize("shards,extra", [(2, False), (1, True)])
def test_snapshot_refuses_missing_or_unexpected_shards_before_destination(tmp_path, shards, extra):
    source = tmp_path / "source.db"
    database(source)
    if extra:
        database(Path(str(source) + "-ledger-shard-0.db"))
    with pytest.raises(SnapshotError):
        snapshot(source, tmp_path / "snapshot", shards=shards, metadata={})
    assert not (tmp_path / "snapshot").exists()


@pytest.mark.parametrize("mutation", [
    "missing", "corrupt", "checksum", "version", "topology", "traversal",
    "absolute", "duplicate", "extra", "bool-topology", "record-shape", "manifest-json",
])
def test_restore_rejects_invalid_snapshot_before_creating_target(tmp_path, mutation):
    source = tmp_path / "source.db"
    database(source)
    manifest = snapshot(source, tmp_path / "snapshot", shards=1, metadata={})
    payload = manifest.parent / "usage.db"
    if mutation == "missing":
        payload.unlink()
    elif mutation == "corrupt":
        payload.write_bytes(b"not a SQLite database")
        edit_manifest(manifest, lambda d: d["files"][0].update(size=payload.stat().st_size, sha256=hashlib.sha256(payload.read_bytes()).hexdigest()))
    elif mutation == "checksum":
        payload.write_bytes(payload.read_bytes() + b"changed")
    elif mutation == "version":
        edit_manifest(manifest, lambda d: d.update(format_version=2))
    elif mutation == "topology":
        edit_manifest(manifest, lambda d: d.update(shards=2))
    elif mutation in ("traversal", "absolute"):
        edit_manifest(manifest, lambda d: d["files"][0].update(name="../source.db" if mutation == "traversal" else str(source)))
    elif mutation == "duplicate":
        edit_manifest(manifest, lambda d: d["files"].append(d["files"][0]))
    elif mutation == "extra":
        (manifest.parent / "usage.db-wal").write_bytes(b"untrusted sidecar")
    elif mutation == "bool-topology":
        edit_manifest(manifest, lambda d: d.update(shards=True))
    elif mutation == "record-shape":
        edit_manifest(manifest, lambda d: d["files"][0].update(other="unexpected"))
    else:
        manifest.write_text("not JSON")
    with pytest.raises(SnapshotError):
        restore(manifest.parent, tmp_path / "restored", expected_shards=1)
    assert not (tmp_path / "restored").exists()
    assert values(source) == [("before",)]


def test_restore_rejects_requested_topology_change(tmp_path):
    source = tmp_path / "source.db"
    database(source)
    manifest = snapshot(source, tmp_path / "snapshot", shards=1, metadata={})
    with pytest.raises(SnapshotError, match="topology"):
        restore(manifest.parent, tmp_path / "restored", expected_shards=2)
    assert not (tmp_path / "restored").exists()


def test_neither_snapshot_nor_restore_overwrites_existing_directory(tmp_path):
    source = tmp_path / "source.db"
    database(source)
    manifest = snapshot(source, tmp_path / "snapshot", shards=1, metadata={})
    original = manifest.read_bytes()
    with pytest.raises(FileExistsError):
        snapshot(source, manifest.parent, shards=1, metadata={})
    with pytest.raises(FileExistsError):
        restore(manifest.parent, tmp_path, expected_shards=1)
    assert manifest.read_bytes() == original
    assert values(source) == [("before",)]


@pytest.mark.parametrize("location", ["same", "ancestor", "descendant"])
def test_restore_rejects_overlapping_target_without_changing_archive(tmp_path, location):
    source = tmp_path / "source.db"
    database(source)
    original = source.read_bytes()
    archive = snapshot(source, tmp_path / "snapshot", shards=1, metadata={}).parent
    before = {path.name: path.read_bytes() for path in archive.iterdir()}
    target = {"same": archive, "ancestor": tmp_path, "descendant": archive / "restored"}[location]
    with pytest.raises((SnapshotError, FileExistsError)):
        restore(archive, target, expected_shards=1)
    assert {path.name: path.read_bytes() for path in archive.iterdir()} == before
    assert source.read_bytes() == original
    # Refused overlap must leave the archive usable for a subsequent isolated restore.
    restored = restore(archive, tmp_path / "isolated", expected_shards=1)
    assert values(restored) == [("before",)]


@pytest.mark.parametrize("location", ["same", "ancestor", "descendant"])
def test_snapshot_rejects_overlapping_destination_without_changing_source(tmp_path, location):
    source = tmp_path / "source.db"
    database(source)
    before = source.read_bytes()
    destination = {"same": source, "ancestor": tmp_path, "descendant": source / "snapshot"}[location]
    with pytest.raises((SnapshotError, FileExistsError)):
        snapshot(source, destination, shards=1, metadata={})
    assert source.read_bytes() == before
    assert {path.name for path in tmp_path.iterdir()} == {source.name}


def test_indented_metadata_size_is_checked_before_snapshot_creation(tmp_path):
    source = tmp_path / "source.db"
    database(source)
    before = source.read_bytes()
    # Compact JSON fits under the cap, but the actual indented publication does not.
    metadata = {f"k{i}": "" for i in range(70000)}
    assert len(json.dumps(metadata).encode()) < 1024 * 1024 - 16384
    with pytest.raises(SnapshotError, match="too large"):
        snapshot(source, tmp_path / "snapshot", shards=1, metadata=metadata)
    assert not (tmp_path / "snapshot").exists()
    assert source.read_bytes() == before


@pytest.mark.parametrize("location", ["source", "source-parent", "destination", "manifest", "payload", "restore-target"])
def test_symlinks_are_rejected_without_touching_referents(tmp_path, location):
    source_dir = tmp_path / "source-dir"
    source_dir.mkdir()
    source = source_dir / "source.db"
    database(source)
    before = source.read_bytes()
    manifest = snapshot(source, tmp_path / "snapshot", shards=1, metadata={})
    link = tmp_path / "link"
    if location == "source":
        link.symlink_to(source)
        call = lambda: snapshot(link, tmp_path / "other", shards=1, metadata={})
    elif location == "source-parent":
        link.symlink_to(source_dir, target_is_directory=True)
        call = lambda: snapshot(link / source.name, tmp_path / "other", shards=1, metadata={})
    elif location == "destination":
        link.symlink_to(source_dir, target_is_directory=True)
        call = lambda: snapshot(source, link / "other", shards=1, metadata={})
    elif location in ("manifest", "payload"):
        path = manifest if location == "manifest" else manifest.parent / "usage.db"
        path.unlink()
        path.symlink_to(source)
        call = lambda: restore(manifest.parent, tmp_path / "restored", expected_shards=1)
    else:
        link.symlink_to(source_dir, target_is_directory=True)
        call = lambda: restore(manifest.parent, link / "restored", expected_shards=1)
    with pytest.raises(SnapshotError):
        call()
    assert source.read_bytes() == before
    assert not (source_dir / "restored").exists()


@pytest.mark.parametrize("shards", [0, 65, True, "1"])
def test_invalid_shard_count_is_rejected_without_output(tmp_path, shards):
    source = tmp_path / "source.db"
    database(source)
    with pytest.raises(SnapshotError):
        snapshot(source, tmp_path / "snapshot", shards=shards, metadata={})
    assert not (tmp_path / "snapshot").exists()


@pytest.mark.parametrize("fault", ["missing-base", "missing-parent", "corrupt", "wal-symlink", "metadata"])
def test_failed_snapshot_never_publishes_a_manifest(tmp_path, fault):
    source = tmp_path / "source.db"
    database(source)
    metadata = {}
    if fault == "missing-base":
        source = tmp_path / "absent.db"
    elif fault == "missing-parent":
        source = tmp_path / "absent" / "source.db"
    elif fault == "corrupt":
        source.write_bytes(b"not SQLite")
    elif fault == "wal-symlink":
        Path(str(source) + "-wal").symlink_to(source)
    else:
        metadata = {"not-json": object()}
    with pytest.raises(SnapshotError):
        snapshot(source, tmp_path / "snapshot", shards=1, metadata=metadata)
    assert not (tmp_path / "snapshot" / "manifest.json").exists()
