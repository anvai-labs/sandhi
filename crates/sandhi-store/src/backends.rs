//! Bounded, non-secret backend definitions. Credential bytes remain in the vault.
use std::{collections::HashSet, sync::Mutex, time::Duration};

use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
pub use sandhi_core::EndpointProfileV1 as BackendProfile;
use sandhi_core::{EndpointFamilyV1, ModelDescriptorV1};
use serde::{Deserialize, Serialize};

pub const MAX_BACKENDS: usize = 256;
pub const MAX_DEFINITION_BYTES: usize = 65_536;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BackendEndpoint {
    pub endpoint_family: EndpointFamilyV1,
    #[serde(default)]
    pub endpoint_profile: BackendProfile,
}

impl BackendEndpoint {
    pub const fn standard(endpoint_family: EndpointFamilyV1) -> Self {
        Self {
            endpoint_family,
            endpoint_profile: BackendProfile::Standard,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BackendDefinition {
    pub id: String,
    pub provider: String,
    pub credential_ref: String,
    pub endpoint: BackendEndpoint,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_agent: Option<String>,
    pub models: Vec<ModelDescriptorV1>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_policy: Option<sandhi_core::BackendCachePolicyV1>,
}

impl BackendDefinition {
    pub fn route_ref(&self) -> String {
        format!("backend/{}", self.id)
    }

    pub fn validate(&self) -> Result<(), &'static str> {
        self.endpoint
            .endpoint_profile
            .validate_family(self.endpoint.endpoint_family)?;
        crate::vault::validate_reference("backend", &self.id).map_err(|_| "invalid backend id")?;
        crate::vault::validate_reference(&self.provider, "default")
            .map_err(|_| "invalid provider label")?;
        let (provider, label) = self
            .credential_ref
            .split_once(':')
            .ok_or("credential_ref requires provider:label")?;
        crate::vault::validate_reference(provider, label)
            .map_err(|_| "invalid credential reference")?;
        if self
            .base_url
            .as_ref()
            .is_some_and(|url| url.len() > 2048 || url.chars().any(char::is_control))
        {
            return Err("invalid backend base URL");
        }
        if self.user_agent.as_ref().is_some_and(|value| {
            value.is_empty()
                || value.len() > 256
                || !value.is_ascii()
                || value.chars().any(char::is_control)
        }) {
            return Err("invalid backend user agent");
        }
        if self.models.is_empty() || self.models.len() > 128 {
            return Err("backend requires between 1 and 128 models");
        }
        let mut ids = HashSet::new();
        for model in &self.models {
            if model.id.trim().is_empty()
                || model.id.len() > 512
                || model.id.chars().any(char::is_control)
                || !ids.insert(model.id.to_ascii_lowercase())
            {
                return Err("backend model ids must be nonempty, bounded and unique");
            }
            // Aliases become executable mappings in M4. Do not accept inert routing
            // controls or alternate model URLs as though they were implemented.
            if !model.aliases.is_empty()
                || model.endpoint_url.is_some()
                || !model.extensions.is_empty()
            {
                return Err(
                    "backend model aliases, endpoint overrides and extensions are not supported",
                );
            }
        }
        if let Some(policy) = &self.cache_policy {
            use sandhi_core::{
                CacheCapabilityProfileV1 as P, CacheModeV1 as M, CacheNamespaceV1 as N,
            };
            if policy.models.len() != self.models.len()
                || policy
                    .models
                    .keys()
                    .any(|key| !self.models.iter().any(|model| &model.id == key))
            {
                return Err(
                    "cache policy must declare capabilities for every exact backend model id",
                );
            }
            for capabilities in policy.models.values() {
                capabilities.validate(self.endpoint.endpoint_family)?;
                if self.endpoint.endpoint_profile == BackendProfile::ChatGptCodex
                    && capabilities.profile != P::NativeOnly
                {
                    return Err(
                        "Codex cache capabilities remain native-only until independently qualified",
                    );
                }
                if policy.namespace == N::ScopedKey
                    && !matches!(capabilities.profile, P::OpenaiLegacy | P::OpenaiExplicit)
                {
                    return Err(
                        "scoped cache keys require a declared key-capable OpenAI cache profile",
                    );
                }
                if let Some(intent) = &policy.default_intent {
                    intent.validate()?;
                    if intent.mode == M::Auto && capabilities.profile == P::NativeOnly {
                        return Err(
                            "automatic cache default requires declared caching capabilities",
                        );
                    }
                    if intent
                        .lifetime
                        .is_some_and(|ttl| !capabilities.lifetimes.contains(&ttl))
                    {
                        return Err("default cache lifetime is not declared for every model");
                    }
                    if matches!(intent.mode, M::AutoStablePrefix | M::AutoConversationPrefix)
                        && !matches!(
                            capabilities.profile,
                            P::AnthropicBreakpoints | P::OpenaiExplicit
                        )
                    {
                        return Err(
                            "automatic cache placement requires declared breakpoint capabilities",
                        );
                    }
                }
            }
        }
        if serde_json::to_vec(self)
            .map_err(|_| "invalid backend definition")?
            .len()
            > MAX_DEFINITION_BYTES
        {
            return Err("backend definition exceeds size limit");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct BackendRecord {
    pub definition: BackendDefinition,
    pub revision: u64,
    pub status: String,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug)]
pub enum BackendStoreError {
    Invalid(&'static str),
    Conflict,
    NotFound,
    Capacity,
    Corrupt,
    Database(rusqlite::Error),
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
impl From<rusqlite::Error> for BackendStoreError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Database(error)
    }
}

pub struct BackendStore {
    conn: Mutex<Connection>,
}

impl BackendStore {
    pub fn open(path: &str) -> Result<Self, BackendStoreError> {
        Self::from_connection(Connection::open(path)?)
    }
    pub fn in_memory() -> Result<Self, BackendStoreError> {
        Self::from_connection(Connection::open_in_memory()?)
    }
    fn from_connection(conn: Connection) -> Result<Self, BackendStoreError> {
        conn.busy_timeout(Duration::from_secs(5))?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS backends (
            id TEXT PRIMARY KEY CHECK(length(id) BETWEEN 1 AND 128),
            definition_json TEXT NOT NULL CHECK(length(definition_json) <= 65536),
            revision INTEGER NOT NULL CHECK(revision > 0),
            status TEXT NOT NULL CHECK(status IN ('active','revoked')),
            created_at TEXT NOT NULL, updated_at TEXT NOT NULL
        );",
        )?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    pub fn get(&self, id: &str) -> Result<Option<BackendRecord>, BackendStoreError> {
        crate::vault::validate_reference("backend", id)
            .map_err(|_| BackendStoreError::Invalid("invalid backend id"))?;
        let conn = self.conn.lock().map_err(|_| BackendStoreError::Corrupt)?;
        get_record(&conn, id)
    }

    pub fn list(&self) -> Result<Vec<BackendRecord>, BackendStoreError> {
        let conn = self.conn.lock().map_err(|_| BackendStoreError::Corrupt)?;
        let mut stmt = conn.prepare("SELECT id, substr(definition_json,1,65537), revision, status, created_at, updated_at FROM backends ORDER BY id LIMIT 257")?;
        let rows = stmt
            .query_map([], raw_record)?
            .collect::<Result<Vec<_>, _>>()?;
        if rows.len() > MAX_BACKENDS {
            return Err(BackendStoreError::Capacity);
        }
        rows.into_iter().map(decode_record).collect()
    }

    /// Create is idempotent only for an identical active definition. Changes and
    /// reactivation require the current revision, checked inside an immediate transaction.
    pub fn put(
        &self,
        definition: &BackendDefinition,
        expected_revision: Option<u64>,
    ) -> Result<BackendRecord, BackendStoreError> {
        definition.validate().map_err(BackendStoreError::Invalid)?;
        let mut conn = self.conn.lock().map_err(|_| BackendStoreError::Corrupt)?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let previous = get_record(&tx, &definition.id)?;
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
            let count: i64 = tx.query_row("SELECT COUNT(*) FROM backends", [], |row| row.get(0))?;
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
        let created_at = previous.map_or_else(|| now.clone(), |record| record.created_at);
        let json = serde_json::to_string(definition)
            .map_err(|_| BackendStoreError::Invalid("invalid backend definition"))?;
        tx.execute("INSERT INTO backends (id,definition_json,revision,status,created_at,updated_at)
            VALUES (?1,?2,?3,'active',?4,?5) ON CONFLICT(id) DO UPDATE SET
            definition_json=excluded.definition_json,revision=excluded.revision,status='active',updated_at=excluded.updated_at",
            params![definition.id,json,revision as i64,created_at,now])?;
        tx.commit()?;
        Ok(BackendRecord {
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
        let mut conn = self.conn.lock().map_err(|_| BackendStoreError::Corrupt)?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut record = get_record(&tx, id)?.ok_or(BackendStoreError::NotFound)?;
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
        tx.execute(
            "UPDATE backends SET status='revoked',revision=?2,updated_at=?3 WHERE id=?1",
            params![id, record.revision as i64, record.updated_at],
        )?;
        tx.commit()?;
        Ok(record)
    }
}

type RawRecord = (String, String, i64, String, String, String);
fn raw_record(row: &rusqlite::Row<'_>) -> rusqlite::Result<RawRecord> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
    ))
}
fn decode_record(
    (id, json, revision, status, created_at, updated_at): RawRecord,
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
    Ok(BackendRecord {
        definition,
        revision: revision as u64,
        status,
        created_at,
        updated_at,
    })
}
fn get_record(conn: &Connection, id: &str) -> Result<Option<BackendRecord>, BackendStoreError> {
    conn.query_row("SELECT id, substr(definition_json,1,65537), revision, status, created_at, updated_at FROM backends WHERE id=?1",
        [id], raw_record).optional()?.map(decode_record).transpose()
}
