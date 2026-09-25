//! Internal settlement receipts, not physical-attempt or public usage events (TD-0026 W05a).
//! Includes opt-in execution usage snapshots and settlement from their frozen charge.
//! Existing untracked HTTP/settlement callers are unchanged.
//! Receipt delivery is at least once;
//! consumers must deduplicate by receipt ID. No network exporter is provided here.

#[cfg(test)]
mod tests;

use super::*;
use sandhi_core::{UsageBasis, UsageCompleteness, UsageV2};

/// Immutable execution-level usage snapshot, not proof of a physical send or settlement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalObservation {
    pub version: u32,
    pub execution_id: String,
    pub reservation_id: u64,
    pub scope: String,
    pub usage: UsageV2,
    pub frozen_charge: Option<u64>,
    pub observed_at: i64,
}

/// In-process cursor for one scope on the original ledger and fixed topology.
/// Not portable across database replacement/restore; not an authorization credential.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveryCursor {
    scope: String,
    after_id: u64,
    through_id: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecoveryState {
    MissingObservation,
    UnresolvedObservation,
    ReadyToSettle,
    Settled(SettlementReceipt),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveryEntry {
    pub execution_id: String,
    pub reservation_id: u64,
    pub state: RecoveryState,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveryPage {
    pub entries: Vec<RecoveryEntry>,
    pub next: Option<RecoveryCursor>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ObservationOutcome {
    Recorded(TerminalObservation),
    AlreadyRecorded(TerminalObservation),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettlementReceipt {
    pub receipt_id: String,
    pub reservation_id: u64,
    pub scope: String,
    pub charged_tokens: u64,
    pub settled_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SettlementOutcome {
    Committed(SettlementReceipt),
    AlreadyCommitted(SettlementReceipt),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaimedSettlement {
    pub receipt: SettlementReceipt,
    /// Local worker fencing token; not a tenant authorization credential.
    pub claim_token: String,
    pub lease_until: i64,
}

#[derive(Debug)]
pub enum EvidenceError {
    InvalidInput,
    IntentCapacity,
    MissingIntent,
    MissingObservation,
    UnresolvedObservation,
    TrackedSettlementRequired,
    ConflictingObservation,
    AlreadySettled,
    CorruptObservation,
    InconsistentSettlement,
    MissingReservation,
    WrongScope,
    ConflictingCharge,
    LegacySettlement,
    InvalidShard,
    ShardPoisoned,
    EntropyUnavailable,
    Storage(rusqlite::Error),
}

impl std::fmt::Display for EvidenceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Do not render caller metadata or storage internals into operator-facing errors.
        f.write_str(match self {
            Self::IntentCapacity => "durable execution intent capacity exhausted",
            Self::MissingIntent => "durable execution intent missing",
            Self::MissingObservation => "terminal observation missing",
            Self::UnresolvedObservation => "terminal usage is not final provider-reported evidence",
            Self::TrackedSettlementRequired => "tracked reservation requires terminal settlement",
            Self::ConflictingObservation => "terminal observation conflicts with stored evidence",
            Self::AlreadySettled => "reservation was settled before terminal observation",
            Self::CorruptObservation => "terminal observation cannot be decoded safely",
            Self::InconsistentSettlement => "terminal settlement evidence is inconsistent",
            Self::InvalidInput => "invalid settlement evidence input",
            Self::MissingReservation => "reservation missing or reclaimed",
            Self::WrongScope => "reservation scope mismatch",
            Self::ConflictingCharge => "settlement charge conflicts with receipt",
            Self::LegacySettlement => "legacy settlement has no receipt",
            Self::InvalidShard => "invalid evidence shard",
            Self::ShardPoisoned => "settlement evidence shard poisoned",
            Self::EntropyUnavailable => "receipt identity unavailable",
            Self::Storage(_) => "settlement evidence storage failed",
        })
    }
}

impl std::error::Error for EvidenceError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Storage(error) => Some(error),
            _ => None,
        }
    }
}

impl From<rusqlite::Error> for EvidenceError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Storage(error)
    }
}

pub(super) fn init(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS budget_execution_intent (
            execution_id TEXT PRIMARY KEY NOT NULL,
            reservation_id INTEGER UNIQUE NOT NULL CHECK(reservation_id > 0),
            created_at INTEGER NOT NULL
         );
         CREATE TABLE IF NOT EXISTS budget_settlement_outbox (
            receipt_id TEXT PRIMARY KEY NOT NULL,
            reservation_id INTEGER UNIQUE NOT NULL CHECK(reservation_id > 0),
            scope TEXT NOT NULL,
            charged_tokens INTEGER NOT NULL CHECK(charged_tokens >= 0),
            settled_at INTEGER NOT NULL,
            claim_token TEXT,
            claim_until INTEGER,
             acknowledged_at INTEGER
         );
         CREATE TABLE IF NOT EXISTS budget_terminal_observation (
            execution_id TEXT PRIMARY KEY NOT NULL REFERENCES budget_execution_intent(execution_id),
            version INTEGER NOT NULL CHECK(version = 1),
            usage_json TEXT NOT NULL CHECK(length(CAST(usage_json AS BLOB)) <= 16384),
            frozen_charge INTEGER CHECK(frozen_charge >= 0),
            observed_at INTEGER NOT NULL
         );
         CREATE INDEX IF NOT EXISTS idx_settlement_pending
             ON budget_settlement_outbox(settled_at, receipt_id)
             WHERE acknowledged_at IS NULL;",
    )
}

fn opaque_id() -> Result<String, EvidenceError> {
    use std::fmt::Write;
    let mut bytes = [0u8; 32];
    getrandom::getrandom(&mut bytes).map_err(|_| EvidenceError::EntropyUnavailable)?;
    let mut result = String::with_capacity(64);
    for byte in bytes {
        write!(result, "{byte:02x}").expect("writing to String");
    }
    Ok(result)
}

fn receipt(row: &rusqlite::Row<'_>) -> rusqlite::Result<SettlementReceipt> {
    Ok(SettlementReceipt {
        receipt_id: row.get(0)?,
        reservation_id: row.get(1)?,
        scope: row.get(2)?,
        charged_tokens: row.get(3)?,
        settled_at: row.get(4)?,
    })
}

/// Ledger-generated execution identity, distinct from logical request/dedup IDs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecutionIntent {
    pub execution_id: String,
    pub reservation: Reservation,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IntentAdmission {
    Admitted(ExecutionIntent),
    Denied(Denied),
}

fn validate_execution_id(execution_id: &str) -> Result<(), EvidenceError> {
    if execution_id.len() != 64 || !execution_id.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(EvidenceError::InvalidInput);
    }
    Ok(())
}

fn validate_scope(scope: &str) -> Result<(), EvidenceError> {
    if scope.is_empty() || scope.len() > 4096 {
        return Err(EvidenceError::InvalidInput);
    }
    Ok(())
}

fn encode_usage(usage: &UsageV2) -> Result<String, EvidenceError> {
    if usage.outcome.as_ref().is_some_and(|v| v.len() > 256)
        || usage
            .upstream_request_id
            .as_ref()
            .is_some_and(|v| v.len() > 1024)
        || usage
            .cache_read_observation
            .is_some_and(|v| v.validated().is_none())
    {
        return Err(EvidenceError::InvalidInput);
    }
    let encoded = serde_json::to_string(usage).map_err(|_| EvidenceError::InvalidInput)?;
    if encoded.len() > 16384 {
        return Err(EvidenceError::InvalidInput);
    }
    Ok(encoded)
}

fn intent_binding(
    conn: &Connection,
    execution_id: &str,
) -> Result<Option<(u64, String, bool)>, EvidenceError> {
    let binding: Option<(Option<u64>, Option<String>, Option<bool>)> = conn
        .query_row(
            "SELECT r.id, r.scope, r.settled FROM budget_execution_intent i
         LEFT JOIN budget_reservation r ON r.id = i.reservation_id WHERE i.execution_id = ?1",
            [execution_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    match binding {
        Some((Some(id), Some(scope), Some(settled))) => Ok(Some((id, scope, settled))),
        Some(_) => Err(EvidenceError::CorruptObservation),
        None => {
            let retained: bool = conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM budget_terminal_observation WHERE execution_id = ?1)",
                [execution_id],
                |r| r.get(0),
            )?;
            if retained {
                Err(EvidenceError::CorruptObservation)
            } else {
                Ok(None)
            }
        }
    }
}

fn read_observation(
    conn: &Connection,
    execution_id: &str,
    reservation_id: u64,
    scope: &str,
) -> Result<Option<TerminalObservation>, EvidenceError> {
    // Bound allocation even if a newer/foreign writer violated this version's contract.
    let row: Option<(u32, Option<String>, Option<u64>, i64)> = conn.query_row(
        "SELECT version, CASE WHEN length(CAST(usage_json AS BLOB)) <= 16384 THEN usage_json END,
                frozen_charge, observed_at FROM budget_terminal_observation WHERE execution_id = ?1",
        [execution_id],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
    ).optional()?;
    row.map(|(version, encoded, frozen_charge, observed_at)| {
        let encoded = encoded.ok_or(EvidenceError::CorruptObservation)?;
        let usage: UsageV2 =
            serde_json::from_str(&encoded).map_err(|_| EvidenceError::CorruptObservation)?;
        // Reject unsupported fields/versions and lossy forgiving deserialization. The
        // frozen charge is read, never recalculated under a potentially changed formula.
        if version != 1
            || encode_usage(&usage).map_err(|_| EvidenceError::CorruptObservation)? != encoded
            || (usage.completeness == UsageCompleteness::Unavailable) != frozen_charge.is_none()
        {
            return Err(EvidenceError::CorruptObservation);
        }
        Ok(TerminalObservation {
            version,
            execution_id: execution_id.to_string(),
            reservation_id,
            scope: scope.to_string(),
            usage,
            frozen_charge,
            observed_at,
        })
    })
    .transpose()
}

// The one eligibility rule shared by recovery inventory and canonical settlement.
fn terminal_charge(observed: &TerminalObservation) -> Result<i64, EvidenceError> {
    if observed.usage.completeness != UsageCompleteness::Final
        || observed.usage.basis != UsageBasis::ProviderReported
    {
        return Err(EvidenceError::UnresolvedObservation);
    }
    i64::try_from(
        observed
            .frozen_charge
            .ok_or(EvidenceError::CorruptObservation)?,
    )
    .map_err(|_| EvidenceError::CorruptObservation)
}

fn checked_terminal_receipt(
    conn: &Connection,
    scope: &str,
    id: u64,
    charge: Option<i64>,
) -> Result<Option<SettlementReceipt>, EvidenceError> {
    let (scope_matches, settled, actual, settled_at): (bool, i64, i64, Option<i64>) = conn
        .query_row(
            "SELECT scope = ?2, settled, actual, settled_at FROM budget_reservation WHERE id = ?1",
            params![id, scope],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )?;
    let stored = conn
        .query_row(
            "SELECT CASE WHEN length(CAST(receipt_id AS BLOB)) = 64 THEN receipt_id END,
                reservation_id, CASE WHEN scope = ?2 THEN ?2 END, charged_tokens, settled_at
         FROM budget_settlement_outbox WHERE reservation_id = ?1",
            params![id, scope],
            receipt,
        )
        .optional()?;
    if !scope_matches {
        return Err(EvidenceError::WrongScope);
    }
    match (settled, stored) {
        (0, None) if actual == 0 && settled_at.is_none() => Ok(None),
        (1, None) => Err(EvidenceError::LegacySettlement),
        (1, Some(receipt))
            if charge == Some(actual)
                && actual >= 0
                && receipt.charged_tokens == actual as u64
                && receipt.scope == scope
                && receipt.reservation_id == id
                && Some(receipt.settled_at) == settled_at
                && validate_execution_id(&receipt.receipt_id).is_ok() =>
        {
            Ok(Some(receipt))
        }
        _ => Err(EvidenceError::InconsistentSettlement),
    }
}

fn settle_receipt(
    tx: &rusqlite::Transaction<'_>,
    scope: &str,
    id: i64,
    charge: i64,
) -> Result<SettlementOutcome, EvidenceError> {
    let existing = tx
        .query_row(
            "SELECT receipt_id, reservation_id, scope, charged_tokens, settled_at
         FROM budget_settlement_outbox WHERE reservation_id = ?1",
            [id],
            receipt,
        )
        .optional()?;
    if let Some(existing) = existing {
        if existing.scope != scope {
            return Err(EvidenceError::WrongScope);
        }
        if existing.charged_tokens != charge as u64 {
            return Err(EvidenceError::ConflictingCharge);
        }
        return Ok(SettlementOutcome::AlreadyCommitted(existing));
    }
    let lease: Option<(String, bool)> = tx
        .query_row(
            "SELECT scope, settled FROM budget_reservation WHERE id = ?1",
            [id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let (stored_scope, settled) = lease.ok_or(EvidenceError::MissingReservation)?;
    if stored_scope != scope {
        return Err(EvidenceError::WrongScope);
    }
    if settled {
        return Err(EvidenceError::LegacySettlement);
    }
    let receipt = SettlementReceipt {
        receipt_id: opaque_id()?,
        reservation_id: id as u64,
        scope: stored_scope,
        charged_tokens: charge as u64,
        settled_at: OffsetDateTime::now_utc().unix_timestamp(),
    };
    let updated = tx.execute(
        "UPDATE budget_reservation SET actual = ?2, settled = 1, settled_at = ?3
         WHERE id = ?1 AND settled = 0",
        params![id, charge, receipt.settled_at],
    )?;
    if updated != 1 {
        return Err(EvidenceError::Storage(rusqlite::Error::QueryReturnedNoRows));
    }
    let inserted = tx.execute(
        "INSERT INTO budget_settlement_outbox
         (receipt_id, reservation_id, scope, charged_tokens, settled_at)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![receipt.receipt_id, id, scope, charge, receipt.settled_at],
    )?;
    if inserted != 1 {
        return Err(EvidenceError::Storage(rusqlite::Error::QueryReturnedNoRows));
    }
    Ok(SettlementOutcome::Committed(receipt))
}

pub(super) fn is_tracked(conn: &Connection, id: i64) -> rusqlite::Result<bool> {
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM budget_execution_intent WHERE reservation_id = ?1)",
        [id],
        |row| row.get(0),
    )
}

impl SqliteLedger {
    /// Read 1..=100 scoped records, ordered by the existing reservation identity.
    /// Each page is a short read transaction; the cursor freezes the admission upper
    /// bound, not state across pages. Start a new sweep for new/late observations.
    /// No state filtering before LIMIT, claiming, settlement, release or inference retry.
    /// Scope matching is not caller authorization. Cursors belong to the original
    /// ledger/fixed topology, not database replacement/restore. Corruption is an error;
    /// completion is not a global integrity certificate or proof of no pending work.
    /// Bounded returned records do not guarantee bounded SQLite execution latency.
    pub fn recovery_page_durable(
        &mut self,
        scope: &str,
        cursor: Option<&RecoveryCursor>,
        limit: usize,
    ) -> Result<RecoveryPage, EvidenceError> {
        validate_scope(scope)?;
        if !(1..=100).contains(&limit) {
            return Err(EvidenceError::InvalidInput);
        }
        if let Some(cursor) = cursor {
            if cursor.scope != scope {
                return Err(EvidenceError::WrongScope);
            }
            if cursor.after_id == 0
                || cursor.after_id > cursor.through_id
                || cursor.through_id > i64::MAX as u64
            {
                return Err(EvidenceError::InvalidInput);
            }
        }
        let tx = self.conn.transaction()?;
        // Orphan scope is unknowable. Fail closed without returning metadata from any
        // scope; a scoped inner join alone would silently omit these retained intents.
        let orphaned: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM budget_execution_intent i
             LEFT JOIN budget_reservation r ON r.id = i.reservation_id WHERE r.id IS NULL)
             OR EXISTS(SELECT 1 FROM budget_terminal_observation o
             LEFT JOIN budget_execution_intent i ON i.execution_id = o.execution_id
             WHERE i.execution_id IS NULL)",
            [],
            |r| r.get(0),
        )?;
        if orphaned {
            return Err(EvidenceError::CorruptObservation);
        }
        let (after_id, through_id) = match cursor {
            Some(cursor) => (cursor.after_id, cursor.through_id),
            None => (
                0,
                tx.query_row(
                    "SELECT COALESCE(MAX(i.reservation_id), 0) FROM budget_execution_intent i
                 JOIN budget_reservation r ON r.id = i.reservation_id WHERE r.scope = ?1",
                    [scope],
                    |r| r.get::<_, u64>(0),
                )?,
            ),
        };
        // One lookahead record tells the caller whether another page exists.
        let mut rows = {
            let mut statement = tx.prepare(
                "SELECT CASE WHEN length(CAST(i.execution_id AS BLOB)) = 64 THEN i.execution_id END,
                 i.reservation_id FROM budget_execution_intent i
                 JOIN budget_reservation r ON r.id = i.reservation_id
                 WHERE r.scope = ?1 AND i.reservation_id > ?2 AND i.reservation_id <= ?3
                 ORDER BY i.reservation_id LIMIT ?4",
            )?;
            let mapped = statement
                .query_map(params![scope, after_id, through_id, limit + 1], |r| {
                    Ok((r.get::<_, Option<String>>(0)?, r.get::<_, u64>(1)?))
                })?;
            mapped.collect::<Result<Vec<_>, _>>()?
        };
        let has_more = rows.len() > limit;
        rows.truncate(limit);
        let mut entries = Vec::with_capacity(rows.len());
        for (execution_id, reservation_id) in rows {
            let execution_id = execution_id.ok_or(EvidenceError::CorruptObservation)?;
            validate_execution_id(&execution_id).map_err(|_| EvidenceError::CorruptObservation)?;
            let observed = read_observation(&tx, &execution_id, reservation_id, scope)?;
            let charge = observed.as_ref().map(terminal_charge).transpose();
            let charge = match charge {
                Ok(value) => value,
                Err(EvidenceError::UnresolvedObservation) => None,
                Err(error) => return Err(error),
            };
            let receipt = checked_terminal_receipt(&tx, scope, reservation_id, charge)?;
            let state = match (receipt, observed, charge) {
                (Some(receipt), _, _) => RecoveryState::Settled(receipt),
                (None, None, _) => RecoveryState::MissingObservation,
                (None, Some(_), None) => RecoveryState::UnresolvedObservation,
                (None, Some(_), Some(_)) => RecoveryState::ReadyToSettle,
            };
            entries.push(RecoveryEntry {
                execution_id,
                reservation_id,
                state,
            });
        }
        let next = if has_more {
            entries.last().map(|last| RecoveryCursor {
                scope: scope.to_string(),
                after_id: last.reservation_id,
                through_id,
            })
        } else {
            None
        };
        tx.commit()?;
        Ok(RecoveryPage { entries, next })
    }

    /// Settle a tracked execution using its immutable stored charge. Only final
    /// provider-reported usage can release the reservation; partial/estimated/unknown
    /// usage retains liability until a separately reviewed amendment/recovery policy.
    /// Replays, including after receipt acknowledgement, return the original receipt.
    pub fn settle_terminal_durable(
        &mut self,
        scope: &str,
        execution_id: &str,
    ) -> Result<SettlementOutcome, EvidenceError> {
        validate_scope(scope)?;
        validate_execution_id(execution_id)?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (reservation_id, stored_scope, _) =
            intent_binding(&tx, execution_id)?.ok_or(EvidenceError::MissingIntent)?;
        if stored_scope != scope {
            return Err(EvidenceError::WrongScope);
        }
        let observed = read_observation(&tx, execution_id, reservation_id, &stored_scope)?
            .ok_or(EvidenceError::MissingObservation)?;
        let id = i64::try_from(reservation_id).map_err(|_| EvidenceError::CorruptObservation)?;
        let charge = terminal_charge(&observed)?;
        checked_terminal_receipt(&tx, scope, reservation_id, Some(charge))?;
        let outcome = settle_receipt(&tx, scope, id, charge)?;
        tx.commit()?;
        Ok(outcome)
    }

    /// Persist one immutable execution-level terminal usage snapshot. The intent owns
    /// reservation/scope identity; this method never settles, releases or retries inference.
    /// Exact replay returns the stored timestamp/charge. Conflicting later evidence is
    /// rejected, including refinements of unavailable/partial usage. Outcome strings are
    /// bounded metadata, not physical-send proof. Use the canonical terminal settlement API
    /// for tracked reservations and retain a compatible owner and fixed ledger topology.
    pub fn record_terminal_durable(
        &mut self,
        scope: &str,
        execution_id: &str,
        usage: &UsageV2,
    ) -> Result<ObservationOutcome, EvidenceError> {
        validate_scope(scope)?;
        validate_execution_id(execution_id)?;
        let encoded = encode_usage(usage)?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (reservation_id, stored_scope, settled) =
            intent_binding(&tx, execution_id)?.ok_or(EvidenceError::MissingIntent)?;
        if stored_scope != scope {
            return Err(EvidenceError::WrongScope);
        }
        if let Some(original) = read_observation(&tx, execution_id, reservation_id, scope)? {
            if original.usage != *usage {
                return Err(EvidenceError::ConflictingObservation);
            }
            return Ok(ObservationOutcome::AlreadyRecorded(original));
        }
        if settled {
            return Err(EvidenceError::AlreadySettled);
        }
        let frozen_charge = match usage.completeness {
            UsageCompleteness::Unavailable => None,
            UsageCompleteness::Final | UsageCompleteness::Partial => {
                Some(sandhi_core::billable(usage))
            }
        };
        let charge = frozen_charge
            .map(i64::try_from)
            .transpose()
            .map_err(|_| EvidenceError::InvalidInput)?;
        let observed_at = OffsetDateTime::now_utc().unix_timestamp();
        let inserted = tx.execute(
            "INSERT INTO budget_terminal_observation (execution_id, version, usage_json, frozen_charge, observed_at)
             VALUES (?1, 1, ?2, ?3, ?4)",
            params![execution_id, encoded, charge, observed_at],
        )?;
        if inserted != 1 {
            return Err(EvidenceError::Storage(rusqlite::Error::QueryReturnedNoRows));
        }
        tx.commit()?;
        Ok(ObservationOutcome::Recorded(TerminalObservation {
            version: 1,
            execution_id: execution_id.to_string(),
            reservation_id,
            scope: stored_scope,
            usage: usage.clone(),
            frozen_charge,
            observed_at,
        }))
    }

    /// Read a retained snapshot from the original ledger. Scope matching is not caller
    /// authorization. Unknown IDs/unobserved intents return None; invalid/corrupt data fails.
    pub fn terminal_durable(
        &self,
        scope: &str,
        execution_id: &str,
    ) -> Result<Option<TerminalObservation>, EvidenceError> {
        validate_scope(scope)?;
        validate_execution_id(execution_id)?;
        let Some((reservation_id, stored_scope, _)) = intent_binding(&self.conn, execution_id)?
        else {
            return Ok(None);
        };
        if stored_scope != scope {
            return Err(EvidenceError::WrongScope);
        }
        read_observation(&self.conn, execution_id, reservation_id, scope)
    }

    /// Opt-in library foundation: commit admission and its execution identity together
    /// before dispatch. No HTTP activation or recovery worker.
    /// Retained identities (including settled ones) count toward `retained_limit`,
    /// bounded to 1..=100_000 per ledger. Exhaustion refuses new tracked admission.
    /// No automatic pruning: unresolved leases retain capacity even after expiry.
    /// Use the original ledger and fixed topology; never replay inference using this ID.
    pub fn reserve_with_intent_durable(
        &mut self,
        scope: &str,
        ceiling: u64,
        now: OffsetDateTime,
        ttl: Duration,
        retained_limit: usize,
    ) -> Result<IntentAdmission, EvidenceError> {
        if !(1..=100_000).contains(&retained_limit)
            || scope.is_empty()
            || scope.len() > 4096
            || ceiling > i64::MAX as u64
            || ttl <= Duration::ZERO
            || now.checked_add(ttl).is_none()
        {
            return Err(EvidenceError::InvalidInput);
        }
        let execution_id = opaque_id()?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let count: i64 = tx.query_row("SELECT COUNT(*) FROM budget_execution_intent", [], |r| {
            r.get(0)
        })?;
        if count >= retained_limit as i64 {
            return Err(EvidenceError::IntentCapacity);
        }
        let reservation = match Self::reserve_in_transaction(&tx, scope, ceiling, now, ttl)? {
            ReserveOutcome::Admitted(reservation) => reservation,
            ReserveOutcome::Denied(denied) => return Ok(IntentAdmission::Denied(denied)),
        };
        let inserted = tx.execute(
            "INSERT INTO budget_execution_intent (execution_id, reservation_id, created_at) VALUES (?1, ?2, ?3)",
            params![execution_id, reservation.id, now.unix_timestamp()],
        )?;
        if inserted != 1 {
            return Err(EvidenceError::Storage(rusqlite::Error::QueryReturnedNoRows));
        }
        tx.commit()?;
        Ok(IntentAdmission::Admitted(ExecutionIntent {
            execution_id,
            reservation,
        }))
    }

    /// Read the original admission after reconnecting to the same ledger. This is
    /// not proof of dispatch, measured usage, settlement or caller authorization.
    pub fn intent_durable(
        &self,
        execution_id: &str,
    ) -> Result<Option<ExecutionIntent>, EvidenceError> {
        validate_execution_id(execution_id)?;
        let row: Option<(u64, String, u64, i64)> = self
            .conn
            .query_row(
                "SELECT r.id, r.scope, r.ceiling, r.expires_at FROM budget_execution_intent i
             JOIN budget_reservation r ON r.id = i.reservation_id WHERE i.execution_id = ?1",
                [execution_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()?;
        row.map(|(id, scope, ceiling, expires_at)| {
            let expires_at = OffsetDateTime::from_unix_timestamp(expires_at)
                .map_err(|_| EvidenceError::InvalidInput)?;
            Ok(ExecutionIntent {
                execution_id: execution_id.to_string(),
                reservation: Reservation {
                    id,
                    scope,
                    ceiling,
                    expires_at,
                },
            })
        })
        .transpose()
    }

    /// Atomically settle one lease and persist its immutable receipt. Replays return the
    /// original receipt, including after acknowledgement. Missing or legacy leases are NOT
    /// successful settlements. Scope is checked in storage, not just used for shard routing.
    /// Tracked reservations reject caller charges; use `settle_terminal_durable` instead.
    pub fn settle_with_evidence_durable(
        &mut self,
        scope: &str,
        reservation_id: u64,
        charged_tokens: u64,
    ) -> Result<SettlementOutcome, EvidenceError> {
        let id = i64::try_from(reservation_id).map_err(|_| EvidenceError::InvalidInput)?;
        let charge = i64::try_from(charged_tokens).map_err(|_| EvidenceError::InvalidInput)?;
        if id <= 0 {
            return Err(EvidenceError::InvalidInput);
        }
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if is_tracked(&tx, id)? {
            return Err(EvidenceError::TrackedSettlementRequired);
        }
        let outcome = settle_receipt(&tx, scope, id, charge)?;
        tx.commit()?;
        Ok(outcome)
    }

    /// Claim at most 1..=1000 pending receipts for 1..=3600 seconds. The caller supplies
    /// a trusted local clock, never a remote request timestamp. Reissued claims fence old
    /// acknowledgements. Clock rollback can delay reclaim; it cannot imply receiver commit.
    pub fn claim_settlements_durable(
        &mut self,
        limit: usize,
        now: OffsetDateTime,
        lease_seconds: u32,
    ) -> Result<Vec<ClaimedSettlement>, EvidenceError> {
        if !(1..=1000).contains(&limit) || !(1..=3600).contains(&lease_seconds) {
            return Err(EvidenceError::InvalidInput);
        }
        let now = now.unix_timestamp();
        let until = now
            .checked_add(i64::from(lease_seconds))
            .ok_or(EvidenceError::InvalidInput)?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let pending = {
            let mut stmt = tx.prepare(
                "SELECT receipt_id, reservation_id, scope, charged_tokens, settled_at
                 FROM budget_settlement_outbox WHERE acknowledged_at IS NULL
                 AND (claim_until IS NULL OR claim_until <= ?1)
                 ORDER BY settled_at, receipt_id LIMIT ?2",
            )?;
            let rows = stmt
                .query_map(params![now, limit as i64], receipt)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            rows
        };
        let mut claims = Vec::with_capacity(pending.len());
        for receipt in pending {
            let claim_token = opaque_id()?;
            let updated = tx.execute(
                "UPDATE budget_settlement_outbox SET claim_token = ?2, claim_until = ?3
                 WHERE receipt_id = ?1",
                params![receipt.receipt_id, claim_token, until],
            )?;
            if updated != 1 {
                return Err(EvidenceError::Storage(rusqlite::Error::QueryReturnedNoRows));
            }
            claims.push(ClaimedSettlement {
                receipt,
                claim_token,
                lease_until: until,
            });
        }
        tx.commit()?;
        Ok(claims)
    }

    /// Acknowledge only a current, unexpired claim. Returns false for unknown, expired,
    /// superseded OR already acknowledged claims. Retains the immutable receipt forever
    /// until a separately reviewed retention/migration policy exists (W05e/TD-0024).
    pub fn acknowledge_settlement_durable(
        &mut self,
        receipt_id: &str,
        claim_token: &str,
        now: OffsetDateTime,
    ) -> Result<bool, EvidenceError> {
        Ok(self.conn.execute(
            "UPDATE budget_settlement_outbox SET acknowledged_at = ?3
             WHERE receipt_id = ?1 AND claim_token = ?2 AND claim_until > ?3
             AND acknowledged_at IS NULL",
            params![receipt_id, claim_token, now.unix_timestamp()],
        )? == 1)
    }
}
