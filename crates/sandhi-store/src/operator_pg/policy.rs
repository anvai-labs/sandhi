//! Postgres admission-receipt store — the PG port of [`crate::policy::PolicyAuditStore`].
//! Metadata only, never prompts or matched values. The SQLite clock default
//! `strftime('%Y-%m-%dT%H:%M:%fZ','now')` becomes `to_char(now() AT TIME ZONE 'UTC',
//! 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')` (byte-identical UTC text, milliseconds like `%f`),
//! and `lower(hex(randomblob(16)))` becomes `md5(...)` (32 lowercase hex chars, the same
//! id shape). Errors render through [`crate::OperatorStoreError`].
use super::{init_ddl, OperatorPool};
use crate::OperatorStoreError;
use sandhi_core::policy::{Decision, Identity};

/// The SQL default clock: SQLite's `strftime('%Y-%m-%dT%H:%M:%fZ','now')` dialect twin —
/// UTC, `HH:MM:SS.mmm` (3-digit milliseconds, like `%f`), trailing `Z`. Pinned by a
/// dialect test.
const CLOCK_DEFAULT_SQL: &str =
    "to_char(now() AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"')";

/// The SQL default id: SQLite's `lower(hex(randomblob(16)))` dialect twin — 32 lowercase
/// hex chars over per-row entropy (clock timestamp + per-connection random).
const ID_DEFAULT_SQL: &str = "md5(random()::text || clock_timestamp()::text || $5::text)";

/// Idempotent schema init, mirroring the SQLite DDL column-for-column.
const DDL: &[&str] = &["CREATE TABLE IF NOT EXISTS policy_receipts (
        id text PRIMARY KEY,
        created_at text NOT NULL,
        issuer text,
        subject_id text,
        virtual_key_id text NOT NULL,
        decision_json text NOT NULL
    )"];

/// A Postgres-backed mandatory admission-receipt store.
pub struct PostgresPolicyAuditStore {
    pool: OperatorPool,
    max_rows: u64,
}

impl PostgresPolicyAuditStore {
    /// Open a store bounded to `max_rows` receipts.
    ///
    /// Must be called on a `tokio` runtime; that runtime's handle is retained for every
    /// later blocking statement.
    pub async fn connect(dsn: &str, max_rows: u64) -> Result<Self, OperatorStoreError> {
        let options: sqlx::postgres::PgConnectOptions = dsn
            .parse()
            .map_err(|e| OperatorStoreError(format!("invalid DSN: {e}")))?;
        Self::connect_with(options, max_rows).await
    }

    /// Like [`Self::connect`] from pre-parsed connect options: the startup wiring sets
    /// `application_name` (and the test suite points `search_path` at a per-test schema).
    pub async fn connect_with(
        options: sqlx::postgres::PgConnectOptions,
        max_rows: u64,
    ) -> Result<Self, OperatorStoreError> {
        if max_rows == 0 || max_rows > 100_000 {
            return Err(OperatorStoreError(
                "policy receipt capacity must be between 1 and 100000".into(),
            ));
        }
        let pool = OperatorPool::connect_with(options, 2)
            .await
            .map_err(|e| OperatorStoreError(e.to_string()))?;
        init_ddl(pool.pool(), DDL).await.map_err(|e| OperatorStoreError(e.to_string()))?;
        Ok(Self { pool, max_rows })
    }

    /// Bounded metadata-only inventory; failures must not look like an empty audit log.
    pub fn summary(&self) -> Result<serde_json::Value, OperatorStoreError> {
        self.block(async {
            let used: i64 = sqlx::query_scalar("SELECT count(*) FROM policy_receipts")
                .fetch_one(self.pool.pool())
                .await?;
            let used = used as u64;
            Ok(serde_json::json!({"used": used, "capacity": self.max_rows,
                "remaining": self.max_rows.saturating_sub(used),
                "full": used >= self.max_rows}))
        })
    }

    /// Record one admission receipt and return its id. Capacity is mandatory: a full
    /// store refuses (never truncates, never fails open).
    pub fn record(
        &self,
        identity: &Identity,
        key_id: &str,
        decision: &Decision,
    ) -> Result<String, OperatorStoreError> {
        let json = serde_json::to_string(decision)
            .map_err(|_| OperatorStoreError("invalid decision".into()))?;
        if json.len() > 8192
            || key_id.len() > 256
            || identity.issuer.as_ref().is_some_and(|s| s.len() > 2048)
            || identity.subject.as_ref().is_some_and(|s| s.len() > 256)
        {
            return Err(OperatorStoreError("invalid policy receipt".into()));
        }
        self.block(self.record_async(identity, key_id, json))
    }

    async fn record_async(
        &self,
        identity: &Identity,
        key_id: &str,
        json: String,
    ) -> Result<String, OperatorStoreError> {
        let mut tx = self.pool.pool().begin().await?;
        // SQLite's `BEGIN IMMEDIATE` capacity check: the count-then-insert sequence
        // serializes on one advisory lock so concurrent writers cannot overshoot the
        // mandatory bound.
        super::store_lock(&mut tx, "policy_receipts").await?;
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM policy_receipts")
            .fetch_one(&mut *tx)
            .await?;
        if count as u64 >= self.max_rows {
            return Err(OperatorStoreError(
                "policy receipt capacity exceeded".into(),
            ));
        }
        // The id and created_at are SQL defaults, exactly like the SQLite twin: the id is
        // 32 lowercase hex chars, created_at the pinned UTC millisecond clock text.
        let id: String = sqlx::query_scalar(&format!(
            "INSERT INTO policy_receipts (id, created_at, issuer, subject_id, virtual_key_id, \
             decision_json) \
             VALUES ({ID_DEFAULT_SQL}, {CLOCK_DEFAULT_SQL}, $1, $2, $3, $4) RETURNING id"
        ))
        .bind(identity.issuer.as_deref())
        .bind(identity.subject.as_deref())
        .bind(key_id)
        .bind(&json)
        // Unique filler for the md5 id entropy (bound, not concatenated into SQL).
        .bind(std::process::id().to_string())
        .fetch_one(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(id)
    }

    fn block<T>(&self, fut: impl std::future::Future<Output = T>) -> T {
        self.pool.block(fut)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sandhi_core::policy::Disposition;

    fn decision() -> Decision {
        Decision {
            revision: 1,
            disposition: Disposition::Quarantine,
            matched_rules: vec!["test".into()],
            reason: "evaluated".into(),
            elapsed_us: 1,
        }
    }

    fn identity() -> Identity {
        Identity {
            subject: Some("alice".into()),
            ..Default::default()
        }
    }

    fn pg_policy(tag: &str, max_rows: u64) -> (String, PostgresPolicyAuditStore) {
        super::super::pg_test::connect_in_schema(tag, |options| async move {
            PostgresPolicyAuditStore::connect_with(options, max_rows)
                .await
                .map_err(|e| sqlx::Error::Configuration(e.to_string().into()))
        })
    }

    fn drop_schema(schema: &str) {
        super::super::pg_test::drop_schema_best_effort(schema)
    }

    // ── Non-DB dialect tests (run wherever the `postgres` feature compiles). ──

    #[test]
    fn dialect_pins_the_clock_and_id_defaults() {
        // Byte-identical UTC text with 3-digit milliseconds (SQLite's `%f` shape).
        assert!(CLOCK_DEFAULT_SQL.contains("YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\""));
        assert!(CLOCK_DEFAULT_SQL.contains("AT TIME ZONE 'UTC'"));
        assert!(!CLOCK_DEFAULT_SQL.contains("strftime"));
        // The id default renders the same 32-lowercase-hex shape as
        // lower(hex(randomblob(16))) without a pgcrypto dependency.
        assert!(ID_DEFAULT_SQL.starts_with("md5("));
        assert!(DDL[0].contains("id text PRIMARY KEY"));
        assert!(DDL[0].contains("decision_json text NOT NULL"));
    }

    // ── Live-Postgres tests: SANDHI_TEST_PG_DSN, per-test schema isolation. ──

    #[test]
    #[ignore = "requires SANDHI_TEST_PG_DSN"]
    fn receipts_record_with_the_pinned_shapes_and_capacity_is_mandatory() {
        let (schema, store) = pg_policy("policy-capacity", 1);
        let id = store.record(&identity(), "public-id", &decision()).unwrap();
        assert_eq!(id.len(), 32, "the id is the 32-hex randomblob shape: {id}");
        assert!(
            id.bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()),
            "lowercase hex: {id}"
        );

        assert_eq!(store.summary().unwrap()["used"], 1);
        assert_eq!(store.summary().unwrap()["full"], serde_json::json!(true));
        // Capacity is mandatory: the second receipt is refused, never truncated.
        assert!(store.record(&identity(), "public-id", &decision()).is_err());
        drop_schema(&schema);
    }

    #[test]
    #[ignore = "requires SANDHI_TEST_PG_DSN"]
    fn receipts_survive_restart_and_carry_the_decision_identity() {
        let (schema, store) = pg_policy("policy-restart", 10);
        store.record(&identity(), "public-id", &decision()).unwrap();
        drop(store);

        let reopened =
            super::super::pg_test::connect_in_schema("policy-restart", |options| async move {
                PostgresPolicyAuditStore::connect_with(options, 10)
                    .await
                    .map_err(|e| sqlx::Error::Configuration(e.to_string().into()))
            })
            .1;
        let summary = reopened.summary().unwrap();
        assert_eq!(summary["used"], 1);
        assert_eq!(summary["capacity"], serde_json::json!(10));
        assert_eq!(summary["remaining"], serde_json::json!(9));
        assert_eq!(
            summary["full"],
            serde_json::json!(false),
            "an unavailable audit log must not look like a full one"
        );
        drop_schema(&schema);
    }

    #[test]
    #[ignore = "requires SANDHI_TEST_PG_DSN"]
    fn clock_default_renders_sqlite_strftime_shape() {
        let (schema, store) = pg_policy("policy-clock", 10);
        store.record(&identity(), "public-id", &decision()).unwrap();
        store.block(async {
            let created_at: String = sqlx::query_scalar("SELECT created_at FROM policy_receipts")
                .fetch_one(store.pool.pool())
                .await
                .unwrap();
            // strftime('%Y-%m-%dT%H:%M:%fZ','now') shape: 2026-…T…:…:….<3 digits>Z
            let (date, rest) = created_at.split_once('T').expect("T separator");
            assert_eq!(date.len(), 10);
            let (time, suffix) = rest.split_once('Z').expect("Z suffix");
            assert!(suffix.is_empty());
            let (hms, millis) = time.split_once('.').expect("millis separator");
            assert_eq!(hms.len(), 8);
            assert_eq!(millis.len(), 3, "SQLite %f renders 3-digit milliseconds");
        });
        drop_schema(&schema);
    }
}
