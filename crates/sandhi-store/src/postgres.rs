//! Optional Postgres backend for the [`crate::UsageStore`] seam (cargo feature `postgres`,
//! NON-default). Selected at runtime by the operator via `SANDHI_USAGE_DSN` in `sandhi-proxy`;
//! when unset the default [`crate::SqliteStore`] is used and nothing here compiles into the
//! binary.
//!
//! Scope is exactly the usage-event sink + its aggregation queries. The enforcement ledger,
//! credential vault and virtual-key stores stay SQLite-only this phase — `SANDHI_STORE` (the
//! ordinary SQLite file) remains mandatory for those even when this backend is active, and the
//! SQLite `usage_events` table simply stops receiving writes.
//!
//! Design notes:
//!
//! - **Runtime API only.** The house SQL is dynamic (`$N` placeholders built at runtime); no
//!   query! macros, no offline mode, so nothing here needs `sqlx-data` or a live DB to compile.
//! - **Blocking facade over an async pool.** The sink chain calls `emit` from the
//!   `BufferedSink` writer thread and every dashboard read runs under `spawn_blocking`, so all
//!   call sites are ordinary blocking threads. The store keeps the `tokio::runtime::Handle`
//!   that owned its connection pool and `block_on`s each statement there — callers MUST be off
//!   an async executor (the proxy satisfies this for every usage-store call site).
//! - **Same fold semantics as SQLite.** The billable/cache-coverage fragments mirror
//!   `sandhi_core`'s per-call folds exactly like the SQLite fragments do; only the dialect
//!   differs (`$N` binds, `to_char` month/day buckets, explicit `::bigint` casts because
//!   `SUM(bigint)` widens to `numeric` in Postgres).
//! - **Ordering key.** SQLite orders diagnostics by `rowid`; Postgres rows carry an explicit
//!   `id bigserial PRIMARY KEY` for the same newest-first insertion order. The SQLite schema is
//!   NOT changed.
//! - **No lakehouse DDL in code.** Init is idempotent `CREATE TABLE IF NOT EXISTS` + indexes
//!   only. Operators who manage the table externally (e.g. a pgdatalake lakehouse table created
//!   with `USING lakehouse`) create it with the same column names — including the `id` ordering
//!   key — BEFORE first proxy start; auto-init then skips creation. See
//!   `docs/operator/postgres-usage-store.md`.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use sandhi_core::{
    Backend, CacheReadCoverage, CacheReadObservation, LatencySummary, Sink, UsageAggregateV1,
    UsageEvent,
};
use serde_json::{json, Map, Value};
use sqlx::postgres::{PgPoolOptions, PgRow};
use sqlx::{PgPool, Row};

use crate::diagnostics::{
    finish_diagnostic_row, DiagnosticQuery, DiagnosticResponse, FieldKind, FIELDS, MAX_FIELD_BYTES,
    MAX_RESPONSE_BYTES,
};
use crate::{
    cache_observation, latency_source_label, UsageStore, UsageStoreError, LATENCY_SAMPLE_LIMIT,
};

/// One aggregation row (or the grand total) — the same backend-agnostic contract type both
/// backends compute.
type Bucket = UsageAggregateV1;

/// Per-row mirror of [`sandhi_core::billable_parts_with_reasoning`], summed — the Postgres
/// dialect of the SQLite fragment, with an explicit `::bigint` because `SUM(bigint)` widens to
/// `numeric` in Postgres.
const BILLABLE_SQL: &str = "COALESCE(SUM(tokens_in + cache_creation_tokens + cache_read_tokens \
     + tokens_out + CASE WHEN reasoning_included = 0 OR \
     (reasoning_included IS NULL AND COALESCE(reasoning_tokens,0) > tokens_out) \
     THEN COALESCE(reasoning_tokens,0) ELSE 0 END),0)::bigint";

// Same known-status/source fold as the SQLite fragment; `SUM(bigint)` widens to `numeric` in
// Postgres, so every sum is cast back to `bigint` for the `u64` Bucket fields.
const CACHE_COVERAGE_SQL: &str = "\
 COALESCE(SUM(CASE WHEN cache_read_status='reported' AND cache_read_source IN ('origin_usage','caller_supplied') THEN 1 ELSE 0 END),0)::bigint,\
 COALESCE(SUM(CASE WHEN cache_read_status='absent' AND cache_read_source='origin_usage' THEN 1 ELSE 0 END),0)::bigint,\
 COALESCE(SUM(CASE WHEN cache_read_status='malformed' AND cache_read_source='origin_usage' THEN 1 ELSE 0 END),0)::bigint,\
 COALESCE(SUM(CASE WHEN cache_read_status='unsupported' AND cache_read_source='explicit_capability' THEN 1 ELSE 0 END),0)::bigint";

/// Idempotent schema init. Column names mirror the SQLite `usage_events` table exactly (the
/// migration script and any externally managed lakehouse table depend on that); storage types
/// map TEXT→text, INTEGER→bigint, REAL→double precision, and `occurred_at` becomes
/// `timestamptz`. The `id` ordering key replaces SQLite's implicit `rowid`.
const DDL: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS usage_events (
        request_id text, occurred_at timestamptz, provider text, model text, backend text,
        virtual_key_id text, subject_id text, group_id text, route text, session_id text,
        tokens_in bigint, tokens_out bigint,
        cache_creation_tokens bigint, cache_read_tokens bigint, gpu_seconds double precision,
        duration_ms bigint, duration_source text,
        time_to_first_token_ms bigint, time_to_first_token_source text,
        reasoning_tokens bigint,
        run_id text, step_id text, parent_id text,
        reasoning_included bigint,
        cache_read_status text, cache_read_source text,
        id bigserial PRIMARY KEY
    )",
    "CREATE INDEX IF NOT EXISTS idx_usage_subject ON usage_events(subject_id)",
    "CREATE INDEX IF NOT EXISTS idx_usage_group ON usage_events(group_id)",
    "CREATE INDEX IF NOT EXISTS idx_usage_provider ON usage_events(provider)",
    "CREATE INDEX IF NOT EXISTS idx_usage_model ON usage_events(model)",
    "CREATE INDEX IF NOT EXISTS idx_usage_vkey ON usage_events(virtual_key_id)",
    "CREATE INDEX IF NOT EXISTS idx_usage_session ON usage_events(session_id)",
    "CREATE INDEX IF NOT EXISTS idx_usage_occurred ON usage_events(occurred_at)",
    "CREATE INDEX IF NOT EXISTS idx_usage_run ON usage_events(run_id)",
    "CREATE INDEX IF NOT EXISTS idx_usage_request ON usage_events(request_id)",
];

const INSERT_SQL: &str = "INSERT INTO usage_events (
    request_id, occurred_at, provider, model, backend,
    virtual_key_id, subject_id, group_id, route, session_id,
    tokens_in, tokens_out, cache_creation_tokens, cache_read_tokens, gpu_seconds,
    duration_ms, duration_source, time_to_first_token_ms, time_to_first_token_source,
    reasoning_tokens, run_id, step_id, parent_id, reasoning_included,
    cache_read_status, cache_read_source
) VALUES ($1,$2::timestamptz,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17,$18,$19,$20,$21,$22,$23,$24,$25,$26)";

/// A Postgres-backed usage store: the same read/emit surface as [`crate::SqliteStore`].
pub struct PostgresStore {
    pool: PgPool,
    /// The runtime that owns the pool's background tasks. All statements are `block_on`'d
    /// through this handle from ordinary blocking threads (sink writer, `spawn_blocking`
    /// readers). Never call store methods from an async executor task.
    rt: tokio::runtime::Handle,
    /// How many `emit`s failed at the DB — the best-effort-sink counter (ADR-0047 D7),
    /// identical semantics to the SQLite backend.
    emit_failures: AtomicU64,
}

impl PostgresStore {
    /// Connect and initialize the schema (idempotent `CREATE IF NOT EXISTS`). The pool is small
    /// on purpose: one writer thread plus a few dashboard readers is all the usage store serves.
    ///
    /// Must be called on a `tokio` runtime (the proxy connects inside its startup `block_on`);
    /// that runtime's handle is retained for every later blocking statement.
    pub async fn connect(dsn: &str) -> Result<Self, sqlx::Error> {
        let pool = PgPoolOptions::new()
            .max_connections(4)
            .acquire_timeout(Duration::from_secs(5))
            .connect(dsn)
            .await?;
        for statement in DDL {
            sqlx::query(statement).execute(&pool).await?;
        }
        Ok(Self {
            pool,
            rt: tokio::runtime::Handle::current(),
            emit_failures: AtomicU64::new(0),
        })
    }

    /// Run one statement batch on the retained runtime handle. Callers must be blocking
    /// threads (see the module docs) — `Handle::block_on` panics inside an async task by
    /// design, which keeps this mis-use loud instead of silently serializing a worker.
    fn block<T>(&self, fut: impl std::future::Future<Output = T>) -> T {
        self.rt.block_on(fut)
    }

    /// One best-effort durable write: never panics, counts and logs failures (ADR-0047 D7).
    fn record(&self, event: &UsageEvent) -> bool {
        if let Err(e) = self.block(self.insert_async(event)) {
            self.emit_failures.fetch_add(1, Ordering::Relaxed);
            tracing::error!(
                target: "sandhi.store.emit_failed",
                error = %e,
                request_id = %event.request_id,
                provider = %event.provider,
                "usage-event emit FAILED against Postgres; event lost (best-effort sink, ADR-0047 D7)"
            );
            return false;
        }
        true
    }

    async fn insert_async(&self, e: &UsageEvent) -> Result<(), sqlx::Error> {
        let backend = match e.backend {
            Backend::External => "external",
            Backend::SelfHosted => "self_hosted",
        };
        let observation = e
            .cache_read_observation
            .and_then(CacheReadObservation::validated)
            .map(|value| serde_json::to_value(value).expect("bounded cache observation"));
        sqlx::query(INSERT_SQL)
            .bind(e.request_id.clone())
            .bind(e.occurred_at.clone())
            .bind(e.provider.clone())
            .bind(e.model.clone())
            .bind(backend)
            .bind(e.virtual_key_id.clone())
            .bind(e.subject_id.clone())
            .bind(e.group_id.clone())
            .bind(e.route.clone())
            .bind(e.session_id.clone())
            .bind(e.tokens_in as i64)
            .bind(e.tokens_out as i64)
            .bind(e.cache_creation_tokens as i64)
            .bind(e.cache_read_tokens as i64)
            .bind(e.gpu_seconds)
            .bind(e.duration_ms.map(|v| v as i64))
            .bind(latency_source_label(e.duration_source))
            .bind(e.time_to_first_token_ms.map(|v| v as i64))
            .bind(latency_source_label(e.time_to_first_token_source))
            .bind(e.reasoning_tokens.map(|v| v as i64))
            .bind(e.run_id.clone())
            .bind(e.step_id.clone())
            .bind(e.parent_id.clone())
            .bind(e.reasoning_included.map(i64::from))
            .bind(observation.as_ref().and_then(|v| v["status"].as_str()))
            .bind(observation.as_ref().and_then(|v| v["source"].as_str()))
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Totals grouped by a fixed column within optional RFC-3339 bounds (SQLite parity:
    /// `from` inclusive, `to` exclusive, busiest billable first, then latency sampling).
    async fn totals_grouped_between_async(
        &self,
        col: &str,
        from: Option<&str>,
        to: Option<&str>,
    ) -> Result<Vec<Bucket>, sqlx::Error> {
        let (where_clause, bounds) = bounds_clause(from, to);
        let sql = format!(
            "SELECT COALESCE({col}, '(none)') AS k, COUNT(*), \
                COALESCE(SUM(tokens_in),0)::bigint, COALESCE(SUM(tokens_out),0)::bigint, \
                COALESCE(SUM(cache_creation_tokens),0)::bigint, COALESCE(SUM(cache_read_tokens),0)::bigint, \
                COALESCE(SUM(reasoning_tokens),0)::bigint, {BILLABLE_SQL}, {CACHE_COVERAGE_SQL} \
             FROM usage_events {where_clause} GROUP BY k \
             ORDER BY {BILLABLE_SQL} DESC"
        );
        let mut query = sqlx::query(&sql);
        for bound in bounds {
            query = query.bind(bound);
        }
        let mut buckets = query
            .fetch_all(&self.pool)
            .await?
            .iter()
            .map(grouped_bucket)
            .collect::<Result<Vec<_>, sqlx::Error>>()?;
        let samples = self.latency_samples_async(col, from).await?;
        for b in &mut buckets {
            if let Some((d, t)) = samples.get(&b.key) {
                b.latency = LatencySummary::from_samples(d, t);
            }
        }
        Ok(buckets)
    }

    /// Latency samples per group key — the bounded most-recent sample set
    /// ([`LatencySummary::from_samples`] computes the percentiles, one definition, TD-0009 D6).
    /// `id DESC` breaks timestamp ties in insertion order, matching SQLite's `rowid` behavior.
    async fn latency_samples_async(
        &self,
        col: &str,
        since: Option<&str>,
    ) -> Result<BTreeMap<String, (Vec<u64>, Vec<u64>)>, sqlx::Error> {
        let (where_clause, mut binds): (String, Vec<String>) = match since {
            Some(s) => (
                "WHERE occurred_at >= $1::timestamptz AND duration_ms IS NOT NULL".into(),
                vec![s.to_string()],
            ),
            None => ("WHERE duration_ms IS NOT NULL".into(), vec![]),
        };
        let sql = format!(
            "SELECT COALESCE({col}, '(none)') AS k, duration_ms, time_to_first_token_ms \
             FROM usage_events {where_clause} \
             ORDER BY occurred_at DESC, id DESC LIMIT {LATENCY_SAMPLE_LIMIT}"
        );
        let mut query = sqlx::query(&sql);
        for bound in binds.drain(..) {
            query = query.bind(bound);
        }
        let mut out: BTreeMap<String, (Vec<u64>, Vec<u64>)> = BTreeMap::new();
        for row in query.fetch_all(&self.pool).await? {
            let key: String = row.get(0);
            let duration: i64 = row.get(1);
            let ttft: Option<i64> = row.get(2);
            let entry = out.entry(key).or_default();
            entry.0.push(duration as u64);
            if let Some(t) = ttft {
                entry.1.push(t as u64);
            }
        }
        Ok(out)
    }

    /// One aggregate row (COUNT + sums + billable + coverage + an explicit latency summary).
    async fn total_row_async(
        &self,
        where_clause: &str,
        binds: &[String],
        latency: Option<sandhi_core::LatencySummary>,
    ) -> Result<Bucket, sqlx::Error> {
        let sql = format!(
            "SELECT COUNT(*), COALESCE(SUM(tokens_in),0)::bigint, COALESCE(SUM(tokens_out),0)::bigint, \
                COALESCE(SUM(cache_creation_tokens),0)::bigint, COALESCE(SUM(cache_read_tokens),0)::bigint, \
                COALESCE(SUM(reasoning_tokens),0)::bigint, {BILLABLE_SQL}, {CACHE_COVERAGE_SQL} \
             FROM usage_events {where_clause}"
        );
        let mut query = sqlx::query(&sql);
        for bound in binds {
            query = query.bind(bound);
        }
        let row = query.fetch_one(&self.pool).await?;
        Ok(total_bucket(&row, latency))
    }

    async fn grand_total_async(&self) -> Result<Bucket, sqlx::Error> {
        // `'total'` funnels every sampled row into one key, exactly like the SQLite path —
        // no second latency query to keep in step.
        let samples = self.latency_samples_async("'total'", None).await?;
        let latency = samples
            .get("total")
            .and_then(|(d, t)| LatencySummary::from_samples(d, t));
        self.total_row_async("WHERE 1=1", &[], latency).await
    }

    async fn grand_total_between_async(&self, from: &str, to: &str) -> Result<Bucket, sqlx::Error> {
        let samples = self.latency_samples_async("'total'", Some(from)).await?;
        let latency = samples
            .get("total")
            .and_then(|(d, t)| LatencySummary::from_samples(d, t));
        self.total_row_async(
            "WHERE occurred_at >= $1::timestamptz AND occurred_at < $2::timestamptz",
            &[from.to_string(), to.to_string()],
            latency,
        )
        .await
    }

    async fn windowed_async(
        &self,
        bucket: &str,
        from: &str,
        to: &str,
    ) -> Result<Vec<Bucket>, sqlx::Error> {
        // UTC rendering keeps the bucket keys identical to SQLite's strftime output
        // ("2026-07" / "2026-07-01") regardless of the server's TimeZone setting.
        let pattern = match bucket {
            "month" => "YYYY-MM",
            "day" => "YYYY-MM-DD",
            _ => {
                return Err(sqlx::Error::Configuration(
                    format!("unknown bucket: {bucket}").into(),
                ))
            }
        };
        let sql = format!(
            "SELECT to_char(occurred_at AT TIME ZONE 'UTC', '{pattern}') AS k, COUNT(*), \
                COALESCE(SUM(tokens_in),0)::bigint, COALESCE(SUM(tokens_out),0)::bigint, \
                COALESCE(SUM(cache_creation_tokens),0)::bigint, COALESCE(SUM(cache_read_tokens),0)::bigint, \
                COALESCE(SUM(reasoning_tokens),0)::bigint, {BILLABLE_SQL}, {CACHE_COVERAGE_SQL} \
             FROM usage_events WHERE occurred_at >= $1::timestamptz AND occurred_at < $2::timestamptz \
             GROUP BY k ORDER BY k ASC"
        );
        sqlx::query(&sql)
            .bind(from)
            .bind(to)
            .fetch_all(&self.pool)
            .await?
            .iter()
            .map(grouped_bucket)
            .collect()
    }

    async fn run_cost_tree_async(
        &self,
        run_id: &str,
    ) -> Result<Option<sandhi_core::RunCostTreeV1>, sqlx::Error> {
        // SQL only filters — the fold is `RunCostTreeV1::from_events`, the one definition
        // (TD-0009 D6), exactly as on the SQLite backend.
        let rows = sqlx::query(
            "SELECT tokens_in, tokens_out, cache_creation_tokens, cache_read_tokens, \
                    reasoning_tokens, step_id, parent_id, reasoning_included, \
                    cache_read_status, cache_read_source \
             FROM usage_events WHERE run_id = $1",
        )
        .bind(run_id)
        .fetch_all(&self.pool)
        .await?;
        if rows.is_empty() {
            return Ok(None);
        }
        let events = rows
            .iter()
            .map(|r| {
                let tokens_in: i64 = r.try_get(0)?;
                let tokens_out: i64 = r.try_get(1)?;
                let cache_creation: i64 = r.try_get(2)?;
                let cache_read: i64 = r.try_get(3)?;
                let reasoning: Option<i64> = r.try_get(4)?;
                let step_id: Option<String> = r.try_get(5)?;
                let parent_id: Option<String> = r.try_get(6)?;
                let reasoning_included: Option<i64> = r.try_get(7)?;
                let status: Option<String> = r.try_get(8)?;
                let source: Option<String> = r.try_get(9)?;
                let mut event = UsageEvent::new("", "", "", "", Backend::External)
                    .with_tokens(tokens_in as u64, tokens_out as u64)
                    .with_cache(cache_creation as u64, cache_read as u64)
                    .with_reasoning(reasoning.map(|v| v as u64))
                    .with_reasoning_included(reasoning_included.map(|v| v != 0))
                    .with_identity(None, Some(run_id.to_string()), step_id, parent_id, None);
                event.cache_read_observation =
                    cache_observation(status.as_deref(), source.as_deref());
                Ok(event)
            })
            .collect::<Result<Vec<_>, sqlx::Error>>()?;
        Ok(Some(sandhi_core::RunCostTreeV1::from_events(
            run_id, &events,
        )))
    }

    async fn diagnostics_async(
        &self,
        query: &DiagnosticQuery,
    ) -> Result<DiagnosticResponse, sqlx::Error> {
        let mut result = DiagnosticResponse::empty();
        // Reserve the largest possible envelope up front (byte-budget accounting identical to
        // the SQLite backend, ADR-0011).
        let mut envelope = DiagnosticResponse::empty();
        envelope.returned_rows = crate::diagnostics::MAX_LIMIT;
        envelope.truncation_reason = Some("byte_limit");
        let mut used = serde_json::to_vec(&envelope)
            .expect("fixed JSON envelope")
            .len();
        let rows = sqlx::query(&diagnostics_sql(query))
            .bind(query.selector.value())
            .bind((query.limit + 1) as i64)
            .fetch_all(&self.pool)
            .await?;
        for row in &rows {
            if result.rows.len() == query.limit {
                result.truncated = true;
                result.truncation_reason = Some("row_limit");
                break;
            }
            let value = project_row(row);
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
        Ok(result)
    }
}

/// Optional RFC-3339 window bounds as a `$N::timestamptz` cast clause — RFC-3339 strings bind
/// directly as text and Postgres parses them (SQLite compares the raw strings; `timestamptz`
/// compares the instants, which is strictly more correct for mixed offsets).
fn bounds_clause(from: Option<&str>, to: Option<&str>) -> (String, Vec<String>) {
    match (from, to) {
        (Some(f), Some(t)) => (
            "WHERE occurred_at >= $1::timestamptz AND occurred_at < $2::timestamptz".into(),
            vec![f.to_string(), t.to_string()],
        ),
        (Some(f), None) => (
            "WHERE occurred_at >= $1::timestamptz".into(),
            vec![f.to_string()],
        ),
        (None, Some(t)) => (
            "WHERE occurred_at < $1::timestamptz".into(),
            vec![t.to_string()],
        ),
        (None, None) => ("WHERE 1=1".into(), vec![]),
    }
}

/// Aggregate row → [`Bucket`] in the grouped/windowed column order (key at 0, coverage at 8).
fn grouped_bucket(row: &PgRow) -> Result<Bucket, sqlx::Error> {
    let calls = row.get::<i64, _>(1) as u64;
    Ok(Bucket {
        key: row.get(0),
        calls,
        tokens_in: row.get::<i64, _>(2) as u64,
        tokens_out: row.get::<i64, _>(3) as u64,
        cache_creation_tokens: row.get::<i64, _>(4) as u64,
        cache_read_tokens: row.get::<i64, _>(5) as u64,
        reasoning_tokens: row.get::<i64, _>(6) as u64,
        billable_tokens: row.get::<i64, _>(7) as u64,
        latency: None,
        cache_read_coverage: Some(partial_cache_coverage(row, 8, calls)),
    })
}

/// Aggregate row → [`Bucket`] in the grand-total column order (COUNT at 0, coverage at 7).
fn total_bucket(row: &PgRow, latency: Option<sandhi_core::LatencySummary>) -> Bucket {
    let calls = row.get::<i64, _>(0) as u64;
    Bucket {
        key: "total".to_string(),
        calls,
        tokens_in: row.get::<i64, _>(1) as u64,
        tokens_out: row.get::<i64, _>(2) as u64,
        cache_creation_tokens: row.get::<i64, _>(3) as u64,
        cache_read_tokens: row.get::<i64, _>(4) as u64,
        reasoning_tokens: row.get::<i64, _>(5) as u64,
        billable_tokens: row.get::<i64, _>(6) as u64,
        latency,
        cache_read_coverage: Some(partial_cache_coverage(row, 7, calls)),
    }
}

/// The four known cache-coverage counters from `CACHE_COVERAGE_SQL`; unknown = the remainder.
fn partial_cache_coverage(row: &PgRow, start: usize, calls: u64) -> CacheReadCoverage {
    let reported = row.get::<i64, _>(start) as u64;
    let absent = row.get::<i64, _>(start + 1) as u64;
    let malformed = row.get::<i64, _>(start + 2) as u64;
    let unsupported = row.get::<i64, _>(start + 3) as u64;
    CacheReadCoverage {
        reported,
        absent,
        malformed,
        unsupported,
        unknown: calls.saturating_sub(reported + absent + malformed + unsupported),
    }
}

/// PG-dialect diagnostic projection: the same allowlist and value/present column pairs as the
/// SQLite `typeof` guards. Typed Postgres columns make the type checks structural (a `bigint`
/// column cannot hold text), so each guard checks only what a Postgres row can still get
/// wrong: NULL-ness, sign and the byte-length budget.
fn diagnostics_sql(query: &DiagnosticQuery) -> String {
    let projection = FIELDS
        .iter()
        .map(|(name, kind)| {
            // Timestamps project back to canonical UTC RFC-3339 text (matching
            // the SQLite stored form); octet_length on a timestamptz column
            // would be a prepare-time type error, and the type is structural.
            let value_expr = match kind {
                FieldKind::Text => name.to_string(),
                FieldKind::Timestamp => format!(
                    "to_char({name} AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"')"
                ),
                FieldKind::Count | FieldKind::Boolean => name.to_string(),
            };
            let valid = match kind {
                FieldKind::Text => {
                    format!("{name} IS NOT NULL AND octet_length({name}) <= {MAX_FIELD_BYTES}")
                }
                FieldKind::Timestamp => format!("{name} IS NOT NULL"),
                FieldKind::Count => format!("{name} IS NOT NULL AND {name} >= 0"),
                FieldKind::Boolean => format!("{name} IS NOT NULL AND {name} IN (0,1)"),
            };
            // The second projection distinguishes an absent value from a rejected value.
            format!("CASE WHEN {valid} THEN {value_expr} ELSE NULL END, {name} IS NOT NULL")
        })
        .collect::<Vec<_>>()
        .join(", ");
    let selection = match query.selector {
        crate::diagnostics::DiagnosticSelector::Request(_) => "WHERE request_id = $1",
        crate::diagnostics::DiagnosticSelector::Session(_) => "WHERE session_id = $1",
        crate::diagnostics::DiagnosticSelector::Run(_) => "WHERE run_id = $1",
    };
    // Newest insertion first: `id DESC` replaces SQLite's `rowid DESC`.
    format!("SELECT {projection} FROM usage_events {selection} ORDER BY id DESC LIMIT $2")
}

/// Map one diagnostic row to its bounded JSON projection — the same canonicalization rules as
/// the SQLite `project` (empty/control/malformed values are omitted with a warning, never
/// coerced), sharing [`finish_diagnostic_row`].
fn project_row(row: &PgRow) -> Value {
    let mut object = Map::new();
    let mut warnings = BTreeSet::new();
    for (index, (name, kind)) in FIELDS.iter().enumerate() {
        let canonical = match kind {
            // The Timestamp projection is already to_char text (canonical UTC).
            FieldKind::Text | FieldKind::Timestamp => row
                .try_get::<Option<String>, _>(index * 2)
                .ok()
                .flatten()
                .filter(|value| !value.is_empty() && !value.chars().any(char::is_control))
                .map(Value::String),
            FieldKind::Count => row
                .try_get::<Option<i64>, _>(index * 2)
                .ok()
                .flatten()
                .filter(|value| *value >= 0)
                .map(|value| json!(value)),
            FieldKind::Boolean => row
                .try_get::<Option<i64>, _>(index * 2)
                .ok()
                .flatten()
                .filter(|value| *value == 0 || *value == 1)
                .map(|value| json!(value == 1)),
        };
        let present = row.try_get::<bool, _>(index * 2 + 1).unwrap_or(false);
        if let Some(value) = canonical {
            object.insert((*name).into(), value);
        } else if present {
            warnings.insert((*name).into());
        }
    }
    finish_diagnostic_row(object, warnings)
}

impl UsageStore for PostgresStore {
    fn emit(&self, event: &UsageEvent) -> bool {
        self.record(event)
    }

    fn emit_failures(&self) -> u64 {
        self.emit_failures.load(Ordering::Relaxed)
    }

    fn grand_total(&self) -> Result<Bucket, UsageStoreError> {
        self.block(self.grand_total_async())
            .map_err(UsageStoreError::from)
    }

    fn grand_total_between(&self, from: &str, to: &str) -> Result<Bucket, UsageStoreError> {
        self.block(self.grand_total_between_async(from, to))
            .map_err(UsageStoreError::from)
    }

    fn totals_grouped_between(
        &self,
        col: &str,
        from: Option<&str>,
        to: Option<&str>,
    ) -> Result<Vec<Bucket>, UsageStoreError> {
        self.block(self.totals_grouped_between_async(col, from, to))
            .map_err(UsageStoreError::from)
    }

    fn totals_windowed(
        &self,
        bucket: &str,
        from: &str,
        to: &str,
    ) -> Result<Vec<Bucket>, UsageStoreError> {
        self.block(self.windowed_async(bucket, from, to))
            .map_err(UsageStoreError::from)
    }

    fn totals_by_subject(&self) -> Result<Vec<Bucket>, UsageStoreError> {
        self.totals_grouped_between("subject_id", None, None)
    }

    fn totals_by_group(&self) -> Result<Vec<Bucket>, UsageStoreError> {
        self.totals_grouped_between("group_id", None, None)
    }

    fn totals_by_provider(&self) -> Result<Vec<Bucket>, UsageStoreError> {
        self.totals_grouped_between("provider", None, None)
    }

    fn totals_by_model(&self) -> Result<Vec<Bucket>, UsageStoreError> {
        self.totals_grouped_between("model", None, None)
    }

    fn totals_by_virtual_key(&self) -> Result<Vec<Bucket>, UsageStoreError> {
        self.totals_grouped_between("virtual_key_id", None, None)
    }

    fn totals_by_session(&self) -> Result<Vec<Bucket>, UsageStoreError> {
        self.totals_grouped_between("session_id", None, None)
    }

    fn totals_by_run(&self) -> Result<Vec<Bucket>, UsageStoreError> {
        self.totals_grouped_between("run_id", None, None)
    }

    fn totals_since(
        &self,
        dimension: &str,
        since: &str,
    ) -> Result<Option<Vec<Bucket>>, UsageStoreError> {
        let col = match dimension {
            "subject" | "user" => "subject_id",
            "group" => "group_id",
            "provider" => "provider",
            "model" => "model",
            "key" | "virtual_key" => "virtual_key_id",
            "session" => "session_id",
            "run" => "run_id",
            _ => return Ok(None),
        };
        Ok(Some(self.totals_grouped_between(col, Some(since), None)?))
    }

    fn run_cost_tree(
        &self,
        run_id: &str,
    ) -> Result<Option<sandhi_core::RunCostTreeV1>, UsageStoreError> {
        self.block(self.run_cost_tree_async(run_id))
            .map_err(UsageStoreError::from)
    }

    fn diagnostics(&self, query: &DiagnosticQuery) -> Result<DiagnosticResponse, UsageStoreError> {
        query
            .validate()
            .map_err(|_| UsageStoreError("invalid diagnostic query".into()))?;
        self.block(self.diagnostics_async(query))
            .map_err(UsageStoreError::from)
    }
}

impl Sink for PostgresStore {
    fn emit(&self, event: &UsageEvent) {
        // Best-effort — a storage failure must never break the caller (ADR-0047 D7); `record`
        // counts it and logs so an operator knows spend/attribution is incomplete.
        let _ = self.record(event);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diagnostics::DiagnosticSelector;
    use sandhi_core::LatencySource;

    fn ev(provider: &str, subject: &str, group: &str, tin: u64, tout: u64) -> UsageEvent {
        UsageEvent::new(
            "r",
            "2026-07-19T00:00:00Z",
            provider,
            "m",
            Backend::External,
        )
        .with_attribution(Some("vk".into()), Some(subject.into()), Some(group.into()))
        .with_tokens(tin, tout)
        .with_cache(0, 5)
    }

    #[test]
    fn bounds_clause_covers_all_window_shapes() {
        let (clause, binds) = bounds_clause(Some("f"), Some("t"));
        assert_eq!(
            clause,
            "WHERE occurred_at >= $1::timestamptz AND occurred_at < $2::timestamptz"
        );
        assert_eq!(binds, vec!["f".to_string(), "t".to_string()]);
        let (clause, binds) = bounds_clause(Some("f"), None);
        assert_eq!(clause, "WHERE occurred_at >= $1::timestamptz");
        assert_eq!(binds, vec!["f".to_string()]);
        let (clause, binds) = bounds_clause(None, Some("t"));
        assert_eq!(clause, "WHERE occurred_at < $1::timestamptz");
        assert_eq!(binds, vec!["t".to_string()]);
        let (clause, binds) = bounds_clause(None, None);
        assert_eq!(clause, "WHERE 1=1");
        assert!(binds.is_empty());
    }

    #[test]
    fn diagnostics_sql_orders_newest_first_and_binds_selector_and_limit() {
        for selector in [
            DiagnosticSelector::Request("r".into()),
            DiagnosticSelector::Session("s".into()),
            DiagnosticSelector::Run("run".into()),
        ] {
            let query = DiagnosticQuery {
                selector,
                limit: 10,
            };
            let sql = diagnostics_sql(&query);
            assert!(sql.contains("ORDER BY id DESC LIMIT $2"), "{sql}");
            assert!(sql.contains("octet_length(request_id)"), "{sql}");
            // occurred_at is timestamptz: guarded by NULL-ness only and
            // projected to canonical UTC RFC-3339 text — never octet_length.
            assert!(
                sql.contains("to_char(occurred_at AT TIME ZONE 'UTC'"),
                "{sql}"
            );
            assert!(!sql.contains("octet_length(occurred_at)"), "{sql}");
            assert!(!sql.contains("virtual_key_id"), "{sql}");
            assert!(!sql.contains("subject_id"), "{sql}");
            assert!(!sql.contains("group_id"), "{sql}");
            assert!(!sql.contains("route"), "{sql}");
            assert!(
                sql.contains("reasoning_included IS NOT NULL AND reasoning_included IN (0,1)"),
                "{sql}"
            );
        }
    }

    #[test]
    fn ddl_creates_the_full_column_set_with_the_ordering_key() {
        let joined = DDL[0];
        for column in [
            "request_id",
            "occurred_at timestamptz",
            "gpu_seconds double precision",
            "reasoning_included bigint",
            "cache_read_status text",
            "cache_read_source text",
            "id bigserial PRIMARY KEY",
        ] {
            assert!(joined.contains(column), "missing {column}");
        }
        // Every attribution dimension and the time index are indexed at init.
        for index in [
            "idx_usage_subject",
            "idx_usage_group",
            "idx_usage_provider",
            "idx_usage_model",
            "idx_usage_vkey",
            "idx_usage_session",
            "idx_usage_occurred",
            "idx_usage_run",
            "idx_usage_request",
        ] {
            assert!(DDL.iter().any(|s| s.contains(index)), "missing {index}");
        }
    }

    // ── Live-Postgres tests. ──
    //
    // These run ONLY when the `postgres` cargo feature is enabled AND a real Postgres is
    // reachable, so CI's default gate never needs a database:
    //
    //   SANDHI_TEST_PG_DSN=postgres://user:pass@localhost/sandhi_test \
    //     cargo test -p sandhi-store --features postgres -- --ignored
    //
    // The database must allow CREATE TABLE/INDEX (the store initializes its schema on
    // connect). Tests only touch rows tagged with the unique `pg-phase2-` run id, so a
    // shared instance stays usable; nothing is dropped.

    /// A unique run id per process so shared instances never cross-test-contaminate.
    fn test_run_id() -> String {
        format!("pg-phase2-{}", std::process::id())
    }

    fn pg_store() -> Option<PostgresStore> {
        let dsn = std::env::var("SANDHI_TEST_PG_DSN")
            .ok()
            .filter(|dsn| !dsn.trim().is_empty())?;
        let dsn = dsn.trim().to_string();
        // The runtime is created per test and leaked: the store retains its handle, and the
        // pool's background tasks must outlive every blocking statement below.
        let rt = tokio::runtime::Runtime::new().expect("test runtime");
        let store = rt.block_on(PostgresStore::connect(&dsn)).expect("connect");
        std::mem::forget(rt);
        Some(store)
    }

    #[test]
    #[ignore = "requires a live Postgres via SANDHI_TEST_PG_DSN"]
    fn emit_aggregates_and_windows_match_the_sqlite_contract() {
        let Some(store) = pg_store() else {
            panic!("set SANDHI_TEST_PG_DSN to run the live-Postgres tests");
        };
        let run = test_run_id();
        crate::UsageStore::emit(&store, &ev("openai", "alice", "team-a", 100, 20));
        crate::UsageStore::emit(
            &store,
            &ev("anthropic", "bob", "team-b", 200, 40).with_identity(
                None,
                Some(run.clone()),
                Some("root".into()),
                None,
                None,
            ),
        );
        assert_eq!(store.emit_failures(), 0);

        let total = store.grand_total().expect("grand total");
        assert!(total.calls >= 2);
        assert!(total.tokens_in >= 300);

        let by_subject = store.totals_by_subject().expect("by subject");
        let alice = by_subject.iter().find(|b| b.key == "alice").expect("alice");
        assert_eq!(alice.calls, 1);
        assert_eq!(alice.tokens_in, 100);
        assert_eq!(alice.billable_tokens, 125); // 100 in + 5 cache-read + 20 out
        assert_eq!(
            alice
                .cache_read_coverage
                .as_ref()
                .expect("coverage")
                .unknown,
            1
        );

        // Window bounds are inclusive/exclusive and buckets render like SQLite's strftime.
        let months = store
            .totals_windowed("month", "2026-07-01T00:00:00Z", "2026-08-01T00:00:00Z")
            .expect("months");
        assert_eq!(months.len(), 1);
        assert_eq!(months[0].key, "2026-07");
        assert_eq!(months[0].calls, 2);
        assert_eq!(months[0].tokens_in, 300);
        let days = store
            .totals_windowed("day", "2026-07-01T00:00:00Z", "2026-08-01T00:00:00Z")
            .expect("days");
        assert_eq!(days.len(), 1);
        assert_eq!(days[0].key, "2026-07-19");
        assert!(store
            .totals_windowed("week", "2026-07-01T00:00:00Z", "2026-08-01T00:00:00Z")
            .is_err());

        let empty = store
            .totals_since("provider", "2099-01-01T00:00:00Z")
            .expect("since")
            .expect("known dimension");
        assert!(empty.is_empty());
        assert!(store
            .totals_since("nope", "2026-01-01T00:00:00Z")
            .unwrap()
            .is_none());
    }

    #[test]
    #[ignore = "requires a live Postgres via SANDHI_TEST_PG_DSN"]
    fn run_cost_tree_and_diagnostics_round_trip() {
        let Some(store) = pg_store() else {
            panic!("set SANDHI_TEST_PG_DSN to run the live-Postgres tests");
        };
        let run = test_run_id();
        crate::UsageStore::emit(
            &store,
            &ev("openai", "alice", "team-a", 10, 5)
                .with_identity(None, Some(run.clone()), Some("root".into()), None, None)
                .with_latency(Some(40), Some(20))
                .with_latency_sources(Some(LatencySource::Origin), Some(LatencySource::Boundary)),
        );
        crate::UsageStore::emit(
            &store,
            &ev("openai", "alice", "team-a", 20, 10)
                .with_identity(
                    None,
                    Some(run.clone()),
                    Some("plan".into()),
                    Some("root".into()),
                    None,
                )
                .with_reasoning(Some(8)),
        );

        let tree = store
            .run_cost_tree(&run)
            .expect("tree")
            .expect("run exists");
        assert_eq!(tree.total.calls, 2);
        assert_eq!(tree.total.tokens_in, 30);
        // Two steps: the root step (no parent) and the child step under it.
        let root = tree
            .roots
            .iter()
            .find(|n| n.step_id == "root")
            .expect("root step");
        assert_eq!(root.parent_id, None);
        assert!(
            root.children.iter().any(|c| c.step_id == "plan"),
            "{:?}",
            root.children
        );
        assert_eq!(root.rollup.tokens_in, 30, "subtree-inclusive rollup");
        assert!(store.run_cost_tree("no-such-run-404").unwrap().is_none());

        // Diagnostics return the newest-inserted row first and export no attribution columns.
        let query = DiagnosticQuery {
            selector: DiagnosticSelector::Run(run),
            limit: 10,
        };
        let response = store.diagnostics(&query).expect("diagnostics");
        assert_eq!(response.returned_rows, 2);
        let first = &response.rows[0];
        assert_eq!(first["duration_ms"], 20); // the second emit was inserted last
        assert_eq!(first["time_to_first_token_ms"], 10);
        assert_eq!(first["reasoning_tokens"], 8);
        assert!(first.get("subject_id").is_none());
        assert!(first.get("virtual_key_id").is_none());

        let invalid = DiagnosticQuery {
            selector: DiagnosticSelector::Run("".into()),
            limit: 10,
        };
        assert!(store.diagnostics(&invalid).is_err());
    }
}
