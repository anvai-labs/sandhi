//! Postgres alert-rule store — the PG port of [`crate::alerts::AlertStore`] (TD-0003 P2,
//! component 3). Same records, same methods, same ordering; only the storage dialect
//! differs. Errors render through [`crate::OperatorStoreError`] (the SQLite twin's
//! `rusqlite::Result` seam).

use super::{init_ddl, OperatorPool};
use crate::OperatorStoreError;

pub use crate::alerts::{AlertRuleRecord, CreateAlertRequest};

/// Idempotent schema init, mirroring the SQLite DDL column-for-column (types map
/// TEXT→text, INTEGER→integer — a percent fits easily; the timestamps stay RFC-3339
/// text so `ORDER BY created_at DESC` orders identically).
const DDL: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS alert_rules (
        id              text PRIMARY KEY,
        scope           text NOT NULL,
        threshold_pct   integer NOT NULL,
        channel         text NOT NULL,
        created_at      text NOT NULL,
        last_fired_at   text,
        acked_at        text
    )",
    "CREATE INDEX IF NOT EXISTS idx_alert_rules_scope ON alert_rules(scope)",
];

/// The raw 7-column row behind [`AlertRuleRecord`].
type AlertRow = (
    String,
    String,
    i32,
    String,
    String,
    Option<String>,
    Option<String>,
);

/// A Postgres-backed alert-rule store.
pub struct PostgresAlertStore {
    pool: OperatorPool,
}

impl PostgresAlertStore {
    /// Connect and initialize the schema (idempotent `CREATE IF NOT EXISTS`).
    ///
    /// Must be called on a `tokio` runtime; that runtime's handle is retained for every
    /// later blocking statement.
    /// Like [`Self::connect`] from pre-parsed connect options: the startup wiring sets
    /// `application_name` (and the test suite points `search_path` at a per-test schema).
    pub async fn connect_with(
        options: sqlx::postgres::PgConnectOptions,
    ) -> Result<Self, sqlx::Error> {
        let pool = OperatorPool::connect_with(options, 2).await?;
        init_ddl(pool.pool(), DDL).await?;
        Ok(Self { pool })
    }

    /// Open the store on `dsn` and initialize the schema (idempotent).
    pub async fn connect(dsn: &str) -> Result<Self, sqlx::Error> {
        let options: sqlx::postgres::PgConnectOptions = dsn
            .parse()
            .map_err(|e| sqlx::Error::Configuration(format!("invalid DSN: {e}").into()))?;
        Self::connect_with(options).await
    }

    /// Create a rule. Returns the persisted record.
    pub fn create(&self, req: CreateAlertRequest) -> Result<AlertRuleRecord, OperatorStoreError> {
        self.block(self.create_async(req))
    }

    async fn create_async(
        &self,
        req: CreateAlertRequest,
    ) -> Result<AlertRuleRecord, OperatorStoreError> {
        let id = generate_id();
        let created_at = now_rfc3339();
        let channel = req.channel.as_str();
        sqlx::query(
            "INSERT INTO alert_rules (id, scope, threshold_pct, channel, created_at, \
             last_fired_at, acked_at) VALUES ($1, $2, $3, $4, $5, NULL, NULL)",
        )
        .bind(&id)
        .bind(&req.scope)
        .bind(i32::from(req.threshold_pct))
        .bind(channel)
        .bind(&created_at)
        .execute(self.pool.pool())
        .await?;
        self.find_by_id_async(&id)
            .await?
            .ok_or(OperatorStoreError("inserted rule not found".into()))
    }

    /// All rules, newest first.
    pub fn list(&self) -> Result<Vec<AlertRuleRecord>, OperatorStoreError> {
        self.block(self.list_async(None))
    }

    /// Rules scoped to a single budget scope (used to rehydrate the registry selectively).
    pub fn list_by_scope(&self, scope: &str) -> Result<Vec<AlertRuleRecord>, OperatorStoreError> {
        self.block(self.list_async(Some(scope.to_string())))
    }

    async fn list_async(
        &self,
        scope: Option<String>,
    ) -> Result<Vec<AlertRuleRecord>, OperatorStoreError> {
        // The same SELECT both list paths serve; the optional scope filter is the only
        // difference, so the projection cannot drift between the two.
        let rows: Vec<AlertRow> = sqlx::query_as(
            "SELECT id, scope, threshold_pct, channel, created_at, last_fired_at, acked_at \
                 FROM alert_rules WHERE ($1::text IS NULL OR scope = $1) \
                 ORDER BY created_at DESC",
        )
        .bind(scope)
        .fetch_all(self.pool.pool())
        .await?;
        Ok(rows.into_iter().map(record_from_row).collect())
    }

    /// Find a rule by id.
    pub fn find_by_id(&self, id: &str) -> Result<Option<AlertRuleRecord>, OperatorStoreError> {
        self.block(self.find_by_id_async(id))
    }

    async fn find_by_id_async(
        &self,
        id: &str,
    ) -> Result<Option<AlertRuleRecord>, OperatorStoreError> {
        let row: Option<AlertRow> = sqlx::query_as(
            "SELECT id, scope, threshold_pct, channel, created_at, last_fired_at, acked_at \
                 FROM alert_rules WHERE id = $1",
        )
        .bind(id)
        .fetch_optional(self.pool.pool())
        .await?;
        Ok(row.map(record_from_row))
    }

    /// Record the most-recent fire timestamp for a rule (mirrors the in-memory registry dedup).
    pub fn mark_fired(&self, id: &str) -> Result<bool, OperatorStoreError> {
        self.block(async {
            let changed = sqlx::query("UPDATE alert_rules SET last_fired_at = $1 WHERE id = $2")
                .bind(now_rfc3339())
                .bind(id)
                .execute(self.pool.pool())
                .await?
                .rows_affected();
            Ok(changed > 0)
        })
    }

    /// Acknowledge a fired alert (clears it from the operator's attention queue).
    pub fn ack(&self, id: &str) -> Result<bool, OperatorStoreError> {
        self.block(async {
            let changed = sqlx::query(
                "UPDATE alert_rules SET acked_at = $1 WHERE id = $2 AND acked_at IS NULL",
            )
            .bind(now_rfc3339())
            .bind(id)
            .execute(self.pool.pool())
            .await?
            .rows_affected();
            Ok(changed > 0)
        })
    }

    /// Delete a rule.
    pub fn delete(&self, id: &str) -> Result<bool, OperatorStoreError> {
        self.block(async {
            let changed = sqlx::query("DELETE FROM alert_rules WHERE id = $1")
                .bind(id)
                .execute(self.pool.pool())
                .await?
                .rows_affected();
            Ok(changed > 0)
        })
    }

    fn block<T>(&self, fut: impl std::future::Future<Output = T>) -> T {
        self.pool.block(fut)
    }
}

fn record_from_row(
    (id, scope, threshold_pct, channel, created_at, last_fired_at, acked_at): (
        String,
        String,
        i32,
        String,
        String,
        Option<String>,
        Option<String>,
    ),
) -> AlertRuleRecord {
    AlertRuleRecord {
        id,
        scope,
        threshold_pct: threshold_pct as u8,
        channel,
        created_at,
        last_fired_at,
        acked_at,
    }
}

/// 48 bits of OS CSPRNG entropy (non-secret public id) — the same shape as the SQLite twin.
fn generate_id() -> String {
    format!("alert_{}", super::random_hex(6))
}

fn now_rfc3339() -> String {
    super::now_rfc3339()
}

#[cfg(test)]
mod tests {

    use super::*;
    use sandhi_core::AlertChannel;

    fn req(scope: &str, pct: u8) -> CreateAlertRequest {
        CreateAlertRequest {
            scope: scope.into(),
            threshold_pct: pct,
            channel: AlertChannel::Log,
        }
    }

    fn pg_alerts(tag: &str) -> (String, PostgresAlertStore) {
        super::super::pg_test::connect_in_schema(tag, PostgresAlertStore::connect_with)
    }

    fn drop_schema(schema: &str) {
        super::super::pg_test::drop_schema_best_effort(schema)
    }

    // ── Non-DB dialect tests (run wherever the `postgres` feature compiles). ──

    #[test]
    fn ddl_pins_the_alert_rules_shape() {
        let joined = DDL.join("\n");
        assert!(joined.contains("CREATE TABLE IF NOT EXISTS alert_rules"));
        assert!(joined.contains("threshold_pct   integer NOT NULL"));
        assert!(joined.contains("created_at      text NOT NULL"));
        assert!(joined.contains("last_fired_at   text"));
        assert!(joined.contains("idx_alert_rules_scope ON alert_rules(scope)"));
        assert!(!joined.contains("INSERT OR"));
    }

    // ── Live-Postgres tests: SANDHI_TEST_PG_DSN, per-test schema isolation. ──

    #[test]
    #[ignore = "requires SANDHI_TEST_PG_DSN"]
    fn create_list_find_ack_delete_round_trip() {
        let (schema, store) = pg_alerts("alerts-crud");
        let rec = store.create(req("group:platform", 80)).unwrap();
        assert!(rec.id.starts_with("alert_"));
        assert_eq!(rec.scope, "group:platform");
        assert_eq!(rec.threshold_pct, 80);
        assert_eq!(rec.channel_enum(), AlertChannel::Log);
        assert!(rec.last_fired_at.is_none());
        assert!(rec.acked_at.is_none());

        let list = store.list().unwrap();
        assert_eq!(list.len(), 1);

        let found = store.find_by_id(&rec.id).unwrap().unwrap();
        assert_eq!(found.scope, "group:platform");

        assert!(store.mark_fired(&rec.id).unwrap());
        assert!(store
            .find_by_id(&rec.id)
            .unwrap()
            .unwrap()
            .last_fired_at
            .is_some());
        assert!(store.ack(&rec.id).unwrap());
        assert!(store
            .find_by_id(&rec.id)
            .unwrap()
            .unwrap()
            .acked_at
            .is_some());
        assert!(!store.ack(&rec.id).unwrap(), "second ack is a no-op");

        assert!(store.delete(&rec.id).unwrap());
        assert!(store.find_by_id(&rec.id).unwrap().is_none());
        assert!(!store.delete(&rec.id).unwrap());
        drop_schema(&schema);
    }

    #[test]
    #[ignore = "requires SANDHI_TEST_PG_DSN"]
    fn list_by_scope_isolates_and_survives_reopen() {
        let (schema, store) = pg_alerts("alerts-scope");
        store.create(req("group:a", 80)).unwrap();
        store.create(req("group:b", 90)).unwrap();
        assert_eq!(store.list_by_scope("group:a").unwrap().len(), 1);
        assert_eq!(store.list_by_scope("group:b").unwrap().len(), 1);
        assert_eq!(store.list_by_scope("group:c").unwrap().len(), 0);

        let reopened = super::super::pg_test::connect_in_schema(
            "alerts-scope",
            PostgresAlertStore::connect_with,
        )
        .1;
        assert_eq!(reopened.list().unwrap().len(), 2, "rules survive restart");
        drop_schema(&schema);
    }

    #[test]
    #[ignore = "requires SANDHI_TEST_PG_DSN"]
    fn webhook_channel_round_trips_through_store() {
        let (schema, store) = pg_alerts("alerts-webhook");
        let rec = store
            .create(CreateAlertRequest {
                scope: "group:w".into(),
                threshold_pct: 50,
                channel: AlertChannel::Webhook {
                    url: "https://hooks.example/x".into(),
                },
            })
            .unwrap();
        let rule = rec.to_rule();
        match rule.channel {
            AlertChannel::Webhook { url } => assert_eq!(url, "https://hooks.example/x"),
            _ => panic!("expected webhook channel"),
        }
        drop_schema(&schema);
    }
}
