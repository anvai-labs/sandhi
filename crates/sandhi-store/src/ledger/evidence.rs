//! Internal receipts, execution context, paired ownership and tracked transport evidence.
//! Includes immutable usage snapshots and settlement from their frozen charge.
//! Existing untracked HTTP/settlement callers are unchanged.
//! Receipt delivery is at least once;
//! consumers must deduplicate by receipt ID. No network exporter is provided here.

mod attempts;
pub mod context;
pub use attempts::{
    AttemptEvidence, ObservedProvider, PhysicalAttempt, TransportErrorClass, TransportErrorSubtype,
    TransportOutcome, TransportProgress, TransportTerminal, TransportTimeoutPhase,
};
mod dispatch;
mod paired;
mod progress;
pub(crate) use progress::mark as mark_admission;
pub(crate) use progress::AdmissionStage;
pub use progress::{AdmissionProgress, AdmissionSnapshot};
#[cfg(test)]
mod tests;
pub use dispatch::{ClosureOutcome, DispatchOutcome, DispatchPermit, PreDispatchClosure};

use super::*;
use crate::LedgerStoreError;
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
/// Fields are crate-visible so both ledger backends construct and validate cursors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveryCursor {
    pub(crate) scope: String,
    pub(crate) after_id: u64,
    pub(crate) through_id: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecoveryState {
    Prepared,
    MayHaveDispatched,
    ClosedBeforeDispatch(PreDispatchClosure),
    MissingObservation,
    /// A durable unresolved tombstone. The reservation remains held; no inferred
    /// zero charge, automatic replay, or provider substitution may resolve it.
    UnresolvedObservation(UnresolvedObservationReason),
    ReadyToSettle,
    Settled(SettlementReceipt),
    /// Terminal by policy (ADR-0048): the retention window elapsed without final
    /// source evidence, so the held budget liability was released uncharged and
    /// the row stops counting against readiness. The stored evidence is intact;
    /// no zero-usage receipt exists and none may be inferred from this state.
    ReleasedAfterRetention,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnresolvedObservationReason {
    /// Physical send/terminal evidence is missing or incomplete.
    TransportEvidenceIncomplete,
    /// The terminal fact exists, but final provider-reported usage does not.
    ProviderUsageUnavailable,
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
    UnfencedIntent,
    DispatchNotAuthorized,
    InconsistentDispatch,
    ExpiredBeforeDispatch,
    InvalidInput,
    IntentCapacity,
    MissingIntent,
    MissingObservation,
    UnresolvedObservation(UnresolvedObservationReason),
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
    UnsupportedTrackedLedger,
    ShardPoisoned,
    EntropyUnavailable,
    /// The storage layer failed. Wraps [`crate::LedgerStoreError`] so both backends share one
    /// evidence error: the SQLite backend maps `rusqlite::Error` into
    /// [`LedgerStoreError::Backend`], the Postgres ledger (`postgres` feature) maps
    /// `sqlx::Error` the same way — consumers never see a driver type.
    Storage(crate::LedgerStoreError),
}

impl std::fmt::Display for EvidenceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Do not render caller metadata or storage internals into operator-facing errors.
        f.write_str(match self {
            Self::UnfencedIntent => "execution has no durable dispatch fence",
            Self::DispatchNotAuthorized => "execution is not authorized for terminal observation",
            Self::InconsistentDispatch => "dispatch evidence is inconsistent",
            Self::ExpiredBeforeDispatch => "reservation expired before dispatch authorization",
            Self::IntentCapacity => "durable execution intent capacity exhausted",
            Self::MissingIntent => "durable execution intent missing",
            Self::MissingObservation => "terminal observation missing",
            Self::UnresolvedObservation(
                UnresolvedObservationReason::TransportEvidenceIncomplete,
            ) => "transport evidence is incomplete",
            Self::UnresolvedObservation(UnresolvedObservationReason::ProviderUsageUnavailable) => {
                "terminal usage is not final provider-reported evidence"
            }
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
            Self::UnsupportedTrackedLedger => {
                "tracked settlement requires a single file-backed ledger"
            }
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
        Self::Storage(LedgerStoreError::Backend(error.to_string()))
    }
}

#[cfg(feature = "postgres")]
impl From<sqlx::Error> for EvidenceError {
    fn from(error: sqlx::Error) -> Self {
        Self::Storage(error.into())
    }
}

pub(super) fn init(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS budget_execution_intent (
            execution_id TEXT PRIMARY KEY NOT NULL,
            reservation_id INTEGER UNIQUE NOT NULL CHECK(reservation_id > 0),
            created_at INTEGER NOT NULL
         );
         CREATE TABLE IF NOT EXISTS budget_request_correlation (
            execution_id TEXT PRIMARY KEY NOT NULL REFERENCES budget_execution_intent(execution_id),
            request_id TEXT NOT NULL CHECK(length(CAST(request_id AS BLOB)) BETWEEN 1 AND 4096)
         );
         CREATE TABLE IF NOT EXISTS budget_dispatch_fence (
            execution_id TEXT PRIMARY KEY NOT NULL REFERENCES budget_execution_intent(execution_id),
            phase INTEGER NOT NULL CHECK(phase IN (0, 1, 2)),
            transitioned_at INTEGER,
            CHECK((phase = 0 AND transitioned_at IS NULL) OR
                  (phase IN (1, 2) AND transitioned_at IS NOT NULL))
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
    )?;
    // ADR-0048: policy release timestamp for aged unresolved liabilities. NULL
    // means the liability is still held; a timestamp means the retention window
    // elapsed uncharged (evidence columns untouched, no receipt written).
    let released: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info('budget_execution_intent') WHERE name='liability_released_at')",
        [],
        |r| r.get(0),
    )?;
    if !released {
        conn.execute(
            "ALTER TABLE budget_execution_intent ADD COLUMN liability_released_at INTEGER",
            [],
        )?;
    }
    context::init(conn)?;
    paired::init(conn)?;
    attempts::init(conn)
}

pub(crate) fn opaque_id() -> Result<String, EvidenceError> {
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
        reservation_id: row.get::<_, i64>(1)? as u64,
        scope: row.get(2)?,
        charged_tokens: row.get::<_, i64>(3)? as u64,
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

pub(crate) fn validate_execution_id(execution_id: &str) -> Result<(), EvidenceError> {
    if execution_id.len() != 64 || !execution_id.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(EvidenceError::InvalidInput);
    }
    Ok(())
}

pub(crate) fn validate_scope(scope: &str) -> Result<(), EvidenceError> {
    if scope.is_empty() || scope.len() > 4096 {
        return Err(EvidenceError::InvalidInput);
    }
    Ok(())
}

pub(crate) fn encode_usage(usage: &UsageV2) -> Result<String, EvidenceError> {
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
    let binding: Option<(Option<i64>, Option<String>, Option<bool>)> = conn
        .query_row(
            "SELECT r.id, r.scope, r.settled FROM budget_execution_intent i
         LEFT JOIN budget_reservation r ON r.id = i.reservation_id WHERE i.execution_id = ?1",
            [execution_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    match binding {
        Some((Some(id), Some(scope), Some(settled))) => Ok(Some((id as u64, scope, settled))),
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
    let row: Option<(u32, Option<String>, Option<i64>, i64)> = conn.query_row(
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
            frozen_charge: frozen_charge.map(|v| v as u64),
            observed_at,
        })
    })
    .transpose()
}

// The one eligibility rule shared by recovery inventory and canonical settlement.
fn terminal_unresolved_reason(
    conn: &Connection,
    observed: &TerminalObservation,
) -> Result<Option<UnresolvedObservationReason>, EvidenceError> {
    if attempts::read(conn, &observed.execution_id)?
        .is_some_and(|a| a.evidence.is_none_or(|e| !e.complete()))
    {
        return Ok(Some(
            UnresolvedObservationReason::TransportEvidenceIncomplete,
        ));
    }
    if observed.usage.completeness != UsageCompleteness::Final
        || observed.usage.basis != UsageBasis::ProviderReported
    {
        return Ok(Some(UnresolvedObservationReason::ProviderUsageUnavailable));
    }
    Ok(None)
}

fn terminal_charge(
    conn: &Connection,
    observed: &TerminalObservation,
) -> Result<i64, EvidenceError> {
    if let Some(reason) = terminal_unresolved_reason(conn, observed)? {
        return Err(EvidenceError::UnresolvedObservation(reason));
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
            params![id as i64, scope],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )?;
    let stored = conn
        .query_row(
            "SELECT CASE WHEN length(CAST(receipt_id AS BLOB)) = 64 THEN receipt_id END,
                reservation_id, CASE WHEN scope = ?2 THEN ?2 END, charged_tokens, settled_at
         FROM budget_settlement_outbox WHERE reservation_id = ?1",
            params![id as i64, scope],
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
        return Err(EvidenceError::Storage(LedgerStoreError::Backend(
            "statement affected no rows".into(),
        )));
    }
    let inserted = tx.execute(
        "INSERT INTO budget_settlement_outbox
         (receipt_id, reservation_id, scope, charged_tokens, settled_at)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![receipt.receipt_id, id, scope, charge, receipt.settled_at],
    )?;
    if inserted != 1 {
        return Err(EvidenceError::Storage(LedgerStoreError::Backend(
            "statement affected no rows".into(),
        )));
    }
    Ok(SettlementOutcome::Committed(receipt))
}

pub(super) fn is_tracked(conn: &Connection, id: i64) -> rusqlite::Result<bool> {
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM budget_execution_intent WHERE reservation_id = ?1)
         OR EXISTS(SELECT 1 FROM budget_execution_back_leg WHERE reservation_id = ?1)
         OR EXISTS(SELECT 1 FROM budget_dispatch_fence WHERE back_reservation_id = ?1)",
        [id],
        |row| row.get(0),
    )
}

/// Opaque scope inventory cursor on the original ledger; not an authorization token.
#[derive(Debug, Clone)]
pub struct ScopeCursor {
    pub(crate) after: String,
    pub(crate) through_id: u64,
}
#[derive(Debug)]
pub struct ScopePage {
    pub scopes: Vec<String>,
    pub next: Option<ScopeCursor>,
}

impl SqliteLedger {
    /// Bounded scope inventory includes unbudgeted and removed-key scopes. New admissions
    /// beyond the frozen upper bound are visited by the next sweep.
    pub fn recovery_scopes_durable(
        &mut self,
        cursor: Option<&ScopeCursor>,
        limit: usize,
    ) -> Result<ScopePage, EvidenceError> {
        if !(1..=100).contains(&limit) {
            return Err(EvidenceError::InvalidInput);
        }
        let tx = self.conn.transaction()?;
        let through_id = match cursor {
            Some(c) => c.through_id,
            None => tx.query_row(
                "SELECT COALESCE(MAX(reservation_id),0) FROM budget_execution_intent",
                [],
                |r| Ok(r.get::<_, i64>(0)? as u64),
            )?,
        };
        let after = cursor.map_or("", |c| c.after.as_str());
        let mut stmt = tx.prepare("SELECT DISTINCT r.scope FROM budget_execution_intent i LEFT JOIN budget_reservation r ON r.id=i.reservation_id WHERE i.reservation_id<=?1 AND (r.scope>?2 OR r.scope IS NULL) ORDER BY r.scope LIMIT ?3")?;
        let scopes = stmt
            .query_map(params![through_id as i64, after, limit as i64], |r| {
                r.get::<_, Option<String>>(0)
            })?
            .map(|r| {
                r.map_err(EvidenceError::from)
                    .and_then(|s| s.ok_or(EvidenceError::MissingReservation))
            })
            .collect::<Result<Vec<_>, _>>()?;
        for scope in &scopes {
            validate_scope(scope)?;
        }
        let next = (scopes.len() == limit).then(|| ScopeCursor {
            after: scopes.last().unwrap().clone(),
            through_id,
        });
        drop(stmt);
        tx.commit()?;
        Ok(ScopePage { scopes, next })
    }
    /// Prepared admission with immutable gateway correlation, never a replay key.
    #[allow(clippy::too_many_arguments)]
    pub fn reserve_correlated_durable(
        &mut self,
        scope: &str,
        ceiling: u64,
        now: OffsetDateTime,
        ttl: Duration,
        retained_limit: usize,
        request_id: &str,
    ) -> Result<IntentAdmission, EvidenceError> {
        if !self.is_file_backed() {
            return Err(EvidenceError::UnsupportedTrackedLedger);
        }
        if request_id.is_empty() || request_id.len() > 4096 {
            return Err(EvidenceError::InvalidInput);
        }
        self.reserve_intent(
            scope,
            ceiling,
            now,
            ttl,
            retained_limit,
            true,
            Some(request_id),
            None,
            None,
        )
    }
    pub fn request_id_durable(
        &self,
        scope: &str,
        execution_id: &str,
    ) -> Result<Option<String>, EvidenceError> {
        validate_scope(scope)?;
        let intent = self
            .intent_durable(execution_id)?
            .ok_or(EvidenceError::MissingIntent)?;
        if intent.reservation.scope != scope {
            return Err(EvidenceError::WrongScope);
        }
        let value: Option<String> = self
            .conn
            .query_row(
                "SELECT request_id FROM budget_request_correlation WHERE execution_id = ?1",
                [execution_id],
                |r| r.get(0),
            )
            .optional()?;
        if value
            .as_ref()
            .is_some_and(|v| v.is_empty() || v.len() > 4096)
        {
            return Err(EvidenceError::CorruptObservation);
        }
        Ok(value)
    }
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
             WHERE i.execution_id IS NULL)
             OR EXISTS(SELECT 1 FROM budget_dispatch_fence f
             LEFT JOIN budget_execution_intent i ON i.execution_id = f.execution_id
             WHERE i.execution_id IS NULL)
             OR EXISTS(SELECT 1 FROM budget_execution_back_leg b
             LEFT JOIN budget_execution_intent i USING(execution_id)
             LEFT JOIN budget_reservation r ON r.id=b.reservation_id
             WHERE i.execution_id IS NULL OR r.id IS NULL)
             OR EXISTS(SELECT 1 FROM budget_physical_attempt a
             LEFT JOIN budget_execution_intent i USING(execution_id)
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
                    |r| Ok(r.get::<_, i64>(0)? as u64),
                )?,
            ),
        };
        // One lookahead record tells the caller whether another page exists.
        let mut rows = {
            let mut statement = tx.prepare(
                "SELECT CASE WHEN length(CAST(i.execution_id AS BLOB)) = 64 THEN i.execution_id END,
                 i.reservation_id, i.liability_released_at FROM budget_execution_intent i
                 JOIN budget_reservation r ON r.id = i.reservation_id
                 WHERE r.scope = ?1 AND i.reservation_id > ?2 AND i.reservation_id <= ?3
                 ORDER BY i.reservation_id LIMIT ?4",
            )?;
            let mapped = statement.query_map(
                params![
                    scope,
                    after_id as i64,
                    through_id as i64,
                    (limit + 1) as i64
                ],
                |r| {
                    Ok((
                        r.get::<_, Option<String>>(0)?,
                        r.get::<_, i64>(1)? as u64,
                        r.get::<_, Option<i64>>(2)?,
                    ))
                },
            )?;
            mapped.collect::<Result<Vec<_>, _>>()?
        };
        let has_more = rows.len() > limit;
        rows.truncate(limit);
        let mut entries = Vec::with_capacity(rows.len());
        for (execution_id, reservation_id, released_at) in rows {
            let execution_id = execution_id.ok_or(EvidenceError::CorruptObservation)?;
            validate_execution_id(&execution_id).map_err(|_| EvidenceError::CorruptObservation)?;
            // ADR-0048: policy-released rows classify terminal BEFORE the receipt
            // machinery — their reservation is settled with no receipt, which
            // `checked_terminal_receipt` would otherwise reject. The stamping job
            // never releases a row that had (or later gained) final evidence.
            if released_at.is_some() {
                entries.push(RecoveryEntry {
                    execution_id,
                    reservation_id,
                    state: RecoveryState::ReleasedAfterRetention,
                });
                continue;
            }
            let fence = dispatch::checked_fence(&tx, scope, &execution_id, reservation_id)?;
            let immediate_state = match &fence {
                Some(dispatch::Fence::Prepared) => Some(RecoveryState::Prepared),
                Some(dispatch::Fence::Closed(closure)) => {
                    Some(RecoveryState::ClosedBeforeDispatch(closure.clone()))
                }
                _ => None,
            };
            if let Some(state) = immediate_state {
                entries.push(RecoveryEntry {
                    execution_id,
                    reservation_id,
                    state,
                });
                continue;
            }
            let observed = read_observation(&tx, &execution_id, reservation_id, scope)?;
            let unresolved_reason = observed
                .as_ref()
                .map(|o| terminal_unresolved_reason(&tx, o))
                .transpose()?
                .flatten();
            let charge = if unresolved_reason.is_some() {
                None
            } else {
                observed
                    .as_ref()
                    .map(|o| terminal_charge(&tx, o))
                    .transpose()?
            };
            let receipt = checked_terminal_receipt(&tx, scope, reservation_id, charge)?;
            let state = match (receipt, observed, charge) {
                (Some(receipt), _, _) => RecoveryState::Settled(receipt),
                (None, None, _) if fence.is_some() => RecoveryState::MayHaveDispatched,
                (None, None, _) => RecoveryState::MissingObservation,
                (None, Some(_), None) => RecoveryState::UnresolvedObservation(
                    unresolved_reason.ok_or(EvidenceError::CorruptObservation)?,
                ),
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

    /// ADR-0048: release the held budget liability of every execution intent whose
    /// admission age exceeds `cutoff` and that has NO final source evidence
    /// (Prepared / MayHaveDispatched / MissingObservation / UnresolvedObservation).
    /// Released rows keep their full evidence, gain `liability_released_at`, and
    /// their reservation closes as an explicit zero-charge policy release —
    /// `spent` is never touched and no settlement receipt is written, so nothing
    /// is fabricated. The paired back leg closes with the same zero-charge
    /// release (on the normal path it rides the primary's settle); an
    /// already-settled back leg is untouched. Rows that settled or carry a
    /// settleable charge are skipped untouched (recovery settles those
    /// normally). Re-verification happens inside the same transaction as the
    /// stamp, so a race with live settlement resolves to the receipt, never to
    /// a release. Returns the released count.
    pub fn release_aged_unresolved_liabilities_durable(
        &mut self,
        now: OffsetDateTime,
        cutoff: OffsetDateTime,
    ) -> Result<usize, EvidenceError> {
        let released_at = now.unix_timestamp();
        let cutoff_ts = cutoff.unix_timestamp();
        let mut released = 0usize;
        let mut converged = false;
        let mut after_id = 0i64;
        loop {
            // Immediate: classification reads the post-commit state of any
            // concurrently settling transaction instead of racing a stale
            // WAL snapshot into a SQLITE_BUSY_SNAPSHOT error.
            let mut tx = self
                .conn
                .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            // Converge residuals from releases that shipped before the paired
            // close: their intents are already stamped, so the candidate loop
            // never revisits them, and reclaim explicitly exempts back legs —
            // the stranded upstream holds were permanent admission denial
            // (reserved_durable ignores expiry). Stamped with the intent's OWN
            // release stamp, not the sweep clock: paired::read requires the
            // state mirrors to be exactly equal, and the primary kept the
            // original stamp. Once per sweep call, not per page transaction —
            // budget_reservation only grows; idempotent via settled = 0.
            if !converged {
                tx.execute(
                    "UPDATE budget_reservation SET actual = 0, settled = 1,
                        settled_at = (SELECT i.liability_released_at
                                      FROM budget_execution_back_leg b
                                      JOIN budget_execution_intent i
                                        ON i.execution_id = b.execution_id
                                      WHERE b.reservation_id = budget_reservation.id)
                     WHERE settled = 0 AND EXISTS (
                       SELECT 1 FROM budget_execution_back_leg b
                       JOIN budget_execution_intent i ON i.execution_id = b.execution_id
                       WHERE b.reservation_id = budget_reservation.id
                         AND i.liability_released_at IS NOT NULL)",
                    [],
                )?;
                converged = true;
            }
            let candidates: Vec<(String, i64, String)> = {
                let mut statement = tx.prepare(
                    "SELECT i.execution_id, i.reservation_id, r.scope
                     FROM budget_execution_intent i
                     JOIN budget_reservation r ON r.id = i.reservation_id
                     WHERE i.liability_released_at IS NULL AND i.created_at <= ?1
                       AND r.settled = 0
                       AND i.reservation_id > ?2
                     ORDER BY i.reservation_id LIMIT 200",
                )?;
                let mapped = statement.query_map(params![cutoff_ts, after_id], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, i64>(1)?,
                        r.get::<_, String>(2)?,
                    ))
                })?;
                mapped.collect::<Result<Vec<_>, _>>()?
            };
            if candidates.is_empty() {
                tx.commit()?;
                return Ok(released);
            }
            let candidate_count = candidates.len();
            for (execution_id, reservation_id, scope) in candidates {
                after_id = after_id.max(reservation_id);
                // One savepoint per candidate: the stamp and the release commit
                // together or not at all, and a poisoned row rolls back alone — one
                // inconsistent record can never block the release of another aged
                // liability (the sweep converges around it, retaining it in place).
                let mut sp = tx.savepoint()?;
                match Self::stamp_if_liable(
                    &sp,
                    released_at,
                    &execution_id,
                    reservation_id as u64,
                    &scope,
                ) {
                    Ok(stamped) => {
                        // Savepoint drop rolls back by default; commit the
                        // stamp+release explicitly, rollback the poisoned row.
                        sp.commit()?;
                        released += stamped as usize;
                    }
                    Err(error) => {
                        tracing::warn!(
                            %error,
                            execution_id = %execution_id,
                            "aged liability release skipped; row retained in place"
                        );
                        let _ = sp.rollback();
                    }
                }
            }
            let full_page = candidate_count == 200;
            tx.commit()?;
            if !full_page {
                return Ok(released);
            }
        }
    }

    /// Classify one aged candidate and, when it is the unresolved-liability incident
    /// class, stamp the release and settle it uncharged in place. Returns whether the
    /// row was released; every other state (terminal, settleable, already stamped)
    /// leaves the row untouched for recovery or a later sweep. Same classification as
    /// `recovery_page_durable`, evaluated inside the caller's transaction/savepoint.
    fn stamp_if_liable(
        conn: &rusqlite::Connection,
        released_at: i64,
        execution_id: &str,
        reservation_id: u64,
        scope: &str,
    ) -> Result<bool, EvidenceError> {
        validate_execution_id(execution_id).map_err(|_| EvidenceError::CorruptObservation)?;
        validate_scope(scope)?;
        let fence = dispatch::checked_fence(conn, scope, execution_id, reservation_id)?;
        // Closed-before-dispatch is already terminal; a Prepared intent is
        // stampable — at retention age nothing is mid-flight anymore.
        if matches!(&fence, Some(dispatch::Fence::Closed(_))) {
            return Ok(false);
        }
        let observed = read_observation(conn, execution_id, reservation_id, scope)?;
        let unresolved_reason = observed
            .as_ref()
            .map(|o| terminal_unresolved_reason(conn, o))
            .transpose()?
            .flatten();
        let charge = if unresolved_reason.is_some() {
            None
        } else {
            observed
                .as_ref()
                .map(|o| terminal_charge(conn, o))
                .transpose()?
        };
        let receipt = checked_terminal_receipt(conn, scope, reservation_id, charge)?;
        let stampable = match (receipt, observed, charge) {
            // Final usage exists: recovery settles it, never the release.
            (_, Some(_), Some(_)) | (Some(_), _, _) => false,
            // Dispatched or observed without final usage: the incident class.
            (None, _, _) => true,
        };
        if !stampable {
            return Ok(false);
        }
        conn.execute(
            "UPDATE budget_execution_intent SET liability_released_at = ?1
             WHERE execution_id = ?2 AND liability_released_at IS NULL",
            params![released_at, execution_id],
        )?;
        let changed = conn.execute(
            "UPDATE budget_reservation SET actual = 0, settled = 1, settled_at = ?1
             WHERE id = ?2 AND settled = 0",
            params![released_at, reservation_id as i64],
        )?;
        if changed != 1 {
            return Err(EvidenceError::CorruptObservation);
        }
        // The paired back leg rides the primary's settle on the normal
        // path (paired::settle); a policy release must close it the same
        // way or its upstream hold stays unsettled forever. Same
        // zero-charge release semantics; an already-settled or absent
        // back leg is left untouched.
        conn.execute(
            "UPDATE budget_reservation SET actual = 0, settled = 1, settled_at = ?1
             WHERE id = (SELECT reservation_id FROM budget_execution_back_leg
                         WHERE execution_id = ?2)
               AND settled = 0",
            params![released_at, execution_id],
        )?;
        Ok(true)
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
        dispatch::require_observable(&tx, scope, execution_id, reservation_id)?;
        let observed = read_observation(&tx, execution_id, reservation_id, &stored_scope)?
            .ok_or(EvidenceError::MissingObservation)?;
        let id = i64::try_from(reservation_id).map_err(|_| EvidenceError::CorruptObservation)?;
        let charge = terminal_charge(&tx, &observed)?;
        checked_terminal_receipt(&tx, scope, reservation_id, Some(charge))?;
        let back = paired::read(&tx, execution_id, reservation_id)?;
        let outcome = settle_receipt(&tx, scope, id, charge)?;
        paired::settle(&tx, back.as_ref(), &outcome)?;
        paired::read(&tx, execution_id, reservation_id)?;
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
        cache_read_bp: u32,
    ) -> Result<ObservationOutcome, EvidenceError> {
        self.record_terminal_inner(scope, execution_id, usage, cache_read_bp, None)
    }

    /// Atomically retain transport evidence and qualified execution usage. Exact
    /// replay is accepted; later refinements and missing required evidence refuse.
    pub fn record_terminal_with_attempt_durable(
        &mut self,
        scope: &str,
        execution_id: &str,
        usage: &UsageV2,
        cache_read_bp: u32,
        attempt: &AttemptEvidence,
    ) -> Result<ObservationOutcome, EvidenceError> {
        self.record_terminal_inner(scope, execution_id, usage, cache_read_bp, Some(attempt))
    }

    fn record_terminal_inner(
        &mut self,
        scope: &str,
        execution_id: &str,
        usage: &UsageV2,
        cache_read_bp: u32,
        attempt: Option<&AttemptEvidence>,
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
        dispatch::require_observable(&tx, scope, execution_id, reservation_id)?;
        if context::read(&tx, execution_id)?.is_some_and(|context| {
            context.accounting_policy.cache_read_weight.basis_points() != cache_read_bp
        }) {
            return Err(EvidenceError::ConflictingCharge);
        }
        attempts::record(&tx, execution_id, attempt)?;
        if let Some(original) = read_observation(&tx, execution_id, reservation_id, scope)? {
            if original.usage != *usage {
                return Err(EvidenceError::ConflictingObservation);
            }
            return Ok(ObservationOutcome::AlreadyRecorded(original));
        }
        if settled {
            return Err(EvidenceError::AlreadySettled);
        }
        // Frozen at the operator's cache-read weight (10,000 bp = the neutral
        // billable). The weight is server-admitted config threaded from the
        // settlement path - never caller data (TD-0032 constraint).
        let frozen_charge = match usage.completeness {
            UsageCompleteness::Unavailable => None,
            UsageCompleteness::Final | UsageCompleteness::Partial => {
                Some(sandhi_core::billable_weighted(usage, cache_read_bp))
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
            return Err(EvidenceError::Storage(LedgerStoreError::Backend(
                "statement affected no rows".into(),
            )));
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
    /// authorization. Unknown IDs/unobserved intents return None; invalid/corrupt usage fails.
    /// Use recovery_page_durable for dispatch-phase and settlement consistency checks.
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
        self.reserve_intent(
            scope,
            ceiling,
            now,
            ttl,
            retained_limit,
            false,
            None,
            None,
            None,
        )
    }

    /// Opt-in storage prerequisite for dispatch fencing. Only file-backed ledgers.
    /// Caller must obtain the one-use permit before dispatch; HTTP does not use this yet.
    /// Existing reserve_with_intent_durable callers remain unfenced/unknown.
    pub fn reserve_prepared_durable(
        &mut self,
        scope: &str,
        ceiling: u64,
        now: OffsetDateTime,
        ttl: Duration,
        retained_limit: usize,
    ) -> Result<IntentAdmission, EvidenceError> {
        if !self.is_file_backed() {
            return Err(EvidenceError::UnsupportedTrackedLedger);
        }
        self.reserve_intent(
            scope,
            ceiling,
            now,
            ttl,
            retained_limit,
            true,
            None,
            None,
            None,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn reserve_intent(
        &mut self,
        scope: &str,
        ceiling: u64,
        now: OffsetDateTime,
        ttl: Duration,
        retained_limit: usize,
        prepared: bool,
        request_id: Option<&str>,
        context: Option<context::PreparedContext<'_>>,
        progress: Option<&AdmissionProgress>,
    ) -> Result<IntentAdmission, EvidenceError> {
        mark_admission(progress, AdmissionStage::ValidateAdmission);
        if !(1..=100_000).contains(&retained_limit)
            || scope.is_empty()
            || scope.len() > 4096
            || ceiling > i64::MAX as u64
            || ttl <= Duration::ZERO
            || now.checked_add(ttl).is_none()
        {
            return Err(EvidenceError::InvalidInput);
        }
        mark_admission(progress, AdmissionStage::GenerateIdentity);
        let execution_id = opaque_id()?;
        mark_admission(progress, AdmissionStage::BeginTransaction);
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        mark_admission(progress, AdmissionStage::ReadCapacity);
        let count: i64 = tx.query_row("SELECT COUNT(*) FROM budget_execution_intent", [], |r| {
            r.get(0)
        })?;
        if count >= retained_limit as i64 {
            return Err(EvidenceError::IntentCapacity);
        }
        mark_admission(progress, AdmissionStage::ReservePrimary);
        let reservation = match Self::reserve_in_transaction(&tx, scope, ceiling, now, ttl)? {
            ReserveOutcome::Admitted(reservation) => reservation,
            ReserveOutcome::Denied(denied) => return Ok(IntentAdmission::Denied(denied)),
        };
        let back = if let Some(back_scope) = context.as_ref().and_then(|c| c.paired_scope) {
            if back_scope == scope {
                return Err(EvidenceError::InvalidInput);
            }
            mark_admission(progress, AdmissionStage::ReserveBackLeg);
            match Self::reserve_in_transaction(&tx, back_scope, ceiling, now, ttl)? {
                ReserveOutcome::Admitted(back) => Some(back),
                ReserveOutcome::Denied(denied) => return Ok(IntentAdmission::Denied(denied)),
            }
        } else {
            None
        };
        mark_admission(progress, AdmissionStage::WriteIntent);
        let inserted = tx.execute(
            "INSERT INTO budget_execution_intent (execution_id, reservation_id, created_at) VALUES (?1, ?2, ?3)",
            params![execution_id, reservation.id as i64, now.unix_timestamp()],
        )?;
        if inserted != 1 {
            return Err(EvidenceError::Storage(LedgerStoreError::Backend(
                "statement affected no rows".into(),
            )));
        }
        if prepared {
            mark_admission(progress, AdmissionStage::WriteFence);
            let inserted = tx.execute(
                "INSERT INTO budget_dispatch_fence (execution_id, phase) VALUES (?1, 0)",
                [&execution_id],
            )?;
            if inserted != 1 {
                return Err(EvidenceError::Storage(LedgerStoreError::Backend(
                    "statement affected no rows".into(),
                )));
            }
        }
        if let Some(request_id) = request_id {
            mark_admission(progress, AdmissionStage::WriteCorrelation);
            tx.execute(
                "INSERT INTO budget_request_correlation(execution_id, request_id) VALUES (?1, ?2)",
                params![execution_id, request_id],
            )?;
        }
        if let Some(context) = context {
            mark_admission(progress, AdmissionStage::WriteContext);
            tx.execute("INSERT INTO budget_execution_context(execution_id,version,context_json) VALUES (?1,1,?2)", params![execution_id,context.json])?;
            tx.execute(
                "UPDATE budget_dispatch_fence SET context_version=1 WHERE execution_id=?1",
                [&execution_id],
            )?;
        }
        if let Some(back) = back {
            mark_admission(progress, AdmissionStage::WriteBackLeg);
            tx.execute("INSERT INTO budget_execution_back_leg(execution_id,version,reservation_id) VALUES (?1,1,?2)",params![execution_id,back.id as i64])?;
            tx.execute(
                "UPDATE budget_dispatch_fence SET back_reservation_id=?2,attempt_version=1 WHERE execution_id=?1",
                params![execution_id, back.id as i64],
            )?;
        }
        mark_admission(progress, AdmissionStage::Commit);
        tx.commit()?;
        mark_admission(progress, AdmissionStage::Returned);
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
        let row: Option<(i64, String, i64, i64)> = self
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
                    id: id as u64,
                    scope,
                    ceiling: ceiling as u64,
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
                return Err(EvidenceError::Storage(LedgerStoreError::Backend(
                    "statement affected no rows".into(),
                )));
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
