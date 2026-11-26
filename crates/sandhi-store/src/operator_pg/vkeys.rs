//! Postgres virtual-key store — the PG port of [`crate::vkeys::VirtualKeyStore`]
//! (TD-0003 P1, component 2). Only a **hash** of the presented secret is stored
//! (lookup), never the plaintext, exactly like the SQLite twin. Errors render through
//! [`crate::OperatorStoreError`].
use super::{init_ddl, now_rfc3339, random_hex, OperatorPool};
use crate::{hash_secret, OperatorStoreError};

/// A persisted virtual-key record (masked — the secret is never present, only its hash).
/// The same record shape as [`crate::vkeys::VirtualKeyRecord`].
pub use crate::vkeys::{MintRequest, MintedKey, VirtualKeyRecord};

/// The one SELECT projection shared by every record read, so the column order cannot
/// drift between `find_by_secret` / `find_by_id` / `list`.
const RECORD_COLUMNS: &str = "id, subject_id, group_id, upstream_ref, models, budget_scope, \
     model_budget_scope, expires_at, rate_limit_per_min, secret_hash, created_at, revoked_at";

/// Idempotent schema init, mirroring the SQLite DDL column-for-column. **Born upgraded:**
/// `model_budget_scope` is in the CREATE TABLE (the SQLite twin probes and ALTERs it in
/// for pre-existing databases; a Postgres store has no legacy databases — recorded
/// decision, mirroring the ledger core). The redundant `idx_vkeys_hash` the SQLite twin
/// drops on init was never born here: `secret_hash NOT NULL UNIQUE` maintains the only
/// lookup index.
const DDL: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS virtual_keys (
        id                 text PRIMARY KEY,
        subject_id         text,
        group_id           text,
        upstream_ref       text NOT NULL,
        models             text,
        budget_scope       text,
        model_budget_scope text,
        expires_at         text,
        rate_limit_per_min bigint,
        secret_hash        text NOT NULL UNIQUE,
        created_at         text NOT NULL,
        revoked_at         text
    )",
    "CREATE TABLE IF NOT EXISTS oidc_key_delegations (
        key_id        text PRIMARY KEY REFERENCES virtual_keys(id),
        identity_json text NOT NULL
    )",
];

/// A Postgres-backed virtual-key store.
pub struct PostgresVirtualKeyStore {
    pool: OperatorPool,
}

impl PostgresVirtualKeyStore {
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

    /// Mint a new virtual key. The plaintext secret is generated, returned once, and only
    /// its SHA-256 hash is persisted.
    pub fn mint(&self, req: MintRequest) -> Result<MintedKey, OperatorStoreError> {
        self.mint_inner(req, None)
    }

    /// Atomically bind a new credential to authenticated OIDC delegation metadata.
    pub fn mint_delegated(
        &self,
        req: MintRequest,
        identity_json: &str,
    ) -> Result<MintedKey, OperatorStoreError> {
        self.mint_inner(req, Some(identity_json))
    }

    /// The delegation identity bound to an active key, if any.
    pub fn delegation(&self, id: &str) -> Result<Option<String>, OperatorStoreError> {
        self.block(async {
            let identity: Option<String> = sqlx::query_scalar(
                "SELECT d.identity_json FROM oidc_key_delegations d \
                 JOIN virtual_keys k ON k.id = d.key_id \
                 WHERE d.key_id = $1 AND k.revoked_at IS NULL",
            )
            .bind(id)
            .fetch_optional(self.pool.pool())
            .await?;
            Ok(identity)
        })
    }

    fn mint_inner(
        &self,
        req: MintRequest,
        delegation: Option<&str>,
    ) -> Result<MintedKey, OperatorStoreError> {
        self.block(self.mint_inner_async(req, delegation))
    }

    async fn mint_inner_async(
        &self,
        req: MintRequest,
        delegation: Option<&str>,
    ) -> Result<MintedKey, OperatorStoreError> {
        let secret = generate_secret();
        let hash = hash_secret(&secret);
        let id = generate_id();
        let models_csv = if req.models.is_empty() {
            None
        } else {
            Some(req.models.join(","))
        };
        let created_at = now_rfc3339();
        let record = VirtualKeyRecord {
            id: id.clone(),
            subject_id: req.subject_id,
            group_id: req.group_id,
            upstream_ref: req.upstream_ref,
            models: models_csv,
            budget_scope: req.budget_scope,
            model_budget_scope: req.model_budget_scope,
            expires_at: req.expires_at,
            rate_limit_per_min: req.rate_limit_per_min,
            secret_hash: hash,
            created_at,
            revoked_at: None,
        };
        let mut tx = self.pool.pool().begin().await?;
        sqlx::query(
            "INSERT INTO virtual_keys \
             (id, subject_id, group_id, upstream_ref, models, budget_scope, \
              model_budget_scope, expires_at, rate_limit_per_min, secret_hash, created_at, \
              revoked_at) \
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,NULL)",
        )
        .bind(&record.id)
        .bind(&record.subject_id)
        .bind(&record.group_id)
        .bind(&record.upstream_ref)
        .bind(&record.models)
        .bind(&record.budget_scope)
        .bind(&record.model_budget_scope)
        .bind(&record.expires_at)
        .bind(record.rate_limit_per_min.map(i64::from))
        .bind(&record.secret_hash)
        .bind(&record.created_at)
        .execute(&mut *tx)
        .await?;
        if let Some(identity) = delegation {
            sqlx::query("INSERT INTO oidc_key_delegations(key_id, identity_json) VALUES ($1,$2)")
                .bind(&record.id)
                .bind(identity)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(MintedKey { record, secret })
    }

    /// Look up a record by hashing the presented secret. Excludes revoked keys.
    pub fn find_by_secret(
        &self,
        presented_secret: &str,
    ) -> Result<Option<VirtualKeyRecord>, OperatorStoreError> {
        let hash = hash_secret(presented_secret);
        self.find_by_hash(&hash)
    }

    /// Look up a record by its stored secret hash. Excludes revoked keys.
    pub fn find_by_hash(&self, hash: &str) -> Result<Option<VirtualKeyRecord>, OperatorStoreError> {
        self.block(async {
            let row = sqlx::query_as::<_, VirtualKeyRow>(&format!(
                "SELECT {RECORD_COLUMNS} FROM virtual_keys \
                 WHERE secret_hash = $1 AND revoked_at IS NULL"
            ))
            .bind(hash)
            .fetch_optional(self.pool.pool())
            .await?;
            Ok(row.map(record_from_row))
        })
    }

    /// Find a record by its public id.
    pub fn find_by_id(&self, id: &str) -> Result<Option<VirtualKeyRecord>, OperatorStoreError> {
        self.block(async {
            let row = sqlx::query_as::<_, VirtualKeyRow>(&format!(
                "SELECT {RECORD_COLUMNS} FROM virtual_keys WHERE id = $1"
            ))
            .bind(id)
            .fetch_optional(self.pool.pool())
            .await?;
            Ok(row.map(record_from_row))
        })
    }

    /// All records (masked), newest first.
    pub fn list(&self) -> Result<Vec<VirtualKeyRecord>, OperatorStoreError> {
        self.block(async {
            let rows = sqlx::query_as::<_, VirtualKeyRow>(&format!(
                "SELECT {RECORD_COLUMNS} FROM virtual_keys ORDER BY created_at DESC"
            ))
            .fetch_all(self.pool.pool())
            .await?;
            Ok(rows.into_iter().map(record_from_row).collect())
        })
    }

    /// Revoke (soft-delete) by public id. Returns whether it existed and was active.
    pub fn revoke(&self, id: &str) -> Result<bool, OperatorStoreError> {
        self.block(async {
            let changed = sqlx::query(
                "UPDATE virtual_keys SET revoked_at = $1 WHERE id = $2 AND revoked_at IS NULL",
            )
            .bind(now_rfc3339())
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

/// The raw 12-column row (sqlx `query_as` tuple) behind [`VirtualKeyRecord`].
type VirtualKeyRow = (
    String,
    Option<String>,
    Option<String>,
    String,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<i64>,
    String,
    String,
    Option<String>,
);

fn record_from_row(row: VirtualKeyRow) -> VirtualKeyRecord {
    VirtualKeyRecord {
        id: row.0,
        subject_id: row.1,
        group_id: row.2,
        upstream_ref: row.3,
        models: row.4,
        budget_scope: row.5,
        model_budget_scope: row.6,
        expires_at: row.7,
        rate_limit_per_min: row.8.map(|v| v as u32),
        secret_hash: row.9,
        created_at: row.10,
        revoked_at: row.11,
    }
}

/// 32 hex chars (128 bits) of OS CSPRNG entropy — unguessable `vk_…` secret.
fn generate_secret() -> String {
    format!("vk_{}", random_hex(16))
}

fn generate_id() -> String {
    // 48 bits of entropy is ample for a non-secret public id.
    format!("key_{}", random_hex(6))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(upstream: &str) -> MintRequest {
        MintRequest {
            subject_id: Some("alice".into()),
            group_id: Some("platform".into()),
            upstream_ref: upstream.into(),
            models: vec!["claude-x".into()],
            budget_scope: Some("group:platform".into()),
            model_budget_scope: None,
            expires_at: None,
            rate_limit_per_min: Some(60),
        }
    }

    fn pg_vkeys(tag: &str) -> (String, PostgresVirtualKeyStore) {
        super::super::pg_test::connect_in_schema(tag, PostgresVirtualKeyStore::connect_with)
    }

    fn drop_schema(schema: &str) {
        super::super::pg_test::drop_schema_best_effort(schema)
    }

    // ── Non-DB dialect tests (run wherever the `postgres` feature compiles). ──

    #[test]
    fn ddl_pins_the_vkey_tables_and_never_carries_the_redundant_hash_index() {
        let joined = DDL.join("\n");
        assert!(joined.contains("CREATE TABLE IF NOT EXISTS virtual_keys"));
        assert!(joined.contains("CREATE TABLE IF NOT EXISTS oidc_key_delegations"));
        assert!(
            joined.contains("secret_hash        text NOT NULL UNIQUE"),
            "the UNIQUE constraint is the only hash lookup index"
        );
        assert!(!joined.contains("idx_vkeys_hash"));
        assert!(joined.contains("REFERENCES virtual_keys(id)"));
        assert!(!joined.contains("INSERT OR"));
    }

    // ── Live-Postgres tests: SANDHI_TEST_PG_DSN, per-test schema isolation. ──

    #[test]
    #[ignore = "requires SANDHI_TEST_PG_DSN"]
    fn mint_then_present_resolves_with_scope_and_never_persists_the_plaintext() {
        let (schema, store) = pg_vkeys("vkeys-mint");
        let minted = store.mint(req("anthropic:default")).unwrap();
        assert!(minted.secret.starts_with("vk_"));
        assert!(minted.record.id.starts_with("key_"));

        let resolved = store
            .find_by_secret(&minted.secret)
            .unwrap()
            .expect("minted key resolves");
        assert_eq!(resolved.id, minted.record.id);
        assert_eq!(resolved.subject_id.as_deref(), Some("alice"));
        assert_eq!(resolved.upstream_ref, "anthropic:default");
        assert_eq!(resolved.budget_scope.as_deref(), Some("group:platform"));
        assert_eq!(resolved.rate_limit_per_min, Some(60));
        assert_eq!(resolved.model_list(), vec!["claude-x".to_string()]);

        let serialized = serde_json::to_string(&resolved).unwrap();
        assert!(
            !serialized.contains(&minted.secret),
            "plaintext secret must never be persisted/serialized"
        );
        assert!(serialized.contains(resolved.secret_hash.as_str()));

        assert!(store.find_by_secret("vk_nope").unwrap().is_none());
        drop_schema(&schema);
    }

    #[test]
    #[ignore = "requires SANDHI_TEST_PG_DSN"]
    fn delegated_model_budget_templates_survive_reopen() {
        let (schema, store) = pg_vkeys("vkeys-delegated");
        let minted = store
            .mint_delegated(
                MintRequest {
                    subject_id: None,
                    group_id: None,
                    upstream_ref: "route/coding".into(),
                    models: vec!["coding-fast".into()],
                    budget_scope: Some("coding/shared".into()),
                    model_budget_scope: Some("coding/{model}".into()),
                    expires_at: None,
                    rate_limit_per_min: None,
                },
                "{}",
            )
            .unwrap();
        assert_eq!(
            store.delegation(&minted.record.id).unwrap().as_deref(),
            Some("{}")
        );

        let reopened = super::super::pg_test::connect_in_schema(
            "vkeys-delegated",
            PostgresVirtualKeyStore::connect_with,
        )
        .1;
        let record = reopened
            .find_by_secret(&minted.secret)
            .unwrap()
            .expect("template key survives reopen");
        assert_eq!(record.budget_scope.as_deref(), Some("coding/shared"));
        assert_eq!(record.model_budget_scope.as_deref(), Some("coding/{model}"));
        drop_schema(&schema);
    }

    #[test]
    #[ignore = "requires SANDHI_TEST_PG_DSN"]
    fn revoked_key_no_longer_resolves_but_stays_auditable() {
        let (schema, store) = pg_vkeys("vkeys-revoke");
        let minted = store.mint(req("openai:default")).unwrap();
        assert!(store.revoke(&minted.record.id).unwrap());

        assert!(store.find_by_secret(&minted.secret).unwrap().is_none());
        assert!(store.delegation(&minted.record.id).unwrap().is_none());
        let rec = store.find_by_id(&minted.record.id).unwrap().unwrap();
        assert!(rec.revoked_at.is_some());
        assert!(
            !store.revoke(&minted.record.id).unwrap(),
            "re-revoke is a no-op"
        );

        let list = store.list().unwrap();
        assert_eq!(list.len(), 1, "revoked records stay listed for audit");
        drop_schema(&schema);
    }

    #[test]
    #[ignore = "requires SANDHI_TEST_PG_DSN"]
    fn list_returns_all_records_masked() {
        let (schema, store) = pg_vkeys("vkeys-list");
        let a = store.mint(req("anthropic:default")).unwrap();
        let _b = store.mint(req("openai:default")).unwrap();
        let list = store.list().unwrap();
        assert_eq!(list.len(), 2);
        let serialized = serde_json::to_string(&list).unwrap();
        assert!(!serialized.contains(&a.secret));
        drop_schema(&schema);
    }
}
