//! Postgres dialect of "two reservations, one execution/observation/receipt" (SQLite
//! twin: `ledger::evidence::paired`). State-mirror validation is a direct port; the
//! `typeof(scope)='text'` bound becomes an `octet_length` CASE projection.
use super::attempts::ensure_column;
use super::*;

pub(super) const TABLE_SQL: &str = "CREATE TABLE IF NOT EXISTS budget_execution_back_leg (
        execution_id  text PRIMARY KEY NOT NULL REFERENCES budget_execution_intent(execution_id),
        version       bigint NOT NULL CHECK(version = 1),
        reservation_id bigint UNIQUE NOT NULL REFERENCES budget_reservation(id) CHECK(reservation_id > 0)
    )";

pub(super) async fn init(pool: &PgPool) -> Result<(), EvidenceError> {
    sqlx::query(TABLE_SQL).execute(pool).await?;
    ensure_column(
        pool,
        "budget_dispatch_fence",
        "back_reservation_id",
        "bigint CHECK(back_reservation_id > 0)",
    )
    .await?;
    sqlx::query(
        "CREATE UNIQUE INDEX IF NOT EXISTS idx_dispatch_back_reservation \
         ON budget_dispatch_fence(back_reservation_id) WHERE back_reservation_id IS NOT NULL",
    )
    .execute(pool)
    .await?;
    Ok(())
}

#[derive(PartialEq, Eq)]
struct State {
    actual: i64,
    settled: i64,
    at: Option<i64>,
}

type ReservationRow = (Option<String>, i64, i64, i64, i64, Option<i64>);

async fn reservation(
    conn: &mut PgConnection,
    id: u64,
) -> Result<(Reservation, State), EvidenceError> {
    let row: Option<ReservationRow> = sqlx::query_as(
        "SELECT CASE WHEN octet_length(scope) <= 4096 THEN scope END, \
         ceiling, expires_at, actual, settled, settled_at \
         FROM budget_reservation WHERE id = $1",
    )
    .bind(id as i64)
    .fetch_optional(&mut *conn)
    .await?;
    let (scope, ceiling, expires, actual, settled, at) =
        row.ok_or(EvidenceError::MissingReservation)?;
    let scope = scope.ok_or(EvidenceError::CorruptObservation)?;
    validate_scope(&scope)?;
    if id == 0
        || id > i64::MAX as u64
        || ceiling < 0
        || actual < 0
        || !matches!((settled, at.as_ref()), (0, None) | (1, Some(_)))
        || (settled == 0 && actual != 0)
        || at.is_some_and(|at| OffsetDateTime::from_unix_timestamp(at).is_err())
    {
        return Err(EvidenceError::InconsistentSettlement);
    }
    Ok((
        Reservation {
            id,
            scope,
            ceiling: ceiling as u64,
            expires_at: OffsetDateTime::from_unix_timestamp(expires)
                .map_err(|_| EvidenceError::CorruptObservation)?,
        },
        State {
            actual,
            settled,
            at,
        },
    ))
}

/// Validate required linkage and both reservation states in the caller's transaction.
/// A linked back leg has no independent receipt or caller-selected charge.
pub(super) async fn read(
    conn: &mut PgConnection,
    execution_id: &str,
    primary_id: u64,
) -> Result<Option<Reservation>, EvidenceError> {
    let row: Option<(Option<i64>, Option<i64>, Option<i64>)> = sqlx::query_as(
        "SELECT f.back_reservation_id, b.version, b.reservation_id \
         FROM budget_execution_intent i \
         LEFT JOIN budget_dispatch_fence f USING(execution_id) \
         LEFT JOIN budget_execution_back_leg b USING(execution_id) WHERE i.execution_id = $1",
    )
    .bind(execution_id)
    .fetch_optional(&mut *conn)
    .await?;
    let id = match row {
        None | Some((None, None, None)) => return Ok(None),
        Some((Some(required), Some(1), Some(id)))
            if required == id && id > 0 && id as u64 != primary_id =>
        {
            id
        }
        _ => return Err(EvidenceError::CorruptObservation),
    };
    let context = context::read(&mut *conn, execution_id)
        .await?
        .ok_or(EvidenceError::CorruptObservation)?;
    let (primary, primary_state) = reservation(&mut *conn, primary_id).await?;
    let (back, back_state) = reservation(&mut *conn, id as u64).await?;
    let other_claim: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM budget_execution_intent WHERE reservation_id = $1) \
         OR EXISTS(SELECT 1 FROM budget_settlement_outbox WHERE reservation_id = $1)",
    )
    .bind(id)
    .fetch_one(&mut *conn)
    .await?;
    if other_claim
        || back.scope != context.upstream_budget_scope
        || back.scope == primary.scope
        || back.ceiling != primary.ceiling
        || back.expires_at != primary.expires_at
        || back_state != primary_state
    {
        return Err(EvidenceError::InconsistentSettlement);
    }
    Ok(Some(back))
}

pub(super) async fn close(
    conn: &mut PgConnection,
    back: Option<&Reservation>,
    at: i64,
) -> Result<(), EvidenceError> {
    if let Some(back) = back {
        let changed = sqlx::query(
            "UPDATE budget_reservation SET settled = 1, actual = 0, settled_at = $2 \
             WHERE id = $1 AND settled = 0",
        )
        .bind(back.id as i64)
        .bind(at)
        .execute(&mut *conn)
        .await?;
        if changed.rows_affected() != 1 {
            return Err(EvidenceError::InconsistentSettlement);
        }
    }
    Ok(())
}

pub(super) async fn settle(
    conn: &mut PgConnection,
    back: Option<&Reservation>,
    outcome: &SettlementOutcome,
) -> Result<(), EvidenceError> {
    if let Some(back) = back {
        let receipt = match outcome {
            SettlementOutcome::Committed(r) | SettlementOutcome::AlreadyCommitted(r) => r,
        };
        sqlx::query(
            "UPDATE budget_reservation SET settled = 1, actual = $2, settled_at = $3 \
             WHERE id = $1 AND settled = 0",
        )
        .bind(back.id as i64)
        .bind(receipt.charged_tokens as i64)
        .bind(receipt.settled_at)
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}

impl PostgresLedger {
    /// Scope-checked evidence lookup; this ID grants no inference/replay authority.
    pub fn execution_back_leg_durable(
        &self,
        scope: &str,
        execution_id: &str,
    ) -> Result<Option<Reservation>, EvidenceError> {
        validate_scope(scope)?;
        validate_execution_id(execution_id)?;
        self.block(self.execution_back_leg_async(scope, execution_id))
    }

    async fn execution_back_leg_async(
        &self,
        scope: &str,
        execution_id: &str,
    ) -> Result<Option<Reservation>, EvidenceError> {
        let mut tx = self.pool.begin().await?;
        let (id, stored_scope, _) = intent_binding(&mut tx, execution_id)
            .await?
            .ok_or(EvidenceError::MissingIntent)?;
        if stored_scope != scope {
            return Err(EvidenceError::WrongScope);
        }
        let back = read(&mut tx, execution_id, id).await?;
        tx.commit().await?;
        Ok(back)
    }
}
