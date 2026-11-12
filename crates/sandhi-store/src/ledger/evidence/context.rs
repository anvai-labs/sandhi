//! Immutable admission context, separate from proof of a physical send or receipt.
use super::*;
use sandhi_core::{allowance::Weight, CacheCapabilityProfileV1, CacheIntentV1, EndpointFamilyV1};
use serde::{Deserialize, Serialize};

pub const MAX_CONTEXT_BYTES: usize = 16_384;
pub(super) struct PreparedContext<'a> {
    pub json: &'a str,
    pub paired_scope: Option<&'a str>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Delivery {
    Buffered,
    Streaming,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Plane {
    Transparent,
    Translation,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouteContext {
    pub id: String,
    pub revision: u64,
    pub binding_sha256: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BackendContext {
    pub id: String,
    pub revision: u64,
    pub credential_ref: String,
    pub credential_generation: u64,
    pub endpoint_profile: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CacheContext {
    pub plan_sha256: String,
    pub binding_sha256: String,
    pub intent: CacheIntentV1,
    pub capability: CacheCapabilityProfileV1,
    pub native_preserved: bool,
}
/// Version 1 freezes the existing weighted-billable formula, not dollars or quota.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AccountingPolicy {
    pub version: u8,
    pub cache_read_weight: Weight,
}
/// No arbitrary extensions, prompt/body, raw identity hint, URL, header or secret.
/// This is trusted composition data; deserialization does not confer authority.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionContext {
    pub version: u8,
    pub ingress: EndpointFamilyV1,
    pub delivery: Delivery,
    pub plane: Plane,
    pub upstream_ref: String,
    pub upstream_budget_scope: String,
    pub requested_model: String,
    pub actual_model: String,
    pub provider: String,
    pub endpoint_family: EndpointFamilyV1,
    /// Wrapped actual transport identity; never the standalone credential digest.
    pub transport_binding_sha256: String,
    pub route: Option<RouteContext>,
    pub backend: Option<BackendContext>,
    pub cache: Option<CacheContext>,
    pub accounting_policy: AccountingPolicy,
    pub content_policy_revision: Option<u64>,
}

fn label(value: &str, max: usize) -> bool {
    !value.trim().is_empty() && value.len() <= max && !value.chars().any(char::is_control)
}
fn digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
impl ExecutionContext {
    pub fn validate(&self) -> Result<(), EvidenceError> {
        let valid = self.version == 1
            && self.accounting_policy.version == 1
            && label(&self.upstream_ref, 512)
            && label(&self.upstream_budget_scope, 512)
            && label(&self.requested_model, 512)
            && label(&self.actual_model, 512)
            && label(&self.provider, 128)
            && digest(&self.transport_binding_sha256)
            && self.content_policy_revision.is_none_or(|v| v > 0)
            && self.route.as_ref().is_none_or(|r| {
                label(&r.id, 128)
                    && r.revision > 0
                    && digest(&r.binding_sha256)
                    && self.upstream_ref == format!("route/{}", r.id)
                    && self.backend.is_some()
            })
            && self.backend.as_ref().is_none_or(|b| {
                label(&b.id, 128)
                    && b.revision > 0
                    && label(&b.credential_ref, 512)
                    && b.credential_generation > 0
                    && matches!(
                        b.endpoint_profile.as_str(),
                        "standard"
                            | "chatgpt_codex"
                            | "anthropic_implicit_cache"
                            | "openai_terminal_chunk_usage"
                    )
                    && (b.endpoint_profile != "anthropic_implicit_cache"
                        || self.endpoint_family == EndpointFamilyV1::AnthropicMessages)
                    && (b.endpoint_profile != "openai_terminal_chunk_usage"
                        || self.endpoint_family == EndpointFamilyV1::OpenaiChatCompletions)
                    && (b.endpoint_profile != "chatgpt_codex"
                        || self.endpoint_family == EndpointFamilyV1::OpenaiResponses)
            })
            && self.cache.as_ref().is_none_or(|c| {
                digest(&c.plan_sha256) && digest(&c.binding_sha256) && c.intent.validate().is_ok()
            });
        if !valid {
            return Err(EvidenceError::InvalidInput);
        }
        Ok(())
    }
    pub(super) fn encode(&self) -> Result<String, EvidenceError> {
        self.validate()?;
        let encoded = serde_json::to_string(self).map_err(|_| EvidenceError::InvalidInput)?;
        if encoded.len() > MAX_CONTEXT_BYTES {
            return Err(EvidenceError::InvalidInput);
        }
        Ok(encoded)
    }
}

pub(super) fn init(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS budget_execution_context (
        execution_id TEXT PRIMARY KEY NOT NULL REFERENCES budget_execution_intent(execution_id),
        version INTEGER NOT NULL CHECK(version=1),
        context_json TEXT NOT NULL CHECK(length(CAST(context_json AS BLOB))<=16384)
    );",
    )?;
    let exists: bool = conn.query_row("SELECT EXISTS(SELECT 1 FROM pragma_table_info('budget_dispatch_fence') WHERE name='context_version')", [], |r| r.get(0))?;
    if !exists {
        conn.execute("ALTER TABLE budget_dispatch_fence ADD COLUMN context_version INTEGER NOT NULL DEFAULT 0 CHECK(context_version IN (0,1))", [])?;
    }
    Ok(())
}

pub(super) fn read(
    conn: &Connection,
    execution_id: &str,
) -> Result<Option<ExecutionContext>, EvidenceError> {
    let row: Option<(Option<i64>, Option<i64>, Option<String>)> = conn.query_row(
        "SELECT f.context_version,c.version,CASE WHEN typeof(c.context_json)='text' AND length(CAST(c.context_json AS BLOB))<=16384 THEN c.context_json END
         FROM budget_execution_intent i LEFT JOIN budget_dispatch_fence f USING(execution_id)
         LEFT JOIN budget_execution_context c USING(execution_id) WHERE i.execution_id=?1",
        [execution_id], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
    match row {
        None | Some((None | Some(0), None, None)) => Ok(None),
        Some((Some(1), Some(1), Some(encoded))) => {
            let value: ExecutionContext =
                serde_json::from_str(&encoded).map_err(|_| EvidenceError::CorruptObservation)?;
            if value
                .encode()
                .map_err(|_| EvidenceError::CorruptObservation)?
                != encoded
            {
                return Err(EvidenceError::CorruptObservation);
            }
            Ok(Some(value))
        }
        _ => Err(EvidenceError::CorruptObservation),
    }
}

impl SqliteLedger {
    /// Admission + correlation + frozen selection/policy commit in one transaction.
    /// An error does not grant retry or inference authority.
    #[allow(clippy::too_many_arguments)]
    pub fn reserve_contextual_durable(
        &mut self,
        scope: &str,
        ceiling: u64,
        now: OffsetDateTime,
        ttl: Duration,
        retained_limit: usize,
        request_id: &str,
        context: &ExecutionContext,
    ) -> Result<IntentAdmission, EvidenceError> {
        self.reserve_contextual(
            scope,
            ceiling,
            now,
            ttl,
            retained_limit,
            request_id,
            context,
            false,
            None,
        )
    }
    /// Atomically admit both client and upstream scopes with one selected context.
    #[allow(clippy::too_many_arguments)]
    pub fn reserve_paired_contextual_durable(
        &mut self,
        scope: &str,
        ceiling: u64,
        now: OffsetDateTime,
        ttl: Duration,
        retained_limit: usize,
        request_id: &str,
        context: &ExecutionContext,
    ) -> Result<IntentAdmission, EvidenceError> {
        self.reserve_paired_contextual_durable_observed(
            scope,
            ceiling,
            now,
            ttl,
            retained_limit,
            request_id,
            context,
            None,
        )
    }

    /// Same atomic admission, with optional process-local stage observations only.
    #[allow(clippy::too_many_arguments)]
    pub fn reserve_paired_contextual_durable_observed(
        &mut self,
        scope: &str,
        ceiling: u64,
        now: OffsetDateTime,
        ttl: Duration,
        retained_limit: usize,
        request_id: &str,
        context: &ExecutionContext,
        progress: Option<&super::AdmissionProgress>,
    ) -> Result<IntentAdmission, EvidenceError> {
        self.reserve_contextual(
            scope,
            ceiling,
            now,
            ttl,
            retained_limit,
            request_id,
            context,
            true,
            progress,
        )
    }
    #[allow(clippy::too_many_arguments)]
    fn reserve_contextual(
        &mut self,
        scope: &str,
        ceiling: u64,
        now: OffsetDateTime,
        ttl: Duration,
        retained_limit: usize,
        request_id: &str,
        context: &ExecutionContext,
        paired: bool,
        progress: Option<&super::AdmissionProgress>,
    ) -> Result<IntentAdmission, EvidenceError> {
        super::mark_admission(progress, super::AdmissionStage::ValidateContext);
        if !self.is_file_backed() {
            return Err(EvidenceError::UnsupportedTrackedLedger);
        }
        if !label(request_id, 4096) {
            return Err(EvidenceError::InvalidInput);
        }
        let encoded = context.encode()?;
        self.reserve_intent(
            scope,
            ceiling,
            now,
            ttl,
            retained_limit,
            true,
            Some(request_id),
            Some(PreparedContext {
                json: &encoded,
                paired_scope: paired.then_some(context.upstream_budget_scope.as_str()),
            }),
            progress,
        )
    }
    pub fn execution_context_durable(
        &self,
        scope: &str,
        execution_id: &str,
    ) -> Result<Option<ExecutionContext>, EvidenceError> {
        validate_scope(scope)?;
        validate_execution_id(execution_id)?;
        let tx = self.conn.unchecked_transaction()?;
        let (_, stored_scope, _) =
            intent_binding(&tx, execution_id)?.ok_or(EvidenceError::MissingIntent)?;
        if stored_scope != scope {
            return Err(EvidenceError::WrongScope);
        }
        let value = read(&tx, execution_id)?;
        tx.commit()?;
        Ok(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context() -> ExecutionContext {
        ExecutionContext {
            version: 1,
            ingress: EndpointFamilyV1::AnthropicMessages,
            delivery: Delivery::Buffered,
            plane: Plane::Translation,
            upstream_ref: "openai:default".into(),
            upstream_budget_scope: "upstream:openai".into(),
            requested_model: "physical".into(),
            actual_model: "physical".into(),
            provider: "openai".into(),
            endpoint_family: EndpointFamilyV1::OpenaiChatCompletions,
            transport_binding_sha256: "a".repeat(64),
            route: None,
            backend: None,
            cache: None,
            accounting_policy: AccountingPolicy {
                version: 1,
                cache_read_weight: Weight::new(1000).unwrap(),
            },
            content_policy_revision: None,
        }
    }
    fn admit(
        ledger: &mut SqliteLedger,
        context: &ExecutionContext,
    ) -> Result<ExecutionIntent, EvidenceError> {
        match ledger.reserve_contextual_durable(
            "team",
            100,
            OffsetDateTime::now_utc(),
            Duration::seconds(60),
            100,
            "request",
            context,
        )? {
            IntentAdmission::Admitted(intent) => Ok(intent),
            _ => panic!("unlimited admission"),
        }
    }
    #[test]
    fn terminal_chat_profile_persists_only_for_its_declared_endpoint_family() {
        let dir = tempfile::tempdir().unwrap();
        let mut ledger =
            SqliteLedger::open(dir.path().join("ledger.db").to_str().unwrap()).unwrap();
        let mut value = context();
        value.upstream_ref = "backend/fixture".into();
        value.backend = Some(BackendContext {
            id: "fixture".into(),
            revision: 1,
            credential_ref: "openai:fixture".into(),
            credential_generation: 1,
            endpoint_profile: "openai_terminal_chunk_usage".into(),
        });
        let intent = admit(&mut ledger, &value).unwrap();
        assert_eq!(
            ledger
                .execution_context_durable("team", &intent.execution_id)
                .unwrap(),
            Some(value.clone())
        );
        for family in [
            EndpointFamilyV1::OpenaiResponses,
            EndpointFamilyV1::AnthropicMessages,
            EndpointFamilyV1::GeminiGenerateContent,
        ] {
            value.endpoint_family = family;
            assert!(matches!(value.validate(), Err(EvidenceError::InvalidInput)));
        }
    }
    #[test]
    fn context_admission_is_atomic_and_settlement_cannot_change_its_weight() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.db");
        let path = path.to_str().unwrap();
        let mut ledger = SqliteLedger::open(path).unwrap();
        ledger.conn.execute_batch("CREATE TRIGGER fail_context BEFORE INSERT ON budget_execution_context BEGIN SELECT RAISE(ABORT,'fixture'); END;").unwrap();
        assert!(admit(&mut ledger, &context()).is_err());
        for table in [
            "budget_reservation",
            "budget_execution_intent",
            "budget_dispatch_fence",
            "budget_request_correlation",
            "budget_execution_context",
        ] {
            assert_eq!(
                ledger
                    .conn
                    .query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r
                        .get::<_, i64>(0))
                    .unwrap(),
                0
            );
        }
        ledger
            .conn
            .execute_batch("DROP TRIGGER fail_context")
            .unwrap();
        let desired = context();
        let intent = admit(&mut ledger, &desired).unwrap();
        assert_eq!(
            ledger
                .execution_context_durable("team", &intent.execution_id)
                .unwrap(),
            Some(desired.clone())
        );
        assert!(matches!(
            ledger.execution_context_durable("wrong", &intent.execution_id),
            Err(EvidenceError::WrongScope)
        ));
        assert!(matches!(
            ledger
                .authorize_dispatch_durable("team", &intent.execution_id)
                .unwrap(),
            DispatchOutcome::Authorized(_)
        ));
        let usage = UsageV2 {
            tokens_in: 3,
            tokens_out: 2,
            cache_read_tokens: 10,
            completeness: UsageCompleteness::Final,
            ..Default::default()
        };
        assert!(matches!(
            ledger.record_terminal_durable("team", &intent.execution_id, &usage, 10_000),
            Err(EvidenceError::ConflictingCharge)
        ));
        assert!(ledger
            .terminal_durable("team", &intent.execution_id)
            .unwrap()
            .is_none());
        ledger
            .record_terminal_durable("team", &intent.execution_id, &usage, 1000)
            .unwrap();
        drop(ledger);
        let mut ledger = SqliteLedger::open(path).unwrap();
        assert_eq!(
            ledger
                .execution_context_durable("team", &intent.execution_id)
                .unwrap(),
            Some(desired)
        );
        assert!(
            matches!(ledger.settle_terminal_durable("team",&intent.execution_id).unwrap(),SettlementOutcome::Committed(receipt) if receipt.charged_tokens==6)
        );
        assert!(
            matches!(ledger.settle_terminal_durable("team",&intent.execution_id).unwrap(),SettlementOutcome::AlreadyCommitted(receipt) if receipt.charged_tokens==6)
        );
    }
    #[test]
    fn missing_or_corrupt_required_context_never_degrades_to_legacy() {
        for corrupt in ["missing", "unknown-field", "oversized", "version", "marker"] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("ledger.db");
            let mut ledger = SqliteLedger::open(path.to_str().unwrap()).unwrap();
            let intent = admit(&mut ledger, &context()).unwrap();
            ledger
                .conn
                .execute_batch("PRAGMA ignore_check_constraints=ON")
                .unwrap();
            match corrupt {
                "missing" => {
                    ledger
                        .conn
                        .execute("DELETE FROM budget_execution_context", [])
                        .unwrap();
                }
                "unknown-field" => {
                    ledger.conn.execute("UPDATE budget_execution_context SET context_json=json_set(context_json,'$.secret','forbidden')",[]).unwrap();
                }
                "oversized" => {
                    ledger
                        .conn
                        .execute(
                            "UPDATE budget_execution_context SET context_json=?1",
                            ["x".repeat(MAX_CONTEXT_BYTES + 1)],
                        )
                        .unwrap();
                }
                "version" => {
                    ledger
                        .conn
                        .execute("UPDATE budget_execution_context SET version=2", [])
                        .unwrap();
                }
                "marker" => {
                    ledger
                        .conn
                        .execute("UPDATE budget_dispatch_fence SET context_version=0", [])
                        .unwrap();
                }
                _ => unreachable!(),
            }
            assert!(
                matches!(
                    ledger.execution_context_durable("team", &intent.execution_id),
                    Err(EvidenceError::CorruptObservation)
                ),
                "{corrupt}"
            );
            assert!(
                ledger
                    .authorize_dispatch_durable("team", &intent.execution_id)
                    .is_err(),
                "{corrupt}"
            );
        }
    }
    #[test]
    fn old_context_free_intents_migrate_without_invented_evidence() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.db");
        let path = path.to_str().unwrap();
        let mut ledger = SqliteLedger::open(path).unwrap();
        let IntentAdmission::Admitted(intent) = ledger
            .reserve_prepared_durable(
                "team",
                100,
                OffsetDateTime::now_utc(),
                Duration::seconds(60),
                100,
            )
            .unwrap()
        else {
            panic!("admission")
        };
        ledger
            .conn
            .execute_batch("ALTER TABLE budget_dispatch_fence DROP COLUMN context_version")
            .unwrap();
        drop(ledger);
        let mut ledger = SqliteLedger::open(path).unwrap();
        assert_eq!(
            ledger
                .execution_context_durable("team", &intent.execution_id)
                .unwrap(),
            None
        );
        assert!(matches!(
            ledger
                .authorize_dispatch_durable("team", &intent.execution_id)
                .unwrap(),
            DispatchOutcome::Authorized(_)
        ));
        let mut invalid = context();
        invalid.actual_model = "x".repeat(513);
        assert!(matches!(
            admit(&mut ledger, &invalid),
            Err(EvidenceError::InvalidInput)
        ));
    }
}
