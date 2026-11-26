//! Optional Postgres operator stores (cargo feature `postgres`, NON-default) — Phase 3 PR-C.
//! Direct ports of the SQLite operator surfaces onto the same PG dialect the ledger core
//! (PR-A) and evidence subsystem (PR-B) already use. Store for store, method for method:
//!
//! | SQLite twin (unchanged)                          | PG port (this module)                |
//! |--------------------------------------------------|--------------------------------------|
//! | [`crate::vault::VaultStore`] (metadata only)     | [`vault::PostgresVaultStore`]        |
//! | [`crate::vkeys::VirtualKeyStore`]                | [`vkeys::PostgresVirtualKeyStore`]   |
//! | [`crate::backends::BackendStore`]                | [`backends::PostgresBackendStore`]   |
//! | [`crate::profiles::ProfileStore`]                | [`profiles::PostgresProfileStore`]   |
//! | [`crate::alerts::AlertStore`]                    | [`alerts::PostgresAlertStore`]       |
//! | [`crate::routes::RouteStore`]                    | [`routes::PostgresRouteStore`]       |
//! | [`crate::references::ReferenceStore`]            | [`references::PostgresReferenceStore`] |
//! | [`crate::policy::PolicyAuditStore`]              | [`policy::PostgresPolicyAuditStore`] |
//!
//! Recorded decisions (all following the PR-A/PR-B house pattern):
//!
//! - **Connection model.** A small `PgPool` plus the owning tokio [`tokio::runtime::Handle`]
//!   ([`OperatorPool`]); every statement is `block_on`'d there — callers must be ordinary
//!   blocking threads, never async-executor tasks. Runtime sqlx API only (no `query!`
//!   macros), so nothing here needs a live DB to compile.
//! - **No `Mutex<Connection>`.** The pool is the shared resource and every store takes
//!   `&self`, exactly like [`crate::ledger_pg::PostgresLedger`].
//! - **DDL per store in an idempotent init array** (`CREATE TABLE IF NOT EXISTS` + indexes,
//!   verbatim-translated: TEXT→text, INTEGER→bigint/integer, AUTOINCREMENT→identity).
//!   `strftime('%Y-%m-%dT%H:%M:%fZ','now')`-style SQL defaults become `to_char(now() AT
//!   TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')` — byte-identical UTC text.
//! - **Additive migrations** (the columns the SQLite stores still `ALTER TABLE` in with
//!   ignore-duplicate attempts) arrive through `information_schema.columns` probes +
//!   `ALTER TABLE .. ADD COLUMN`, mirroring [`crate::ledger_pg::evidence`].
//! - **Write serialization.** SQLite's `BEGIN IMMEDIATE` write transactions become READ
//!   COMMITTED transactions under a transactional advisory lock keyed by the table name
//!   ([`store_lock`], reusing the ledger's stable FNV-1a scope-key identity). Read-modify-
//!   write sequences (`put`/`revoke`/`confirm`/`issue`/`record`) hold that lock across the
//!   whole check-then-write, which is SQLite's whole-file write lock narrowed to one table.
//! - **No `INSERT OR` anything.** Every SQLite upsert's conflict target is mirrored with
//!   `ON CONFLICT (...) DO UPDATE`.
//! - **Error seams.** Rich SQLite error enums (`backends::BackendStoreError`,
//!   `routes::Error`) are mirrored variant-for-variant with identical `Display` strings —
//!   the proxy maps variants to HTTP statuses and renders the message. The
//!   `rusqlite::Result` twins use [`crate::OperatorStoreError`]; the vault twins keep
//!   [`crate::VaultError`] (its `Backend` arm wraps the sqlx message); the reference store
//!   reuses [`crate::references::Error`] verbatim.
//! - **Secrets never move.** The vault store still indexes metadata only; credential bytes
//!   stay in the OS keyring / SentinelPass via `SANDHI_VAULT_BACKEND`.
//!
//! The proxy wiring (constructing these stores at startup behind a DSN) is PR-D's concern;
//! this PR ships the stores + DDL + tests only. Everything is behind the `postgres`
//! feature: default-feature builds never compile any of it.

pub mod alerts;
pub mod backends;
pub mod policy;
pub mod profiles;
pub mod references;
pub mod routes;
pub mod vault;
pub mod vkeys;

use std::future::Future;
use std::time::Duration;

use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{PgConnection, PgPool};

pub use alerts::PostgresAlertStore;
pub use backends::PostgresBackendStore;
pub use policy::PostgresPolicyAuditStore;
pub use profiles::PostgresProfileStore;
pub use references::PostgresReferenceStore;
pub use routes::PostgresRouteStore;
pub use vault::PostgresVaultStore;
pub use vkeys::PostgresVirtualKeyStore;

/// A small Postgres pool plus the retained runtime handle that owns it — the shared
/// connection core of every operator store in this module. All statements are `block_on`'d
/// through the handle from ordinary blocking threads; `Handle::block_on` panics inside an
/// async task by design, which keeps that mis-use loud instead of silently serializing a
/// worker.
pub(crate) struct OperatorPool {
    pool: PgPool,
    rt: tokio::runtime::Handle,
}

impl OperatorPool {
    /// Connect a pool against `dsn`. Small on purpose: the operator stores serve the admin
    /// API and startup rehydration, not the request path.
    pub(crate) async fn connect(dsn: &str, max_connections: u32) -> Result<Self, sqlx::Error> {
        let options: PgConnectOptions = dsn
            .parse()
            .map_err(|e| sqlx::Error::Configuration(format!("invalid DSN: {e}").into()))?;
        Self::connect_with(options, max_connections).await
    }

    /// Like [`Self::connect`] from pre-parsed options (the test suite points `search_path`
    /// at a per-test schema for isolation against shared instances).
    pub(crate) async fn connect_with(
        options: PgConnectOptions,
        max_connections: u32,
    ) -> Result<Self, sqlx::Error> {
        let pool = PgPoolOptions::new()
            .max_connections(max_connections)
            .acquire_timeout(Duration::from_secs(5))
            .connect_with(options)
            .await?;
        Ok(Self {
            pool,
            rt: tokio::runtime::Handle::current(),
        })
    }

    fn block<T>(&self, fut: impl Future<Output = T>) -> T {
        self.rt.block_on(fut)
    }

    /// The pool itself, for the few direct-pool reads (and the test helpers).
    pub(crate) fn pool(&self) -> &PgPool {
        &self.pool
    }
}

/// Run one DDL batch on the pool (idempotent `CREATE IF NOT EXISTS` statements).
pub(crate) async fn init_ddl(pool: &PgPool, ddl: &[&str]) -> Result<(), sqlx::Error> {
    for statement in ddl {
        sqlx::query(statement).execute(pool).await?;
    }
    Ok(())
}

/// The transactional advisory lock serializing one store's read-modify-write transaction —
/// the PG dialect of SQLite's `BEGIN IMMEDIATE`, narrowed from the whole file to one
/// table. Keyed by `fnv1a64("op:" ++ table)` — a namespace PREFIX over the ledger's
/// scope-key derivation, so an operator-supplied ledger budget scope whose text equals
/// a table name can never serialize unrelated ledger and operator transactions (the
/// evidence reserved lanes are the dedicated negatives -1/-2 and stay disjoint).
pub(crate) async fn store_lock(conn: &mut PgConnection, table: &str) -> Result<(), sqlx::Error> {
    let key = crate::ledger_pg::scope_advisory_key(&format!("op:{table}"));
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(key)
        .execute(&mut *conn)
        .await?;
    Ok(())
}

/// The `information_schema.columns` probe that replaces SQLite's `pragma_table_info`
/// probe, column-for-column (the same shape [`crate::ledger_pg::evidence`] uses). Bounded
/// to the session schema so per-test schemas isolate.
pub(crate) async fn column_exists(
    pool: &PgPool,
    table: &str,
    column: &str,
) -> Result<bool, sqlx::Error> {
    let exists: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM information_schema.columns \
         WHERE table_schema = current_schema() AND table_name = $1 AND column_name = $2)",
    )
    .bind(table)
    .bind(column)
    .fetch_one(pool)
    .await?;
    Ok(exists)
}

/// Additive migration step: `ALTER TABLE .. ADD COLUMN` when the probe says the column is
/// missing — mirroring the SQLite stores' attempt-and-ignore-duplicate ALTERs.
pub(crate) async fn ensure_column(
    pool: &PgPool,
    table: &str,
    column: &str,
    declaration: &str,
) -> Result<(), sqlx::Error> {
    if !column_exists(pool, table, column).await? {
        sqlx::query(&format!(
            "ALTER TABLE {table} ADD COLUMN {column} {declaration}"
        ))
        .execute(pool)
        .await?;
    }
    Ok(())
}

/// 32 hex chars (128 bits) of OS CSPRNG entropy — the same generator the SQLite stores
/// use for virtual-key secrets and non-secret public ids.
pub(crate) fn random_hex(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    // getrandom reads from the OS CSPRNG; it is effectively infallible on the platforms
    // Sandhi targets. If it ever fails, panicking is correct — a key we cannot randomize
    // is unsafe to issue.
    getrandom::getrandom(&mut buf).expect("OS CSPRNG unavailable");
    let mut out = String::with_capacity(buf.len() * 2);
    for byte in &buf {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// RFC-3339 UTC timestamp in the same shape every SQLite operator store stamps.
pub(crate) fn now_rfc3339() -> String {
    use time::format_description::well_known::Rfc3339;
    time::OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_default()
}

/// Shared live-Postgres test support for the operator suites: per-test schemas of one
/// shared DSN (the same isolation model as the ledger tests, `operator_test_` prefixed).
#[cfg(test)]
pub(crate) mod pg_test {
    use std::time::Duration;

    use sqlx::postgres::{PgConnectOptions, PgPoolOptions};

    pub(crate) fn dsn() -> String {
        std::env::var("SANDHI_TEST_PG_DSN")
            .ok()
            .map(|dsn| dsn.trim().to_string())
            .filter(|dsn| !dsn.is_empty())
            .unwrap_or_else(|| {
                panic!("set SANDHI_TEST_PG_DSN to run the live-Postgres operator-store tests")
            })
    }

    pub(crate) fn schema(tag: &str) -> String {
        format!("operator_test_{}_{}", std::process::id(), tag)
    }

    /// Create the per-test schema and return `(schema, options)` with `search_path` pointed
    /// at it. Each call creates the schema through a short-lived pool, so a fresh or
    /// previously dirtied schema behave identically.
    pub(crate) fn scoped_options(tag: &str) -> (String, PgConnectOptions) {
        let dsn = dsn();
        let schema = schema(tag);
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("schema runtime");
        rt.block_on(async {
            let root: PgConnectOptions = dsn.parse().expect("parse DSN");
            let pool = PgPoolOptions::new()
                .max_connections(1)
                .acquire_timeout(Duration::from_secs(5))
                .connect_with(root.clone())
                .await
                .expect("connect root pool");
            sqlx::query(&format!(r#"CREATE SCHEMA IF NOT EXISTS "{schema}""#))
                .execute(&pool)
                .await
                .expect("create schema");
            pool.close().await;
        });
        let root: PgConnectOptions = dsn.parse().expect("parse DSN");
        let scoped = root.options([("search_path", schema.clone())]);
        (schema, scoped)
    }

    /// Connect a store into the per-test schema, leaking the owning runtime (the store
    /// retains its handle and the pool's background tasks must outlive every blocking
    /// statement below).
    pub(crate) fn connect_in_schema<T, F, Fut>(tag: &str, connect: F) -> (String, T)
    where
        F: FnOnce(PgConnectOptions) -> Fut,
        Fut: std::future::Future<Output = Result<T, sqlx::Error>>,
    {
        let (schema, options) = scoped_options(tag);
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("test runtime");
        let store = rt
            .block_on(connect(options))
            .expect("connect operator store");
        std::mem::forget(rt);
        (schema, store)
    }

    pub(crate) fn drop_schema_best_effort(schema: &str) {
        let Ok(dsn) = std::env::var("SANDHI_TEST_PG_DSN") else {
            return;
        };
        let Ok(rt) = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        else {
            return;
        };
        let _ = rt.block_on(async {
            let root: PgConnectOptions = dsn.trim().parse().ok()?;
            let pool = PgPoolOptions::new()
                .max_connections(1)
                .acquire_timeout(Duration::from_secs(5))
                .connect_with(root)
                .await
                .ok()?;
            let _ = sqlx::query(&format!(r#"DROP SCHEMA IF EXISTS "{schema}" CASCADE"#))
                .execute(&pool)
                .await;
            pool.close().await;
            Some(())
        });
    }
}
