//! Internal settlement receipts, not physical-attempt or public usage events (TD-0026 W05a).
//! Existing `settle_durable` callers are deliberately unchanged. Delivery is at least once;
//! consumers must deduplicate by receipt ID. No network exporter is provided here.

#[cfg(test)]
mod tests;

use super::*;

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
    MissingReservation,
    WrongScope,
    ConflictingCharge,
    LegacySettlement,
    InvalidShard,
    EntropyUnavailable,
    Storage(rusqlite::Error),
}

impl std::fmt::Display for EvidenceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Do not render caller metadata or storage internals into operator-facing errors.
        f.write_str(match self {
            Self::InvalidInput => "invalid settlement evidence input",
            Self::MissingReservation => "reservation missing or reclaimed",
            Self::WrongScope => "reservation scope mismatch",
            Self::ConflictingCharge => "settlement charge conflicts with receipt",
            Self::LegacySettlement => "legacy settlement has no receipt",
            Self::InvalidShard => "invalid evidence shard",
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
        "CREATE TABLE IF NOT EXISTS budget_settlement_outbox (
            receipt_id TEXT PRIMARY KEY NOT NULL,
            reservation_id INTEGER UNIQUE NOT NULL CHECK(reservation_id > 0),
            scope TEXT NOT NULL,
            charged_tokens INTEGER NOT NULL CHECK(charged_tokens >= 0),
            settled_at INTEGER NOT NULL,
            claim_token TEXT,
            claim_until INTEGER,
            acknowledged_at INTEGER
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

impl SqliteLedger {
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
