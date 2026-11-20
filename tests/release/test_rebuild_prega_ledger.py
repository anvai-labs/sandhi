import importlib.util
import hashlib
import json
from pathlib import Path
import sqlite3

import pytest

ROOT = Path(__file__).resolve().parents[2]
SPEC = importlib.util.spec_from_file_location(
    "rebuild_prega_ledger", ROOT / "scripts/rebuild_prega_ledger.py"
)
MODULE = importlib.util.module_from_spec(SPEC)
assert SPEC.loader is not None
SPEC.loader.exec_module(MODULE)


def ledger(path: Path) -> None:
    connection = sqlite3.connect(path)
    connection.executescript("""
        CREATE TABLE budget_execution_context (
          execution_id TEXT PRIMARY KEY,
          version INTEGER NOT NULL CHECK(version=1),
          context_json TEXT NOT NULL
        );
        CREATE TABLE budget_execution_intent (
          execution_id TEXT PRIMARY KEY,
          reservation_id INTEGER NOT NULL,
          created_at INTEGER NOT NULL
        );
        CREATE TABLE budget_reservation (
          id INTEGER PRIMARY KEY,
          scope TEXT NOT NULL,
          ceiling INTEGER NOT NULL,
          actual INTEGER NOT NULL,
          settled INTEGER NOT NULL,
          settled_at INTEGER,
          expires_at INTEGER NOT NULL
        );
        CREATE TABLE budget_physical_attempt (
          execution_id TEXT PRIMARY KEY,
          attempt_id TEXT NOT NULL,
          authorized_at INTEGER NOT NULL,
          evidence_json TEXT
        );
        CREATE TABLE budget_dispatch_fence (
          execution_id TEXT PRIMARY KEY,
          phase INTEGER NOT NULL,
          transitioned_at INTEGER,
          context_version INTEGER NOT NULL,
          attempt_version INTEGER NOT NULL,
          attempt_id TEXT
        );
        CREATE TABLE backends (
          id TEXT PRIMARY KEY,
          definition_json TEXT NOT NULL,
          revision INTEGER NOT NULL,
          status TEXT NOT NULL,
          created_at TEXT NOT NULL,
          updated_at TEXT NOT NULL
        );
        """)
    connection.execute(
        "INSERT INTO budget_reservation VALUES (?,?,?,?,?,?,?)",
        (1, "vk:fixture", 1000, 0, 0, None, 2000),
    )
    connection.execute(
        "INSERT INTO budget_execution_intent VALUES (?,?,?)", ("exec", 1, 1000)
    )
    connection.execute(
        "INSERT INTO budget_execution_context VALUES (?,?,?)",
        (
            "exec",
            1,
            json.dumps(
                {
                    "version": 1,
                    "ingress": "openai_chat_completions",
                    "delivery": "streaming",
                    "plane": "transparent",
                    "upstream_ref": "backend/source",
                    "upstream_budget_scope": "upstream:openai",
                    "requested_model": "physical",
                    "actual_model": "physical",
                    "provider": "openai",
                    "endpoint_family": "openai_chat_completions",
                    "transport_binding_sha256": "a" * 64,
                    "accounting_policy": {"version": 1, "cache_read_weight": 1000},
                },
                separators=(",", ":"),
            ),
        ),
    )
    connection.execute(
        "INSERT INTO budget_physical_attempt VALUES (?,?,?,?)",
        (
            "exec",
            "b" * 64,
            1000,
            json.dumps(
                {
                    "version": 2,
                    "attempt_id": "b" * 64,
                    "attempt_ordinal": 1,
                    "dispatch_observed_at_ms": 1000,
                    "terminal": {
                        "observed_at_ms": 31000,
                        "outcome": "timeout",
                        "provider_status": None,
                        "provider_request_id": None,
                    },
                    "dropped_observations": 0,
                    "invalid": False,
                    "observed_provider": {"provider": "openai", "model": "physical"},
                },
                separators=(",", ":"),
            ),
        ),
    )
    connection.execute(
        "INSERT INTO budget_dispatch_fence VALUES (?,?,?,?,?,?)",
        ("exec", 1, 1000, 2, 2, "b" * 64),
    )
    connection.execute(
        "INSERT INTO backends VALUES (?,?,?,?,?,?)",
        (
            "source",
            json.dumps(
                {
                    "id": "source",
                    "provider": "openai",
                    "credential_ref": "openai:source",
                    "endpoint": {
                        "endpoint_family": "openai_chat_completions",
                        "endpoint_profile": "standard",
                    },
                    "models": [{"id": "fixture"}],
                },
                separators=(",", ":"),
            ),
            1,
            "active",
            "2026-11-19T00:00:00Z",
            "2026-11-19T00:00:00Z",
        ),
    )
    connection.commit()
    connection.close()


def test_rebuild_makes_a_clean_version_one_copy(tmp_path):
    source = tmp_path / "source.db"
    output = tmp_path / "rebuilt.db"
    manifest = tmp_path / "manifest.json"
    ledger(source)
    source.chmod(0o600)

    receipt = MODULE.rebuild(source, output, manifest)

    assert receipt["execution_contexts"] == 1
    assert receipt["physical_attempts_with_evidence"] == 1
    assert receipt["legacy_backends_migrated_to_unsupported_thinking"] == 1
    connection = sqlite3.connect(output)
    context = json.loads(
        connection.execute(
            "SELECT context_json FROM budget_execution_context"
        ).fetchone()[0]
    )
    attempt = json.loads(
        connection.execute(
            "SELECT evidence_json FROM budget_physical_attempt"
        ).fetchone()[0]
    )
    assert context["version"] == 1
    assert context["transport"] == {
        "upstream_host": None,
        "policy": "streaming_legacy_unavailable",
    }
    assert attempt["version"] == 1
    progress = attempt["terminal"]["progress"]
    assert "response_headers_received" not in progress
    assert "stream_event_received" not in progress
    assert progress["timeout_phase"] == "unknown"
    assert progress["error_subtype"] == "unknown_timeout"

    second_output = tmp_path / "rebuilt-again.db"
    second_manifest = tmp_path / "second-manifest.json"
    MODULE.rebuild(output, second_output, second_manifest)
    second = sqlite3.connect(second_output)
    second_context = second.execute(
        "SELECT context_json FROM budget_execution_context"
    ).fetchone()[0]
    second_attempt = second.execute(
        "SELECT evidence_json FROM budget_physical_attempt"
    ).fetchone()[0]
    second.close()
    assert second_context == json.dumps(context, separators=(",", ":"))
    assert second_attempt == json.dumps(attempt, separators=(",", ":"))
    assert json.loads(manifest.read_text())["output_sha256"] == receipt["output_sha256"]
    assert receipt["output_sha256"] == hashlib.sha256(output.read_bytes()).hexdigest()
    assert receipt["output_bytes"] == output.stat().st_size
    assert receipt["output_mode"] == source.stat().st_mode & 0o7777
    assert receipt["output_uid"] == source.stat().st_uid
    assert receipt["output_gid"] == source.stat().st_gid
    assert (output.stat().st_mode & 0o7777, output.stat().st_uid, output.stat().st_gid) == (
        receipt["output_mode"],
        receipt["output_uid"],
        receipt["output_gid"],
    )
    assert not Path(str(output) + "-wal").exists()
    assert not Path(str(output) + "-shm").exists()
    assert not Path(str(output) + "-journal").exists()
    backend = json.loads(
        connection.execute("SELECT definition_json FROM backends").fetchone()[0]
    )
    assert backend["models"][0]["thinking"] == {"supported": False}
    connection.close()


def test_rebuild_rejects_output_and_manifest_alias(tmp_path):
    source = tmp_path / "source.db"
    output = tmp_path / "same.json"
    ledger(source)
    with pytest.raises(MODULE.RebuildError, match="distinct"):
        MODULE.rebuild(source, output, output)
    assert not output.exists()


def test_rebuild_rejects_stale_output_sqlite_sidecars(tmp_path):
    source = tmp_path / "source.db"
    output = tmp_path / "rebuilt.db"
    manifest = tmp_path / "manifest.json"
    ledger(source)
    Path(str(output) + "-wal").write_bytes(b"stale")
    with pytest.raises(MODULE.RebuildError, match="sidecars"):
        MODULE.rebuild(source, output, manifest)
    assert not output.exists()
    assert Path(str(output) + "-wal").read_bytes() == b"stale"


@pytest.mark.parametrize("table", ["budget_execution_context", "budget_physical_attempt"])
def test_rebuild_rejects_nonzero_dispatch_marker_without_child_row(table, tmp_path):
    source = tmp_path / "source.db"
    output = tmp_path / "rebuilt.db"
    manifest = tmp_path / "manifest.json"
    ledger(source)
    connection = sqlite3.connect(source)
    connection.execute(f"DELETE FROM {table}")
    connection.commit()
    connection.close()
    match = "context dispatch" if table == "budget_execution_context" else "attempt dispatch"
    with pytest.raises(MODULE.RebuildError, match=match):
        MODULE.rebuild(source, output, manifest)
    assert not output.exists()
    assert not manifest.exists()


def test_rebuild_rejects_null_attempt_dispatch_link(tmp_path):
    source = tmp_path / "source.db"
    output = tmp_path / "rebuilt.db"
    manifest = tmp_path / "manifest.json"
    ledger(source)
    connection = sqlite3.connect(source)
    connection.execute("UPDATE budget_dispatch_fence SET attempt_id=NULL")
    connection.commit()
    connection.close()
    with pytest.raises(MODULE.RebuildError, match="attempt identity linkage"):
        MODULE.rebuild(source, output, manifest)
    assert not output.exists()
    assert not manifest.exists()


def test_rebuild_accepts_valid_terminal_chat_usage_profiles(tmp_path):
    source = tmp_path / "source.db"
    output = tmp_path / "rebuilt.db"
    manifest = tmp_path / "manifest.json"
    ledger(source)
    connection = sqlite3.connect(source)
    value = json.loads(
        connection.execute(
            "SELECT context_json FROM budget_execution_context"
        ).fetchone()[0]
    )
    value["backend"] = {
        "id": "source",
        "revision": 1,
        "credential_ref": "openai:source",
        "credential_generation": 1,
        "endpoint_profile": "zai_final_choice_usage",
    }
    connection.execute(
        "UPDATE budget_execution_context SET context_json=?",
        (json.dumps(value, separators=(",", ":")),),
    )
    connection.commit()
    connection.close()

    MODULE.rebuild(source, output, manifest)

    assert manifest.exists()


def test_rebuild_accepts_ipv6_transport_host(tmp_path):
    source = tmp_path / "source.db"
    output = tmp_path / "rebuilt.db"
    manifest = tmp_path / "manifest.json"
    ledger(source)
    connection = sqlite3.connect(source)
    value = json.loads(
        connection.execute(
            "SELECT context_json FROM budget_execution_context"
        ).fetchone()[0]
    )
    value["transport"] = {
        "upstream_host": "2001:db8::1",
        "policy": "streaming_legacy_unavailable",
    }
    connection.execute(
        "UPDATE budget_execution_context SET context_json=?",
        (json.dumps(value, separators=(",", ":")),),
    )
    connection.commit()
    connection.close()

    MODULE.rebuild(source, output, manifest)

    assert manifest.exists()


@pytest.mark.parametrize(
    "thinking",
    [
        {"supported": True, "encoding": "gemini_generate_content", "effort": {"values": ["low"]}},
        {"supported": False, "encoding": "openai_chat"},
        "historically-ignored",
    ],
)
def test_rebuild_conservatively_disables_all_legacy_backend_thinking(thinking, tmp_path):
    source = tmp_path / "source.db"
    output = tmp_path / "rebuilt.db"
    manifest = tmp_path / "manifest.json"
    ledger(source)
    connection = sqlite3.connect(source)
    definition = json.loads(
        connection.execute("SELECT definition_json FROM backends").fetchone()[0]
    )
    definition["models"][0]["thinking"] = thinking
    connection.execute(
        "UPDATE backends SET definition_json=?",
        (json.dumps(definition, separators=(",", ":")),),
    )
    connection.commit()
    connection.close()
    MODULE.rebuild(source, output, manifest)
    connection = sqlite3.connect(output)
    backend = json.loads(connection.execute("SELECT definition_json FROM backends").fetchone()[0])
    connection.close()
    assert backend["models"][0]["thinking"] == {"supported": False}
    assert json.loads(manifest.read_text())["legacy_backends_migrated_to_unsupported_thinking"] == 1


def test_rebuild_rejects_backend_references_runtime_would_corrupt(tmp_path):
    source = tmp_path / "source.db"
    output = tmp_path / "rebuilt.db"
    manifest = tmp_path / "manifest.json"
    ledger(source)
    connection = sqlite3.connect(source)
    definition = json.loads(
        connection.execute("SELECT definition_json FROM backends").fetchone()[0]
    )
    definition["id"] = "Source"
    connection.execute("UPDATE backends SET id='Source'")
    connection.execute(
        "UPDATE backends SET definition_json=?",
        (json.dumps(definition, separators=(",", ":")),),
    )
    connection.commit()
    connection.close()
    with pytest.raises(MODULE.RebuildError, match="valid backend reference"):
        MODULE.rebuild(source, output, manifest)


def test_rebuild_rejects_backend_cache_policy_runtime_would_corrupt(tmp_path):
    source = tmp_path / "source.db"
    output = tmp_path / "rebuilt.db"
    manifest = tmp_path / "manifest.json"
    ledger(source)
    connection = sqlite3.connect(source)
    definition = json.loads(
        connection.execute("SELECT definition_json FROM backends").fetchone()[0]
    )
    definition["cache_policy"] = {
        "models": {
            "fixture": {"profile": "anthropic_breakpoints", "lifetimes": ["5m"]}
        }
    }
    connection.execute(
        "UPDATE backends SET definition_json=?",
        (json.dumps(definition, separators=(",", ":")),),
    )
    connection.commit()
    connection.close()
    with pytest.raises(MODULE.RebuildError, match="cache capability profile"):
        MODULE.rebuild(source, output, manifest)


@pytest.mark.parametrize(
    ("mutation", "match"),
    [
        (lambda definition: definition.update(base_url="ftp://example.test"), "backend base URL"),
        (
            lambda definition: definition.update(
                base_url="http://user:pass@example.test"
            ),
            "backend base URL",
        ),
        (
            lambda definition: definition.update(base_url="http://example.test:bad/"),
            "backend base URL",
        ),
        (
            lambda definition: definition.update(
                base_url="http://example.test:99999999/"
            ),
            "backend base URL",
        ),
        (
            lambda definition: definition.update(base_url="http://a%b/"),
            "backend base URL host",
        ),
        (
            lambda definition: definition.update(base_url="http://a b/"),
            "backend base URL host",
        ),
        (
            lambda definition: definition.update(base_url="http://a_b/"),
            "backend base URL host",
        ),
        (
            lambda definition: definition["models"][0].update(max_input_tokens=2**64),
            "max_input_tokens",
        ),
        (lambda definition: definition["models"][0].update(aliases=None), "aliases"),
    ],
)
def test_rebuild_rejects_backend_values_runtime_cannot_decode(
    mutation, match, tmp_path
):
    source = tmp_path / "source.db"
    output = tmp_path / "rebuilt.db"
    manifest = tmp_path / "manifest.json"
    ledger(source)
    connection = sqlite3.connect(source)
    definition = json.loads(
        connection.execute("SELECT definition_json FROM backends").fetchone()[0]
    )
    mutation(definition)
    connection.execute(
        "UPDATE backends SET definition_json=?",
        (json.dumps(definition, separators=(",", ":")),),
    )
    connection.commit()
    connection.close()
    with pytest.raises(MODULE.RebuildError, match=match):
        MODULE.rebuild(source, output, manifest)


def test_rebuild_preserves_null_observed_model_and_drops_triggers(tmp_path):
    source = tmp_path / "source.db"
    output = tmp_path / "rebuilt.db"
    manifest = tmp_path / "manifest.json"
    ledger(source)
    connection = sqlite3.connect(source)
    evidence = json.loads(
        connection.execute(
            "SELECT evidence_json FROM budget_physical_attempt"
        ).fetchone()[0]
    )
    evidence["observed_provider"]["model"] = None
    connection.execute(
        "UPDATE budget_physical_attempt SET evidence_json=?",
        (json.dumps(evidence, separators=(",", ":")),),
    )
    connection.execute(
        "CREATE TRIGGER tamper_context AFTER UPDATE ON budget_execution_context "
        "BEGIN UPDATE budget_execution_context SET context_json='tampered'; END"
    )
    connection.commit()
    connection.close()

    MODULE.rebuild(source, output, manifest)

    rebuilt = sqlite3.connect(output)
    assert rebuilt.execute(
        "SELECT count(*) FROM sqlite_master WHERE type='trigger'"
    ).fetchone()[0] == 0
    encoded = json.loads(
        rebuilt.execute(
            "SELECT evidence_json FROM budget_physical_attempt"
        ).fetchone()[0]
    )
    rebuilt.close()
    assert encoded["observed_provider"]["model"] is None


@pytest.mark.parametrize(
    ("mutation", "field", "match"),
    [
        (
            lambda value: value.update(dispatch_observed_at_ms=253_402_300_800_000),
            "evidence_json",
            "dispatch timestamp",
        ),
        (
            lambda value: value["terminal"].update(observed_at_ms=253_402_300_800_000),
            "evidence_json",
            "terminal timestamp",
        ),
        (
            lambda value: value["terminal"].update(
                progress={
                    "error_class": "timeout",
                    "timeout_phase": "unknown",
                    "error_subtype": "unknown_timeout",
                    "time_to_first_stream_event_ms": 4_294_967_296,
                }
            ),
            "evidence_json",
            "time_to_first_stream_event_ms",
        ),
    ],
)
def test_rebuild_rejects_attempt_ranges_runtime_would_corrupt(
    mutation, field, match, tmp_path
):
    source = tmp_path / "source.db"
    output = tmp_path / "rebuilt.db"
    manifest = tmp_path / "manifest.json"
    ledger(source)
    connection = sqlite3.connect(source)
    value = json.loads(
        connection.execute(f"SELECT {field} FROM budget_physical_attempt").fetchone()[0]
    )
    mutation(value)
    connection.execute(
        f"UPDATE budget_physical_attempt SET {field}=?",
        (json.dumps(value, separators=(",", ":")),),
    )
    connection.commit()
    connection.close()
    with pytest.raises(MODULE.RebuildError, match=match):
        MODULE.rebuild(source, output, manifest)


def test_rebuild_rejects_noncanonical_attempt_ordinal_type(tmp_path):
    source = tmp_path / "source.db"
    output = tmp_path / "rebuilt.db"
    manifest = tmp_path / "manifest.json"
    ledger(source)
    connection = sqlite3.connect(source)
    evidence = json.loads(
        connection.execute(
            "SELECT evidence_json FROM budget_physical_attempt"
        ).fetchone()[0]
    )
    evidence["attempt_ordinal"] = True
    connection.execute(
        "UPDATE budget_physical_attempt SET evidence_json=?",
        (json.dumps(evidence, separators=(",", ":")),),
    )
    connection.commit()
    connection.close()
    with pytest.raises(MODULE.RebuildError, match="attempt version or ordinal"):
        MODULE.rebuild(source, output, manifest)


def test_rebuild_rejects_malformed_nested_evidence(tmp_path):
    source = tmp_path / "source.db"
    output = tmp_path / "rebuilt.db"
    manifest = tmp_path / "manifest.json"
    ledger(source)
    connection = sqlite3.connect(source)
    value = json.loads(
        connection.execute(
            "SELECT context_json FROM budget_execution_context"
        ).fetchone()[0]
    )
    value["transport_binding_sha256"] = "not-a-digest"
    connection.execute(
        "UPDATE budget_execution_context SET context_json=?",
        (json.dumps(value),),
    )
    connection.commit()
    connection.close()
    with pytest.raises(MODULE.RebuildError, match="SHA-256"):
        MODULE.rebuild(source, output, manifest)
    assert not output.exists()
    assert not manifest.exists()


@pytest.mark.parametrize(
    ("mutation", "match"),
    [
        (
            lambda value: value["accounting_policy"].update(
                cache_read_weight=1_000_001
            ),
            "cache_read_weight",
        ),
        (
            lambda value: value.update(
                route={
                    "id": "route",
                    "revision": 1,
                    "binding_sha256": "not-a-digest",
                },
                backend={
                    "id": "source",
                    "revision": 1,
                    "credential_ref": "backend:source",
                    "credential_generation": 1,
                    "endpoint_profile": "standard",
                },
                upstream_ref="route/route",
            ),
            "SHA-256",
        ),
        (lambda value: value.update(operation="embedding"), "embedding operation"),
        (lambda value: value.update(version=True), "version must be 1"),
        (
            lambda value: value.update(
                transport={
                    "upstream_host": None,
                    "policy": {"buffered": {"duration_ms": 2**64}},
                }
            ),
            "duration_ms",
        ),
    ],
)
def test_rebuild_rejects_noncanonical_context(mutation, match, tmp_path):
    source = tmp_path / "source.db"
    output = tmp_path / "rebuilt.db"
    manifest = tmp_path / "manifest.json"
    ledger(source)
    connection = sqlite3.connect(source)
    value = json.loads(
        connection.execute(
            "SELECT context_json FROM budget_execution_context"
        ).fetchone()[0]
    )
    mutation(value)
    connection.execute(
        "UPDATE budget_execution_context SET context_json=?",
        (json.dumps(value),),
    )
    connection.commit()
    connection.close()
    with pytest.raises(MODULE.RebuildError, match=match):
        MODULE.rebuild(source, output, manifest)


def test_rebuild_rejects_embedded_attempt_identity_mismatch(tmp_path):
    source = tmp_path / "source.db"
    output = tmp_path / "rebuilt.db"
    manifest = tmp_path / "manifest.json"
    ledger(source)
    connection = sqlite3.connect(source)
    value = json.loads(
        connection.execute(
            "SELECT evidence_json FROM budget_physical_attempt"
        ).fetchone()[0]
    )
    value["attempt_id"] = "c" * 64
    connection.execute(
        "UPDATE budget_physical_attempt SET evidence_json=?",
        (json.dumps(value),),
    )
    connection.commit()
    connection.close()
    with pytest.raises(MODULE.RebuildError, match="attempt identity"):
        MODULE.rebuild(source, output, manifest)


def test_rebuild_converts_old_buffered_policy_and_preserves_active_context(tmp_path):
    source = tmp_path / "source.db"
    output = tmp_path / "rebuilt.db"
    manifest = tmp_path / "manifest.json"
    ledger(source)
    connection = sqlite3.connect(source)
    value = json.loads(
        connection.execute(
            "SELECT context_json FROM budget_execution_context"
        ).fetchone()[0]
    )
    value.update(
        delivery="buffered",
        plane="translation",
        operation="chat",
        transport={"upstream_host": "upstream.test", "policy": "buffered"},
        route=None,
        backend=None,
        cache={
            "plan_sha256": "a" * 64,
            "binding_sha256": "b" * 64,
            "intent": {"mode": "auto", "lifetime": "5m"},
            "capability": "native_only",
            "native_preserved": True,
        },
        content_policy_revision=7,
    )
    connection.execute(
        "UPDATE budget_execution_context SET context_json=?",
        (json.dumps(value),),
    )
    connection.commit()
    connection.close()

    MODULE.rebuild(source, output, manifest)

    connection = sqlite3.connect(output)
    encoded = connection.execute(
        "SELECT context_json FROM budget_execution_context"
    ).fetchone()[0]
    connection.close()
    assert json.loads(encoded)["transport"] == {
        "upstream_host": None,
        "policy": "buffered_legacy_unavailable",
    }
    assert encoded.index('"transport"') < encoded.index('"route"')


def test_rebuild_reads_wal_without_touching_source(tmp_path):
    source = tmp_path / "source.db"
    output = tmp_path / "rebuilt.db"
    manifest = tmp_path / "manifest.json"
    ledger(source)
    writer = sqlite3.connect(source)
    writer.execute("PRAGMA journal_mode=WAL")
    writer.execute("PRAGMA wal_autocheckpoint=0")
    writer.execute("UPDATE backends SET revision=2 WHERE id='source'")
    writer.commit()
    before = {
        path.name: path.read_bytes()
        for path in tmp_path.iterdir()
        if path.name.startswith("source.db")
    }
    assert Path(str(source) + "-wal").exists()
    receipt = MODULE.rebuild(source, output, manifest)
    after = {
        path.name: path.read_bytes()
        for path in tmp_path.iterdir()
        if path.name.startswith("source.db")
    }
    writer.close()
    assert before == after
    assert receipt["execution_contexts"] == 1
