//! Postgres backend-definition store — the PG port of [`crate::backends::BackendStore`]
//! (bounded, non-secret backend definitions; credential bytes remain in the vault). The
//! definition types (`BackendDefinition`, `BackendEndpoint`, `BackendRecord`) are reused
//! verbatim from the SQLite module. The `backend_budget_namespace_immutable` SQLite
//! `RAISE(ABORT)` trigger becomes a plpgsql `RAISE EXCEPTION` trigger created in init,
//! and the namespace migration keeps the SQLite twin's probe/backfill/corrupt-check
//! order (via `information_schema`). Errors mirror [`crate::backends::BackendStoreError`]
//! variant-for-variant with identical `Display` strings — the proxy maps the variants to
//! HTTP statuses and renders the message.
use sqlx::{PgConnection, PgPool};

use crate::backends::{legacy_budget_scope, BackendDefinition, BackendRecord};
pub use crate::backends::{MAX_BACKENDS, MAX_DEFINITION_BYTES};
pub use sandhi_core::EndpointProfileV1 as BackendProfile;

/// The PG twin of [`crate::backends::BackendStoreError`]: identical variants and
/// identical `Display` strings; only the `Database` arm carries the sqlx driver error.
#[derive(Debug)]
pub enum BackendStoreError {
    Invalid(&'static str),
    Conflict,
    NotFound,
    Capacity,
    Corrupt,
    Database(sqlx::Error),
}

impl std::fmt::Display for BackendStoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid(message) => f.write_str(message),
            Self::Conflict => f.write_str("backend revision conflict"),
            Self::NotFound => f.write_str("backend not found"),
            Self::Capacity => f.write_str("backend registry capacity exceeded"),
            Self::Corrupt => f.write_str("backend registry contains invalid metadata"),
            Self::Database(_) => f.write_str("backend registry unavailable"),
        }
    }
}
impl std::error::Error for BackendStoreError {}
impl From<sqlx::Error> for BackendStoreError {
    fn from(error: sqlx::Error) -> Self {
        Self::Database(error)
    }
}

/// The shared validation helpers (`legacy_budget_scope`, `BackendDefinition::validate`)
/// return the SQLite module's error type; every variant they can produce converts
/// losslessly, and the unreachable rusqlite arm maps to `Corrupt` (fail closed).
impl From<crate::backends::BackendStoreError> for BackendStoreError {
    fn from(error: crate::backends::BackendStoreError) -> Self {
        match error {
            crate::backends::BackendStoreError::Invalid(message) => Self::Invalid(message),
            crate::backends::BackendStoreError::Conflict => Self::Conflict,
            crate::backends::BackendStoreError::NotFound => Self::NotFound,
            crate::backends::BackendStoreError::Capacity => Self::Capacity,
            _ => Self::Corrupt,
        }
    }
}

/// The base table (the SQLite twin's original column set); `upstream_budget_scope`
/// arrives through the same probe + backfill migration the SQLite twin runs.
const DDL: &[&str] = &["CREATE TABLE IF NOT EXISTS backends (
        id text PRIMARY KEY CHECK(octet_length(id) BETWEEN 1 AND 128),
        definition_json text NOT NULL CHECK(octet_length(definition_json) <= 65536),
        revision bigint NOT NULL CHECK(revision > 0),
        status text NOT NULL CHECK(status IN ('active','revoked')),
        created_at text NOT NULL, updated_at text NOT NULL
    )"];

/// The `RAISE(ABORT)`-equivalent trigger: a namespace change is refused with the SQLite
/// twin's exact message. `CREATE OR REPLACE FUNCTION` keeps init idempotent; PG has no
/// `CREATE TRIGGER IF NOT EXISTS`, so the trigger is dropped and recreated on every
/// connect.
const TRIGGER_SQL: &[&str] = &[
    // `IS DISTINCT FROM` is the Postgres spelling of the SQLite trigger's `IS NOT` guard
    // (null-safe inequality, so a NULL→value transition is refused exactly like the
    // SQLite twin).
    "CREATE OR REPLACE FUNCTION backend_budget_namespace_immutable_guard() RETURNS trigger \
     LANGUAGE plpgsql AS $f$ \
     BEGIN \
       IF OLD.upstream_budget_scope IS DISTINCT FROM NEW.upstream_budget_scope THEN \
         RAISE EXCEPTION 'backend budget namespace is immutable'; \
       END IF; \
       RETURN NEW; \
     END $f$;",
    "DROP TRIGGER IF EXISTS backend_budget_namespace_immutable ON backends",
    "CREATE TRIGGER backend_budget_namespace_immutable \
     BEFORE UPDATE OF upstream_budget_scope ON backends \
     FOR EACH ROW EXECUTE FUNCTION backend_budget_namespace_immutable_guard()",
];

/// The raw 7-column row (`id, definition_json, revision, status, created_at, updated_at,
/// upstream_budget_scope`).
type RawRecord = (String, String, i64, String, String, String, Option<String>);

/// A Postgres-backed backend-definition store.
pub struct PostgresBackendStore {
    pool: super::OperatorPool,
}

impl PostgresBackendStore {
    /// Connect and initialize the schema (idempotent `CREATE IF NOT EXISTS` + the
    /// namespace migration + the immutability trigger).
    ///
    /// Must be called on a `tokio` runtime; that runtime's handle is retained for every
    /// later blocking statement.
    /// Like [`Self::connect`] from pre-parsed connect options: the startup wiring sets
    /// `application_name` (and the test suite points `search_path` at a per-test schema).
    pub async fn connect_with(options: sqlx::postgres::PgConnectOptions) -> Result<Self, sqlx::Error> {
        let pool = super::OperatorPool::connect_with(options, 2).await?;
        Self::init(pool.pool()).await.map_err(|error| match error {
            BackendStoreError::Database(error) => error,
            other => sqlx::Error::Configuration(other.to_string().into()),
        })?;
        Ok(Self { pool })
    }

    /// Open the store on `dsn` and run the idempotent init (table, namespace migration,
    /// corrupt-binding check, immutability trigger).
    pub async fn connect(dsn: &str) -> Result<Self, BackendStoreError> {
        let pool = super::OperatorPool::connect(dsn, 2).await?;
        Self::init(pool.pool()).await?;
        Ok(Self { pool })
    }

    /// Idempotent init in the SQLite twin's exact order: table → namespace probe +
    /// backfill → corrupt-binding check → trigger.
    async fn init(pool: &PgPool) -> Result<(), BackendStoreError> {
        super::init_ddl(pool, DDL).await?;
        let mut tx = pool.begin().await?;
        serialize_writes(&mut tx).await?;
        if !super::column_exists(pool, "backends", "upstream_budget_scope").await? {
            // The probe checks on the pool while the migration writes inside the
            // transaction; no concurrent creator exists (single owner at startup), so
            // the ADD COLUMN runs first in the transaction.
            sqlx::query("ALTER TABLE backends ADD COLUMN upstream_budget_scope text")
                .execute(&mut *tx)
                .await?;
            let rows: Vec<(String, String)> = sqlx::query_as(
                "SELECT id, substr(definition_json,1,65537) FROM backends LIMIT 257",
            )
            .fetch_all(&mut *tx)
            .await?;
            if rows.len() > MAX_BACKENDS {
                return Err(BackendStoreError::Corrupt);
            }
            for (id, json) in rows {
                let definition: BackendDefinition =
                    serde_json::from_str(&json).map_err(|_| BackendStoreError::Corrupt)?;
                definition
                    .validate()
                    .map_err(|_| BackendStoreError::Corrupt)?;
                if definition.id != id {
                    return Err(BackendStoreError::Corrupt);
                }
                let scope = legacy_budget_scope(&definition)?;
                sqlx::query("UPDATE backends SET upstream_budget_scope=$2 WHERE id=$1")
                    .bind(&id)
                    .bind(scope)
                    .execute(&mut *tx)
                    .await?;
            }
        }
        // An existing column with missing/corrupt bindings is not another legacy
        // migration. Do not infer a replacement namespace from a possibly changed
        // definition. (`typeof`/`length(CAST(.. AS BLOB))` become NULL-ness + byte bound
        // on a typed column.)
        let bad: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM backends \
             WHERE upstream_budget_scope IS NULL OR octet_length(upstream_budget_scope)>137)",
        )
        .fetch_one(&mut *tx)
        .await?;
        if bad {
            return Err(BackendStoreError::Corrupt);
        }
        for statement in TRIGGER_SQL {
            sqlx::query(statement).execute(&mut *tx).await?;
        }
        tx.commit().await?;
        Ok(())
    }

    pub fn get(&self, id: &str) -> Result<Option<BackendRecord>, BackendStoreError> {
        self.block(async {
            crate::vault::validate_reference("backend", id)
                .map_err(|_| BackendStoreError::Invalid("invalid backend id"))?;
            get_record(self.pool.pool(), id).await
        })
    }

    pub fn list(&self) -> Result<Vec<BackendRecord>, BackendStoreError> {
        self.block(async {
            let rows: Vec<RawRecord> = sqlx::query_as(
                "SELECT id, substr(definition_json,1,65537), revision, status, created_at, \
                        updated_at, upstream_budget_scope \
                 FROM backends ORDER BY id LIMIT 257",
            )
            .fetch_all(self.pool.pool())
            .await?;
            if rows.len() > MAX_BACKENDS {
                return Err(BackendStoreError::Capacity);
            }
            rows.into_iter().map(decode_record).collect()
        })
    }

    /// Create is idempotent only for an identical active definition. Changes and
    /// reactivation require the current revision, checked under the store's write lock.
    pub fn put(
        &self,
        definition: &BackendDefinition,
        expected_revision: Option<u64>,
    ) -> Result<BackendRecord, BackendStoreError> {
        definition.validate().map_err(BackendStoreError::Invalid)?;
        self.block(self.put_async(definition, expected_revision))
    }

    async fn put_async(
        &self,
        definition: &BackendDefinition,
        expected_revision: Option<u64>,
    ) -> Result<BackendRecord, BackendStoreError> {
        let mut tx = self.pool.pool().begin().await?;
        serialize_writes(&mut tx).await?;
        let previous = get_record(&mut *tx, &definition.id).await?;
        if let Some(previous) = &previous {
            if expected_revision.is_some_and(|revision| revision != previous.revision) {
                return Err(BackendStoreError::Conflict);
            }
            if previous.definition == *definition && previous.status == "active" {
                return Ok(previous.clone());
            }
            if expected_revision.is_none() {
                return Err(BackendStoreError::Conflict);
            }
        } else {
            if expected_revision.is_some() {
                return Err(BackendStoreError::Conflict);
            }
            let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM backends")
                .fetch_one(&mut *tx)
                .await?;
            if count >= MAX_BACKENDS as i64 {
                return Err(BackendStoreError::Capacity);
            }
        }
        let revision = previous
            .as_ref()
            .map_or(Some(1), |record| record.revision.checked_add(1))
            .filter(|revision| *revision <= i64::MAX as u64)
            .ok_or(BackendStoreError::Corrupt)?;
        let now = time::OffsetDateTime::now_utc()
            .format(&time::format_description::well_known::Rfc3339)
            .map_err(|_| BackendStoreError::Corrupt)?;
        let upstream_budget_scope = previous.as_ref().map_or_else(
            || legacy_budget_scope(definition),
            |record| Ok(record.upstream_budget_scope.clone()),
        )?;
        let created_at = previous.map_or_else(|| now.clone(), |record| record.created_at);
        let json = serde_json::to_string(definition)
            .map_err(|_| BackendStoreError::Invalid("invalid backend definition"))?;
        sqlx::query(
            "INSERT INTO backends \
             (id, definition_json, revision, status, created_at, updated_at, \
              upstream_budget_scope) \
             VALUES ($1,$2,$3,'active',$4,$5,$6) ON CONFLICT(id) DO UPDATE SET \
             definition_json=excluded.definition_json, revision=excluded.revision, \
             status='active', updated_at=excluded.updated_at",
        )
        .bind(&definition.id)
        .bind(&json)
        .bind(revision as i64)
        .bind(&created_at)
        .bind(&now)
        .bind(&upstream_budget_scope)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(BackendRecord {
            upstream_budget_scope,
            definition: definition.clone(),
            revision,
            status: "active".into(),
            created_at,
            updated_at: now,
        })
    }

    pub fn revoke(
        &self,
        id: &str,
        expected_revision: u64,
    ) -> Result<BackendRecord, BackendStoreError> {
        self.block(self.revoke_async(id, expected_revision))
    }

    async fn revoke_async(
        &self,
        id: &str,
        expected_revision: u64,
    ) -> Result<BackendRecord, BackendStoreError> {
        let mut tx = self.pool.pool().begin().await?;
        serialize_writes(&mut tx).await?;
        let mut record = get_record(&mut *tx, id)
            .await?
            .ok_or(BackendStoreError::NotFound)?;
        if record.revision != expected_revision {
            return Err(BackendStoreError::Conflict);
        }
        if record.status == "revoked" {
            return Ok(record);
        }
        record.revision = record
            .revision
            .checked_add(1)
            .filter(|n| *n <= i64::MAX as u64)
            .ok_or(BackendStoreError::Corrupt)?;
        record.status = "revoked".into();
        record.updated_at = time::OffsetDateTime::now_utc()
            .format(&time::format_description::well_known::Rfc3339)
            .map_err(|_| BackendStoreError::Corrupt)?;
        sqlx::query("UPDATE backends SET status='revoked',revision=$2,updated_at=$3 WHERE id=$1")
            .bind(id)
            .bind(record.revision as i64)
            .bind(&record.updated_at)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(record)
    }

    fn block<T>(&self, fut: impl std::future::Future<Output = T>) -> T {
        self.pool.block(fut)
    }
}

/// SQLite's `BEGIN IMMEDIATE` for the backends table: one transactional advisory lock
/// over the read-modify-write sequences and the init migration.
async fn serialize_writes(tx: &mut PgConnection) -> Result<(), BackendStoreError> {
    super::store_lock(tx, "backends").await?;
    Ok(())
}

fn valid_budget_scope(scope: &str) -> bool {
    scope
        .strip_prefix("upstream:")
        .is_some_and(|name| crate::vault::validate_reference(name, "namespace").is_ok())
}

fn decode_record(
    (id, json, revision, status, created_at, updated_at, scope): RawRecord,
) -> Result<BackendRecord, BackendStoreError> {
    if json.len() > MAX_DEFINITION_BYTES
        || revision <= 0
        || !matches!(status.as_str(), "active" | "revoked")
    {
        return Err(BackendStoreError::Corrupt);
    }
    let definition: BackendDefinition =
        serde_json::from_str(&json).map_err(|_| BackendStoreError::Corrupt)?;
    definition
        .validate()
        .map_err(|_| BackendStoreError::Corrupt)?;
    if definition.id != id {
        return Err(BackendStoreError::Corrupt);
    }
    let upstream_budget_scope = scope
        .filter(|scope| valid_budget_scope(scope))
        .ok_or(BackendStoreError::Corrupt)?;
    Ok(BackendRecord {
        upstream_budget_scope,
        definition,
        revision: revision as u64,
        status,
        created_at,
        updated_at,
    })
}

/// Record reads run both on the pool (`get`) and inside a transaction
/// (`put`/`revoke`/init migration), so the helper is executor-generic.
async fn get_record<'e, E>(
    executor: E,
    id: &str,
) -> Result<Option<BackendRecord>, BackendStoreError>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    let row: Option<RawRecord> = sqlx::query_as(
        "SELECT id, substr(definition_json,1,65537), revision, status, created_at, updated_at, \
                upstream_budget_scope \
         FROM backends WHERE id=$1",
    )
    .bind(id)
    .fetch_optional(executor)
    .await?;
    row.map(decode_record).transpose()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn definition(
        id: &str,
        provider: &str,
        family: sandhi_core::EndpointFamilyV1,
    ) -> BackendDefinition {
        serde_json::from_value(serde_json::json!({
            "id": id,
            "provider": provider,
            "credential_ref": "vendor:fixture",
            "endpoint": {"endpoint_family": family},
            "models": [{"id": "fixture", "thinking": {"supported": false}}]
        }))
        .unwrap()
    }

    fn pg_backends(tag: &str) -> (String, PostgresBackendStore) {
        super::super::pg_test::connect_in_schema(tag, PostgresBackendStore::connect_with)
    }

    fn drop_schema(schema: &str) {
        super::super::pg_test::drop_schema_best_effort(schema)
    }

    // ── Non-DB dialect tests (run wherever the `postgres` feature compiles). ──

    #[test]
    fn ddl_pins_the_backends_table_and_the_immutability_trigger() {
        let joined = DDL.join("\n");
        assert!(joined.contains("CREATE TABLE IF NOT EXISTS backends"));
        assert!(joined.contains("octet_length(definition_json) <= 65536"));
        assert!(joined.contains("revision bigint NOT NULL CHECK(revision > 0)"));
        assert!(joined.contains("status IN ('active','revoked')"));
        // The RAISE(ABORT) dialect: a plpgsql function + trigger with the SQLite twin's
        // exact message.
        assert!(TRIGGER_SQL[0].contains("RAISE EXCEPTION 'backend budget namespace is immutable'"));
        assert!(
            TRIGGER_SQL[1].contains("DROP TRIGGER IF EXISTS backend_budget_namespace_immutable")
        );
        assert!(TRIGGER_SQL[2].contains(
            "BEFORE UPDATE OF upstream_budget_scope ON backends \
             FOR EACH ROW EXECUTE FUNCTION backend_budget_namespace_immutable_guard()"
        ));
        assert!(!joined.contains("INSERT OR"));
    }

    // ── Live-Postgres tests: SANDHI_TEST_PG_DSN, per-test schema isolation. ──

    #[test]
    #[ignore = "requires SANDHI_TEST_PG_DSN"]
    fn put_get_revoke_and_revision_conflicts_survive_reopen() {
        let (schema, store) = pg_backends("backends-crud");
        assert!(store.get("source").unwrap().is_none());
        let record = store
            .put(
                &definition(
                    "source",
                    "vendor-one",
                    sandhi_core::EndpointFamilyV1::OpenaiChatCompletions,
                ),
                None,
            )
            .unwrap();
        assert_eq!(record.revision, 1);
        assert_eq!(record.status, "active");
        assert_eq!(record.upstream_budget_scope, "upstream:vendor-one");

        // Identical definition is idempotent at the same revision.
        let same = store
            .put(
                &definition(
                    "source",
                    "vendor-one",
                    sandhi_core::EndpointFamilyV1::OpenaiChatCompletions,
                ),
                Some(1),
            )
            .unwrap();
        assert_eq!(same.revision, 1);

        // A change without the current revision conflicts.
        assert!(matches!(
            store.put(
                &definition(
                    "source",
                    "vendor-two",
                    sandhi_core::EndpointFamilyV1::OpenaiChatCompletions
                ),
                None
            ),
            Err(BackendStoreError::Conflict)
        ));
        assert!(matches!(
            store.revoke("source", 2),
            Err(BackendStoreError::Conflict)
        ));

        let updated = store
            .put(
                &definition(
                    "source",
                    "vendor-two",
                    sandhi_core::EndpointFamilyV1::OpenaiChatCompletions,
                ),
                Some(1),
            )
            .unwrap();
        assert_eq!(updated.revision, 2);
        assert_eq!(
            updated.upstream_budget_scope, "upstream:vendor-one",
            "the namespace never follows the provider label"
        );

        let revoked = store.revoke("source", 2).unwrap();
        assert_eq!(revoked.status, "revoked");
        assert_eq!(revoked.revision, 3);
        assert!(store.get("nope").unwrap().is_none());
        assert!(matches!(
            store.revoke("nope", 1),
            Err(BackendStoreError::NotFound)
        ));

        let reopened = super::super::pg_test::connect_in_schema(
            "backends-crud",
            PostgresBackendStore::connect_with,
        )
        .1;
        let read = reopened.get("source").unwrap().unwrap();
        assert_eq!(read.revision, 3);
        assert_eq!(read.status, "revoked");
        assert_eq!(read.upstream_budget_scope, "upstream:vendor-one");
        drop_schema(&schema);
    }

    /// THE namespace-immutability invariant: the RAISE-EXCEPTION trigger refuses any
    /// UPDATE that changes `upstream_budget_scope` (the PG dialect of the SQLite
    /// RAISE(ABORT) test, which asserts `.is_err()` on a raw UPDATE).
    #[test]
    #[ignore = "requires SANDHI_TEST_PG_DSN"]
    fn namespace_immutability_trigger_rejects_the_update() {
        let (schema, store) = pg_backends("backends-trigger");
        store
            .put(
                &definition(
                    "source",
                    "vendor-one",
                    sandhi_core::EndpointFamilyV1::AnthropicMessages,
                ),
                None,
            )
            .unwrap();
        assert_eq!(
            store.get("source").unwrap().unwrap().upstream_budget_scope,
            "upstream:anthropic"
        );

        // A raw UPDATE renaming the namespace must be rejected by the trigger.
        store.block(async {
            let error =
                sqlx::query("UPDATE backends SET upstream_budget_scope='upstream:vendor-two'")
                    .execute(store.pool.pool())
                    .await
                    .expect_err("the immutability trigger must refuse the rename");
            let message = error.to_string();
            assert!(
                message.contains("backend budget namespace is immutable"),
                "the trigger message must surface verbatim: {message}"
            );
        });

        // The namespace is unchanged after the refused update.
        assert_eq!(
            store.get("source").unwrap().unwrap().upstream_budget_scope,
            "upstream:anthropic"
        );
        // Ordinary updates (definition/revision/status) still pass the trigger.
        let changed = store
            .put(
                &definition(
                    "source",
                    "vendor-two",
                    sandhi_core::EndpointFamilyV1::OpenaiChatCompletions,
                ),
                Some(1),
            )
            .unwrap();
        assert_eq!(changed.revision, 2);
        assert_eq!(changed.upstream_budget_scope, "upstream:anthropic");
        drop_schema(&schema);
    }

    #[test]
    #[ignore = "requires SANDHI_TEST_PG_DSN"]
    fn list_is_ordered_and_capacity_bounded() {
        let (schema, store) = pg_backends("backends-list");
        for (id, provider) in [("a", "vendor-a"), ("b", "vendor-b")] {
            store
                .put(
                    &definition(
                        id,
                        provider,
                        sandhi_core::EndpointFamilyV1::OpenaiChatCompletions,
                    ),
                    None,
                )
                .unwrap();
        }
        let list = store.list().unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].definition.id, "a");
        assert_eq!(list[1].definition.id, "b");
        drop_schema(&schema);
    }
}
