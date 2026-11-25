//! Postgres dialect of the immutable admission context (SQLite twin:
//! `ledger::evidence::context`). The context *types* (`ExecutionContext` and friends)
//! are shared verbatim from the SQLite module — only the storage statements differ.
use super::*;

/// The `information_schema.columns` probe set shared by the evidence submodules.
pub(super) use super::attempts::ensure_column;

pub(crate) struct PreparedContext<'a> {
    pub json: &'a str,
    pub paired_scope: Option<&'a str>,
}

pub(super) const TABLE_SQL: &str = "CREATE TABLE IF NOT EXISTS budget_execution_context (
        execution_id text PRIMARY KEY NOT NULL REFERENCES budget_execution_intent(execution_id),
        version      bigint NOT NULL CHECK(version = 1),
        context_json text NOT NULL CHECK(octet_length(context_json) <= 16384)
    )";

pub(super) async fn init(pool: &PgPool) -> Result<(), EvidenceError> {
    sqlx::query(TABLE_SQL).execute(pool).await?;
    ensure_column(
        pool,
        "budget_dispatch_fence",
        "context_version",
        "bigint NOT NULL DEFAULT 0 CHECK(context_version IN (0, 1))",
    )
    .await
}

pub(super) async fn read(
    conn: &mut PgConnection,
    execution_id: &str,
) -> Result<Option<ExecutionContext>, EvidenceError> {
    let row: Option<(Option<i64>, Option<i64>, Option<String>)> = sqlx::query_as(
        "SELECT f.context_version, c.version, \
         CASE WHEN octet_length(c.context_json) <= 16384 THEN c.context_json END \
         FROM budget_execution_intent i LEFT JOIN budget_dispatch_fence f USING(execution_id) \
         LEFT JOIN budget_execution_context c USING(execution_id) WHERE i.execution_id = $1",
    )
    .bind(execution_id)
    .fetch_optional(&mut *conn)
    .await?;
    match row {
        None | Some((None | Some(0), None, None)) => Ok(None),
        Some((Some(1), Some(1), Some(encoded))) => {
            let value: ExecutionContext =
                serde_json::from_str(&encoded).map_err(|_| EvidenceError::CorruptObservation)?;
            // Old rows keep the historical transport policy; everything else must
            // re-encode byte-exactly or it is corruption, never legacy data.
            if value
                .validate_historical()
                .map_err(|_| EvidenceError::CorruptObservation)
                .is_err()
                && value
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

impl PostgresLedger {
    /// Admission + correlation + frozen selection/policy commit in one transaction.
    /// An error does not grant retry or inference authority.
    #[allow(clippy::too_many_arguments)]
    pub fn reserve_contextual_durable(
        &self,
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
        &self,
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
        &self,
        scope: &str,
        ceiling: u64,
        now: OffsetDateTime,
        ttl: Duration,
        retained_limit: usize,
        request_id: &str,
        context: &ExecutionContext,
        progress: Option<&AdmissionProgress>,
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
        &self,
        scope: &str,
        ceiling: u64,
        now: OffsetDateTime,
        ttl: Duration,
        retained_limit: usize,
        request_id: &str,
        context: &ExecutionContext,
        paired: bool,
        progress: Option<&AdmissionProgress>,
    ) -> Result<IntentAdmission, EvidenceError> {
        super::mark_admission(progress, AdmissionStage::ValidateContext);
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
        self.block(self.execution_context_async(scope, execution_id))
    }

    async fn execution_context_async(
        &self,
        scope: &str,
        execution_id: &str,
    ) -> Result<Option<ExecutionContext>, EvidenceError> {
        let mut tx = self.pool.begin().await?;
        let (_, stored_scope, _) = intent_binding(&mut tx, execution_id)
            .await?
            .ok_or(EvidenceError::MissingIntent)?;
        if stored_scope != scope {
            return Err(EvidenceError::WrongScope);
        }
        let value = read(&mut tx, execution_id).await?;
        tx.commit().await?;
        Ok(value)
    }
}

/// The shared context-label validator (same bound as the SQLite module's private
/// `label`): non-empty after trim, capped length, no control characters.
pub(super) fn label(value: &str, max: usize) -> bool {
    !value.trim().is_empty() && value.len() <= max && !value.chars().any(char::is_control)
}
