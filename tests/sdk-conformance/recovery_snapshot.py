"""Disposable offline recovery-drill helper, NOT a production backup facility.

Callers must stop all writers and keep source, snapshot and destination directories
exclusive and stable for the entire operation. No live cross-file snapshot, hostile
concurrent filesystem, authentication, encryption, or power-loss guarantee is made.
Checksums detect accidental corruption, not an attacker able to rewrite the manifest.
Only SQLite data is copied: config, external vault secrets/grants and client-held
plaintext virtual keys must be supplied separately. Database contents remain sensitive.

Destinations must not exist. On an I/O failure an incomplete newly created directory
may remain; never boot it or retry over it. Successful snapshot publication is the
final manifest write. Restores never modify the snapshot or the original database.
"""

from __future__ import annotations

from contextlib import closing
import hashlib
import json
from pathlib import Path
import re
import shutil
import sqlite3
import stat
import tempfile
import time


class SnapshotError(ValueError):
    """The disposable rehearsal input or snapshot failed validation."""


BASE = "usage.db"
MANIFEST = "manifest.json"
VERSION = 1
MAX_MANIFEST_BYTES = 1024 * 1024
SIDECARS = ("-wal", "-shm", "-journal")


def _names(shards: int) -> list[str]:
    if type(shards) is not int or not 1 <= shards <= 64:
        raise SnapshotError("shards must be an integer between 1 and 64")
    return [BASE] + ([f"{BASE}-ledger-shard-{i}.db" for i in range(shards)] if shards > 1 else [])


def _path(path: Path) -> Path:
    path = Path(path)
    if ".." in path.parts:
        raise SnapshotError("parent traversal is not allowed")
    path = path.absolute()
    for component in [*reversed(path.parents), path]:
        if component.is_symlink():
            raise SnapshotError("symlink paths are not allowed")
    return path


def _regular(path: Path) -> None:
    try:
        mode = path.lstat().st_mode
    except FileNotFoundError as error:
        raise SnapshotError("required snapshot database is missing") from error
    if not stat.S_ISREG(mode):
        raise SnapshotError("snapshot inputs must be regular files")


def _new_directory(path: Path) -> Path:
    path = _path(path)
    if path.exists():
        raise FileExistsError(path)
    if not path.parent.is_dir():
        raise SnapshotError("destination parent must already exist")
    return path


def _disjoint(source: Path, destination: Path) -> None:
    if source.is_relative_to(destination) or destination.is_relative_to(source):
        raise SnapshotError("source and destination must not overlap")


def _manifest_bytes(document: dict) -> bytes:
    return (json.dumps(document, indent=2, sort_keys=True, allow_nan=False) + "\n").encode("utf-8")


def _copy_new(source: Path, destination: Path) -> None:
    with source.open("rb") as reader, destination.open("xb") as writer:
        destination.chmod(0o600)
        shutil.copyfileobj(reader, writer)


def _digest(path: Path) -> str:
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def _integrity(path: Path) -> None:
    # Published files are standalone; immutable avoids creating WAL/SHM in inputs.
    try:
        with closing(sqlite3.connect(path.as_uri() + "?mode=ro&immutable=1", uri=True)) as connection:
            if connection.execute("PRAGMA integrity_check").fetchall() != [("ok",)]:
                raise SnapshotError("SQLite integrity check failed")
    except sqlite3.Error as error:
        raise SnapshotError("SQLite integrity check failed") from error


def _source_set(source: Path, shards: int) -> list[Path]:
    names = _names(shards)
    if not source.parent.is_dir():
        raise SnapshotError("source directory is missing")
    sources = [source] + [source.with_name(source.name + name[len(BASE):]) for name in names[1:]]
    expected = {path.name for path in sources[1:]}
    found = set()
    prefix = source.name + "-ledger-shard-"
    for entry in source.parent.iterdir():
        if entry.name.startswith(prefix):
            name = entry.name
            for suffix in SIDECARS:
                if name.endswith(suffix):
                    name = name[:-len(suffix)]
                    break
            if name not in expected:
                raise SnapshotError("unexpected shard file; topology must remain fixed")
            _regular(entry)
            if entry.name == name:
                found.add(name)
    if found != expected:
        raise SnapshotError("required shard file is missing")
    for path in sources:
        _regular(path)
        for suffix in SIDECARS:
            sidecar = path.with_name(path.name + suffix)
            if sidecar.exists() or sidecar.is_symlink():
                _regular(sidecar)
    return sources


def snapshot(source_db: Path, destination_dir: Path, *, shards: int, metadata: dict) -> Path:
    """Snapshot stopped, fixed-topology SQLite files; return the published manifest path."""
    source = _path(source_db)
    destination = _new_directory(destination_dir)
    _disjoint(source, destination)
    sources = _source_set(source, shards)
    if not isinstance(metadata, dict):
        raise SnapshotError("metadata must be a JSON object without credentials")
    try:
        metadata = json.loads(json.dumps(metadata, allow_nan=False))
    except (TypeError, ValueError) as error:
        raise SnapshotError("metadata must be a finite JSON object") from error
    document = {"format_version": VERSION, "shards": shards, "metadata": metadata, "files": []}
    # Use publication's exact serialization, reserving space for fixed file records.
    if len(_manifest_bytes(document)) > MAX_MANIFEST_BYTES - 16384:
        raise SnapshotError("manifest metadata is too large")
    destination.mkdir(mode=0o700)
    # Stage DB+WAL/hot journal. SQLite may rebuild SHM/recover journals only in this private
    # copy, never in the source directory. Copying files requires the caller's writer cutoff.
    with tempfile.TemporaryDirectory(prefix=".stage-", dir=destination) as staging:
        for source, name in zip(sources, _names(shards), strict=True):
            staged = Path(staging) / name
            _copy_new(source, staged)
            for suffix in ("-wal", "-journal"):
                sidecar = source.with_name(source.name + suffix)
                if sidecar.exists():
                    _copy_new(sidecar, staged.with_name(staged.name + suffix))
            target = destination / name
            # Claim the destination exclusively before SQLite opens it.
            with target.open("xb"):
                target.chmod(0o600)
            deadline = time.monotonic() + 10

            def progress(_status, _remaining, _total):
                if time.monotonic() >= deadline:
                    raise SnapshotError("disposable SQLite backup exceeded its deadline")

            try:
                with closing(sqlite3.connect(staged)) as reader, closing(sqlite3.connect(target)) as writer:
                    reader.backup(writer, pages=128, progress=progress, sleep=0.01)
                    # Final publication contains no sidecars and can be restored independently.
                    writer.execute("PRAGMA journal_mode=DELETE")
            except sqlite3.Error as error:
                raise SnapshotError("SQLite backup failed") from error
            _integrity(target)
            document["files"].append({"name": name, "size": target.stat().st_size, "sha256": _digest(target)})
    manifest = destination / MANIFEST
    encoded = _manifest_bytes(document)
    if len(encoded) > MAX_MANIFEST_BYTES:
        raise SnapshotError("manifest is too large")
    with manifest.open("xb") as output:
        manifest.chmod(0o600)
        output.write(encoded)
    return manifest


def restore(snapshot_dir: Path, target_dir: Path, *, expected_shards: int) -> Path:
    """Validate an entire snapshot before creating a new isolated restore directory."""
    names = _names(expected_shards)
    source = _path(snapshot_dir)
    target = _new_directory(target_dir)
    _disjoint(source, target)
    if not source.is_dir():
        raise SnapshotError("snapshot directory is missing")
    manifest = source / MANIFEST
    _regular(manifest)
    if manifest.stat().st_size > MAX_MANIFEST_BYTES:
        raise SnapshotError("manifest is too large")
    try:
        document = json.loads(manifest.read_text(encoding="utf-8"))
    except (ValueError, UnicodeError) as error:
        raise SnapshotError("manifest is not valid JSON") from error
    if not isinstance(document, dict) or set(document) != {"format_version", "shards", "metadata", "files"}:
        raise SnapshotError("invalid manifest shape")
    if type(document["format_version"]) is not int or document["format_version"] != VERSION:
        raise SnapshotError("unsupported manifest version")
    if type(document["shards"]) is not int or document["shards"] != expected_shards:
        raise SnapshotError("snapshot shard topology does not match")
    if not isinstance(document["metadata"], dict) or not isinstance(document["files"], list):
        raise SnapshotError("invalid manifest shape")
    if len(document["files"]) != len(names):
        raise SnapshotError("manifest database set is incomplete")
    if {entry.name for entry in source.iterdir()} != {MANIFEST, *names}:
        raise SnapshotError("snapshot file set is incomplete or unexpected")
    for record, name in zip(document["files"], names, strict=True):
        if not isinstance(record, dict) or set(record) != {"name", "size", "sha256"}:
            raise SnapshotError("invalid file record")
        if record["name"] != name:
            raise SnapshotError("manifest filenames must match the fixed database set")
        if type(record["size"]) is not int or record["size"] <= 0:
            raise SnapshotError("invalid database size")
        if not isinstance(record["sha256"], str) or not re.fullmatch("[0-9a-f]{64}", record["sha256"]):
            raise SnapshotError("invalid database checksum")
        database = source / name
        _regular(database)
        if database.stat().st_size != record["size"] or _digest(database) != record["sha256"]:
            raise SnapshotError("database checksum or size mismatch")
        _integrity(database)
    target.mkdir(mode=0o700)
    for name in names:
        _copy_new(source / name, target / name)
    return target / BASE
