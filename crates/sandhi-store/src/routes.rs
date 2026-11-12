//! Non-secret logical routes and bounded, durable session affinity.
//! Routes grant only declared backend/model pairs; they never contain credentials.
use std::{collections::HashSet, sync::Mutex, time::Duration};

use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};

pub const MAX_ROUTES: usize = 256;
pub const MAX_AFFINITIES: usize = 16_384;
const MAX_BYTES: usize = 65_536;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Selection {
    #[default]
    Priority,
    Rendezvous,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Affinity {
    None,
    #[default]
    Session,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Target {
    pub backend: String,
    pub model: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LogicalModel {
    pub id: String,
    pub targets: Vec<Target>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouteDefinition {
    pub id: String,
    pub models: Vec<LogicalModel>,
    #[serde(default)]
    pub selection: Selection,
    #[serde(default)]
    pub affinity: Affinity,
    #[serde(default = "default_ttl")]
    pub affinity_ttl_seconds: u32,
}
fn default_ttl() -> u32 {
    86_400
}

impl RouteDefinition {
    pub fn route_ref(&self) -> String {
        format!("route/{}", self.id)
    }

    pub fn validate(&self) -> Result<(), Error> {
        crate::vault::validate_reference("route", &self.id)
            .map_err(|_| Error::Invalid("invalid route id"))?;
        if !(60..=604_800).contains(&self.affinity_ttl_seconds) {
            return Err(Error::Invalid(
                "affinity TTL must be between 60 seconds and seven days",
            ));
        }
        if self.models.is_empty() || self.models.len() > 128 {
            return Err(Error::Invalid(
                "route requires between 1 and 128 logical models",
            ));
        }
        let valid_model = |id: &str| {
            !id.trim().is_empty() && id.len() <= 512 && !id.chars().any(char::is_control)
        };
        let mut aliases = HashSet::new();
        for model in &self.models {
            if !valid_model(&model.id) || !aliases.insert(model.id.to_ascii_lowercase()) {
                return Err(Error::Invalid(
                    "logical model ids must be bounded and unique",
                ));
            }
            if model.targets.is_empty() || model.targets.len() > 16 {
                return Err(Error::Invalid(
                    "logical model requires between 1 and 16 targets",
                ));
            }
            let mut targets = HashSet::new();
            for target in &model.targets {
                crate::vault::validate_reference("backend", &target.backend)
                    .map_err(|_| Error::Invalid("route targets require explicit backend ids"))?;
                if !valid_model(&target.model)
                    || !targets.insert((target.backend.clone(), target.model.to_ascii_lowercase()))
                {
                    return Err(Error::Invalid(
                        "route target models must be bounded and unique",
                    ));
                }
            }
        }
        if serde_json::to_vec(self).map_err(|_| Error::Corrupt)?.len() > MAX_BYTES {
            return Err(Error::Invalid("route definition exceeds size limit"));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RouteRecord {
    pub definition: RouteDefinition,
    pub revision: u64,
    pub status: String,
}

/// Fingerprint is over the selected immutable backend/profile/account/model binding.
/// Namespace is a server-generated digest; neither field stores prompt or identity text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pin {
    pub namespace: String,
    pub target: Target,
    pub fingerprint: String,
    pub expires_at: i64,
}

#[derive(Debug)]
pub enum Error {
    Invalid(&'static str),
    Conflict,
    NotFound,
    Capacity,
    Corrupt,
    Database(rusqlite::Error),
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
impl From<rusqlite::Error> for Error {
    fn from(value: rusqlite::Error) -> Self {
        Self::Database(value)
    }
}

pub struct RouteStore {
    conn: Mutex<Connection>,
}
impl RouteStore {
    pub fn open(path: &str) -> Result<Self, Error> {
        Self::from_connection(Connection::open(path)?)
    }
    pub fn in_memory() -> Result<Self, Error> {
        Self::from_connection(Connection::open_in_memory()?)
    }
    fn from_connection(conn: Connection) -> Result<Self, Error> {
        conn.busy_timeout(Duration::from_secs(5))?;
        conn.execute_batch("CREATE TABLE IF NOT EXISTS logical_routes (
            id TEXT PRIMARY KEY, definition_json TEXT NOT NULL CHECK(length(definition_json)<=65536),
            revision INTEGER NOT NULL CHECK(typeof(revision)='integer' AND revision>0),
            status TEXT NOT NULL CHECK(status IN ('active','revoked')));
            CREATE TABLE IF NOT EXISTS route_affinity (
            namespace TEXT PRIMARY KEY CHECK(length(namespace)=64), route_id TEXT NOT NULL,
            backend TEXT NOT NULL, model TEXT NOT NULL, fingerprint TEXT NOT NULL CHECK(length(fingerprint)=64),
            expires_at INTEGER NOT NULL CHECK(typeof(expires_at)='integer' AND expires_at>0));
            CREATE INDEX IF NOT EXISTS route_affinity_expiry ON route_affinity(expires_at);")?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }
    pub fn get(&self, id: &str) -> Result<Option<RouteRecord>, Error> {
        let conn = self.conn.lock().map_err(|_| Error::Corrupt)?;
        read_route(&conn, id)
    }
    pub fn list(&self) -> Result<Vec<RouteRecord>, Error> {
        let conn = self.conn.lock().map_err(|_| Error::Corrupt)?;
        let mut statement = conn.prepare("SELECT id,substr(definition_json,1,65537),revision,status FROM logical_routes ORDER BY id LIMIT 257")?;
        let rows = statement
            .query_map([], raw_route)?
            .collect::<Result<Vec<_>, _>>()?;
        if rows.len() > MAX_ROUTES {
            return Err(Error::Capacity);
        }
        rows.into_iter().map(decode_route).collect()
    }
    pub fn put(
        &self,
        definition: &RouteDefinition,
        expected: Option<u64>,
    ) -> Result<RouteRecord, Error> {
        definition.validate()?;
        let mut conn = self.conn.lock().map_err(|_| Error::Corrupt)?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let old = read_route(&tx, &definition.id)?;
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
            let count: i64 =
                tx.query_row("SELECT COUNT(*) FROM logical_routes", [], |row| row.get(0))?;
            if count >= MAX_ROUTES as i64 {
                return Err(Error::Capacity);
            }
        }
        let revision = next_revision(old.as_ref().map_or(0, |record| record.revision))?;
        let json = serde_json::to_string(definition).map_err(|_| Error::Corrupt)?;
        tx.execute(
            "INSERT INTO logical_routes VALUES (?1,?2,?3,'active') ON CONFLICT(id) DO UPDATE SET
            definition_json=excluded.definition_json,revision=excluded.revision,status='active'",
            params![definition.id, json, revision as i64],
        )?;
        tx.commit()?;
        Ok(RouteRecord {
            definition: definition.clone(),
            revision,
            status: "active".into(),
        })
    }
    pub fn revoke(&self, id: &str, expected: u64) -> Result<RouteRecord, Error> {
        let mut conn = self.conn.lock().map_err(|_| Error::Corrupt)?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut record = read_route(&tx, id)?.ok_or(Error::NotFound)?;
        if record.revision != expected {
            return Err(Error::Conflict);
        }
        if record.status == "active" {
            record.revision = next_revision(record.revision)?;
            record.status = "revoked".into();
            tx.execute(
                "UPDATE logical_routes SET status='revoked',revision=?2 WHERE id=?1",
                params![id, record.revision as i64],
            )?;
        }
        tx.commit()?;
        Ok(record)
    }
    pub fn pin(&self, namespace: &str, route_id: &str, now: i64) -> Result<Option<Pin>, Error> {
        let conn = self.conn.lock().map_err(|_| Error::Corrupt)?;
        read_pin(&conn, namespace, route_id, now)
    }
    /// Linearize admission against route edits and concurrent first turns. No secret,
    /// inference, budget reservation or mutable transport is stored in this transaction.
    pub fn confirm(&self, route: &RouteRecord, pin: Option<&Pin>, now: i64) -> Result<(), Error> {
        let mut conn = self.conn.lock().map_err(|_| Error::Corrupt)?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current = read_route(&tx, &route.definition.id)?.ok_or(Error::Conflict)?;
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
            if let Some(existing) = read_pin(&tx, &pin.namespace, &route.definition.id, now)? {
                if existing.target != pin.target || existing.fingerprint != pin.fingerprint {
                    return Err(Error::Conflict);
                }
            } else {
                tx.execute("DELETE FROM route_affinity WHERE expires_at<=?1", [now])?;
                let count: i64 =
                    tx.query_row("SELECT COUNT(*) FROM route_affinity", [], |row| row.get(0))?;
                if count >= MAX_AFFINITIES as i64 {
                    return Err(Error::Capacity);
                }
                tx.execute(
                    "INSERT INTO route_affinity VALUES (?1,?2,?3,?4,?5,?6)",
                    params![
                        pin.namespace,
                        route.definition.id,
                        pin.target.backend,
                        pin.target.model,
                        pin.fingerprint,
                        pin.expires_at
                    ],
                )?;
            }
        }
        tx.commit()?;
        Ok(())
    }
}

fn digest(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}
fn next_revision(old: u64) -> Result<u64, Error> {
    old.checked_add(1)
        .filter(|value| *value <= i64::MAX as u64)
        .ok_or(Error::Corrupt)
}
type RawRoute = (String, String, i64, String);
fn raw_route(row: &rusqlite::Row<'_>) -> rusqlite::Result<RawRoute> {
    Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
}
fn decode_route((id, json, revision, status): RawRoute) -> Result<RouteRecord, Error> {
    if json.len() > MAX_BYTES || revision <= 0 || !matches!(status.as_str(), "active" | "revoked") {
        return Err(Error::Corrupt);
    }
    let definition: RouteDefinition = serde_json::from_str(&json).map_err(|_| Error::Corrupt)?;
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
fn read_route(conn: &Connection, id: &str) -> Result<Option<RouteRecord>, Error> {
    conn.query_row(
        "SELECT id,substr(definition_json,1,65537),revision,status FROM logical_routes WHERE id=?1",
        [id],
        raw_route,
    )
    .optional()?
    .map(decode_route)
    .transpose()
}
fn read_pin(
    conn: &Connection,
    namespace: &str,
    route: &str,
    now: i64,
) -> Result<Option<Pin>, Error> {
    let raw: Option<(String,String,String,String,i64)> = conn.query_row("SELECT substr(route_id,1,129),substr(backend,1,129),substr(model,1,513),substr(fingerprint,1,65),expires_at FROM route_affinity WHERE namespace=?1",
        [namespace],|row|Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?,row.get(4)?))).optional()?;
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
