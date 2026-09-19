//! ADR-0011: bounded, read-only projection of persisted logical-call evidence.
//!
//! Correlation identifiers are lookup keys, not credentials. Authorization and admission
//! belong to the operator handler; this API independently validates query and output bounds.

use std::collections::BTreeSet;

use rusqlite::{params, types::ValueRef, Row};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use crate::SqliteStore;

pub const MAX_REQUEST_BYTES: usize = 4 * 1024;
pub const MAX_RESPONSE_BYTES: usize = 256 * 1024;
pub const MAX_SELECTOR_BYTES: usize = 256;
pub const DEFAULT_LIMIT: usize = 100;
pub const MAX_LIMIT: usize = 500;
const MAX_FIELD_BYTES: usize = 256;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum DiagnosticSelector {
    Request(String),
    Session(String),
    Run(String),
}

impl DiagnosticSelector {
    fn value(&self) -> &str {
        match self {
            Self::Request(value) | Self::Session(value) | Self::Run(value) => value,
        }
    }

    fn selection(&self) -> &'static str {
        match self {
            Self::Request(_) => "WHERE request_id = ?1 ORDER BY rowid DESC LIMIT ?2",
            Self::Session(_) => "WHERE session_id = ?1 ORDER BY rowid DESC LIMIT ?2",
            Self::Run(_) => "WHERE run_id = ?1 ORDER BY rowid DESC LIMIT ?2",
        }
    }
}

const fn default_limit() -> usize {
    DEFAULT_LIMIT
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiagnosticQuery {
    pub selector: DiagnosticSelector,
    #[serde(default = "default_limit")]
    pub limit: usize,
}

impl DiagnosticQuery {
    pub fn validate(&self) -> Result<(), &'static str> {
        let value = self.selector.value();
        if value.is_empty()
            || value.len() > MAX_SELECTOR_BYTES
            || value.trim().is_empty()
            || value.chars().any(char::is_control)
        {
            return Err("invalid diagnostic selector");
        }
        if !(1..=MAX_LIMIT).contains(&self.limit) {
            return Err("invalid diagnostic row limit");
        }
        Ok(())
    }
}

/// Serialize this envelope directly: wrapping it adds bytes outside the enforced budget.
#[derive(Debug, Clone, Serialize)]
pub struct DiagnosticResponse {
    pub schema_version: &'static str,
    pub rows: Vec<Value>,
    pub returned_rows: usize,
    pub truncated: bool,
    pub truncation_reason: Option<&'static str>,
    pub unavailable_evidence: &'static [&'static str],
    pub snapshot_notice: &'static str,
    pub request_id_notice: &'static str,
}

impl DiagnosticResponse {
    fn empty() -> Self {
        Self {
            schema_version: "1",
            rows: Vec::new(),
            returned_rows: 0,
            truncated: false,
            truncation_reason: None,
            unavailable_evidence: &[
                "raw_origin_usage", "completeness", "basis", "outcome", "physical_attempts",
                "request_id_provenance", "separate_upstream_request_id", "separate_admission_request_id",
            ],
            snapshot_notice: "Best-effort persisted logical-call rows, newest insertion first. Active streams may have no row; missing rows do not prove no traffic. Reporting does not prove backend reuse. Caller/origin strings may be sensitive; this export is not anonymized.",
            request_id_notice: "Persisted request_id is upstream-preferred, otherwise admission/late-minted; provenance is not persisted and IDs are not unique.",
        }
    }
}

#[derive(Clone, Copy)]
enum FieldKind {
    Text,
    Count,
    Boolean,
}

// This is an allowlist, deliberately not UsageEvent serialization. No credential, attribution,
// arbitrary metadata, URL, raw usage, header, prompt or response-body column is selected.
const FIELDS: &[(&str, FieldKind)] = &[
    ("request_id", FieldKind::Text),
    ("session_id", FieldKind::Text),
    ("run_id", FieldKind::Text),
    ("step_id", FieldKind::Text),
    ("parent_id", FieldKind::Text),
    ("occurred_at", FieldKind::Text),
    ("provider", FieldKind::Text),
    ("model", FieldKind::Text),
    ("tokens_in", FieldKind::Count),
    ("tokens_out", FieldKind::Count),
    ("cache_creation_tokens", FieldKind::Count),
    ("cache_read_tokens", FieldKind::Count),
    ("reasoning_tokens", FieldKind::Count),
    ("reasoning_included", FieldKind::Boolean),
    ("duration_ms", FieldKind::Count),
    ("duration_source", FieldKind::Text),
    ("time_to_first_token_ms", FieldKind::Count),
    ("time_to_first_token_source", FieldKind::Text),
    ("cache_read_status", FieldKind::Text),
    ("cache_read_source", FieldKind::Text),
];

fn sql(query: &DiagnosticQuery) -> String {
    let projection = FIELDS
        .iter()
        .map(|(name, kind)| {
            let valid = match kind {
                FieldKind::Text => format!(
                    "typeof({name})='text' AND length(CAST({name} AS BLOB))<={MAX_FIELD_BYTES}"
                ),
                FieldKind::Count => format!("typeof({name})='integer' AND {name}>=0"),
                FieldKind::Boolean => format!("typeof({name})='integer' AND {name} IN (0,1)"),
            };
            // The second projection distinguishes an absent value from a rejected value. Even
            // malformed TEXT in an INTEGER-affinity column never crosses into Rust unbounded.
            format!("CASE WHEN {valid} THEN {name} ELSE NULL END, {name} IS NOT NULL")
        })
        .collect::<Vec<_>>()
        .join(", ");
    // All SQL fragments above and below are static allowlist choices, never caller text.
    format!(
        "SELECT {projection} FROM usage_events {}",
        query.selector.selection()
    )
}

fn project(row: &Row<'_>) -> rusqlite::Result<Value> {
    let mut object = Map::new();
    let mut warnings = BTreeSet::new();
    for (index, (name, kind)) in FIELDS.iter().enumerate() {
        let value = row.get_ref(index * 2)?;
        let present: bool = row.get(index * 2 + 1)?;
        let canonical = match (kind, value) {
            (FieldKind::Text, ValueRef::Text(bytes)) => std::str::from_utf8(bytes)
                .ok()
                .filter(|value| !value.is_empty() && !value.chars().any(char::is_control))
                .map(|value| Value::String(value.to_owned())),
            (FieldKind::Count, ValueRef::Integer(value)) if value >= 0 => Some(json!(value)),
            (FieldKind::Boolean, ValueRef::Integer(value @ (0 | 1))) => Some(json!(value == 1)),
            _ => None,
        };
        if let Some(value) = canonical {
            object.insert((*name).into(), value);
        } else if present {
            warnings.insert(*name);
        }
    }
    let status = object.remove("cache_read_status");
    let source = object.remove("cache_read_source");
    let observation = crate::cache_observation(
        status.as_ref().and_then(Value::as_str),
        source.as_ref().and_then(Value::as_str),
    );
    if observation.is_none() && (status.is_some() || source.is_some()) {
        warnings.insert("cache_read_observation");
    }
    object.insert("cache_read_observation".into(), json!(observation));
    for (metric, source) in [
        ("duration_ms", "duration_source"),
        ("time_to_first_token_ms", "time_to_first_token_source"),
    ] {
        let valid_source = matches!(
            object.get(source).and_then(Value::as_str),
            Some("origin" | "boundary")
        );
        if !valid_source || !object.contains_key(metric) {
            if object.remove(source).is_some() {
                warnings.insert(source);
            }
            if object.contains_key(metric) {
                object.insert(source.into(), Value::Null);
            }
        }
    }
    if !warnings.is_empty() {
        object.insert("warnings".into(), json!(warnings));
    }
    Ok(Value::Object(object))
}

impl SqliteStore {
    /// Bounded diagnostic rows; caller must perform admin authentication and fail-fast admission.
    pub fn diagnostics(&self, query: &DiagnosticQuery) -> rusqlite::Result<DiagnosticResponse> {
        query
            .validate()
            .map_err(|_| rusqlite::Error::InvalidQuery)?;
        let mut result = DiagnosticResponse::empty();
        // Reserve the largest possible envelope up front, including its final row count,
        // truncation flag and reason. Per-row lengths include actual JSON escaping.
        let mut envelope = DiagnosticResponse::empty();
        envelope.returned_rows = MAX_LIMIT;
        envelope.truncated = false; // "false" is one byte longer than "true".
        envelope.truncation_reason = Some("byte_limit"); // longer than "row_limit" or null.
        let mut used = serde_json::to_vec(&envelope)
            .expect("fixed JSON envelope")
            .len();
        let conn = self
            .conn
            .lock()
            .map_err(|_| rusqlite::Error::InvalidQuery)?;
        let mut statement = conn.prepare(&sql(query))?;
        let mut rows =
            statement.query(params![query.selector.value(), (query.limit + 1) as i64])?;
        while let Some(row) = rows.next()? {
            if result.rows.len() == query.limit {
                result.truncated = true;
                result.truncation_reason = Some("row_limit");
                break;
            }
            let value = project(row)?;
            let row_bytes = serde_json::to_vec(&value).expect("bounded JSON row").len();
            let next = used + row_bytes + usize::from(!result.rows.is_empty());
            if next > MAX_RESPONSE_BYTES {
                result.truncated = true;
                result.truncation_reason = Some("byte_limit");
                break;
            }
            used = next;
            result.rows.push(value);
        }
        result.returned_rows = result.rows.len();
        debug_assert!(
            serde_json::to_vec(&result).expect("JSON response").len() <= MAX_RESPONSE_BYTES
        );
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sandhi_core::{Backend, CacheReadObservation, CacheReadStatus, LatencySource, UsageEvent};

    fn query(selector: DiagnosticSelector, limit: usize) -> DiagnosticQuery {
        DiagnosticQuery { selector, limit }
    }

    fn event(request: &str, session: &str, run: &str) -> UsageEvent {
        UsageEvent::new(
            request,
            "2026-09-18T12:00:00Z",
            "inferflux",
            "test-model",
            Backend::External,
        )
        .with_session(Some(session.into()))
        .with_identity(None, Some(run.into()), Some("step".into()), None, None)
        .with_tokens(9, 1)
    }

    #[test]
    fn query_rejects_unknown_duplicate_and_invalid_fields() {
        let parsed: DiagnosticQuery =
            serde_json::from_str(r#"{"selector":{"kind":"run","value":"r"}}"#).unwrap();
        assert_eq!(parsed.limit, DEFAULT_LIMIT);
        assert!(parsed.validate().is_ok());
        for invalid in [
            r#"{"selector":{"kind":"run","value":"r"},"other":1}"#,
            r#"{"selector":{"kind":"run","kind":"request","value":"r"}}"#,
            r#"{"selector":{"kind":"run","value":"r","value":"s"}}"#,
            r#"{"selector":{"kind":"run","value":"r","extra":1}}"#,
            r#"{"selector":{"kind":"run","value":"r"},"selector":{"kind":"session","value":"s"}}"#,
            r#"{"selector":{"kind":"run","value":"r"},"limit":1,"limit":2}"#,
            r#"{"selector":{"kind":"all","value":"r"}}"#,
            r#"{"selector":{"kind":"run","value":4}}"#,
            r#"{"selector":{"kind":"run","value":"r"},"limit":-1}"#,
            r#"{"selector":{"kind":"run","value":"r"},"limit":1.0}"#,
            r#"{"selector":{"kind":"run","value":"r"},"limit":true}"#,
            r#"{"selector":{"kind":"run","value":"r"},"limit":"1"}"#,
        ] {
            assert!(
                serde_json::from_str::<DiagnosticQuery>(invalid).is_err(),
                "{invalid}"
            );
        }
        for value in [
            "".into(),
            "   ".into(),
            "r\n".into(),
            "r\0".into(),
            "r\u{85}".into(),
            "é".repeat(129),
        ] {
            assert!(query(DiagnosticSelector::Run(value), 1).validate().is_err());
        }
        for value in ["é".repeat(128), "r".repeat(256), " run ".into()] {
            assert!(query(DiagnosticSelector::Run(value), 1).validate().is_ok());
        }
        for limit in [0, MAX_LIMIT + 1, usize::MAX] {
            assert!(query(DiagnosticSelector::Run("r".into()), limit)
                .validate()
                .is_err());
        }
    }

    #[test]
    fn all_selectors_are_exact_indexed_and_keep_duplicate_ids_in_insertion_order() {
        let store = SqliteStore::in_memory().unwrap();
        for (id, session, run) in [
            ("duplicate", "s", "r"),
            ("unrelated", "other", "other"),
            ("duplicate", "s", "r"),
        ] {
            let mut event = event(id, session, run);
            event.tokens_out = if id == "unrelated" {
                9
            } else {
                store
                    .diagnostics(&query(DiagnosticSelector::Run("r".into()), 10))
                    .unwrap()
                    .returned_rows as u64
                    + 1
            };
            store.insert(&event).unwrap();
        }
        for (selector, index) in [
            (
                DiagnosticSelector::Request("duplicate".into()),
                "idx_usage_request",
            ),
            (DiagnosticSelector::Session("s".into()), "idx_usage_session"),
            (DiagnosticSelector::Run("r".into()), "idx_usage_run"),
        ] {
            let query = query(selector, 10);
            let result = store.diagnostics(&query).unwrap();
            assert_eq!(result.returned_rows, 2);
            assert_eq!(result.rows[0]["tokens_out"], 2);
            assert_eq!(result.rows[1]["tokens_out"], 1);
            assert!(!result.truncated);
            let conn = store.conn.lock().unwrap();
            let mut plan = conn
                .prepare(&format!("EXPLAIN QUERY PLAN {}", sql(&query)))
                .unwrap();
            let details = plan
                .query_map(params![query.selector.value(), 11], |row| {
                    row.get::<_, String>(3)
                })
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap()
                .join(" ");
            assert!(details.contains(index), "{details}");
            assert!(details.contains("SEARCH"), "{details}");
            assert!(!details.contains("TEMP B-TREE"), "{details}");
        }
    }

    #[test]
    fn injection_and_whitespace_are_literal_and_empty_is_not_no_traffic() {
        let store = SqliteStore::in_memory().unwrap();
        let id = "' OR 1=1 --";
        store.insert(&event(id, "s", "r")).unwrap();
        store.insert(&event("other", "s", "r")).unwrap();
        let result = store
            .diagnostics(&query(DiagnosticSelector::Request(id.into()), 5))
            .unwrap();
        assert_eq!(result.returned_rows, 1);
        assert_eq!(result.rows[0]["request_id"], id);
        let empty = store
            .diagnostics(&query(DiagnosticSelector::Request(" other ".into()), 5))
            .unwrap();
        assert_eq!(empty.returned_rows, 0);
        assert!(!empty.truncated);
        assert!(empty
            .snapshot_notice
            .contains("missing rows do not prove no traffic"));
        assert!(empty.request_id_notice.contains("upstream-preferred"));
    }

    #[test]
    fn row_limit_is_detected_without_count_all_and_exact_limit_is_not_truncated() {
        let store = SqliteStore::in_memory().unwrap();
        for index in 0..3 {
            store
                .insert(&event(&format!("r{index}"), "s", "r"))
                .unwrap();
        }
        let limited = store
            .diagnostics(&query(DiagnosticSelector::Run("r".into()), 2))
            .unwrap();
        assert_eq!(limited.returned_rows, 2);
        assert_eq!(limited.rows[0]["request_id"], "r2");
        assert_eq!(limited.truncation_reason, Some("row_limit"));
        let exact = store
            .diagnostics(&query(DiagnosticSelector::Run("r".into()), 3))
            .unwrap();
        assert!(!exact.truncated);
        assert_eq!(exact.truncation_reason, None);
        assert!(store
            .diagnostics(&query(DiagnosticSelector::Run("r".into()), 0))
            .is_err());
    }

    #[test]
    fn byte_budget_includes_escaped_strings_envelope_and_warnings() {
        let store = SqliteStore::in_memory().unwrap();
        let text = "\"".repeat(MAX_FIELD_BYTES);
        let mut event = event(&text, &text, "r");
        event.provider = text.clone();
        event.model = text.clone();
        event.step_id = Some(text.clone());
        event.parent_id = Some(text.clone());
        for _ in 0..MAX_LIMIT {
            store.insert(&event).unwrap();
        }
        // Include fixed-name warnings in every row, without exporting the rejected values.
        store
            .conn
            .lock()
            .unwrap()
            .execute(
                "UPDATE usage_events SET duration_source='invalid', duration_ms=-1",
                [],
            )
            .unwrap();
        let result = store
            .diagnostics(&query(DiagnosticSelector::Run("r".into()), MAX_LIMIT))
            .unwrap();
        assert_eq!(result.truncation_reason, Some("byte_limit"));
        assert!(result.returned_rows > 0 && result.returned_rows < MAX_LIMIT);
        let encoded = serde_json::to_vec(&result).unwrap();
        assert!(encoded.len() <= MAX_RESPONSE_BYTES);
        let row_len = serde_json::to_vec(&result.rows[0]).unwrap().len();
        assert!(MAX_RESPONSE_BYTES - encoded.len() < row_len + 32);
        assert_eq!(
            serde_json::from_slice::<Value>(&encoded).unwrap()["returned_rows"],
            result.returned_rows
        );
    }

    #[test]
    fn malformed_types_large_fields_controls_and_invalid_utf8_are_omitted_not_coerced() {
        let store = SqliteStore::in_memory().unwrap();
        store.insert(&event("r", "s", "run")).unwrap();
        let huge = "NOT_EXPORTED_".repeat(100_000);
        store
            .conn
            .lock()
            .unwrap()
            .execute(
                "UPDATE usage_events SET provider=?1, model=CAST(x'ff' AS TEXT), tokens_in=?1,
             tokens_out=-1, cache_creation_tokens=1.5, reasoning_included=2,
             step_id='control'||char(10), cache_read_status=x'ff', cache_read_source=42,
             duration_ms=9, duration_source='invented', time_to_first_token_ms=-1,
             time_to_first_token_source='origin'",
                params![huge],
            )
            .unwrap();
        let result = store
            .diagnostics(&query(DiagnosticSelector::Run("run".into()), 1))
            .unwrap();
        let row = &result.rows[0];
        for omitted in [
            "provider",
            "model",
            "tokens_in",
            "tokens_out",
            "cache_creation_tokens",
            "reasoning_included",
            "step_id",
            "time_to_first_token_ms",
            "time_to_first_token_source",
        ] {
            assert!(row.get(omitted).is_none(), "{omitted}");
            assert!(
                row["warnings"]
                    .as_array()
                    .unwrap()
                    .contains(&json!(omitted)),
                "{omitted}"
            );
        }
        assert!(row["cache_read_observation"].is_null());
        assert_eq!(row["duration_ms"], 9);
        assert!(row["duration_source"].is_null());
        assert!(!serde_json::to_string(&result)
            .unwrap()
            .contains("NOT_EXPORTED"));
    }

    #[test]
    fn coverage_observations_and_latency_provenance_are_not_invented() {
        let store = SqliteStore::in_memory().unwrap();
        for (id, status) in [
            ("reported", Some(CacheReadStatus::Reported)),
            ("absent", Some(CacheReadStatus::Absent)),
            ("malformed", Some(CacheReadStatus::Malformed)),
            ("legacy", None),
        ] {
            let mut event = event(id, "s", "r");
            event.cache_read_observation = status.map(CacheReadObservation::origin);
            event.duration_ms = Some(0);
            event.duration_source = status.map(|_| LatencySource::Origin);
            store.insert(&event).unwrap();
            let result = store
                .diagnostics(&query(DiagnosticSelector::Request(id.into()), 1))
                .unwrap();
            let row = &result.rows[0];
            assert_eq!(row["cache_read_tokens"], 0);
            assert_eq!(
                row["cache_read_observation"],
                json!(event.cache_read_observation)
            );
            assert_eq!(row["duration_ms"], 0);
            assert_eq!(
                row["duration_source"],
                if status.is_some() {
                    json!("origin")
                } else {
                    Value::Null
                }
            );
            assert!(row.get("completeness").is_none());
            assert!(result.unavailable_evidence.contains(&"completeness"));
            assert!(result.unavailable_evidence.contains(&"raw_origin_usage"));
        }
        store.conn.lock().unwrap().execute("UPDATE usage_events SET cache_read_status='unsupported', cache_read_source='origin_usage' WHERE request_id='legacy'", []).unwrap();
        let result = store
            .diagnostics(&query(DiagnosticSelector::Request("legacy".into()), 1))
            .unwrap();
        assert!(result.rows[0]["cache_read_observation"].is_null());
        assert!(result.rows[0]["warnings"]
            .as_array()
            .unwrap()
            .contains(&json!("cache_read_observation")));
    }

    #[test]
    fn projection_excludes_sensitive_columns_and_query_does_not_mutate_store() {
        let store = SqliteStore::in_memory().unwrap();
        let mut event = event("r", "s", "run");
        event.virtual_key_id = Some("SECRET_KEY_CANARY".into());
        event.subject_id = Some("SECRET_SUBJECT_CANARY".into());
        event.group_id = Some("SECRET_GROUP_CANARY".into());
        event.route = Some("https://SECRET_URL_CANARY".into());
        store.insert(&event).unwrap();
        let before = store.conn.lock().unwrap().total_changes();
        let result = store
            .diagnostics(&query(DiagnosticSelector::Run("run".into()), 1))
            .unwrap();
        assert_eq!(store.conn.lock().unwrap().total_changes(), before);
        let encoded = serde_json::to_string(&result).unwrap();
        assert!(!encoded.contains("SECRET_"));
        for forbidden in [
            "virtual_key_id",
            "subject_id",
            "group_id",
            "route",
            "trace_context",
        ] {
            assert!(result.rows[0].get(forbidden).is_none());
        }
    }

    #[test]
    fn legacy_database_adds_only_index_and_existing_optional_columns() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE usage_events (request_id TEXT, occurred_at TEXT, provider TEXT,
            model TEXT, backend TEXT, virtual_key_id TEXT, subject_id TEXT, group_id TEXT, route TEXT,
            session_id TEXT, tokens_in INTEGER, tokens_out INTEGER, cache_creation_tokens INTEGER,
            cache_read_tokens INTEGER, gpu_seconds REAL);
            INSERT INTO usage_events(request_id,tokens_in,cache_read_tokens) VALUES('old',9,0);").unwrap();
        SqliteStore::setup(&conn).unwrap();
        SqliteStore::setup(&conn).unwrap();
        let store = SqliteStore::from_conn(conn);
        let result = store
            .diagnostics(&query(DiagnosticSelector::Request("old".into()), 1))
            .unwrap();
        assert_eq!(result.returned_rows, 1);
        assert_eq!(result.rows[0]["tokens_in"], 9);
        assert!(result.rows[0]["cache_read_observation"].is_null());
        assert!(result.rows[0].get("duration_ms").is_none());
    }
}
