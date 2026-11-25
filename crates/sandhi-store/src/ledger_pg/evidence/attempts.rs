//! Postgres dialect of the single physical-attempt identity and immutable transport
//! evidence (SQLite twin: `ledger::evidence::attempts`). The evidence *types*
//! (`AttemptEvidence` and friends) are shared verbatim — only the storage statements
//! are dialect-specific: `typeof`/`length(CAST(.. AS BLOB))` guards become
//! `octet_length` CASE projections, and the additive `budget_dispatch_fence` columns
//! arrive through `information_schema.columns` probes instead of `pragma_table_info`.
use super::*;

/// Create the attempt table (idempotent) and evolve the dispatch fence with the same
/// additive columns the SQLite backend's `pragma_table_info` probes install.
pub(super) const TABLE_SQL: &str = "CREATE TABLE IF NOT EXISTS budget_physical_attempt (
        execution_id  text PRIMARY KEY NOT NULL REFERENCES budget_execution_intent(execution_id),
        attempt_id    text UNIQUE NOT NULL CHECK(octet_length(attempt_id) = 64),
        authorized_at bigint NOT NULL,
        evidence_json text CHECK(octet_length(evidence_json) <= 4096)
    )";

pub(super) async fn init(pool: &PgPool) -> Result<(), EvidenceError> {
    sqlx::query(TABLE_SQL).execute(pool).await?;
    for (name, declaration) in [
        (
            "attempt_version",
            "bigint NOT NULL DEFAULT 0 CHECK(attempt_version IN (0, 1))",
        ),
        ("attempt_id", "text CHECK(octet_length(attempt_id) = 64)"),
    ] {
        ensure_column(pool, "budget_dispatch_fence", name, declaration).await?;
    }
    Ok(())
}

/// The `information_schema.columns` probe that replaces SQLite's `pragma_table_info`
/// probe, column-for-column. Bounded to the session schema so per-test schemas isolate.
pub(super) async fn column_exists(
    pool: &PgPool,
    table: &str,
    column: &str,
) -> Result<bool, EvidenceError> {
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

/// Additive migration step: `ALTER TABLE .. ADD COLUMN` when the probe says the column
/// is missing (matching the SQLite evidence schema evolution exactly).
pub(super) async fn ensure_column(
    pool: &PgPool,
    table: &str,
    column: &str,
    declaration: &str,
) -> Result<(), EvidenceError> {
    if !column_exists(pool, table, column).await? {
        sqlx::query(&format!(
            "ALTER TABLE {table} ADD COLUMN {column} {declaration}"
        ))
        .execute(pool)
        .await?;
    }
    Ok(())
}

pub(super) async fn allocate(
    conn: &mut PgConnection,
    execution_id: &str,
    at: i64,
) -> Result<(), EvidenceError> {
    let required: i64 = sqlx::query_scalar(
        "SELECT attempt_version FROM budget_dispatch_fence WHERE execution_id = $1",
    )
    .bind(execution_id)
    .fetch_one(&mut *conn)
    .await?;
    if required == 1 {
        let id = opaque_id()?;
        sqlx::query(
            "INSERT INTO budget_physical_attempt(execution_id, attempt_id, authorized_at) \
             VALUES ($1, $2, $3)",
        )
        .bind(execution_id)
        .bind(&id)
        .bind(at)
        .execute(&mut *conn)
        .await?;
        sqlx::query("UPDATE budget_dispatch_fence SET attempt_id = $2 WHERE execution_id = $1")
            .bind(execution_id)
            .bind(&id)
            .execute(&mut *conn)
            .await?;
    }
    Ok(())
}

pub(super) async fn read(
    conn: &mut PgConnection,
    execution_id: &str,
) -> Result<Option<PhysicalAttempt>, EvidenceError> {
    type Row = (
        Option<i64>,
        Option<i64>,
        Option<i64>,
        Option<String>,
        Option<String>,
        Option<i64>,
        Option<String>,
        bool,
    );
    // The octet_length CASE projections bound allocation even if a newer/foreign writer
    // violated this version's contract; a bound-violating value projects NULL and the
    // row falls to the corruption arm below (the dialect of the SQLite typeof guards).
    let row: Option<Row> = sqlx::query_as(
        "SELECT f.attempt_version, f.phase, f.transitioned_at, \
         CASE WHEN f.attempt_id IS NULL THEN NULL \
              WHEN octet_length(f.attempt_id) = 64 THEN f.attempt_id ELSE '!' END, \
         CASE WHEN octet_length(a.attempt_id) = 64 THEN a.attempt_id END, \
         a.authorized_at, \
         CASE WHEN octet_length(a.evidence_json) <= 4096 THEN a.evidence_json END, \
         a.evidence_json IS NOT NULL \
         FROM budget_execution_intent i LEFT JOIN budget_dispatch_fence f USING(execution_id) \
         LEFT JOIN budget_physical_attempt a USING(execution_id) WHERE i.execution_id = $1",
    )
    .bind(execution_id)
    .fetch_optional(&mut *conn)
    .await?;
    let (id, at, json, has_json) = match row {
        None
        | Some((None | Some(0), _, _, None, None, None, None, false))
        | Some((Some(1), Some(0 | 2), _, None, None, None, None, false)) => return Ok(None),
        Some((
            Some(1),
            Some(1),
            Some(at),
            Some(required),
            Some(id),
            Some(authorized),
            json,
            has_json,
        )) if required == id && at == authorized => (id, at, json, has_json),
        _ => return Err(EvidenceError::CorruptObservation),
    };
    validate_execution_id(&id).map_err(|_| EvidenceError::CorruptObservation)?;
    if OffsetDateTime::from_unix_timestamp(at).is_err() || (has_json && json.is_none()) {
        return Err(EvidenceError::CorruptObservation);
    }
    let evidence = json
        .map(|json| {
            let evidence: AttemptEvidence =
                serde_json::from_str(&json).map_err(|_| EvidenceError::CorruptObservation)?;
            if evidence.attempt_id != id
                || evidence
                    .encode()
                    .map_err(|_| EvidenceError::CorruptObservation)?
                    != json
            {
                return Err(EvidenceError::CorruptObservation);
            }
            Ok(evidence)
        })
        .transpose()?;
    Ok(Some(PhysicalAttempt {
        attempt_id: id,
        authorized_at: at,
        evidence,
    }))
}

pub(super) async fn record(
    conn: &mut PgConnection,
    execution_id: &str,
    evidence: Option<&AttemptEvidence>,
) -> Result<(), EvidenceError> {
    match (read(&mut *conn, execution_id).await?, evidence) {
        (None, None) => Ok(()),
        (Some(attempt), Some(evidence)) if attempt.attempt_id == evidence.attempt_id => {
            let json = evidence.encode()?;
            if let Some(original) = attempt.evidence {
                return if original == *evidence {
                    Ok(())
                } else {
                    Err(EvidenceError::ConflictingObservation)
                };
            }
            let changed = sqlx::query(
                "UPDATE budget_physical_attempt SET evidence_json = $2 \
                 WHERE execution_id = $1 AND evidence_json IS NULL",
            )
            .bind(execution_id)
            .bind(&json)
            .execute(&mut *conn)
            .await?;
            if changed.rows_affected() != 1 {
                return Err(EvidenceError::InconsistentDispatch);
            }
            Ok(())
        }
        _ => Err(EvidenceError::ConflictingObservation),
    }
}

impl PostgresLedger {
    /// Scope-checked evidence lookup; this ID grants no inference/replay authority.
    pub fn physical_attempt_durable(
        &self,
        scope: &str,
        execution_id: &str,
    ) -> Result<Option<PhysicalAttempt>, EvidenceError> {
        validate_scope(scope)?;
        validate_execution_id(execution_id)?;
        self.block(self.physical_attempt_async(scope, execution_id))
    }

    async fn physical_attempt_async(
        &self,
        scope: &str,
        execution_id: &str,
    ) -> Result<Option<PhysicalAttempt>, EvidenceError> {
        let mut tx = self.pool.begin().await?;
        let (id, stored_scope, _) = intent_binding(&mut tx, execution_id)
            .await?
            .ok_or(EvidenceError::MissingIntent)?;
        if stored_scope != scope {
            return Err(EvidenceError::WrongScope);
        }
        dispatch::checked_fence(&mut tx, scope, execution_id, id).await?;
        let attempt = attempts::read(&mut tx, execution_id).await?;
        tx.commit().await?;
        Ok(attempt)
    }
}
