//! Opt-in durable dispatch fence. No network execution, retry or HTTP activation.
//! Older writers must not open ledgers once prepared intents are admitted.
use super::*;

/// Persisted proof of closure before dispatch authorization, not provider usage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreDispatchClosure {
    pub execution_id: String,
    pub reservation_id: u64,
    pub scope: String,
    pub closed_at: i64,
}

/// Only the transaction that changes Prepared to Authorized receives this value.
/// Deliberately not Clone/Deserialize. Losing it never permits inference replay.
/// This is an in-process permit, not authorization to access an external service.
#[derive(Debug)]
#[must_use]
pub struct DispatchPermit {
    execution_id: String,
}
impl DispatchPermit {
    pub fn execution_id(&self) -> &str {
        &self.execution_id
    }
}

#[derive(Debug)]
pub enum DispatchOutcome {
    Authorized(DispatchPermit),
    MayHaveDispatched,
    ClosedBeforeDispatch(PreDispatchClosure),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClosureOutcome {
    Closed(PreDispatchClosure),
    AlreadyClosed(PreDispatchClosure),
    MayHaveDispatched,
}

pub(super) enum Fence {
    Prepared,
    Authorized,
    Closed(PreDispatchClosure),
}

/// Caller owns a read or write transaction, so evidence and phase form one snapshot.
pub(super) fn checked_fence(
    conn: &Connection,
    scope: &str,
    execution_id: &str,
    reservation_id: u64,
) -> Result<Option<Fence>, EvidenceError> {
    let row: Option<(i64, Option<i64>)> = conn
        .query_row(
            "SELECT phase, transitioned_at FROM budget_dispatch_fence WHERE execution_id=?1",
            [execution_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let Some((phase, transitioned_at)) = row else {
        return Ok(None);
    };
    if transitioned_at.is_some_and(|at| OffsetDateTime::from_unix_timestamp(at).is_err()) {
        return Err(EvidenceError::InconsistentDispatch);
    }
    let (matches, settled, actual, settled_at): (bool, i64, i64, Option<i64>) = conn
        .query_row(
            "SELECT scope=?2, settled, actual, settled_at FROM budget_reservation WHERE id=?1",
            params![reservation_id, scope],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .optional()?
        .ok_or(EvidenceError::InconsistentDispatch)?;
    if !matches {
        return Err(EvidenceError::WrongScope);
    }
    let observed = read_observation(conn, execution_id, reservation_id, scope)?;
    let has_receipt: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM budget_settlement_outbox WHERE reservation_id=?1)",
        [reservation_id],
        |r| r.get(0),
    )?;
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
            let charge = match observed.as_ref().map(terminal_charge).transpose() {
                Ok(value) => value,
                Err(EvidenceError::UnresolvedObservation) => None,
                Err(error) => return Err(error),
            };
            checked_terminal_receipt(conn, scope, reservation_id, charge)?;
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

pub(super) fn require_observable(
    conn: &Connection,
    scope: &str,
    execution_id: &str,
    reservation_id: u64,
) -> Result<(), EvidenceError> {
    match checked_fence(conn, scope, execution_id, reservation_id)? {
        None | Some(Fence::Authorized) => Ok(()),
        _ => Err(EvidenceError::DispatchNotAuthorized),
    }
}

// One transactional transition shared by authorization and closure. Returning a
// permit to the external caller happens only AFTER successful commit.
pub(super) fn transition(
    tx: &rusqlite::Transaction<'_>,
    scope: &str,
    execution_id: &str,
    authorize: bool,
) -> Result<(Fence, bool), EvidenceError> {
    let binding = intent_binding(tx, execution_id)?;
    let (reservation_id, stored_scope, _) = match binding {
        Some(binding) => binding,
        None => {
            let fenced: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM budget_dispatch_fence WHERE execution_id=?1)",
                [execution_id],
                |r| r.get(0),
            )?;
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
    let fence = checked_fence(tx, scope, execution_id, reservation_id)?
        .ok_or(EvidenceError::UnfencedIntent)?;
    if !matches!(fence, Fence::Prepared) {
        return Ok((fence, false));
    }
    let now = OffsetDateTime::now_utc().unix_timestamp();
    if authorize {
        let expires_at: i64 = tx.query_row(
            "SELECT expires_at FROM budget_reservation WHERE id=?1",
            [reservation_id],
            |r| r.get(0),
        )?;
        if OffsetDateTime::from_unix_timestamp(expires_at).is_err() {
            return Err(EvidenceError::InconsistentDispatch);
        }
        if expires_at <= now {
            return Err(EvidenceError::ExpiredBeforeDispatch);
        }
    }
    let changed = tx.execute("UPDATE budget_dispatch_fence SET phase=?2, transitioned_at=?3 WHERE execution_id=?1 AND phase=0", params![execution_id,if authorize {1} else {2},now])?;
    if changed != 1 {
        return Err(EvidenceError::InconsistentDispatch);
    }
    if authorize {
        return Ok((Fence::Authorized, true));
    }
    let changed = tx.execute("UPDATE budget_reservation SET settled=1, actual=0, settled_at=?2 WHERE id=?1 AND settled=0", params![reservation_id,now])?;
    if changed != 1 {
        return Err(EvidenceError::InconsistentDispatch);
    }
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

impl SqliteLedger {
    /// Commit dispatch authorization before sending. Only the first commit gets a
    /// permit; unknown commit outcomes/replays must never trigger another send.
    pub fn authorize_dispatch_durable(
        &mut self,
        scope: &str,
        execution_id: &str,
    ) -> Result<DispatchOutcome, EvidenceError> {
        validate_scope(scope)?;
        validate_execution_id(execution_id)?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (fence, changed) = transition(&tx, scope, execution_id, true)?;
        tx.commit()?;
        match fence {
            Fence::Authorized if changed => Ok(DispatchOutcome::Authorized(DispatchPermit {
                execution_id: execution_id.to_string(),
            })),
            Fence::Authorized => Ok(DispatchOutcome::MayHaveDispatched),
            Fence::Closed(value) => Ok(DispatchOutcome::ClosedBeforeDispatch(value)),
            Fence::Prepared => Err(EvidenceError::InconsistentDispatch),
        }
    }

    /// Release only an execution proven not to have authorized dispatch. A closed
    /// execution has separate closure evidence, never a fabricated usage receipt.
    /// Failure leaves the caller's intent available for reconciliation, not replay.
    pub fn close_before_dispatch_durable(
        &mut self,
        scope: &str,
        execution_id: &str,
    ) -> Result<ClosureOutcome, EvidenceError> {
        validate_scope(scope)?;
        validate_execution_id(execution_id)?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (fence, changed) = transition(&tx, scope, execution_id, false)?;
        tx.commit()?;
        match fence {
            Fence::Closed(value) if changed => Ok(ClosureOutcome::Closed(value)),
            Fence::Closed(value) => Ok(ClosureOutcome::AlreadyClosed(value)),
            Fence::Authorized => Ok(ClosureOutcome::MayHaveDispatched),
            Fence::Prepared => Err(EvidenceError::InconsistentDispatch),
        }
    }
}
