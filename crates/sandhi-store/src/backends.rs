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
    /// Immutable enforcement namespace, separate from the descriptive provider and endpoint.
    pub upstream_budget_scope: String,
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
    fn from_connection(mut conn: Connection) -> Result<Self, BackendStoreError> {
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
        migrate_budget_namespaces(&mut conn)?;
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
        let mut stmt = conn.prepare("SELECT id, substr(definition_json,1,65537), revision, status, created_at, updated_at, upstream_budget_scope FROM backends ORDER BY id LIMIT 257")?;
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
        let upstream_budget_scope = previous.as_ref().map_or_else(
            || legacy_budget_scope(definition),
            |record| Ok(record.upstream_budget_scope.clone()),
        )?;
        let created_at = previous.map_or_else(|| now.clone(), |record| record.created_at);
        let json = serde_json::to_string(definition)
            .map_err(|_| BackendStoreError::Invalid("invalid backend definition"))?;
        tx.execute("INSERT INTO backends (id,definition_json,revision,status,created_at,updated_at,upstream_budget_scope)
            VALUES (?1,?2,?3,'active',?4,?5,?6) ON CONFLICT(id) DO UPDATE SET
            definition_json=excluded.definition_json,revision=excluded.revision,status='active',updated_at=excluded.updated_at",
            params![definition.id,json,revision as i64,created_at,now,upstream_budget_scope])?;
        tx.commit()?;
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

/// Preserve the exact pre-migration effective scope. This is compatibility authority,
/// not an assertion that a family name identifies the vendor. Never recompute on update.
fn legacy_budget_scope(definition: &BackendDefinition) -> Result<String, BackendStoreError> {
    let name = match definition.endpoint.endpoint_family {
        EndpointFamilyV1::AnthropicMessages => "anthropic",
        EndpointFamilyV1::GeminiGenerateContent => "gemini",
        EndpointFamilyV1::CohereChat => "cohere",
        EndpointFamilyV1::OllamaChat => "ollama",
        EndpointFamilyV1::OpenaiChatCompletions | EndpointFamilyV1::OpenaiResponses => {
            &definition.provider
        }
        _ => {
            return Err(BackendStoreError::Invalid(
                "backend has no legacy budget namespace",
            ))
        }
    };
    Ok(format!("upstream:{name}"))
}
fn valid_budget_scope(scope: &str) -> bool {
    scope
        .strip_prefix("upstream:")
        .is_some_and(|name| crate::vault::validate_reference(name, "namespace").is_ok())
}
fn migrate_budget_namespaces(conn: &mut Connection) -> Result<(), BackendStoreError> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let exists: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM pragma_table_info('backends') WHERE name='upstream_budget_scope')", [], |row| row.get(0))?;
    if !exists {
        tx.execute(
            "ALTER TABLE backends ADD COLUMN upstream_budget_scope TEXT",
            [],
        )?;
        let rows = {
            let mut stmt =
                tx.prepare("SELECT id, substr(definition_json,1,65537) FROM backends LIMIT 257")?;
            let rows = stmt
                .query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            rows
        };
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
            tx.execute(
                "UPDATE backends SET upstream_budget_scope=?2 WHERE id=?1",
                params![id, scope],
            )?;
        }
    }
    // An existing column with missing/corrupt bindings is not another legacy migration.
    // Do not infer a replacement namespace from a possibly changed definition.
    let bad: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM backends WHERE upstream_budget_scope IS NULL OR typeof(upstream_budget_scope)<>'text' OR length(CAST(upstream_budget_scope AS BLOB))>137)",[],|row|row.get(0))?;
    if bad {
        return Err(BackendStoreError::Corrupt);
    }
    tx.execute_batch("CREATE TRIGGER IF NOT EXISTS backend_budget_namespace_immutable BEFORE UPDATE OF upstream_budget_scope ON backends WHEN OLD.upstream_budget_scope IS NOT NEW.upstream_budget_scope BEGIN SELECT RAISE(ABORT,'backend budget namespace is immutable'); END;")?;
    tx.commit()?;
    Ok(())
}

type RawRecord = (String, String, i64, String, String, String, Option<String>);
fn raw_record(row: &rusqlite::Row<'_>) -> rusqlite::Result<RawRecord> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
        row.get(6)?,
    ))
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
fn get_record(conn: &Connection, id: &str) -> Result<Option<BackendRecord>, BackendStoreError> {
    conn.query_row("SELECT id, substr(definition_json,1,65537), revision, status, created_at, updated_at, upstream_budget_scope FROM backends WHERE id=?1",
        [id], raw_record).optional()?.map(decode_record).transpose()
}

#[cfg(test)]
mod namespace_tests {
    use super::*;
    fn definition(family: EndpointFamilyV1) -> BackendDefinition {
        serde_json::from_value(serde_json::json!({"id":"source","provider":"vendor-one","credential_ref":"vendor:fixture","endpoint":{"endpoint_family":family},"models":[{"id":"fixture"}]})).unwrap()
    }
    fn legacy_connection(path: &str, definition: &BackendDefinition) -> Connection {
        let conn = Connection::open(path).unwrap();
        conn.execute_batch("CREATE TABLE backends(id TEXT PRIMARY KEY,definition_json TEXT NOT NULL,revision INTEGER NOT NULL,status TEXT NOT NULL,created_at TEXT NOT NULL,updated_at TEXT NOT NULL);").unwrap();
        conn.execute(
            "INSERT INTO backends VALUES('source',?1,1,'active','old','old')",
            [serde_json::to_string(definition).unwrap()],
        )
        .unwrap();
        conn
    }
    #[test]
    fn legacy_namespace_survives_label_family_revision_and_restart() {
        for (family, expected) in [
            (EndpointFamilyV1::AnthropicMessages, "upstream:anthropic"),
            (EndpointFamilyV1::GeminiGenerateContent, "upstream:gemini"),
            (EndpointFamilyV1::CohereChat, "upstream:cohere"),
            (EndpointFamilyV1::OllamaChat, "upstream:ollama"),
            (EndpointFamilyV1::OpenaiResponses, "upstream:vendor-one"),
            (
                EndpointFamilyV1::OpenaiChatCompletions,
                "upstream:vendor-one",
            ),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("old.db");
            let path = path.to_str().unwrap();
            let original = definition(family);
            let store = BackendStore::from_connection(legacy_connection(path, &original)).unwrap();
            assert_eq!(
                store.get("source").unwrap().unwrap().upstream_budget_scope,
                expected
            );
            let mut changed = definition(EndpointFamilyV1::GeminiGenerateContent);
            changed.provider = "vendor-two".into();
            let changed = store.put(&changed, Some(1)).unwrap();
            assert_eq!(changed.upstream_budget_scope, expected);
            assert!(matches!(
                store.put(&original, Some(1)),
                Err(BackendStoreError::Conflict)
            ));
            let conn = store.conn.lock().unwrap();
            assert!(conn
                .execute(
                    "UPDATE backends SET upstream_budget_scope='upstream:vendor-two'",
                    []
                )
                .is_err());
            drop(conn);
            drop(store);
            let store = BackendStore::open(path).unwrap();
            let read = store.get("source").unwrap().unwrap();
            assert_eq!(read.upstream_budget_scope, expected);
            assert_eq!(read.definition.provider, "vendor-two");
            assert_eq!(read.revision, 2);
        }
    }
    #[test]
    fn missing_binding_in_already_migrated_database_is_not_reinferred() {
        let conn = legacy_connection(":memory:", &definition(EndpointFamilyV1::AnthropicMessages));
        conn.execute(
            "ALTER TABLE backends ADD COLUMN upstream_budget_scope TEXT",
            [],
        )
        .unwrap();
        assert!(matches!(
            BackendStore::from_connection(conn),
            Err(BackendStoreError::Corrupt)
        ));
    }
    #[test]
    fn old_only_cap_spend_and_unknown_pair_remain_authoritative_after_backend_migration() {
        use crate::ledger::{
            evidence::{
                context::ExecutionContext, AttemptEvidence, DispatchOutcome, EvidenceError,
                IntentAdmission, TransportOutcome, TransportTerminal,
            },
            ReserveOutcome, SqliteLedger,
        };
        use sandhi_core::{Policy, UsageV2, Window};
        use time::{Duration, OffsetDateTime};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("same.db");
        let path = path.to_str().unwrap();
        drop(legacy_connection(
            path,
            &definition(EndpointFamilyV1::AnthropicMessages),
        ));
        let mut ledger = SqliteLedger::open(path).unwrap();
        let now = OffsetDateTime::now_utc();
        ledger
            .set_limit_durable("upstream:anthropic", Some(30), Window::Total, Policy::Block)
            .unwrap();
        let ReserveOutcome::Admitted(spend) = ledger
            .reserve_durable("upstream:anthropic", 5, now, Duration::seconds(60))
            .unwrap()
        else {
            panic!("initial spend")
        };
        ledger.settle_durable(spend.id, 5).unwrap();
        let original:ExecutionContext=serde_json::from_value(serde_json::json!({"version":1,"ingress":"openai_chat_completions","delivery":"buffered","plane":"translation","upstream_ref":"backend/source","upstream_budget_scope":"upstream:anthropic","requested_model":"fixture","actual_model":"fixture","provider":"vendor-one","endpoint_family":"anthropic_messages","transport_binding_sha256":"a".repeat(64),"backend":{"id":"source","revision":1,"credential_ref":"vendor:fixture","credential_generation":1,"endpoint_profile":"standard"},"accounting_policy":{"version":1,"cache_read_weight":1000}})).unwrap();
        let IntentAdmission::Admitted(intent) = ledger
            .reserve_paired_contextual_durable(
                "team",
                20,
                now,
                Duration::seconds(60),
                100,
                "old-request",
                &original,
            )
            .unwrap()
        else {
            panic!("old hold")
        };
        assert!(matches!(
            ledger
                .authorize_dispatch_durable("team", &intent.execution_id)
                .unwrap(),
            DispatchOutcome::Authorized(_)
        ));
        let id = ledger
            .physical_attempt_durable("team", &intent.execution_id)
            .unwrap()
            .unwrap()
            .attempt_id;
        let evidence = AttemptEvidence {
            version: 1,
            attempt_id: id,
            attempt_ordinal: 1,
            dispatch_observed_at_ms: Some(1),
            terminal: Some(TransportTerminal {
                observed_at_ms: 2,
                outcome: TransportOutcome::Success,
                provider_status: Some(200),
                provider_request_id: None,
            }),
            dropped_observations: 0,
            invalid: false,
            observed_provider: None,
        };
        ledger
            .record_terminal_with_attempt_durable(
                "team",
                &intent.execution_id,
                &UsageV2::default(),
                1000,
                &evidence,
            )
            .unwrap();
        let IntentAdmission::Admitted(known) = ledger
            .reserve_paired_contextual_durable(
                "team",
                5,
                now,
                Duration::seconds(60),
                100,
                "known-old-request",
                &original,
            )
            .unwrap()
        else {
            panic!("old qualified hold")
        };
        assert!(matches!(
            ledger
                .authorize_dispatch_durable("team", &known.execution_id)
                .unwrap(),
            DispatchOutcome::Authorized(_)
        ));
        let mut known_evidence = evidence.clone();
        known_evidence.attempt_id = ledger
            .physical_attempt_durable("team", &known.execution_id)
            .unwrap()
            .unwrap()
            .attempt_id;
        let known_usage = UsageV2 {
            tokens_in: 3,
            tokens_out: 2,
            completeness: sandhi_core::UsageCompleteness::Final,
            ..Default::default()
        };
        ledger
            .record_terminal_with_attempt_durable(
                "team",
                &known.execution_id,
                &known_usage,
                1000,
                &known_evidence,
            )
            .unwrap();
        let before = Connection::open(path)
            .unwrap()
            .query_row(
                "SELECT evidence_json FROM budget_physical_attempt WHERE execution_id=?1",
                [&intent.execution_id],
                |r| r.get::<_, String>(0),
            )
            .unwrap();
        drop(ledger);
        let store = BackendStore::open(path).unwrap();
        let mut next = definition(EndpointFamilyV1::GeminiGenerateContent);
        next.provider = "vendor-two".into();
        store.put(&next, Some(1)).unwrap();
        drop(store);
        let store = BackendStore::open(path).unwrap();
        let updated = store.get("source").unwrap().unwrap();
        assert_eq!(updated.upstream_budget_scope, "upstream:anthropic");
        let mut ledger = SqliteLedger::open(path).unwrap();
        assert_eq!(
            ledger
                .execution_context_durable("team", &intent.execution_id)
                .unwrap(),
            Some(original.clone())
        );
        let after = Connection::open(path)
            .unwrap()
            .query_row(
                "SELECT evidence_json FROM budget_physical_attempt WHERE execution_id=?1",
                [&intent.execution_id],
                |r| r.get::<_, String>(0),
            )
            .unwrap();
        assert_eq!(after, before);
        assert_eq!(
            ledger
                .reclaim_expired_durable(now + Duration::hours(1))
                .unwrap(),
            0
        );
        assert!(matches!(
            ledger.settle_terminal_durable("team", &intent.execution_id),
            Err(EvidenceError::UnresolvedObservation)
        ));
        // A genuinely qualified old-v1 execution recovers through its original
        // namespace after the diagnostic identity/family update; no inference occurs.
        ledger
            .settle_terminal_durable("team", &known.execution_id)
            .unwrap();
        assert_eq!(
            ledger
                .execution_context_durable("team", &known.execution_id)
                .unwrap(),
            Some(original.clone())
        );
        let mut selected = original;
        selected.provider = updated.definition.provider;
        selected.endpoint_family = updated.definition.endpoint.endpoint_family;
        selected.upstream_budget_scope = updated.upstream_budget_scope;
        selected.backend.as_mut().unwrap().revision = 2;
        assert!(matches!(
            ledger
                .reserve_paired_contextual_durable(
                    "team",
                    6,
                    now,
                    Duration::seconds(60),
                    100,
                    "new-request",
                    &selected
                )
                .unwrap(),
            IntentAdmission::Denied(_)
        ));
        assert_eq!(ledger.spent_durable("upstream:anthropic").unwrap(), 10);
        assert_eq!(ledger.reserved_durable("upstream:anthropic").unwrap(), 20);
        assert_eq!(ledger.reserved_durable("upstream:vendor-two").unwrap(), 0);
        assert_eq!(
            Connection::open(path)
                .unwrap()
                .query_row("SELECT count(*) FROM budget_reservation", [], |r| r
                    .get::<_, i64>(0))
                .unwrap(),
            5
        );
    }
}
