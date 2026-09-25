//! Internal settlement receipts, not physical-attempt or public usage events (TD-0026 W05a).
//! Includes opt-in execution usage snapshots; existing HTTP/settlement callers are unchanged.
//! Receipt delivery is at least once;
//! consumers must deduplicate by receipt ID. No network exporter is provided here.

#[cfg(test)]
mod tests;

use super::*;
use sandhi_core::{UsageCompleteness, UsageV2};

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
    ConflictingObservation,
    AlreadySettled,
    CorruptObservation,
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
            Self::ConflictingObservation => "terminal observation conflicts with stored evidence",
            Self::AlreadySettled => "reservation was settled before terminal observation",
            Self::CorruptObservation => "terminal observation cannot be decoded safely",
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

impl SqliteLedger {
    /// Persist one immutable execution-level terminal usage snapshot. The intent owns
    /// reservation/scope identity; this method never settles, releases or retries inference.
    /// Exact replay returns the stored timestamp/charge. Conflicting later evidence is
    /// rejected, including refinements of unavailable/partial usage. Outcome strings are
    /// bounded metadata, not physical-send proof. Use one compatible owner and fixed topology:
    /// legacy settlement APIs are not yet linked to this observation contract.
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
            if existing.charged_tokens != charged_tokens {
                return Err(EvidenceError::ConflictingCharge);
            }
            tx.commit()?;
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
            reservation_id,
            scope: stored_scope,
            charged_tokens,
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
        tx.commit()?;
        Ok(SettlementOutcome::Committed(receipt))
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
