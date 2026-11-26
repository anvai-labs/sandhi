//! Postgres logical-route + session-affinity store — the PG port of
//! [`crate::routes::RouteStore`]. The definition types (`RouteDefinition`, `Target`,
//! `LogicalModel`, `Selection`, `Affinity`, `RouteRecord`, `Pin`) are reused verbatim from
//! the SQLite module so both backends stay definition-compatible; only the storage
//! statements and the error's database arm differ.
use sqlx::PgConnection;

use crate::routes::{Error as RouteError, Pin, RouteDefinition, RouteRecord, Target};

pub const MAX_ROUTES: usize = 256;
pub const MAX_AFFINITIES: usize = 16_384;
const MAX_BYTES: usize = 65_536;

/// The PG twin of [`crate::routes::Error`]: identical variants and identical `Display`
/// strings (the proxy maps the variants to HTTP statuses and renders the message); only
/// the `Database` arm carries the sqlx driver error instead of rusqlite.
#[derive(Debug)]
pub enum Error {
    Invalid(&'static str),
    Conflict,
    NotFound,
    Capacity,
    Corrupt,
    Database(sqlx::Error),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Invalid(message) => message,
            Self::Conflict => "route or affinity revision conflict",
            Self::NotFound => "route not found",
            Self::Capacity => "routing storage capacity exceeded",
            _ => "routing storage unavailable",
        })
    }
}
impl std::error::Error for Error {}
impl From<sqlx::Error> for Error {
    fn from(value: sqlx::Error) -> Self {
        Self::Database(value)
    }
}

/// Idempotent schema init, mirroring the SQLite DDL column-for-column. `typeof` guards
/// disappear (typed columns); the byte bounds become `octet_length` CHECKs.
const DDL: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS logical_routes (
        id text PRIMARY KEY,
        definition_json text NOT NULL CHECK(octet_length(definition_json)<=65536),
        revision bigint NOT NULL CHECK(revision>0),
        status text NOT NULL CHECK(status IN ('active','revoked'))
    )",
    "CREATE TABLE IF NOT EXISTS route_affinity (
        namespace text PRIMARY KEY CHECK(length(namespace)=64),
        route_id text NOT NULL,
        backend text NOT NULL,
        model text NOT NULL,
        fingerprint text NOT NULL CHECK(length(fingerprint)=64),
        expires_at bigint NOT NULL CHECK(expires_at>0)
    )",
    "CREATE INDEX IF NOT EXISTS route_affinity_expiry ON route_affinity(expires_at)",
];

/// A Postgres-backed logical-route + session-affinity store.
pub struct PostgresRouteStore {
    pool: super::OperatorPool,
}

/// The raw route row (`id, definition_json, revision, status`).
type RawRoute = (String, String, i64, String);

impl PostgresRouteStore {
    /// Connect and initialize the schema (idempotent `CREATE IF NOT EXISTS`).
    ///
    /// Must be called on a `tokio` runtime; that runtime's handle is retained for every
    /// later blocking statement.
    /// Like [`Self::connect`] from pre-parsed connect options: the startup wiring sets
    /// `application_name` (and the test suite points `search_path` at a per-test schema).
    pub async fn connect_with(
        options: sqlx::postgres::PgConnectOptions,
    ) -> Result<Self, sqlx::Error> {
        let pool = super::OperatorPool::connect_with(options, 2).await?;
        super::init_ddl(pool.pool(), DDL).await?;
        Ok(Self { pool })
    }

    /// Open the store on `dsn` and initialize the schema (idempotent).
    pub async fn connect(dsn: &str) -> Result<Self, sqlx::Error> {
        let options: sqlx::postgres::PgConnectOptions = dsn
            .parse()
            .map_err(|e| sqlx::Error::Configuration(format!("invalid DSN: {e}").into()))?;
        Self::connect_with(options).await
    }

    pub fn get(&self, id: &str) -> Result<Option<RouteRecord>, Error> {
        self.block(async { read_route(self.pool.pool(), id).await })
    }

    pub fn list(&self) -> Result<Vec<RouteRecord>, Error> {
        self.block(async {
            let rows: Vec<RawRoute> = sqlx::query_as(
                "SELECT id, substr(definition_json,1,65537), revision, status \
                 FROM logical_routes ORDER BY id LIMIT 257",
            )
            .fetch_all(self.pool.pool())
            .await?;
            if rows.len() > MAX_ROUTES {
                return Err(Error::Capacity);
            }
            rows.into_iter().map(decode_route).collect()
        })
    }

    pub fn put(
        &self,
        definition: &RouteDefinition,
        expected: Option<u64>,
    ) -> Result<RouteRecord, Error> {
        definition.validate().map_err(convert_validation_error)?;
        self.block(self.put_async(definition, expected))
    }

    async fn put_async(
        &self,
        definition: &RouteDefinition,
        expected: Option<u64>,
    ) -> Result<RouteRecord, Error> {
        let mut tx = self.pool.pool().begin().await?;
        serialize_writes(&mut tx).await?;
        let old = read_route(&mut *tx, &definition.id).await?;
        if let Some(old) = &old {
            if expected.is_some_and(|rev| rev != old.revision) {
                return Err(Error::Conflict);
            }
            if old.definition == *definition && old.status == "active" {
                return Ok(old.clone());
            }
            if expected.is_none() {
                return Err(Error::Conflict);
            }
        } else {
            if expected.is_some() {
                return Err(Error::Conflict);
            }
            let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM logical_routes")
                .fetch_one(&mut *tx)
                .await?;
            if count >= MAX_ROUTES as i64 {
                return Err(Error::Capacity);
            }
        }
        let revision = next_revision(old.as_ref().map_or(0, |record| record.revision))?;
        let json = serde_json::to_string(definition).map_err(|_| Error::Corrupt)?;
        sqlx::query(
            "INSERT INTO logical_routes (id, definition_json, revision, status) \
             VALUES ($1,$2,$3,'active') ON CONFLICT(id) DO UPDATE SET \
             definition_json=excluded.definition_json,revision=excluded.revision,status='active'",
        )
        .bind(&definition.id)
        .bind(&json)
        .bind(revision as i64)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(RouteRecord {
            definition: definition.clone(),
            revision,
            status: "active".into(),
        })
    }

    pub fn revoke(&self, id: &str, expected: u64) -> Result<RouteRecord, Error> {
        self.block(self.revoke_async(id, expected))
    }

    async fn revoke_async(&self, id: &str, expected: u64) -> Result<RouteRecord, Error> {
        let mut tx = self.pool.pool().begin().await?;
        serialize_writes(&mut tx).await?;
        let mut record = read_route(&mut *tx, id).await?.ok_or(Error::NotFound)?;
        if record.revision != expected {
            return Err(Error::Conflict);
        }
        if record.status == "active" {
            record.revision = next_revision(record.revision)?;
            record.status = "revoked".into();
            sqlx::query("UPDATE logical_routes SET status='revoked',revision=$2 WHERE id=$1")
                .bind(id)
                .bind(record.revision as i64)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(record)
    }

    /// The live pin for `(namespace, route)`, if any and unexpired at `now`.
    pub fn pin(&self, namespace: &str, route_id: &str, now: i64) -> Result<Option<Pin>, Error> {
        self.block(async { read_pin(self.pool.pool(), namespace, route_id, now).await })
    }

    /// Linearize admission against route edits and concurrent first turns. No secret,
    /// inference, budget reservation or mutable transport is stored in this transaction.
    pub fn confirm(&self, route: &RouteRecord, pin: Option<&Pin>, now: i64) -> Result<(), Error> {
        self.block(self.confirm_async(route, pin, now))
    }

    async fn confirm_async(
        &self,
        route: &RouteRecord,
        pin: Option<&Pin>,
        now: i64,
    ) -> Result<(), Error> {
        let mut tx = self.pool.pool().begin().await?;
        serialize_writes(&mut tx).await?;
        let current = read_route(&mut *tx, &route.definition.id)
            .await?
            .ok_or(Error::Conflict)?;
        if current.revision != route.revision || current.status != "active" {
            return Err(Error::Conflict);
        }
        if let Some(pin) = pin {
            if !route
                .definition
                .models
                .iter()
                .any(|model| model.targets.contains(&pin.target))
            {
                return Err(Error::Invalid(
                    "affinity target is not declared by this route",
                ));
            }
            if !digest(&pin.namespace)
                || !digest(&pin.fingerprint)
                || pin.expires_at <= now
                || pin.expires_at > now.checked_add(604_800).ok_or(Error::Corrupt)?
            {
                return Err(Error::Invalid("invalid affinity binding"));
            }
            if let Some(existing) =
                read_pin(&mut *tx, &pin.namespace, &route.definition.id, now).await?
            {
                if existing.target != pin.target || existing.fingerprint != pin.fingerprint {
                    return Err(Error::Conflict);
                }
            } else {
                sqlx::query("DELETE FROM route_affinity WHERE expires_at<=$1")
                    .bind(now)
                    .execute(&mut *tx)
                    .await?;
                let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM route_affinity")
                    .fetch_one(&mut *tx)
                    .await?;
                if count >= MAX_AFFINITIES as i64 {
                    return Err(Error::Capacity);
                }
                sqlx::query(
                    "INSERT INTO route_affinity \
                     (namespace, route_id, backend, model, fingerprint, expires_at) \
                     VALUES ($1,$2,$3,$4,$5,$6)",
                )
                .bind(&pin.namespace)
                .bind(&route.definition.id)
                .bind(&pin.target.backend)
                .bind(&pin.target.model)
                .bind(&pin.fingerprint)
                .bind(pin.expires_at)
                .execute(&mut *tx)
                .await?;
            }
        }
        tx.commit().await?;
        Ok(())
    }

    fn block<T>(&self, fut: impl std::future::Future<Output = T>) -> T {
        self.pool.block(fut)
    }
}

/// SQLite's `BEGIN IMMEDIATE` for the route tables: one transactional advisory lock over
/// the read-modify-write sequences (`put` / `revoke` / `confirm`), so two pool
/// connections cannot race a stale revision check.
async fn serialize_writes(tx: &mut PgConnection) -> Result<(), Error> {
    super::store_lock(tx, "logical_routes").await?;
    Ok(())
}

fn digest(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn next_revision(old: u64) -> Result<u64, Error> {
    old.checked_add(1)
        .filter(|value| *value <= i64::MAX as u64)
        .ok_or(Error::Corrupt)
}

/// `RouteDefinition::validate` returns the SQLite module's error type; every validation
/// failure is an `Invalid` (or a serialization `Corrupt`) — the SQLite `Database` arm is
/// unreachable from validation and maps to `Corrupt` here.
fn convert_validation_error(error: RouteError) -> Error {
    match error {
        RouteError::Invalid(message) => Error::Invalid(message),
        _ => Error::Corrupt,
    }
}

fn decode_route((id, json, revision, status): RawRoute) -> Result<RouteRecord, Error> {
    if json.len() > MAX_BYTES || revision <= 0 || !matches!(status.as_str(), "active" | "revoked") {
        return Err(Error::Corrupt);
    }
    let definition: RouteDefinition = serde_json::from_str(&json).map_err(|_| Error::Corrupt)?;
    // Parity with the SQLite twin: a stored row invalidated by validation-rule
    // drift is corrupted storage (503 + reconcile), not a client error.
    definition.validate().map_err(|_| Error::Corrupt)?;
    if definition.id != id {
        return Err(Error::Corrupt);
    }
    Ok(RouteRecord {
        definition,
        revision: revision as u64,
        status,
    })
}

/// Route reads run both on the pool (`get`/`pin`) and inside a transaction
/// (`put`/`revoke`/`confirm`), so the helper is executor-generic.
async fn read_route<'e, E>(executor: E, id: &str) -> Result<Option<RouteRecord>, Error>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    let row: Option<RawRoute> = sqlx::query_as(
        "SELECT id, substr(definition_json,1,65537), revision, status \
         FROM logical_routes WHERE id=$1",
    )
    .bind(id)
    .fetch_optional(executor)
    .await?;
    row.map(decode_route).transpose()
}

/// The corruption-bounding pin read: bounded projections for every text column, exactly
/// like the SQLite twin's `substr` guards. A row for a different route is corruption, not
/// absence (the namespace PK cannot hold two routes — fail closed).
async fn read_pin<'e, E>(
    executor: E,
    namespace: &str,
    route: &str,
    now: i64,
) -> Result<Option<Pin>, Error>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    let raw: Option<(String, String, String, String, i64)> = sqlx::query_as(
        "SELECT substr(route_id,1,129), substr(backend,1,129), substr(model,1,513), \
                substr(fingerprint,1,65), expires_at \
         FROM route_affinity WHERE namespace=$1",
    )
    .bind(namespace)
    .fetch_optional(executor)
    .await?;
    raw.map(|(route_id, backend, model, fingerprint, expires_at)| {
        if route_id != route
            || !digest(namespace)
            || !digest(&fingerprint)
            || backend.len() > 128
            || model.len() > 512
            || expires_at <= 0
        {
            return Err(Error::Corrupt);
        }
        Ok((expires_at > now).then(|| Pin {
            namespace: namespace.into(),
            target: Target { backend, model },
            fingerprint,
            expires_at,
        }))
    })
    .transpose()
    .map(Option::flatten)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn definition(id: &str, backend: &str) -> RouteDefinition {
        serde_json::from_value(json!({
            "id": id,
            "models": [{"id": "fixture", "targets": [{"backend": backend, "model": "fixture"}]}],
            "affinity_ttl_seconds": 3600
        }))
        .unwrap()
    }

    fn pg_routes(tag: &str) -> (String, PostgresRouteStore) {
        super::super::pg_test::connect_in_schema(tag, PostgresRouteStore::connect_with)
    }

    fn drop_schema(schema: &str) {
        super::super::pg_test::drop_schema_best_effort(schema)
    }

    // ── Non-DB dialect tests (run wherever the `postgres` feature compiles). ──

    #[test]
    fn ddl_pins_the_route_tables() {
        let joined = DDL.join("\n");
        assert!(joined.contains("CREATE TABLE IF NOT EXISTS logical_routes"));
        assert!(joined.contains("CREATE TABLE IF NOT EXISTS route_affinity"));
        assert!(joined.contains("octet_length(definition_json)<=65536"));
        assert!(joined.contains("revision bigint NOT NULL CHECK(revision>0)"));
        assert!(joined.contains("route_affinity_expiry ON route_affinity(expires_at)"));
        assert!(!joined.contains("INSERT OR"));
    }

    // ── Live-Postgres tests: SANDHI_TEST_PG_DSN, per-test schema isolation. ──

    #[test]
    #[ignore = "requires SANDHI_TEST_PG_DSN"]
    fn put_get_revoke_and_revision_conflicts_survive_reopen() {
        let (schema, store) = pg_routes("routes-crud");
        assert!(store.get("coding").unwrap().is_none());
        let record = store.put(&definition("coding", "backend_a"), None).unwrap();
        assert_eq!(record.revision, 1);
        assert_eq!(record.status, "active");

        // Identical definition is idempotent at the same revision.
        let same = store
            .put(&definition("coding", "backend_a"), Some(1))
            .unwrap();
        assert_eq!(same.revision, 1);

        // A change without the current revision conflicts.
        assert!(matches!(
            store.put(&definition("coding", "backend_b"), None),
            Err(Error::Conflict)
        ));
        let updated = store
            .put(&definition("coding", "backend_b"), Some(1))
            .unwrap();
        assert_eq!(updated.revision, 2);

        assert!(
            matches!(store.revoke("coding", 1), Err(Error::Conflict)),
            "stale revision cannot revoke"
        );
        let revoked = store.revoke("coding", 2).unwrap();
        assert_eq!(revoked.status, "revoked");
        assert!(matches!(store.revoke("nope", 1), Err(Error::NotFound)));

        let reopened = super::super::pg_test::connect_in_schema(
            "routes-crud",
            PostgresRouteStore::connect_with,
        )
        .1;
        let read = reopened.get("coding").unwrap().unwrap();
        assert_eq!(read.revision, 3);
        assert_eq!(read.status, "revoked");
        drop_schema(&schema);
    }

    #[test]
    #[ignore = "requires SANDHI_TEST_PG_DSN"]
    fn pin_confirm_round_trip_enforces_route_binding() {
        let (schema, store) = pg_routes("routes-pin");
        let route = store.put(&definition("chat", "backend_a"), None).unwrap();
        let pin = Pin {
            namespace: "a".repeat(64),
            target: Target {
                backend: "backend_a".into(),
                model: "fixture".into(),
            },
            fingerprint: "b".repeat(64),
            expires_at: 10_000,
        };
        store.confirm(&route, Some(&pin), 5_000).unwrap();
        let found = store.pin(&pin.namespace, "chat", 5_000).unwrap().unwrap();
        assert_eq!(found.target, pin.target);
        assert_eq!(found.fingerprint, pin.fingerprint);
        assert_eq!(found.expires_at, 10_000);

        // Expired pins grant nothing.
        assert!(store.pin(&pin.namespace, "chat", 10_000).unwrap().is_none());

        // A pin for an undeclared target is refused.
        let rogue = Pin {
            target: Target {
                backend: "backend_b".into(),
                model: "fixture".into(),
            },
            ..pin.clone()
        };
        assert!(matches!(
            store.confirm(&route, Some(&rogue), 5_000),
            Err(Error::Invalid(_))
        ));

        // A conflicting fingerprint on the same namespace conflicts.
        let conflicting = Pin {
            fingerprint: "c".repeat(64),
            ..pin.clone()
        };
        assert!(matches!(
            store.confirm(&route, Some(&conflicting), 5_000),
            Err(Error::Conflict)
        ));

        // Confirm against a stale revision conflicts (a definition CHANGE bumps the
        // revision; an identical put is idempotent at the same revision, like SQLite).
        let _updated = store
            .put(&definition("chat", "backend_b"), Some(1))
            .unwrap();
        assert!(matches!(
            store.confirm(&route, None, 5_000),
            Err(Error::Conflict)
        ));
        drop_schema(&schema);
    }
}
