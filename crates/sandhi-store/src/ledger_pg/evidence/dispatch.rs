//! Postgres dialect of the opt-in durable dispatch fence (SQLite twin:
//! `ledger::evidence::dispatch`). No network execution, retry or HTTP activation.
//! Older writers must not open ledgers once prepared intents are admitted.
//!
//! Serialization note: SQLite's `BEGIN IMMEDIATE` made every transition the whole-file
//! write lock, so a loser always reread the committed winner. On Postgres the same
//! serialization comes from the scope's transactional advisory lock taken at the start
//! of each transition transaction (see [`super::scope_advisory_key`]).
use super::*;

/// Persisted proof of closure before dispatch authorization, re-exported from the
/// shared evidence types.
pub use crate::ledger::evidence::{
    ClosureOutcome, DispatchOutcome, DispatchPermit, PreDispatchClosure,
};

pub(super) enum Fence {
    Prepared,
    Authorized,
    Closed(PreDispatchClosure),
}

/// Caller owns a read or write transaction, so evidence and phase form one snapshot.
pub(super) async fn checked_fence(
    conn: &mut PgConnection,
    scope: &str,
    execution_id: &str,
    reservation_id: u64,
) -> Result<Option<Fence>, EvidenceError> {
    let row: Option<(i64, Option<i64>)> = sqlx::query_as(
        "SELECT phase, transitioned_at FROM budget_dispatch_fence WHERE execution_id = $1",
    )
    .bind(execution_id)
    .fetch_optional(&mut *conn)
    .await?;
    // A new contextual admission cannot silently degrade to an older unbound row.
    context::read(&mut *conn, execution_id).await?;
    paired::read(&mut *conn, execution_id, reservation_id).await?;
    let attempt = attempts::read(&mut *conn, execution_id).await?;
    let Some((phase, transitioned_at)) = row else {
        return Ok(None);
    };
    if transitioned_at.is_some_and(|at| OffsetDateTime::from_unix_timestamp(at).is_err()) {
        return Err(EvidenceError::InconsistentDispatch);
    }
    let (matches, settled, actual, settled_at): (bool, i64, i64, Option<i64>) = sqlx::query_as(
        "SELECT (scope = $2), settled, actual, settled_at \
             FROM budget_reservation WHERE id = $1",
    )
    .bind(reservation_id as i64)
    .bind(scope)
    .fetch_optional(&mut *conn)
    .await?
    .ok_or(EvidenceError::InconsistentDispatch)?;
    if !matches {
        return Err(EvidenceError::WrongScope);
    }
    let observed = read_observation(&mut *conn, execution_id, reservation_id, scope).await?;
    if attempt
        .as_ref()
        .is_some_and(|a| a.evidence.is_some() != observed.is_some())
    {
        return Err(EvidenceError::CorruptObservation);
    }
    let has_receipt: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM budget_settlement_outbox WHERE reservation_id = $1)",
    )
    .bind(reservation_id as i64)
    .fetch_one(&mut *conn)
    .await?;
    match (phase, transitioned_at) {
        (0, None)
            if settled == 0
                && actual == 0
                && settled_at.is_none()
                && observed.is_none()
                && !has_receipt =>
        {
            Ok(Some(Fence::Prepared))
        }
        (1, Some(_)) => {
            let charge = match observed.as_ref() {
                Some(o) => match terminal_charge(&mut *conn, o).await {
                    Ok(value) => Some(value),
                    Err(EvidenceError::UnresolvedObservation(_)) => None,
                    Err(error) => return Err(error),
                },
                None => None,
            };
            checked_terminal_receipt(&mut *conn, scope, reservation_id, charge).await?;
            Ok(Some(Fence::Authorized))
        }
        (2, Some(closed_at))
            if settled == 1
                && actual == 0
                && settled_at == Some(closed_at)
                && observed.is_none()
                && !has_receipt =>
        {
            Ok(Some(Fence::Closed(PreDispatchClosure {
                execution_id: execution_id.to_string(),
                reservation_id,
                scope: scope.to_string(),
                closed_at,
            })))
        }
        _ => Err(EvidenceError::InconsistentDispatch),
    }
}

pub(super) async fn require_observable(
    conn: &mut PgConnection,
    scope: &str,
    execution_id: &str,
    reservation_id: u64,
) -> Result<(), EvidenceError> {
    match checked_fence(conn, scope, execution_id, reservation_id).await? {
        None | Some(Fence::Authorized) => Ok(()),
        _ => Err(EvidenceError::DispatchNotAuthorized),
    }
}

/// One transactional transition shared by authorization and closure. Returning a
/// permit to the external caller happens only AFTER successful commit.
pub(super) async fn transition(
    conn: &mut PgConnection,
    scope: &str,
    execution_id: &str,
    authorize: bool,
) -> Result<(Fence, bool), EvidenceError> {
    let binding = intent_binding(conn, execution_id).await?;
    let (reservation_id, stored_scope, _) = match binding {
        Some(binding) => binding,
        None => {
            let fenced: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM budget_dispatch_fence WHERE execution_id = $1)",
            )
            .bind(execution_id)
            .fetch_one(&mut *conn)
            .await?;
            return Err(if fenced {
                EvidenceError::InconsistentDispatch
            } else {
                EvidenceError::MissingIntent
            });
        }
    };
    if stored_scope != scope {
        return Err(EvidenceError::WrongScope);
    }
    let fence = checked_fence(conn, scope, execution_id, reservation_id)
        .await?
        .ok_or(EvidenceError::UnfencedIntent)?;
    if !matches!(fence, Fence::Prepared) {
        return Ok((fence, false));
    }
    let now = OffsetDateTime::now_utc().unix_timestamp();
    let back = paired::read(conn, execution_id, reservation_id).await?;
    if authorize {
        let expires_at: i64 =
            sqlx::query_scalar("SELECT expires_at FROM budget_reservation WHERE id = $1")
                .bind(reservation_id as i64)
                .fetch_one(&mut *conn)
                .await?;
        if OffsetDateTime::from_unix_timestamp(expires_at).is_err() {
            return Err(EvidenceError::InconsistentDispatch);
        }
        if expires_at <= now {
            return Err(EvidenceError::ExpiredBeforeDispatch);
        }
    }
    let changed = sqlx::query(
        "UPDATE budget_dispatch_fence SET phase = $2, transitioned_at = $3 \
         WHERE execution_id = $1 AND phase = 0",
    )
    .bind(execution_id)
    .bind(if authorize { 1i64 } else { 2i64 })
    .bind(now)
    .execute(&mut *conn)
    .await?;
    if changed.rows_affected() != 1 {
        return Err(EvidenceError::InconsistentDispatch);
    }
    if authorize {
        attempts::allocate(conn, execution_id, now).await?;
        return Ok((Fence::Authorized, true));
    }
    let changed = sqlx::query(
        "UPDATE budget_reservation SET settled = 1, actual = 0, settled_at = $2 \
         WHERE id = $1 AND settled = 0",
    )
    .bind(reservation_id as i64)
    .bind(now)
    .execute(&mut *conn)
    .await?;
    if changed.rows_affected() != 1 {
        return Err(EvidenceError::InconsistentDispatch);
    }
    paired::close(conn, back.as_ref(), now).await?;
    paired::read(conn, execution_id, reservation_id).await?;
    Ok((
        Fence::Closed(PreDispatchClosure {
            execution_id: execution_id.to_string(),
            reservation_id,
            scope: scope.to_string(),
            closed_at: now,
        }),
        true,
    ))
}

impl PostgresLedger {
    /// Commit dispatch authorization before sending. Only the first commit gets a
    /// permit; unknown commit outcomes/replays must never trigger another send.
    pub fn authorize_dispatch_durable(
        &self,
        scope: &str,
        execution_id: &str,
    ) -> Result<DispatchOutcome, EvidenceError> {
        validate_scope(scope)?;
        validate_execution_id(execution_id)?;
        self.block(self.authorize_dispatch_async(scope, execution_id))
    }

    async fn authorize_dispatch_async(
        &self,
        scope: &str,
        execution_id: &str,
    ) -> Result<DispatchOutcome, EvidenceError> {
        let mut tx = self.pool.begin().await?;
        scope_lock(&mut tx, scope).await?;
        let (fence, changed) = transition(&mut tx, scope, execution_id, true).await?;
        let attempt_id = attempts::read(&mut tx, execution_id)
            .await?
            .map(|a| a.attempt_id);
        tx.commit().await?;
        match fence {
            Fence::Authorized if changed => Ok(DispatchOutcome::Authorized(DispatchPermit::new(
                execution_id.to_string(),
                attempt_id,
            ))),
            Fence::Authorized => Ok(DispatchOutcome::MayHaveDispatched),
            Fence::Closed(value) => Ok(DispatchOutcome::ClosedBeforeDispatch(value)),
            Fence::Prepared => Err(EvidenceError::InconsistentDispatch),
        }
    }

    /// Release only an execution proven not to have authorized dispatch. A closed
    /// execution has separate closure evidence, never a fabricated usage receipt.
    /// Failure leaves the caller's intent available for reconciliation, not replay.
    pub fn close_before_dispatch_durable(
        &self,
        scope: &str,
        execution_id: &str,
    ) -> Result<ClosureOutcome, EvidenceError> {
        validate_scope(scope)?;
        validate_execution_id(execution_id)?;
        self.block(self.close_before_dispatch_async(scope, execution_id))
    }

    async fn close_before_dispatch_async(
        &self,
        scope: &str,
        execution_id: &str,
    ) -> Result<ClosureOutcome, EvidenceError> {
        let mut tx = self.pool.begin().await?;
        scope_lock(&mut tx, scope).await?;
        let (fence, changed) = transition(&mut tx, scope, execution_id, false).await?;
        tx.commit().await?;
        match fence {
            Fence::Closed(value) if changed => Ok(ClosureOutcome::Closed(value)),
            Fence::Closed(value) => Ok(ClosureOutcome::AlreadyClosed(value)),
            Fence::Authorized => Ok(ClosureOutcome::MayHaveDispatched),
            Fence::Prepared => Err(EvidenceError::InconsistentDispatch),
        }
    }
}
