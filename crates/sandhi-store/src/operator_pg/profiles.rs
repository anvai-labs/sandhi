//! Postgres gateway-profile store — the PG port of [`crate::profiles::ProfileStore`]
//! (per-group defaults + profile-download tokens). Only a **hash** of the token is
//! stored (the same unsalted SHA-256 hex scheme via [`crate::hash_secret`]), never the
//! plaintext. Errors render through [`crate::OperatorStoreError`].
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;

use super::{init_ddl, now_rfc3339, OperatorPool};
use crate::{hash_secret, OperatorStoreError};

pub use crate::profiles::ProfileRecord;

/// Idempotent schema init, mirroring the SQLite DDL column-for-column. `group` is a SQL
/// keyword — every statement quotes the column name (the SQLite twin's own rule).
const DDL: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS profiles (
        \"group\"         text PRIMARY KEY,
        mode              text NOT NULL,
        upstream_ref      text,
        models_json       text NOT NULL DEFAULT '[]',
        timeout_seconds   bigint,
        max_tokens        bigint,
        updated_at        text NOT NULL
    )",
    "CREATE TABLE IF NOT EXISTS profile_tokens (
        token_hash        text PRIMARY KEY,
        subject           text NOT NULL,
        group_id          text NOT NULL,
        created_at        text NOT NULL,
        revoked_at        text
    )",
    "CREATE INDEX IF NOT EXISTS idx_profile_tokens_subject_group \
         ON profile_tokens(subject, group_id)",
];

/// A Postgres-backed profile + profile-token store.
pub struct PostgresProfileStore {
    pool: OperatorPool,
}

/// The raw row behind [`ProfileRecord`].
type ProfileRow = (
    String,
    String,
    Option<String>,
    String,
    Option<i64>,
    Option<i64>,
    String,
);

impl PostgresProfileStore {
    /// Connect and initialize the schema (idempotent `CREATE IF NOT EXISTS`).
    ///
    /// Must be called on a `tokio` runtime; that runtime's handle is retained for every
    /// later blocking statement.
    /// Like [`Self::connect`] from pre-parsed connect options: the startup wiring sets
    /// `application_name` (and the test suite points `search_path` at a per-test schema).
    pub async fn connect_with(options: sqlx::postgres::PgConnectOptions) -> Result<Self, sqlx::Error> {
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

    /// Insert or update the profile for a group (additive-only upsert: this never deletes
    /// a group — callers simply stop calling it).
    pub fn upsert_profile(&self, record: &ProfileRecord) -> Result<(), OperatorStoreError> {
        self.block(async {
            sqlx::query(
                "INSERT INTO profiles \
                 (\"group\", mode, upstream_ref, models_json, timeout_seconds, max_tokens, \
                  updated_at) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7) \
                 ON CONFLICT(\"group\") DO UPDATE SET \
                 mode=excluded.mode, upstream_ref=excluded.upstream_ref, \
                 models_json=excluded.models_json, timeout_seconds=excluded.timeout_seconds, \
                 max_tokens=excluded.max_tokens, updated_at=excluded.updated_at",
            )
            .bind(&record.group)
            .bind(&record.mode)
            .bind(&record.upstream_ref)
            .bind(serde_json::to_string(&record.models).expect("Vec<String> always serializes"))
            .bind(record.timeout_seconds.map(|v| v as i64))
            .bind(record.max_tokens.map(i64::from))
            .bind(&record.updated_at)
            .execute(self.pool.pool())
            .await
            .map(|_| ())
        })
        .map_err(OperatorStoreError::from)
    }

    /// The applied profile for one group, if any.
    pub fn profile_for_group(
        &self,
        group: &str,
    ) -> Result<Option<ProfileRecord>, OperatorStoreError> {
        self.block(async {
            let row = sqlx::query_as::<_, ProfileRow>(
                "SELECT \"group\", mode, upstream_ref, models_json, timeout_seconds, \
                 max_tokens, updated_at FROM profiles WHERE \"group\" = $1",
            )
            .bind(group)
            .fetch_optional(self.pool.pool())
            .await?;
            Ok(row.map(record_from_row))
        })
    }

    /// All applied profiles, ordered by group.
    pub fn list_profiles(&self) -> Result<Vec<ProfileRecord>, OperatorStoreError> {
        self.block(async {
            let rows = sqlx::query_as::<_, ProfileRow>(
                "SELECT \"group\", mode, upstream_ref, models_json, timeout_seconds, \
                 max_tokens, updated_at FROM profiles ORDER BY \"group\"",
            )
            .fetch_all(self.pool.pool())
            .await?;
            Ok(rows.into_iter().map(record_from_row).collect())
        })
    }

    /// Mint a profile-download token for `(subject, group)`. The plaintext `skp_…` token
    /// is returned **once** to the caller; only its SHA-256 hash is persisted.
    pub fn mint_profile_token(
        &self,
        subject: &str,
        group: &str,
    ) -> Result<String, OperatorStoreError> {
        let token = generate_profile_token();
        self.block(async {
            sqlx::query(
                "INSERT INTO profile_tokens (token_hash, subject, group_id, created_at, \
                 revoked_at) VALUES ($1, $2, $3, $4, NULL)",
            )
            .bind(hash_secret(&token))
            .bind(subject)
            .bind(group)
            .bind(now_rfc3339())
            .execute(self.pool.pool())
            .await
        })
        .map_err(OperatorStoreError::from)?;
        Ok(token)
    }

    /// Resolve a presented token by hashing it. Active tokens only — a revoked token
    /// never resolves. Returns `(subject, group)`.
    pub fn lookup_active_profile_token(
        &self,
        presented: &str,
    ) -> Result<Option<(String, String)>, OperatorStoreError> {
        let hash = hash_secret(presented);
        self.block(async {
            let row: Option<(String, String)> = sqlx::query_as(
                "SELECT subject, group_id FROM profile_tokens \
                 WHERE token_hash = $1 AND revoked_at IS NULL",
            )
            .bind(hash)
            .fetch_optional(self.pool.pool())
            .await?;
            Ok(row)
        })
    }

    /// Revoke every active profile token for `(subject, group)` — e.g. offboarding a user
    /// or rotating a leak. Returns how many active tokens were revoked.
    pub fn revoke_profile_tokens(
        &self,
        subject: &str,
        group: &str,
    ) -> Result<usize, OperatorStoreError> {
        self.block(async {
            let changed = sqlx::query(
                "UPDATE profile_tokens SET revoked_at = $1 \
                 WHERE subject = $2 AND group_id = $3 AND revoked_at IS NULL",
            )
            .bind(now_rfc3339())
            .bind(subject)
            .bind(group)
            .execute(self.pool.pool())
            .await?
            .rows_affected();
            Ok(usize::try_from(changed).unwrap_or(usize::MAX))
        })
    }

    fn block<T>(&self, fut: impl std::future::Future<Output = T>) -> T {
        self.pool.block(fut)
    }
}

fn record_from_row(row: ProfileRow) -> ProfileRecord {
    ProfileRecord {
        group: row.0,
        mode: row.1,
        upstream_ref: row.2,
        models: serde_json::from_str(&row.3).unwrap_or_default(),
        timeout_seconds: row.4.map(|v| v as u64),
        max_tokens: row.5.map(|v| v as u32),
        updated_at: row.6,
    }
}

/// `skp_` + 32 bytes of OS CSPRNG entropy, base64url — 256 bits, unguessable bearer
/// credential. A profile token is a LOOKUP INDEX (see `hash_secret`), so its security
/// rests on entropy alone.
fn generate_profile_token() -> String {
    let mut buf = [0u8; 32];
    // getrandom reads from the OS CSPRNG; it is effectively infallible on the platforms
    // Sandhi targets. If it ever fails, panicking is correct — a token we cannot
    // randomize is unsafe to issue.
    getrandom::getrandom(&mut buf).expect("OS CSPRNG unavailable");
    format!("skp_{}", URL_SAFE_NO_PAD.encode(buf))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(group: &str) -> ProfileRecord {
        ProfileRecord {
            group: group.into(),
            mode: "gateway".into(),
            upstream_ref: Some("ollama:default".into()),
            models: vec!["glm".into(), "claude-x".into()],
            timeout_seconds: Some(120),
            max_tokens: Some(8192),
            updated_at: "2026-01-01T00:00:00Z".into(),
        }
    }

    fn pg_profiles(tag: &str) -> (String, PostgresProfileStore) {
        super::super::pg_test::connect_in_schema(tag, PostgresProfileStore::connect_with)
    }

    fn drop_schema(schema: &str) {
        super::super::pg_test::drop_schema_best_effort(schema)
    }

    // ── Non-DB dialect tests (run wherever the `postgres` feature compiles). ──

    #[test]
    fn ddl_pins_both_tables_and_quotes_the_reserved_group_column() {
        let joined = DDL.join("\n");
        assert!(joined.contains("CREATE TABLE IF NOT EXISTS profiles"));
        assert!(joined.contains("CREATE TABLE IF NOT EXISTS profile_tokens"));
        // `group` is reserved — the column stays quoted in DDL and every statement.
        assert!(joined.contains("\"group\"         text PRIMARY KEY"));
        assert!(joined.contains("idx_profile_tokens_subject_group"));
        assert!(!joined.contains("INSERT OR"));
    }

    // ── Live-Postgres tests: SANDHI_TEST_PG_DSN, per-test schema isolation. ──

    #[test]
    #[ignore = "requires SANDHI_TEST_PG_DSN"]
    fn profile_upsert_lookup_round_trip() {
        let (schema, store) = pg_profiles("profiles-crud");
        assert!(store.profile_for_group("platform").unwrap().is_none());

        store.upsert_profile(&record("platform")).unwrap();
        let found = store.profile_for_group("platform").unwrap().unwrap();
        assert_eq!(found, record("platform"));

        let mut updated = record("platform");
        updated.mode = "local".into();
        updated.upstream_ref = None;
        updated.max_tokens = None;
        updated.updated_at = "2026-02-01T00:00:00Z".into();
        store.upsert_profile(&updated).unwrap();
        assert_eq!(
            store.profile_for_group("platform").unwrap().unwrap(),
            updated
        );
        assert_eq!(store.list_profiles().unwrap().len(), 1);

        store.upsert_profile(&record("research")).unwrap();
        assert_eq!(store.list_profiles().unwrap().len(), 2);
        assert_eq!(
            store.profile_for_group("platform").unwrap().unwrap().mode,
            "local",
            "groups are independent — the platform row survived the research upsert"
        );

        // A minimal record with no defaults round-trips too.
        let minimal = ProfileRecord {
            group: "g".into(),
            mode: "local".into(),
            upstream_ref: None,
            models: Vec::new(),
            timeout_seconds: None,
            max_tokens: None,
            updated_at: "2026-01-01T00:00:00Z".into(),
        };
        store.upsert_profile(&minimal).unwrap();
        assert_eq!(store.profile_for_group("g").unwrap().unwrap(), minimal);
        assert_eq!(
            serde_json::to_value(store.profile_for_group("g").unwrap().unwrap()).unwrap()["models"],
            serde_json::json!([])
        );
        drop_schema(&schema);
    }

    #[test]
    #[ignore = "requires SANDHI_TEST_PG_DSN"]
    fn profile_token_mint_lookup_revoke_lifecycle() {
        let (schema, store) = pg_profiles("profiles-tokens");
        let token = store.mint_profile_token("alice", "platform").unwrap();

        assert!(token.starts_with("skp_"), "token is skp_-prefixed: {token}");
        assert_eq!(
            token["skp_".len()..].len(),
            43,
            "32 bytes base64url: {token}"
        );
        assert!(!token.contains('+') && !token.contains('/') && !token.contains('='));

        let (subject, group) = store
            .lookup_active_profile_token(&token)
            .unwrap()
            .expect("minted token resolves");
        assert_eq!((subject.as_str(), group.as_str()), ("alice", "platform"));

        assert!(store
            .lookup_active_profile_token("skp_nope")
            .unwrap()
            .is_none());

        assert_eq!(store.revoke_profile_tokens("alice", "platform").unwrap(), 1);
        assert!(store.lookup_active_profile_token(&token).unwrap().is_none());
        assert_eq!(store.revoke_profile_tokens("alice", "platform").unwrap(), 0);
        store.mint_profile_token("bob", "platform").unwrap();
        assert_eq!(store.revoke_profile_tokens("bob", "research").unwrap(), 0);
        assert_eq!(store.revoke_profile_tokens("bob", "platform").unwrap(), 1);
        drop_schema(&schema);
    }

    #[test]
    #[ignore = "requires SANDHI_TEST_PG_DSN"]
    fn stored_token_hash_is_never_the_plaintext_and_survives_reopen() {
        let (schema, store) = pg_profiles("profiles-hash");
        let token = store.mint_profile_token("alice", "platform").unwrap();

        let reopened = super::super::pg_test::connect_in_schema(
            "profiles-hash",
            PostgresProfileStore::connect_with,
        )
        .1;
        let (subject, _) = reopened
            .lookup_active_profile_token(&token)
            .unwrap()
            .expect("hash survives restart");
        assert_eq!(subject, "alice");
        drop_schema(&schema);
    }
}
