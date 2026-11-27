//! Postgres evidence subsystem (cargo feature `postgres`) — Phase 3 PR-B. A direct port
//! of [`crate::ledger::evidence`] (the SQLite twin) onto [`PostgresLedger`]: the fenced
//! execution intents, terminal observations, settlement outbox, recovery paging and the
//! ADR-0048 aged-liability release. Semantics are identical; only the storage dialect
//! differs. The evidence *types* (`EvidenceError`, `RecoveryState`, `SettlementReceipt`,
//! `AttemptEvidence`, `ExecutionContext`, …) are shared verbatim from the SQLite module
//! so both backends stay definition-compatible, and `EvidenceError::Storage` wraps
//! [`crate::LedgerStoreError`] for exactly this reason.
//!
//! Dialect decisions (mirroring PR-A's recorded decisions):
//!
//! - **Concurrency.** Writes are READ COMMITTED transactions serialized per scope by
//!   `pg_advisory_xact_lock` — SQLite's `BEGIN IMMEDIATE` narrowed from the whole file to
//!   one scope. Multi-scope transactions (a paired admit, the release sweep's candidate
//!   pages) acquire their scope keys ASCENDING ([`advisory_keys_ascending`]). The
//!   retained-intent `COUNT(*)` serializes on [`RESERVED_COUNT_ADVISORY_KEY`] and the
//!   outbox claim on a second dedicated negative key ([`OUTBOX_CLAIM_ADVISORY_KEY`]) —
//!   both always first in ascending order (negative keys sort before non-negative scope
//!   keys).
//! - **Recovery pagers are REPEATABLE READ READ ONLY** transactions: one snapshot per
//!   page, exactly the read SQLite got from a deferred read transaction, with the
//!   5-way orphan probe first and the one-lookahead-record pagination.
//! - **Corruption reads.** SQLite's `typeof(x)='text' AND length(CAST(x AS BLOB)) <= cap`
//!   guards become `octet_length(x) <= cap` CASE projections: a bound-violating value
//!   projects NULL and falls to the corruption arm (fail-closed,
//!   `EvidenceError::CorruptObservation`), never truncates.
//! - **Schema evolution.** SQLite's `pragma_table_info` probes become
//!   `information_schema.columns` probes + `ALTER TABLE .. ADD COLUMN`, matching the
//!   SQLite evidence schema evolution column-for-column (`liability_released_at`,
//!   `back_reservation_id`, `context_version`, `attempt_version`, `attempt_id`).
//! - **`settled` stays a 0/1 `bigint`**, timestamps stay unix-seconds `bigint`s, and
//!   release-after-retention remains a zero-charge stamp — never a receipt.

mod attempts;
pub mod context;
mod dispatch;
mod paired;

use sqlx::postgres::PgConnection;
use sqlx::PgPool;
use time::{Duration, OffsetDateTime};

use sandhi_core::{Reservation, UsageBasis, UsageCompleteness, UsageV2};

use crate::ledger::evidence::context::ExecutionContext;
use crate::ledger::evidence::{
    encode_usage, mark_admission, opaque_id, validate_execution_id, validate_scope,
    AdmissionProgress, AdmissionStage, AttemptEvidence, ClaimedSettlement, EvidenceError,
    ExecutionIntent, IntentAdmission, ObservationOutcome, PhysicalAttempt, RecoveryCursor,
    RecoveryEntry, RecoveryPage, RecoveryState, ScopeCursor, ScopePage, SettlementOutcome,
    TerminalObservation, UnresolvedObservationReason,
};
use crate::ledger_pg::{
    advisory_keys_ascending, scope_advisory_key, LedgerStoreError, PostgresLedger,
    RESERVED_COUNT_ADVISORY_KEY,
};

/// Advisory key for the settlement-outbox claim (select-then-claim must be atomic —
/// SQLite used `BEGIN IMMEDIATE` for the whole batch). Dedicated negative key, like
/// [`RESERVED_COUNT_ADVISORY_KEY`], so it can never collide with a scope lock.
pub const OUTBOX_CLAIM_ADVISORY_KEY: i64 = -2;

/// Idempotent evidence schema init, mirroring the SQLite evidence DDL column-for-column
/// (types map TEXT→text, INTEGER→bigint). The evolved `budget_dispatch_fence` columns and
/// `liability_released_at` arrive through the same additive migration steps the SQLite
/// backend still runs, guarded by `information_schema.columns` probes instead of
/// `pragma_table_info`.
const EVIDENCE_DDL: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS budget_execution_intent (
        execution_id   text PRIMARY KEY NOT NULL,
        reservation_id bigint UNIQUE NOT NULL CHECK(reservation_id > 0),
        created_at     bigint NOT NULL
    )",
    "CREATE TABLE IF NOT EXISTS budget_request_correlation (
        execution_id text PRIMARY KEY NOT NULL REFERENCES budget_execution_intent(execution_id),
        request_id   text NOT NULL CHECK(octet_length(request_id) BETWEEN 1 AND 4096)
    )",
    "CREATE TABLE IF NOT EXISTS budget_dispatch_fence (
        execution_id    text PRIMARY KEY NOT NULL REFERENCES budget_execution_intent(execution_id),
        phase           bigint NOT NULL CHECK(phase IN (0, 1, 2)),
        transitioned_at bigint,
        CHECK((phase = 0 AND transitioned_at IS NULL) OR
              (phase IN (1, 2) AND transitioned_at IS NOT NULL))
    )",
    "CREATE TABLE IF NOT EXISTS budget_settlement_outbox (
        receipt_id      text PRIMARY KEY NOT NULL,
        reservation_id  bigint UNIQUE NOT NULL CHECK(reservation_id > 0),
        scope           text NOT NULL,
        charged_tokens  bigint NOT NULL CHECK(charged_tokens >= 0),
        settled_at      bigint NOT NULL,
        claim_token     text,
        claim_until     bigint,
        acknowledged_at bigint
    )",
    "CREATE TABLE IF NOT EXISTS budget_terminal_observation (
        execution_id  text PRIMARY KEY NOT NULL REFERENCES budget_execution_intent(execution_id),
        version       bigint NOT NULL CHECK(version = 1),
        usage_json    text NOT NULL CHECK(octet_length(usage_json) <= 16384),
        frozen_charge bigint CHECK(frozen_charge >= 0),
        observed_at   bigint NOT NULL
    )",
    // Same partial index, same name, same predicate as the SQLite evidence schema.
    "CREATE INDEX IF NOT EXISTS idx_settlement_pending \
         ON budget_settlement_outbox(settled_at, receipt_id) WHERE acknowledged_at IS NULL",
];

/// ADR-0048: policy release timestamp for aged unresolved liabilities. NULL means the
/// liability is still held; a timestamp means the retention window elapsed uncharged
/// (evidence columns untouched, no receipt written). Installed through the same
/// additive migration step the SQLite evidence schema uses.
const LIABILITY_RELEASED_AT: (&str, &str) = ("budget_execution_intent", "bigint");

/// The recovery pager and scope inventory run in one snapshot per page.
const RECOVERY_TX_SQL: &str = "SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY";

/// The 5-way orphan probe: a retained intent whose reservation vanished, or evidence
/// whose intent vanished, is unknowable scope-wise — the pager fails closed without
/// returning metadata from ANY scope (a scoped inner join alone would silently omit
/// these retained intents).
const ORPHAN_PROBE_SQL: &str = "SELECT \
     EXISTS(SELECT 1 FROM budget_execution_intent i \
            LEFT JOIN budget_reservation r ON r.id = i.reservation_id WHERE r.id IS NULL) \
     OR EXISTS(SELECT 1 FROM budget_terminal_observation o \
            LEFT JOIN budget_execution_intent i ON i.execution_id = o.execution_id \
            WHERE i.execution_id IS NULL) \
     OR EXISTS(SELECT 1 FROM budget_dispatch_fence f \
            LEFT JOIN budget_execution_intent i ON i.execution_id = f.execution_id \
            WHERE i.execution_id IS NULL) \
     OR EXISTS(SELECT 1 FROM budget_execution_back_leg b \
            LEFT JOIN budget_execution_intent i ON i.execution_id = b.execution_id \
            LEFT JOIN budget_reservation r ON r.id = b.reservation_id \
            WHERE i.execution_id IS NULL OR r.id IS NULL) \
     OR EXISTS(SELECT 1 FROM budget_physical_attempt a \
            LEFT JOIN budget_execution_intent i ON i.execution_id = a.execution_id \
            WHERE i.execution_id IS NULL)";

/// The guarded reclaim: a reservation pinned by execution evidence (intent, back leg or
/// fence back reference) must never be expiry-reclaimed — unknown usage keeps both
/// liabilities through expiry/restart (ADR-0043). Shared by the reserve path and the
/// explicit sweep; the SAME fragment the SQLite ledger carries inline.
pub(super) const EVIDENCE_RECLAIM_GUARD_SQL: &str = "\
     AND NOT EXISTS (SELECT 1 FROM budget_execution_intent \
                     WHERE reservation_id = budget_reservation.id) \
     AND NOT EXISTS (SELECT 1 FROM budget_execution_back_leg \
                     WHERE reservation_id = budget_reservation.id) \
     AND NOT EXISTS (SELECT 1 FROM budget_dispatch_fence \
                     WHERE back_reservation_id = budget_reservation.id)";

/// One-shot convergence for releases that shipped before the paired close: their
/// intents are already stamped (the candidate loop never revisits them) and reclaim
/// explicitly exempts back legs — the stranded upstream holds were permanent admission
/// denial. Stamped with the intent's OWN release stamp, not the sweep clock: the paired
/// state mirrors must be exactly equal and the primary kept the original stamp. Once
/// per sweep call, not per page transaction; idempotent via `settled = 0`.
const BACK_LEG_CONVERGENCE_SQL: &str = "\
     UPDATE budget_reservation SET actual = 0, settled = 1, \
         settled_at = (SELECT i.liability_released_at \
                       FROM budget_execution_back_leg b \
                       JOIN budget_execution_intent i ON i.execution_id = b.execution_id \
                       WHERE b.reservation_id = budget_reservation.id) \
      WHERE settled = 0 AND EXISTS ( \
        SELECT 1 FROM budget_execution_back_leg b \
        JOIN budget_execution_intent i ON i.execution_id = b.execution_id \
        WHERE b.reservation_id = budget_reservation.id \
          AND i.liability_released_at IS NOT NULL)";

/// The evidence→ledger error seam: the storage arm unwraps to its
/// [`LedgerStoreError`]; every evidence-level failure renders operator-safe into a
/// backend error. Used where a `*_durable` ledger method returns `LedgerStoreError`
/// but runs evidence probes internally.
pub(super) fn into_store_error(error: EvidenceError) -> LedgerStoreError {
    match error {
        EvidenceError::Storage(error) => error,
        other => LedgerStoreError::Backend(other.to_string()),
    }
}

/// Take the caller scope's transactional advisory lock inside the caller's transaction.
/// Every write transaction that SQLite ran under `BEGIN IMMEDIATE` takes the scope lock
/// here instead, so a concurrent same-scope transaction reads the committed winner
/// rather than racing a stale snapshot.
pub(super) async fn scope_lock(conn: &mut PgConnection, scope: &str) -> Result<(), EvidenceError> {
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(scope_advisory_key(scope))
        .execute(&mut *conn)
        .await?;
    Ok(())
}

/// `(reservation_id, scope, settled)` of an intent, or `None` when the execution id is
/// unknown. A dangling intent (reservation gone) and an intent shadowed by retained
/// observation without a binding are corruption, never legacy data.
async fn intent_binding(
    conn: &mut PgConnection,
    execution_id: &str,
) -> Result<Option<(u64, String, bool)>, EvidenceError> {
    let binding: Option<(Option<i64>, Option<String>, Option<i64>)> = sqlx::query_as(
        "SELECT r.id, r.scope, r.settled FROM budget_execution_intent i \
         LEFT JOIN budget_reservation r ON r.id = i.reservation_id WHERE i.execution_id = $1",
    )
    .bind(execution_id)
    .fetch_optional(&mut *conn)
    .await?;
    match binding {
        Some((Some(id), Some(scope), Some(settled))) => Ok(Some((id as u64, scope, settled != 0))),
        Some(_) => Err(EvidenceError::CorruptObservation),
        None => {
            let retained: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM budget_terminal_observation WHERE execution_id = $1)",
            )
            .bind(execution_id)
            .fetch_one(&mut *conn)
            .await?;
            if retained {
                Err(EvidenceError::CorruptObservation)
            } else {
                Ok(None)
            }
        }
    }
}

/// Read one retained terminal observation with the corruption-bounding projection.
async fn read_observation(
    conn: &mut PgConnection,
    execution_id: &str,
    reservation_id: u64,
    scope: &str,
) -> Result<Option<TerminalObservation>, EvidenceError> {
    let row: Option<(i64, Option<String>, Option<i64>, i64)> = sqlx::query_as(
        "SELECT version, \
                CASE WHEN octet_length(usage_json) <= 16384 THEN usage_json END, \
                frozen_charge, observed_at \
         FROM budget_terminal_observation WHERE execution_id = $1",
    )
    .bind(execution_id)
    .fetch_optional(&mut *conn)
    .await?;
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
            version: version as u32,
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

/// The one eligibility rule shared by recovery inventory and canonical settlement.
async fn terminal_unresolved_reason(
    conn: &mut PgConnection,
    observed: &TerminalObservation,
) -> Result<Option<UnresolvedObservationReason>, EvidenceError> {
    if attempts::read(&mut *conn, &observed.execution_id)
        .await?
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

async fn terminal_charge(
    conn: &mut PgConnection,
    observed: &TerminalObservation,
) -> Result<i64, EvidenceError> {
    if let Some(reason) = terminal_unresolved_reason(&mut *conn, observed).await? {
        return Err(EvidenceError::UnresolvedObservation(reason));
    }
    i64::try_from(
        observed
            .frozen_charge
            .ok_or(EvidenceError::CorruptObservation)?,
    )
    .map_err(|_| EvidenceError::CorruptObservation)
}

/// The receipt-consistency read behind recovery classification and settlement:
/// `settled = 1` with no receipt is a [`EvidenceError::LegacySettlement`] (kept), and
/// every other mismatch — including caller-supplied charges that disagree with the
/// stored receipt — is `InconsistentSettlement`.
async fn checked_terminal_receipt(
    conn: &mut PgConnection,
    scope: &str,
    id: u64,
    charge: Option<i64>,
) -> Result<Option<crate::ledger::evidence::SettlementReceipt>, EvidenceError> {
    let (scope_matches, settled, actual, settled_at): (bool, i64, i64, Option<i64>) =
        sqlx::query_as(
            "SELECT (scope = $2), settled, actual, settled_at \
             FROM budget_reservation WHERE id = $1",
        )
        .bind(id as i64)
        .bind(scope)
        .fetch_optional(&mut *conn)
        .await?
        .ok_or_else(|| {
            EvidenceError::Storage(LedgerStoreError::Backend(
                "statement affected no rows".into(),
            ))
        })?;
    let stored = receipt_by_reservation(&mut *conn, id, scope).await?;
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

/// The outbox projection for one reservation. A bound-violating `receipt_id` projects
/// NULL and fails the read (storage error), never truncates — the corruption path is
/// the caller's fail-closed arm.
async fn receipt_by_reservation(
    conn: &mut PgConnection,
    id: u64,
    scope: &str,
) -> Result<Option<crate::ledger::evidence::SettlementReceipt>, EvidenceError> {
    let row: Option<(String, i64, String, i64, i64)> = sqlx::query_as(
        "SELECT CASE WHEN octet_length(receipt_id) = 64 THEN receipt_id END, \
                reservation_id, CASE WHEN scope = $2 THEN $2 END, charged_tokens, settled_at \
         FROM budget_settlement_outbox WHERE reservation_id = $1",
    )
    .bind(id as i64)
    .bind(scope)
    .fetch_optional(&mut *conn)
    .await?;
    Ok(row.map(
        |(receipt_id, reservation_id, scope, charged_tokens, settled_at)| {
            crate::ledger::evidence::SettlementReceipt {
                receipt_id,
                reservation_id: reservation_id as u64,
                scope,
                charged_tokens: charged_tokens as u64,
                settled_at,
            }
        },
    ))
}

/// Atomically settle one lease and persist its immutable receipt (in the caller's
/// transaction). Replays return the original receipt.
async fn settle_receipt(
    conn: &mut PgConnection,
    scope: &str,
    id: i64,
    charge: i64,
) -> Result<SettlementOutcome, EvidenceError> {
    let existing: Option<(String, i64, String, i64, i64)> = sqlx::query_as(
        "SELECT receipt_id, reservation_id, scope, charged_tokens, settled_at \
         FROM budget_settlement_outbox WHERE reservation_id = $1",
    )
    .bind(id)
    .fetch_optional(&mut *conn)
    .await?;
    let existing = existing.map(
        |(receipt_id, reservation_id, scope, charged_tokens, settled_at)| {
            crate::ledger::evidence::SettlementReceipt {
                receipt_id,
                reservation_id: reservation_id as u64,
                scope,
                charged_tokens: charged_tokens as u64,
                settled_at,
            }
        },
    );
    if let Some(existing) = existing {
        if existing.scope != scope {
            return Err(EvidenceError::WrongScope);
        }
        if existing.charged_tokens != charge as u64 {
            return Err(EvidenceError::ConflictingCharge);
        }
        return Ok(SettlementOutcome::AlreadyCommitted(existing));
    }
    let lease: Option<(String, i64)> =
        sqlx::query_as("SELECT scope, settled FROM budget_reservation WHERE id = $1")
            .bind(id)
            .fetch_optional(&mut *conn)
            .await?;
    let (stored_scope, settled) = lease.ok_or(EvidenceError::MissingReservation)?;
    if stored_scope != scope {
        return Err(EvidenceError::WrongScope);
    }
    if settled != 0 {
        return Err(EvidenceError::LegacySettlement);
    }
    let settled_at = OffsetDateTime::now_utc().unix_timestamp();
    let receipt_id = opaque_id()?;
    let updated = sqlx::query(
        "UPDATE budget_reservation SET actual = $2, settled = 1, settled_at = $3 \
         WHERE id = $1 AND settled = 0",
    )
    .bind(id)
    .bind(charge)
    .bind(settled_at)
    .execute(&mut *conn)
    .await?;
    if updated.rows_affected() != 1 {
        return Err(EvidenceError::Storage(LedgerStoreError::Backend(
            "statement affected no rows".into(),
        )));
    }
    let inserted = sqlx::query(
        "INSERT INTO budget_settlement_outbox \
         (receipt_id, reservation_id, scope, charged_tokens, settled_at) \
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(&receipt_id)
    .bind(id)
    .bind(&stored_scope)
    .bind(charge)
    .bind(settled_at)
    .execute(&mut *conn)
    .await?;
    if inserted.rows_affected() != 1 {
        return Err(EvidenceError::Storage(LedgerStoreError::Backend(
            "statement affected no rows".into(),
        )));
    }
    Ok(SettlementOutcome::Committed(
        crate::ledger::evidence::SettlementReceipt {
            receipt_id,
            reservation_id: id as u64,
            scope: stored_scope,
            charged_tokens: charge as u64,
            settled_at,
        },
    ))
}

/// Tracked-reservation probe for the legacy settle path: an intent, a back leg or a
/// fence back-reference pins the reservation, which then refuses caller charges.
pub(super) async fn is_tracked(conn: &mut PgConnection, id: i64) -> Result<bool, EvidenceError> {
    let tracked: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM budget_execution_intent WHERE reservation_id = $1) \
         OR EXISTS(SELECT 1 FROM budget_execution_back_leg WHERE reservation_id = $1) \
         OR EXISTS(SELECT 1 FROM budget_dispatch_fence WHERE back_reservation_id = $1)",
    )
    .bind(id)
    .fetch_one(&mut *conn)
    .await?;
    Ok(tracked)
}

impl PostgresLedger {
    /// Opt-in library foundation: commit admission and its execution identity together
    /// before dispatch. Retained identities (including settled ones) count toward
    /// `retained_limit`, bounded to 1..=100_000 per ledger. No automatic pruning:
    /// unresolved leases retain capacity even after expiry.
    pub fn reserve_with_intent_durable(
        &self,
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

    /// Opt-in storage prerequisite for dispatch fencing. Caller must obtain the one-use
    /// permit before dispatch; HTTP does not use this yet.
    pub fn reserve_prepared_durable(
        &self,
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
            true,
            None,
            None,
            None,
        )
    }

    /// Prepared admission with immutable gateway correlation, never a replay key.
    #[allow(clippy::too_many_arguments)]
    pub fn reserve_correlated_durable(
        &self,
        scope: &str,
        ceiling: u64,
        now: OffsetDateTime,
        ttl: Duration,
        retained_limit: usize,
        request_id: &str,
    ) -> Result<IntentAdmission, EvidenceError> {
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

    #[allow(clippy::too_many_arguments)]
    pub(super) fn reserve_intent(
        &self,
        scope: &str,
        ceiling: u64,
        now: OffsetDateTime,
        ttl: Duration,
        retained_limit: usize,
        prepared: bool,
        request_id: Option<&str>,
        context: Option<context::PreparedContext<'_>>,
        progress: Option<&crate::ledger::evidence::AdmissionProgress>,
    ) -> Result<IntentAdmission, EvidenceError> {
        self.block(self.reserve_intent_async(
            scope,
            ceiling,
            now,
            ttl,
            retained_limit,
            prepared,
            request_id,
            context,
            progress,
        ))
    }

    #[allow(clippy::too_many_arguments)]
    async fn reserve_intent_async(
        &self,
        scope: &str,
        ceiling: u64,
        now: OffsetDateTime,
        ttl: Duration,
        retained_limit: usize,
        prepared: bool,
        request_id: Option<&str>,
        context: Option<context::PreparedContext<'_>>,
        progress: Option<&crate::ledger::evidence::AdmissionProgress>,
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
        let mut tx = self.pool.begin().await?;
        // The retained-intent COUNT and every touched scope, in ascending key order
        // (the negative reserved-count key sorts first). This is the global lock order
        // that makes concurrent opposite-order pairing deadlock-free.
        let mut lock_keys = vec![RESERVED_COUNT_ADVISORY_KEY];
        match context.as_ref().map(|c| c.paired_scope) {
            Some(Some(back_scope)) if back_scope != scope => {
                lock_keys.extend(advisory_keys_ascending(&[scope, back_scope]));
            }
            _ => lock_keys.push(scope_advisory_key(scope)),
        }
        for key in lock_keys {
            sqlx::query("SELECT pg_advisory_xact_lock($1)")
                .bind(key)
                .execute(&mut *tx)
                .await?;
        }
        mark_admission(progress, AdmissionStage::ReadCapacity);
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*)::bigint FROM budget_execution_intent")
            .fetch_one(&mut *tx)
            .await?;
        if count >= retained_limit as i64 {
            tx.rollback().await?;
            return Err(EvidenceError::IntentCapacity);
        }
        mark_admission(progress, AdmissionStage::ReservePrimary);
        let reservation = match reserve_in_transaction(&mut tx, scope, ceiling, now, ttl).await? {
            IntentReserve::Admitted(reservation) => reservation,
            IntentReserve::Denied(denied) => {
                tx.rollback().await?;
                return Ok(IntentAdmission::Denied(denied));
            }
        };
        let back = if let Some(back_scope) = context.as_ref().and_then(|c| c.paired_scope) {
            if back_scope == scope {
                return Err(EvidenceError::InvalidInput);
            }
            mark_admission(progress, AdmissionStage::ReserveBackLeg);
            match reserve_in_transaction(&mut tx, back_scope, ceiling, now, ttl).await? {
                IntentReserve::Admitted(back) => Some(back),
                IntentReserve::Denied(denied) => {
                    tx.rollback().await?;
                    return Ok(IntentAdmission::Denied(denied));
                }
            }
        } else {
            None
        };
        mark_admission(progress, AdmissionStage::WriteIntent);
        sqlx::query(
            "INSERT INTO budget_execution_intent (execution_id, reservation_id, created_at) \
             VALUES ($1, $2, $3)",
        )
        .bind(&execution_id)
        .bind(reservation.id as i64)
        .bind(now.unix_timestamp())
        .execute(&mut *tx)
        .await?;
        if prepared {
            mark_admission(progress, AdmissionStage::WriteFence);
            sqlx::query("INSERT INTO budget_dispatch_fence (execution_id, phase) VALUES ($1, 0)")
                .bind(&execution_id)
                .execute(&mut *tx)
                .await?;
        }
        if let Some(request_id) = request_id {
            mark_admission(progress, AdmissionStage::WriteCorrelation);
            sqlx::query(
                "INSERT INTO budget_request_correlation(execution_id, request_id) VALUES ($1, $2)",
            )
            .bind(&execution_id)
            .bind(request_id)
            .execute(&mut *tx)
            .await?;
        }
        if let Some(context) = context {
            mark_admission(progress, AdmissionStage::WriteContext);
            sqlx::query(
                "INSERT INTO budget_execution_context(execution_id, version, context_json) \
                 VALUES ($1, 1, $2)",
            )
            .bind(&execution_id)
            .bind(context.json)
            .execute(&mut *tx)
            .await?;
            sqlx::query(
                "UPDATE budget_dispatch_fence SET context_version = 1 WHERE execution_id = $1",
            )
            .bind(&execution_id)
            .execute(&mut *tx)
            .await?;
        }
        if let Some(back) = back {
            mark_admission(progress, AdmissionStage::WriteBackLeg);
            sqlx::query(
                "INSERT INTO budget_execution_back_leg(execution_id, version, reservation_id) \
                 VALUES ($1, 1, $2)",
            )
            .bind(&execution_id)
            .bind(back.id as i64)
            .execute(&mut *tx)
            .await?;
            sqlx::query(
                "UPDATE budget_dispatch_fence SET back_reservation_id = $2, attempt_version = 1 \
                 WHERE execution_id = $1",
            )
            .bind(&execution_id)
            .bind(back.id as i64)
            .execute(&mut *tx)
            .await?;
        }
        mark_admission(progress, AdmissionStage::Commit);
        tx.commit().await?;
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
        self.block(self.intent_async(execution_id))
    }

    async fn intent_async(
        &self,
        execution_id: &str,
    ) -> Result<Option<ExecutionIntent>, EvidenceError> {
        let row: Option<(i64, String, i64, i64)> = sqlx::query_as(
            "SELECT r.id, r.scope, r.ceiling, r.expires_at FROM budget_execution_intent i \
             JOIN budget_reservation r ON r.id = i.reservation_id WHERE i.execution_id = $1",
        )
        .bind(execution_id)
        .fetch_optional(&self.pool)
        .await?;
        row.map(|(id, scope, ceiling, expires_at)| {
            let expires_at = OffsetDateTime::from_unix_timestamp(expires_at)
                .map_err(|_| EvidenceError::InvalidInput)?;
            Ok(ExecutionIntent {
                execution_id: execution_id.to_string(),
                reservation: sandhi_core::Reservation {
                    id: id as u64,
                    scope,
                    ceiling: ceiling as u64,
                    expires_at,
                },
            })
        })
        .transpose()
    }

    /// Read the immutable gateway correlation recorded at admission.
    pub fn request_id_durable(
        &self,
        scope: &str,
        execution_id: &str,
    ) -> Result<Option<String>, EvidenceError> {
        validate_scope(scope)?;
        self.block(self.request_id_async(scope, execution_id))
    }

    async fn request_id_async(
        &self,
        scope: &str,
        execution_id: &str,
    ) -> Result<Option<String>, EvidenceError> {
        let intent = self
            .intent_async(execution_id)
            .await?
            .ok_or(EvidenceError::MissingIntent)?;
        if intent.reservation.scope != scope {
            return Err(EvidenceError::WrongScope);
        }
        let value: Option<String> = sqlx::query_scalar(
            "SELECT request_id FROM budget_request_correlation WHERE execution_id = $1",
        )
        .bind(execution_id)
        .fetch_optional(&self.pool)
        .await?
        .flatten();
        if value
            .as_ref()
            .is_some_and(|v| v.is_empty() || v.len() > 4096)
        {
            return Err(EvidenceError::CorruptObservation);
        }
        Ok(value)
    }

    /// Bounded scope inventory includes unbudgeted and removed-key scopes. New admissions
    /// beyond the frozen upper bound are visited by the next sweep.
    pub fn recovery_scopes_durable(
        &self,
        cursor: Option<&ScopeCursor>,
        limit: usize,
    ) -> Result<ScopePage, EvidenceError> {
        if !(1..=100).contains(&limit) {
            return Err(EvidenceError::InvalidInput);
        }
        self.block(self.recovery_scopes_async(cursor, limit))
    }

    async fn recovery_scopes_async(
        &self,
        cursor: Option<&ScopeCursor>,
        limit: usize,
    ) -> Result<ScopePage, EvidenceError> {
        let mut tx = self.pool.begin().await?;
        sqlx::query(RECOVERY_TX_SQL).execute(&mut *tx).await?;
        let through_id = match cursor {
            Some(c) => c.through_id,
            None => {
                let max: i64 = sqlx::query_scalar(
                    "SELECT COALESCE(MAX(reservation_id), 0)::bigint FROM budget_execution_intent",
                )
                .fetch_one(&mut *tx)
                .await?;
                max as u64
            }
        };
        let after = cursor.map_or("", |c| c.after.as_str());
        let rows: Vec<Option<String>> = sqlx::query_scalar(
            "SELECT DISTINCT r.scope FROM budget_execution_intent i \
             LEFT JOIN budget_reservation r ON r.id = i.reservation_id \
             WHERE i.reservation_id <= $1 AND (r.scope > $2 OR r.scope IS NULL) \
             ORDER BY r.scope LIMIT $3",
        )
        .bind(through_id as i64)
        .bind(after)
        .bind(limit as i64)
        .fetch_all(&mut *tx)
        .await?;
        let mut scopes = Vec::with_capacity(rows.len());
        for scope in rows {
            let scope = scope.ok_or(EvidenceError::MissingReservation)?;
            validate_scope(&scope)?;
            scopes.push(scope);
        }
        let next = (scopes.len() == limit).then(|| ScopeCursor {
            after: scopes.last().unwrap().clone(),
            through_id,
        });
        tx.commit().await?;
        Ok(ScopePage { scopes, next })
    }

    /// Read 1..=100 scoped records, ordered by the existing reservation identity.
    /// Each page is a short REPEATABLE READ READ ONLY transaction; the cursor freezes
    /// the admission upper bound, not state across pages. No state filtering before
    /// LIMIT, claiming, settlement, release or inference retry. Corruption is an error.
    pub fn recovery_page_durable(
        &self,
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
        self.block(self.recovery_page_async(scope, cursor, limit))
    }

    async fn recovery_page_async(
        &self,
        scope: &str,
        cursor: Option<&RecoveryCursor>,
        limit: usize,
    ) -> Result<RecoveryPage, EvidenceError> {
        let mut tx = self.pool.begin().await?;
        // One snapshot per page: the same read SQLite got from its deferred read
        // transaction, without racing concurrent settlements.
        sqlx::query(RECOVERY_TX_SQL).execute(&mut *tx).await?;
        // Orphan scope is unknowable. Fail closed without returning metadata from any
        // scope; a scoped inner join alone would silently omit these retained intents.
        let orphaned: bool = sqlx::query_scalar(ORPHAN_PROBE_SQL)
            .fetch_one(&mut *tx)
            .await?;
        if orphaned {
            return Err(EvidenceError::CorruptObservation);
        }
        let (after_id, through_id) = match cursor {
            Some(cursor) => (cursor.after_id, cursor.through_id),
            None => {
                let max: i64 = sqlx::query_scalar(
                    "SELECT COALESCE(MAX(i.reservation_id), 0)::bigint \
                     FROM budget_execution_intent i \
                     JOIN budget_reservation r ON r.id = i.reservation_id WHERE r.scope = $1",
                )
                .bind(scope)
                .fetch_one(&mut *tx)
                .await?;
                (0, max as u64)
            }
        };
        // One lookahead record tells the caller whether another page exists.
        let mut rows: Vec<(Option<String>, i64, Option<i64>)> = sqlx::query_as(
            "SELECT CASE WHEN octet_length(i.execution_id) = 64 THEN i.execution_id END, \
                    i.reservation_id, i.liability_released_at \
             FROM budget_execution_intent i \
             JOIN budget_reservation r ON r.id = i.reservation_id \
             WHERE r.scope = $1 AND i.reservation_id > $2 AND i.reservation_id <= $3 \
             ORDER BY i.reservation_id LIMIT $4",
        )
        .bind(scope)
        .bind(after_id as i64)
        .bind(through_id as i64)
        .bind((limit + 1) as i64)
        .fetch_all(&mut *tx)
        .await?;
        let has_more = rows.len() > limit;
        rows.truncate(limit);
        let mut entries = Vec::with_capacity(rows.len());
        for (execution_id, reservation_id, released_at) in rows {
            let reservation_id = reservation_id as u64;
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
            let fence =
                dispatch::checked_fence(&mut tx, scope, &execution_id, reservation_id).await?;
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
            let observed = read_observation(&mut tx, &execution_id, reservation_id, scope).await?;
            let unresolved_reason = match observed.as_ref() {
                Some(o) => terminal_unresolved_reason(&mut tx, o).await?,
                None => None,
            };
            let charge = if unresolved_reason.is_some() {
                None
            } else {
                match observed.as_ref().map(|o| terminal_charge(&mut tx, o)) {
                    Some(future) => Some(future.await?),
                    None => None,
                }
            };
            let receipt = checked_terminal_receipt(&mut tx, scope, reservation_id, charge).await?;
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
        tx.commit().await?;
        Ok(RecoveryPage { entries, next })
    }

    /// ADR-0048: release the held budget liability of every execution intent whose
    /// admission age exceeds `cutoff` and that has NO final source evidence.
    /// Released rows keep their full evidence, gain `liability_released_at`, and their
    /// reservation closes as an explicit zero-charge policy release — `spent` is never
    /// touched and no settlement receipt is written, so nothing is fabricated. The
    /// paired back leg closes with the same zero-charge release. Rows that settled or
    /// carry a settleable charge are skipped untouched. Re-verification happens inside
    /// the same page transaction as the stamp, under every candidate scope's advisory
    /// lock (keys ascending), so a race with live settlement resolves to the receipt,
    /// never to a release. Returns the released count.
    pub fn release_aged_unresolved_liabilities_durable(
        &self,
        now: OffsetDateTime,
        cutoff: OffsetDateTime,
    ) -> Result<usize, EvidenceError> {
        let released_at = now.unix_timestamp();
        let cutoff_ts = cutoff.unix_timestamp();
        self.block(self.release_aged_async(released_at, cutoff_ts))
    }

    async fn release_aged_async(
        &self,
        released_at: i64,
        cutoff_ts: i64,
    ) -> Result<usize, EvidenceError> {
        let mut released = 0usize;
        let mut converged = false;
        let mut after_id = 0i64;
        loop {
            // Page candidates in one read transaction (READ COMMITTED: the page
            // reflects post-commit state of any concurrently settling transaction).
            let mut tx = self.pool.begin().await?;
            // Convergence runs ONCE per sweep call, BEFORE the candidate early-return
            // (SQLite ordering): stamped intents' stranded back legs heal even when the
            // candidate list is empty, because they are never revisited. Guarded by
            // `settled = 0`, so a concurrent settlement always wins the race it should.
            if !converged {
                sqlx::query(BACK_LEG_CONVERGENCE_SQL)
                    .execute(&mut *tx)
                    .await?;
                converged = true;
            }
            // Settled reservations carry no liability — they are excluded here rather
            // than classified, because the receipt-consistency machinery can reject a
            // settled row and must never gate the release of other aged liabilities.
            let candidates: Vec<(String, i64, String)> = sqlx::query_as(
                "SELECT i.execution_id, i.reservation_id, r.scope \
                 FROM budget_execution_intent i \
                 JOIN budget_reservation r ON r.id = i.reservation_id \
                 WHERE i.liability_released_at IS NULL AND i.created_at <= $1 \
                   AND r.settled = 0 \
                   AND i.reservation_id > $2 \
                 ORDER BY i.reservation_id LIMIT 200",
            )
            .bind(cutoff_ts)
            .bind(after_id)
            .fetch_all(&mut *tx)
            .await?;
            tx.commit().await?;
            if candidates.is_empty() {
                return Ok(released);
            }
            let candidate_count = candidates.len();
            for (execution_id, reservation_id, scope) in candidates {
                after_id = after_id.max(reservation_id);
                // One transaction per candidate: the stamp and the release commit
                // together or not at all, and a poisoned row rolls back alone — one
                // inconsistent record can never block the release of another aged
                // liability (the sweep converges around it, retaining it in place).
                let mut ctx = self.pool.begin().await?;
                // Serialize against per-scope settlement transactions.
                scope_lock(&mut ctx, &scope).await?;
                let stamped = Self::stamp_if_liable(
                    &mut ctx,
                    released_at,
                    &execution_id,
                    reservation_id as u64,
                    &scope,
                )
                .await;
                match stamped {
                    Ok(true) => {
                        ctx.commit().await?;
                        released += 1;
                    }
                    Ok(false) => ctx.commit().await?,
                    Err(error) => {
                        tracing::warn!(
                            %error,
                            execution_id = %execution_id,
                            "aged liability release skipped; row retained in place"
                        );
                        ctx.rollback().await.ok();
                    }
                }
            }
            if candidate_count != 200 {
                return Ok(released);
            }
        }
    }

    /// Classify one aged candidate and, when it is the unresolved-liability incident
    /// class, stamp the release and settle it uncharged in place. Returns whether the
    /// row was released; every other state (terminal, settleable, already stamped)
    /// leaves the row untouched for recovery or a later sweep. Same classification as
    /// `recovery_page_durable`, evaluated inside the caller's scope-locked transaction.
    async fn stamp_if_liable(
        conn: &mut PgConnection,
        released_at: i64,
        execution_id: &str,
        reservation_id: u64,
        scope: &str,
    ) -> Result<bool, EvidenceError> {
        validate_execution_id(execution_id).map_err(|_| EvidenceError::CorruptObservation)?;
        validate_scope(scope)?;
        let fence = dispatch::checked_fence(conn, scope, execution_id, reservation_id).await?;
        // Closed-before-dispatch is already terminal; a Prepared intent is
        // stampable — at retention age nothing is mid-flight anymore.
        if matches!(&fence, Some(dispatch::Fence::Closed(_))) {
            return Ok(false);
        }
        let observed = read_observation(conn, execution_id, reservation_id, scope).await?;
        let unresolved_reason = match observed.as_ref() {
            Some(o) => terminal_unresolved_reason(conn, o).await?,
            None => None,
        };
        let charge = if unresolved_reason.is_some() {
            None
        } else {
            match observed.as_ref().map(|o| terminal_charge(conn, o)) {
                Some(future) => Some(future.await?),
                None => None,
            }
        };
        let receipt = checked_terminal_receipt(conn, scope, reservation_id, charge).await?;
        let stampable = match (receipt, observed, charge) {
            // Final usage exists: recovery settles it, never the release.
            (_, Some(_), Some(_)) | (Some(_), _, _) => false,
            // Dispatched or observed without final usage: the incident class.
            (None, _, _) => true,
        };
        if !stampable {
            return Ok(false);
        }
        sqlx::query(
            "UPDATE budget_execution_intent SET liability_released_at = $1 \
             WHERE execution_id = $2 AND liability_released_at IS NULL",
        )
        .bind(released_at)
        .bind(execution_id)
        .execute(&mut *conn)
        .await?;
        let changed = sqlx::query(
            "UPDATE budget_reservation SET actual = 0, settled = 1, settled_at = $1 \
             WHERE id = $2 AND settled = 0",
        )
        .bind(released_at)
        .bind(reservation_id as i64)
        .execute(&mut *conn)
        .await?;
        if changed.rows_affected() != 1 {
            return Err(EvidenceError::CorruptObservation);
        }
        // The paired back leg rides the primary's settle on the normal path
        // (paired::settle); a policy release must close it the same way or its
        // upstream hold stays unsettled forever. An already-settled or absent
        // back leg is left untouched.
        sqlx::query(
            "UPDATE budget_reservation SET actual = 0, settled = 1, settled_at = $1 \
             WHERE id = (SELECT reservation_id FROM budget_execution_back_leg \
                         WHERE execution_id = $2) \
               AND settled = 0",
        )
        .bind(released_at)
        .bind(execution_id)
        .execute(&mut *conn)
        .await?;
        Ok(true)
    }

    /// Settle a tracked execution using its immutable stored charge. Only final
    /// provider-reported usage can release the reservation. Replays, including after
    /// receipt acknowledgement, return the original receipt.
    pub fn settle_terminal_durable(
        &self,
        scope: &str,
        execution_id: &str,
    ) -> Result<SettlementOutcome, EvidenceError> {
        validate_scope(scope)?;
        validate_execution_id(execution_id)?;
        self.block(self.settle_terminal_async(scope, execution_id))
    }

    async fn settle_terminal_async(
        &self,
        scope: &str,
        execution_id: &str,
    ) -> Result<SettlementOutcome, EvidenceError> {
        let mut tx = self.pool.begin().await?;
        scope_lock(&mut tx, scope).await?;
        let (reservation_id, stored_scope, _) = intent_binding(&mut tx, execution_id)
            .await?
            .ok_or(EvidenceError::MissingIntent)?;
        if stored_scope != scope {
            return Err(EvidenceError::WrongScope);
        }
        dispatch::require_observable(&mut tx, scope, execution_id, reservation_id).await?;
        let observed = read_observation(&mut tx, execution_id, reservation_id, &stored_scope)
            .await?
            .ok_or(EvidenceError::MissingObservation)?;
        let id = i64::try_from(reservation_id).map_err(|_| EvidenceError::CorruptObservation)?;
        let charge = terminal_charge(&mut tx, &observed).await?;
        checked_terminal_receipt(&mut tx, scope, reservation_id, Some(charge)).await?;
        let back = paired::read(&mut tx, execution_id, reservation_id).await?;
        let outcome = settle_receipt(&mut tx, scope, id, charge).await?;
        paired::settle(&mut tx, back.as_ref(), &outcome).await?;
        paired::read(&mut tx, execution_id, reservation_id).await?;
        tx.commit().await?;
        Ok(outcome)
    }

    /// Persist one immutable execution-level terminal usage snapshot. The intent owns
    /// reservation/scope identity; this method never settles, releases or retries
    /// inference. Exact replay returns the stored timestamp/charge.
    pub fn record_terminal_durable(
        &self,
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
        &self,
        scope: &str,
        execution_id: &str,
        usage: &UsageV2,
        cache_read_bp: u32,
        attempt: &crate::ledger::evidence::AttemptEvidence,
    ) -> Result<ObservationOutcome, EvidenceError> {
        self.record_terminal_inner(scope, execution_id, usage, cache_read_bp, Some(attempt))
    }

    fn record_terminal_inner(
        &self,
        scope: &str,
        execution_id: &str,
        usage: &UsageV2,
        cache_read_bp: u32,
        attempt: Option<&crate::ledger::evidence::AttemptEvidence>,
    ) -> Result<ObservationOutcome, EvidenceError> {
        self.block(self.record_terminal_async(scope, execution_id, usage, cache_read_bp, attempt))
    }

    async fn record_terminal_async(
        &self,
        scope: &str,
        execution_id: &str,
        usage: &UsageV2,
        cache_read_bp: u32,
        attempt: Option<&crate::ledger::evidence::AttemptEvidence>,
    ) -> Result<ObservationOutcome, EvidenceError> {
        validate_scope(scope)?;
        validate_execution_id(execution_id)?;
        let encoded = encode_usage(usage)?;
        let mut tx = self.pool.begin().await?;
        scope_lock(&mut tx, scope).await?;
        let (reservation_id, stored_scope, settled) = intent_binding(&mut tx, execution_id)
            .await?
            .ok_or(EvidenceError::MissingIntent)?;
        if stored_scope != scope {
            return Err(EvidenceError::WrongScope);
        }
        dispatch::require_observable(&mut tx, scope, execution_id, reservation_id).await?;
        if context::read(&mut tx, execution_id).await?.is_some_and(
            |context: crate::ledger::evidence::context::ExecutionContext| {
                context.accounting_policy.cache_read_weight.basis_points() != cache_read_bp
            },
        ) {
            return Err(EvidenceError::ConflictingCharge);
        }
        attempts::record(&mut tx, execution_id, attempt).await?;
        if let Some(original) =
            read_observation(&mut tx, execution_id, reservation_id, scope).await?
        {
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
        sqlx::query(
            "INSERT INTO budget_terminal_observation \
             (execution_id, version, usage_json, frozen_charge, observed_at) \
             VALUES ($1, 1, $2, $3, $4)",
        )
        .bind(execution_id)
        .bind(&encoded)
        .bind(charge)
        .bind(observed_at)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
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
    /// authorization. Unknown IDs/unobserved intents return None; invalid/corrupt usage
    /// fails.
    pub fn terminal_durable(
        &self,
        scope: &str,
        execution_id: &str,
    ) -> Result<Option<TerminalObservation>, EvidenceError> {
        validate_scope(scope)?;
        validate_execution_id(execution_id)?;
        self.block(self.terminal_async(scope, execution_id))
    }

    async fn terminal_async(
        &self,
        scope: &str,
        execution_id: &str,
    ) -> Result<Option<TerminalObservation>, EvidenceError> {
        let mut tx = self.pool.begin().await?;
        let Some((reservation_id, stored_scope, _)) = intent_binding(&mut tx, execution_id).await?
        else {
            return Ok(None);
        };
        if stored_scope != scope {
            return Err(EvidenceError::WrongScope);
        }
        let observation = read_observation(&mut tx, execution_id, reservation_id, scope).await?;
        tx.commit().await?;
        Ok(observation)
    }

    /// Atomically settle one lease and persist its immutable receipt. Replays return
    /// the original receipt, including after acknowledgement. Missing or legacy leases
    /// are NOT successful settlements. Tracked reservations reject caller charges.
    pub fn settle_with_evidence_durable(
        &self,
        scope: &str,
        reservation_id: u64,
        charged_tokens: u64,
    ) -> Result<SettlementOutcome, EvidenceError> {
        let id = i64::try_from(reservation_id).map_err(|_| EvidenceError::InvalidInput)?;
        let charge = i64::try_from(charged_tokens).map_err(|_| EvidenceError::InvalidInput)?;
        if id <= 0 {
            return Err(EvidenceError::InvalidInput);
        }
        self.block(self.settle_with_evidence_async(scope, id, charge))
    }

    async fn settle_with_evidence_async(
        &self,
        scope: &str,
        id: i64,
        charge: i64,
    ) -> Result<SettlementOutcome, EvidenceError> {
        let mut tx = self.pool.begin().await?;
        scope_lock(&mut tx, scope).await?;
        if is_tracked(&mut tx, id).await? {
            return Err(EvidenceError::TrackedSettlementRequired);
        }
        let outcome = settle_receipt(&mut tx, scope, id, charge).await?;
        tx.commit().await?;
        Ok(outcome)
    }

    /// Claim at most 1..=1000 pending receipts for 1..=3600 seconds. The select-then-
    /// claim batch is one transaction under the dedicated outbox advisory lock — SQLite's
    /// `BEGIN IMMEDIATE` narrowed from the whole file to the outbox lane. Reissued claims
    /// fence old acknowledgements.
    pub fn claim_settlements_durable(
        &self,
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
        self.block(self.claim_settlements_async(limit, now, until))
    }

    async fn claim_settlements_async(
        &self,
        limit: usize,
        now: i64,
        until: i64,
    ) -> Result<Vec<ClaimedSettlement>, EvidenceError> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("SELECT pg_advisory_xact_lock($1)")
            .bind(OUTBOX_CLAIM_ADVISORY_KEY)
            .execute(&mut *tx)
            .await?;
        let pending: Vec<(String, i64, String, i64, i64)> = sqlx::query_as(
            "SELECT receipt_id, reservation_id, scope, charged_tokens, settled_at \
             FROM budget_settlement_outbox WHERE acknowledged_at IS NULL \
             AND (claim_until IS NULL OR claim_until <= $1) \
             ORDER BY settled_at, receipt_id LIMIT $2",
        )
        .bind(now)
        .bind(limit as i64)
        .fetch_all(&mut *tx)
        .await?;
        let mut claims = Vec::with_capacity(pending.len());
        for (receipt_id, reservation_id, scope, charged_tokens, settled_at) in pending {
            let claim_token = opaque_id()?;
            let updated = sqlx::query(
                "UPDATE budget_settlement_outbox SET claim_token = $2, claim_until = $3 \
                 WHERE receipt_id = $1",
            )
            .bind(&receipt_id)
            .bind(&claim_token)
            .bind(until)
            .execute(&mut *tx)
            .await?;
            if updated.rows_affected() != 1 {
                return Err(EvidenceError::Storage(LedgerStoreError::Backend(
                    "statement affected no rows".into(),
                )));
            }
            claims.push(ClaimedSettlement {
                receipt: crate::ledger::evidence::SettlementReceipt {
                    receipt_id,
                    reservation_id: reservation_id as u64,
                    scope,
                    charged_tokens: charged_tokens as u64,
                    settled_at,
                },
                claim_token,
                lease_until: until,
            });
        }
        tx.commit().await?;
        Ok(claims)
    }

    /// Acknowledge only a current, unexpired claim. A bare guarded UPDATE (compare-and-
    /// set on the claim token), exactly like the SQLite arm: false for unknown, expired,
    /// superseded OR already acknowledged claims. The immutable receipt is retained
    /// forever until a separately reviewed retention/migration policy exists.
    pub fn acknowledge_settlement_durable(
        &self,
        receipt_id: &str,
        claim_token: &str,
        now: OffsetDateTime,
    ) -> Result<bool, EvidenceError> {
        let updated = self.block(
            sqlx::query(
                "UPDATE budget_settlement_outbox SET acknowledged_at = $3 \
             WHERE receipt_id = $1 AND claim_token = $2 AND claim_until > $3 \
               AND acknowledged_at IS NULL",
            )
            .bind(receipt_id)
            .bind(claim_token)
            .bind(now.unix_timestamp())
            .execute(&self.pool),
        );
        Ok(updated?.rows_affected() == 1)
    }
}

/// The one reservation calculation shared by every intent admission path — the PG
/// dialect of the SQLite ledger's private `reserve_in_transaction`, returning the
/// denied payload instead of reserving when a hard cap would breach.
async fn reserve_in_transaction(
    conn: &mut PgConnection,
    scope: &str,
    ceiling: u64,
    now: OffsetDateTime,
    ttl: Duration,
) -> Result<IntentReserve, EvidenceError> {
    let checked_ceiling = i64::try_from(ceiling).map_err(|_| EvidenceError::InvalidInput)?;
    let now_ts = now.unix_timestamp();
    let expires_at = (now + ttl).unix_timestamp();
    // Opportunistic reclaim: drop this scope's unsettled, expired leases before
    // measuring — never a reservation pinned by execution evidence (unknown usage
    // keeps both liabilities through expiry/restart, ADR-0043).
    sqlx::query(&format!(
        "DELETE FROM budget_reservation \
         WHERE scope = $1 AND settled = 0 AND expires_at <= $2 {EVIDENCE_RECLAIM_GUARD_SQL}"
    ))
    .bind(scope)
    .bind(now_ts)
    .execute(&mut *conn)
    .await?;

    let budgets: Vec<(Option<i64>, String, String)> = sqlx::query_as(&format!(
        "SELECT limit_tokens, \"window\", policy FROM budget_limit \
         WHERE scope = $1 ORDER BY {}",
        crate::ledger_pg::WINDOW_ORDER_SQL
    ))
    .bind(scope)
    .fetch_all(&mut *conn)
    .await?;

    if !budgets.is_empty() {
        let reserved: i64 = sqlx::query_scalar(
            "SELECT COALESCE(SUM(ceiling), 0)::bigint FROM budget_reservation \
             WHERE scope = $1 AND settled = 0",
        )
        .bind(scope)
        .fetch_one(&mut *conn)
        .await?;
        for (limit, window, policy) in budgets {
            let Some(limit) = limit else { continue };
            let start = crate::window_start_ts(now, sandhi_core::Window::parse(&window));
            let spent: i64 = sqlx::query_scalar(
                "SELECT COALESCE(SUM(actual), 0)::bigint FROM budget_reservation \
                 WHERE scope = $1 AND settled = 1 AND COALESCE(settled_at, 0) > $2",
            )
            .bind(scope)
            .bind(start)
            .fetch_one(&mut *conn)
            .await?;
            // `Warn` is a soft cap: it never denies admission.
            if sandhi_core::Policy::parse(&policy) == sandhi_core::Policy::Block
                && i128::from(spent) + i128::from(reserved) + i128::from(ceiling)
                    > i128::from(limit)
            {
                return Ok(IntentReserve::Denied(sandhi_core::Denied {
                    scope: scope.to_string(),
                    limit: limit.max(0) as u64,
                    spent: spent.max(0) as u64,
                    reserved: reserved.max(0) as u64,
                    requested_ceiling: ceiling,
                }));
            }
        }
    }

    let id: i64 = sqlx::query_scalar(
        "INSERT INTO budget_reservation (scope, ceiling, expires_at) \
         VALUES ($1, $2, $3) RETURNING id",
    )
    .bind(scope)
    .bind(checked_ceiling)
    .bind(expires_at)
    .fetch_one(&mut *conn)
    .await?;
    Ok(IntentReserve::Admitted(sandhi_core::Reservation {
        id: id as u64,
        scope: scope.to_string(),
        ceiling,
        expires_at: now + ttl,
    }))
}

/// Result of [`reserve_in_transaction`]: the reservation to write intent state for, or
/// the denial payload the caller returns (rolling the whole admission back).
enum IntentReserve {
    Admitted(sandhi_core::Reservation),
    Denied(sandhi_core::Denied),
}

impl PostgresLedger {
    /// One full binding check shared by terminal evidence and dispatch transitions —
    /// the PG twin of [`crate::ShardedLedger::checked_intent`]. A caller-supplied intent
    /// is never authorization: the persisted original must match scope, reservation
    /// identity and the binding at the persisted expiry precision.
    fn checked_intent(&self, intent: &ExecutionIntent) -> Result<ExecutionIntent, EvidenceError> {
        let original = self
            .intent_durable(&intent.execution_id)?
            .ok_or(EvidenceError::MissingIntent)?;
        if original.reservation.scope != intent.reservation.scope {
            return Err(EvidenceError::WrongScope);
        }
        // Admission returns the caller's timestamp precision; PG persists expiry as
        // whole Unix seconds too. Compare the binding at that precision.
        if original.reservation.id != intent.reservation.id
            || original.reservation.ceiling != intent.reservation.ceiling
            || original.reservation.expires_at.unix_timestamp()
                != intent.reservation.expires_at.unix_timestamp()
        {
            return Err(EvidenceError::InvalidInput);
        }
        Ok(original)
    }

    /// Validate the original admission before requesting its one dispatch permit.
    /// Scope matching is not caller authorization; a replay never gets another permit.
    pub fn authorize_dispatch_for_intent_durable(
        &self,
        intent: &ExecutionIntent,
    ) -> Result<crate::ledger::evidence::DispatchOutcome, EvidenceError> {
        let original = self.checked_intent(intent)?;
        self.authorize_dispatch_durable(&original.reservation.scope, &original.execution_id)
    }

    /// Close only proven never-authorized admission, with full original binding checks.
    pub fn close_before_dispatch_for_intent_durable(
        &self,
        intent: &ExecutionIntent,
    ) -> Result<crate::ledger::evidence::ClosureOutcome, EvidenceError> {
        let original = self.checked_intent(intent)?;
        self.close_before_dispatch_durable(&original.reservation.scope, &original.execution_id)
    }

    /// Record immutable terminal usage for the exact original admission. Caller-supplied
    /// reservation metadata must match the persisted intent; it confers no caller
    /// authorization and does not enable tracked HTTP admission by itself.
    /// `cache_read_bp` is server-admitted operator config (TD-0032).
    pub fn record_terminal_for_intent_durable(
        &self,
        intent: &ExecutionIntent,
        usage: &sandhi_core::UsageV2,
        cache_read_bp: u32,
    ) -> Result<ObservationOutcome, EvidenceError> {
        let original = self.checked_intent(intent)?;
        self.record_terminal_durable(
            &original.reservation.scope,
            &original.execution_id,
            usage,
            cache_read_bp,
        )
    }

    /// Record terminal usage together with qualified source/attempt evidence, under the
    /// same original-intent binding check.
    pub fn record_terminal_with_attempt_for_intent_durable(
        &self,
        intent: &ExecutionIntent,
        usage: &sandhi_core::UsageV2,
        cache_read_bp: u32,
        attempt: &crate::ledger::evidence::AttemptEvidence,
    ) -> Result<ObservationOutcome, EvidenceError> {
        let original = self.checked_intent(intent)?;
        self.record_terminal_with_attempt_durable(
            &original.reservation.scope,
            &original.execution_id,
            usage,
            cache_read_bp,
            attempt,
        )
    }
}

/// Initialize the evidence tables on a freshly connected ledger. Called from
/// [`PostgresLedger::connect_with`] so every pool owns the complete schema.
pub(super) async fn init(pool: &PgPool) -> Result<(), EvidenceError> {
    for statement in EVIDENCE_DDL {
        sqlx::query(statement).execute(pool).await?;
    }
    let (table, declaration) = LIABILITY_RELEASED_AT;
    attempts::ensure_column(pool, table, "liability_released_at", declaration).await?;
    context::init(pool).await?;
    paired::init(pool).await?;
    attempts::init(pool).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ledger::evidence::{
        AttemptEvidence, ClosureOutcome, DispatchOutcome, ObservationOutcome, SettlementOutcome,
    };
    use crate::ledger_pg::pg_test;
    use sandhi_core::{
        CacheReadObservation, CacheReadSource, CacheReadStatus, LatencySource, Policy, Window,
    };

    // ── Non-DB dialect tests (run wherever the `postgres` feature compiles). ──

    #[test]
    fn evidence_ddl_declares_the_seven_tables_with_pg_typing() {
        // Five tables live in the core evidence DDL; the context/back-leg/attempt
        // tables are created by their own submodule init functions (the SQLite layout).
        let joined = EVIDENCE_DDL.join("\n");
        for table in [
            "budget_execution_intent",
            "budget_request_correlation",
            "budget_dispatch_fence",
            "budget_settlement_outbox",
            "budget_terminal_observation",
        ] {
            assert!(
                joined.contains(&format!("CREATE TABLE IF NOT EXISTS {table}")),
                "{table} missing from the evidence DDL"
            );
        }
        for (table, ddl) in [
            ("budget_execution_context", context::TABLE_SQL),
            ("budget_execution_back_leg", paired::TABLE_SQL),
            ("budget_physical_attempt", attempts::TABLE_SQL),
        ] {
            assert!(
                ddl.contains(&format!("CREATE TABLE IF NOT EXISTS {table}")),
                "{table} missing from its submodule DDL"
            );
        }
        // ADR-0048: the release stamp arrives through the same additive migration step
        // the SQLite backend runs, guarded by an information_schema probe.
        // The ADR-0048 release stamp rides the same additive migration guard as the
        // fence columns; its column name is pinned at the ensure_column call site.
        assert_eq!(LIABILITY_RELEASED_AT.0, "budget_execution_intent");
        // The partial outbox index keeps its exact name and predicate.
        assert!(
            EVIDENCE_DDL.iter().any(|s| {
                s.contains("idx_settlement_pending") && s.contains("WHERE acknowledged_at IS NULL")
            }),
            "{joined}"
        );
        // bigint columns, never boolean; timestamps stay unix-seconds.
        assert!(!joined.contains("boolean"));
        assert!(joined.contains("phase           bigint NOT NULL CHECK(phase IN (0, 1, 2))"));
        // The FK hazard of INSERT OR REPLACE must never appear in this backend's SQL.
        assert!(!joined.contains("INSERT OR REPLACE"));
    }

    #[test]
    fn recovery_sql_pins_the_snapshot_probe_and_lookahead_shape() {
        // One snapshot per page: REPEATABLE READ READ ONLY.
        assert_eq!(
            RECOVERY_TX_SQL,
            "SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY"
        );
        // The 5-way orphan probe fails closed over intent/reservation, observation,
        // fence, back leg and attempt orphans — before any page row is returned.
        for arm in [
            "budget_execution_intent i \
             LEFT JOIN budget_reservation r ON r.id = i.reservation_id WHERE r.id IS NULL",
            "budget_terminal_observation o",
            "budget_dispatch_fence f",
            "budget_execution_back_leg b",
            "budget_physical_attempt a",
        ] {
            assert!(ORPHAN_PROBE_SQL.contains(arm), "probe arm missing: {arm}");
        }
        assert_eq!(
            ORPHAN_PROBE_SQL.matches("EXISTS(SELECT 1").count(),
            5,
            "{ORPHAN_PROBE_SQL}"
        );
        // The convergence pass must restore the intent's OWN release stamp, never the
        // sweep clock (the paired state mirrors must stay exactly equal).
        assert!(
            BACK_LEG_CONVERGENCE_SQL.contains("settled_at = (SELECT i.liability_released_at"),
            "{BACK_LEG_CONVERGENCE_SQL}"
        );
        assert!(BACK_LEG_CONVERGENCE_SQL.contains("WHERE settled = 0"));
        // The reclaim guard: a reservation pinned by intent, back leg or fence survives
        // every expiry reclaim (the SAME fragment the SQLite ledger carries inline).
        for arm in [
            "budget_execution_intent",
            "budget_execution_back_leg",
            "budget_dispatch_fence",
        ] {
            assert!(
                EVIDENCE_RECLAIM_GUARD_SQL
                    .contains(&format!("SELECT 1 FROM {arm} \\\n                     WHERE reservation_id = budget_reservation.id"))
                    || EVIDENCE_RECLAIM_GUARD_SQL.contains(&format!("FROM {arm}")),
                "reclaim guard arm missing: {arm}"
            );
        }
        assert_eq!(
            EVIDENCE_RECLAIM_GUARD_SQL.matches("NOT EXISTS").count(),
            3,
            "{EVIDENCE_RECLAIM_GUARD_SQL}"
        );
    }

    #[test]
    fn advisory_keys_cover_the_reserved_count_and_outbox_lanes() {
        // Dedicated negative keys never collide with a (masked, non-negative) scope key.
        assert_eq!(RESERVED_COUNT_ADVISORY_KEY, -1);
        assert_eq!(OUTBOX_CLAIM_ADVISORY_KEY, -2);
        assert!(RESERVED_COUNT_ADVISORY_KEY.is_negative());
        assert!(OUTBOX_CLAIM_ADVISORY_KEY.is_negative());
        assert_ne!(RESERVED_COUNT_ADVISORY_KEY, OUTBOX_CLAIM_ADVISORY_KEY);
        for scope in ["", "team", "group:h", "vk:k1"] {
            assert!(
                scope_advisory_key(scope) >= 0,
                "{scope} must mask to i64::MAX"
            );
        }
        // Multi-scope transactions take their locks ascending (deadlock-free house rule).
        let paired = advisory_keys_ascending(&["team", "upstream:team"]);
        assert!(paired.windows(2).all(|w| w[0] < w[1]));
    }

    // ── Live-Postgres tests: SANDHI_TEST_PG_DSN, per-test schema isolation. ──

    fn pg_ledger(tag: &str) -> PostgresLedger {
        let ledger = pg_test::ledger_in_schema(&pg_test::schema(tag));
        pg_test::reset_all_tables(&ledger);
        ledger
    }

    fn drop_schema(schema: &str) {
        pg_test::drop_schema_best_effort(schema)
    }

    impl PostgresLedger {
        /// Run arbitrary fixture/corruption SQL (the PG dialect of the SQLite tests'
        /// `ledger.conn.execute_batch`).
        fn exec(&self, sql: &str) {
            self.block(async {
                sqlx::query(sql).execute(&self.pool).await.expect(sql);
            });
        }

        fn scalar(&self, sql: &str) -> i64 {
            self.block(async {
                sqlx::query_scalar(sql)
                    .fetch_one(&self.pool)
                    .await
                    .expect(sql)
            })
        }

        /// Install a PG `RAISE EXCEPTION` fault trigger (the dialect of the SQLite
        /// `RAISE(ABORT)` fixtures). The error aborts the caller's whole transaction.
        fn reject_trigger(&self, name: &str, action: &str) {
            self.exec(
                "CREATE OR REPLACE FUNCTION __reject_fixture() RETURNS trigger \
                 LANGUAGE plpgsql AS $f$ BEGIN RAISE EXCEPTION 'fixture'; END $f$;",
            );
            self.exec(&format!(
                "CREATE TRIGGER {name} {action} \
                 FOR EACH ROW EXECUTE FUNCTION __reject_fixture()"
            ));
        }

        fn drop_trigger(&self, name: &str, table: &str) {
            self.exec(&format!("DROP TRIGGER IF EXISTS {name} ON {table}"));
        }
    }

    fn now() -> OffsetDateTime {
        OffsetDateTime::now_utc()
    }

    fn admit(ledger: &PostgresLedger, scope: &str, ceiling: u64) -> sandhi_core::Reservation {
        match ledger
            .reserve_durable(scope, ceiling, now(), Duration::hours(1))
            .unwrap()
        {
            crate::ledger::ReserveOutcome::Admitted(reservation) => reservation,
            other => panic!("expected admission, got {other:?}"),
        }
    }

    fn tracked(ledger: &PostgresLedger) -> ExecutionIntent {
        let IntentAdmission::Admitted(intent) = ledger
            .reserve_with_intent_durable("team", 100, now(), Duration::seconds(1), 10)
            .unwrap()
        else {
            panic!("denied")
        };
        intent
    }

    fn observed_usage() -> UsageV2 {
        UsageV2 {
            tokens_in: 10,
            tokens_out: 20,
            cache_creation_tokens: 3,
            cache_read_tokens: 4,
            cache_read_observation: Some(CacheReadObservation::origin(CacheReadStatus::Reported)),
            audio_input_tokens: Some(2),
            audio_output_tokens: Some(3),
            reasoning_tokens: Some(5),
            reasoning_included: Some(false),
            accepted_prediction_tokens: Some(6),
            rejected_prediction_tokens: Some(7),
            completeness: UsageCompleteness::Final,
            basis: UsageBasis::ProviderReported,
            attempts: 1,
            outcome: Some("success".into()),
            upstream_request_id: Some("upstream-1".into()),
            duration_ms: Some(123),
            duration_source: Some(LatencySource::Origin),
            time_to_first_token_ms: Some(10),
            time_to_first_token_source: Some(LatencySource::Boundary),
        }
    }

    fn usage() -> UsageV2 {
        UsageV2 {
            tokens_in: 3,
            tokens_out: 2,
            cache_read_tokens: 10,
            completeness: UsageCompleteness::Final,
            ..Default::default()
        }
    }

    fn unwrap_receipt(outcome: SettlementOutcome) -> crate::ledger::evidence::SettlementReceipt {
        match outcome {
            SettlementOutcome::Committed(receipt)
            | SettlementOutcome::AlreadyCommitted(receipt) => receipt,
        }
    }

    fn unwrap_observation(outcome: ObservationOutcome) -> TerminalObservation {
        match outcome {
            ObservationOutcome::Recorded(value) | ObservationOutcome::AlreadyRecorded(value) => {
                value
            }
        }
    }

    fn outbox_count(ledger: &PostgresLedger) -> i64 {
        ledger.scalar("SELECT COUNT(*) FROM budget_settlement_outbox")
    }

    fn paired_context() -> ExecutionContext {
        serde_json::from_value(serde_json::json!({"version":1,"ingress":"openai_chat_completions","delivery":"buffered","plane":"translation",
            "upstream_ref":"openai:default","upstream_budget_scope":"upstream:team","requested_model":"model","actual_model":"model","provider":"models",
            "endpoint_family":"openai_chat_completions","transport":{"upstream_host":"upstream.test","policy":{"buffered":{"duration_ms":30000}}},"transport_binding_sha256":"a".repeat(64),"accounting_policy":{"version":1,"cache_read_weight":1000}})).unwrap()
    }

    fn admit_paired(ledger: &PostgresLedger) -> ExecutionIntent {
        let IntentAdmission::Admitted(intent) = ledger
            .reserve_paired_contextual_durable(
                "team",
                100,
                now(),
                Duration::seconds(60),
                100,
                "request",
                &paired_context(),
            )
            .unwrap()
        else {
            panic!("admission")
        };
        intent
    }

    fn attempt_evidence(ledger: &PostgresLedger, intent: &ExecutionIntent) -> AttemptEvidence {
        AttemptEvidence {
            observed_provider: None,
            version: 1,
            attempt_id: ledger
                .physical_attempt_durable("team", &intent.execution_id)
                .unwrap()
                .unwrap()
                .attempt_id,
            attempt_ordinal: 1,
            dispatch_observed_at_ms: Some(1000),
            terminal: Some(crate::ledger::evidence::TransportTerminal {
                observed_at_ms: 999, // wall clocks can move backwards
                outcome: crate::ledger::evidence::TransportOutcome::Success,
                provider_status: Some(200),
                provider_request_id: Some("fixture-request".into()),
                progress: crate::ledger::evidence::TransportProgress {
                    response_headers_received: Some(true),
                    stream_event_received: Some(true),
                    first_stream_event_at_ms: Some(995),
                    time_to_first_stream_event_ms: Some(5),
                    timeout_phase: None,
                    error_class: crate::ledger::evidence::TransportErrorClass::None,
                    error_subtype: None,
                },
            }),
            dropped_observations: 0,
            invalid: false,
        }
    }

    #[test]
    #[ignore = "requires SANDHI_TEST_PG_DSN"]
    fn weighted_frozen_charge_scales_only_cache_read() {
        let schema = pg_test::schema("pg-ev-weight");
        let ledger = pg_ledger("pg-ev-weight");
        let intent = tracked(&ledger);
        let heavy = UsageV2 {
            tokens_in: 40,
            tokens_out: 20,
            cache_creation_tokens: 0,
            cache_read_tokens: 60,
            completeness: UsageCompleteness::Final,
            ..Default::default()
        };
        let observed = unwrap_observation(
            ledger
                .record_terminal_durable("team", &intent.execution_id, &heavy, 1_000)
                .unwrap(),
        );
        assert_eq!(observed.frozen_charge, Some(66));
        ledger
            .settle_terminal_durable("team", &intent.execution_id)
            .unwrap();
        assert_eq!(ledger.spent_durable("team").unwrap(), 66);
        // Neutral weight on the same shape settles 120.
        let intent2 = tracked(&ledger);
        let observed2 = unwrap_observation(
            ledger
                .record_terminal_durable("team", &intent2.execution_id, &heavy, 10_000)
                .unwrap(),
        );
        assert_eq!(observed2.frozen_charge, Some(120));
        ledger
            .settle_terminal_durable("team", &intent2.execution_id)
            .unwrap();
        assert_eq!(ledger.spent_durable("team").unwrap(), 186);
        drop_schema(&schema);
    }

    #[test]
    #[ignore = "requires SANDHI_TEST_PG_DSN"]
    fn terminal_snapshot_is_immutable_and_survives_settlement() {
        let schema = pg_test::schema("pg-ev-immutable");
        let ledger = pg_ledger("pg-ev-immutable");
        let intent = tracked(&ledger);
        assert_eq!(
            ledger
                .terminal_durable("team", &intent.execution_id)
                .unwrap(),
            None
        );
        let usage = observed_usage();
        let original = unwrap_observation(
            ledger
                .record_terminal_durable("team", &intent.execution_id, &usage, 10_000)
                .unwrap(),
        );
        assert_eq!(original.version, 1);
        assert_eq!(original.usage, usage);
        assert_eq!(original.frozen_charge, Some(42));
        assert_eq!(original.reservation_id, intent.reservation.id);
        assert_eq!(ledger.spent_durable("team").unwrap(), 0);
        assert_eq!(ledger.reserved_durable("team").unwrap(), 100);
        // "Reopen": a fresh pool over the same schema.
        let reopened = pg_test::ledger_in_schema(&schema);
        assert_eq!(
            reopened
                .terminal_durable("team", &intent.execution_id)
                .unwrap(),
            Some(original.clone())
        );
        for changed in [
            UsageV2 {
                outcome: Some("error".into()),
                ..usage.clone()
            },
            UsageV2 {
                basis: UsageBasis::Estimated,
                ..usage.clone()
            },
            UsageV2 {
                completeness: UsageCompleteness::Partial,
                ..usage.clone()
            },
            UsageV2 {
                tokens_in: 11,
                tokens_out: 19,
                ..usage.clone()
            },
        ] {
            assert!(matches!(
                ledger.record_terminal_durable("team", &intent.execution_id, &changed, 10_000),
                Err(EvidenceError::ConflictingObservation)
            ));
        }
        assert!(matches!(
            ledger.record_terminal_durable("other", &intent.execution_id, &usage, 10_000),
            Err(EvidenceError::WrongScope)
        ));
        assert!(matches!(
            ledger.terminal_durable("other", &intent.execution_id),
            Err(EvidenceError::WrongScope)
        ));
        ledger
            .settle_terminal_durable("team", &intent.execution_id)
            .unwrap();
        assert_eq!(
            ledger
                .record_terminal_durable("team", &intent.execution_id, &usage, 10_000)
                .unwrap(),
            ObservationOutcome::AlreadyRecorded(original)
        );
        drop_schema(&schema);
    }

    #[test]
    #[ignore = "requires SANDHI_TEST_PG_DSN"]
    fn terminal_unknown_zero_partial_and_reasoning_keep_their_meanings() {
        let schema = pg_test::schema("pg-ev-meanings");
        let ledger = pg_ledger("pg-ev-meanings");
        for (usage, charge) in [
            (UsageV2::default(), None),
            (
                UsageV2 {
                    completeness: UsageCompleteness::Final,
                    ..UsageV2::default()
                },
                Some(0),
            ),
            (
                UsageV2 {
                    completeness: UsageCompleteness::Partial,
                    basis: UsageBasis::Estimated,
                    ..observed_usage()
                },
                Some(42),
            ),
            (
                UsageV2 {
                    reasoning_included: Some(true),
                    ..observed_usage()
                },
                Some(37),
            ),
        ] {
            let intent = tracked(&ledger);
            let value = unwrap_observation(
                ledger
                    .record_terminal_durable("team", &intent.execution_id, &usage, 10_000)
                    .unwrap(),
            );
            assert_eq!(value.usage, usage);
            assert_eq!(value.frozen_charge, charge);
            // The guarded sweep must NOT reclaim a tracked reservation's liability.
            ledger
                .reclaim_expired_durable(now() + Duration::hours(2))
                .unwrap();
            assert_eq!(
                ledger
                    .terminal_durable("team", &intent.execution_id)
                    .unwrap(),
                Some(value)
            );
        }
        assert_eq!(ledger.reserved_durable("team").unwrap(), 400);
        assert_eq!(ledger.spent_durable("team").unwrap(), 0);
        drop_schema(&schema);
    }

    #[test]
    #[ignore = "requires SANDHI_TEST_PG_DSN"]
    fn terminal_missing_settled_and_lossy_input_are_rejected() {
        let schema = pg_test::schema("pg-ev-invalid");
        let ledger = pg_ledger("pg-ev-invalid");
        let intent = tracked(&ledger);
        for usage in [
            UsageV2 {
                tokens_in: i64::MAX as u64,
                ..observed_usage()
            },
            UsageV2 {
                outcome: Some("x".repeat(257)),
                ..observed_usage()
            },
            UsageV2 {
                upstream_request_id: Some("x".repeat(1025)),
                ..observed_usage()
            },
            UsageV2 {
                cache_read_observation: Some(CacheReadObservation {
                    status: CacheReadStatus::Unsupported,
                    source: CacheReadSource::OriginUsage,
                }),
                ..observed_usage()
            },
        ] {
            assert!(matches!(
                ledger.record_terminal_durable("team", &intent.execution_id, &usage, 10_000),
                Err(EvidenceError::InvalidInput)
            ));
        }
        for id in ["bad".to_string(), "z".repeat(64)] {
            assert!(matches!(
                ledger.record_terminal_durable("team", &id, &observed_usage(), 10_000),
                Err(EvidenceError::InvalidInput)
            ));
            assert!(matches!(
                ledger.terminal_durable("team", &id),
                Err(EvidenceError::InvalidInput)
            ));
        }
        assert!(matches!(
            ledger.record_terminal_durable("team", &"0".repeat(64), &observed_usage(), 10_000),
            Err(EvidenceError::MissingIntent)
        ));
        assert_eq!(
            ledger.terminal_durable("team", &"0".repeat(64)).unwrap(),
            None
        );
        // Historical state from a writer predating tracked-settlement enforcement.
        ledger.exec(&format!(
            "UPDATE budget_reservation SET actual = 7, settled = 1 WHERE id = {}",
            intent.reservation.id
        ));
        assert!(matches!(
            ledger.record_terminal_durable("team", &intent.execution_id, &observed_usage(), 10_000),
            Err(EvidenceError::AlreadySettled)
        ));
        drop_schema(&schema);
    }

    #[test]
    #[ignore = "requires SANDHI_TEST_PG_DSN"]
    fn terminal_corrupt_read_fails_closed_without_writes() {
        let schema = pg_test::schema("pg-ev-corrupt");
        // Ported mutations — every arm must be stageable WITHOUT bypassing checks.
        // Pruned (SQLite-only, needed PRAGMA ignore_check_constraints): version=2 and
        // oversized usage_json (CHECK(version=1)/CHECK(octet_length..) refuse them here,
        // which is itself fail-closed); DELETE FROM budget_execution_intent (FK).
        for mutation in [
            "UPDATE budget_terminal_observation SET usage_json = '{invalid'",
            "UPDATE budget_terminal_observation SET usage_json = \
             jsonb_set(usage_json::jsonb, '{future_field}', '1')::text",
            "UPDATE budget_terminal_observation SET usage_json = jsonb_set( \
             usage_json::jsonb, '{cache_read_observation,status}', '\"future\"')::text",
            "UPDATE budget_terminal_observation SET frozen_charge = NULL",
            "DELETE FROM budget_reservation",
        ] {
            let ledger = pg_ledger("pg-ev-corrupt");
            let intent = tracked(&ledger);
            ledger
                .record_terminal_durable("team", &intent.execution_id, &observed_usage(), 10_000)
                .unwrap();
            ledger.exec(mutation);
            let reserved_after_mutation = ledger.reserved_durable("team").unwrap();
            assert!(
                matches!(
                    ledger.terminal_durable("team", &intent.execution_id),
                    Err(EvidenceError::CorruptObservation) | Err(EvidenceError::MissingReservation)
                ),
                "{mutation}"
            );
            assert_eq!(
                ledger.reserved_durable("team").unwrap(),
                reserved_after_mutation,
                "{mutation}"
            );
            assert_eq!(ledger.spent_durable("team").unwrap(), 0, "{mutation}");
        }
        drop_schema(&schema);
    }

    #[test]
    #[ignore = "requires SANDHI_TEST_PG_DSN"]
    fn atomic_receipt_replay_and_conflict() {
        let schema = pg_test::schema("pg-ev-replay");
        let ledger = pg_ledger("pg-ev-replay");
        let id = admit(&ledger, "team", 100).id;
        let original = unwrap_receipt(ledger.settle_with_evidence_durable("team", id, 42).unwrap());
        assert_eq!(original.receipt_id.len(), 64);
        assert_eq!(original.scope, "team");
        assert_eq!(original.charged_tokens, 42);
        assert_eq!(ledger.spent_durable("team").unwrap(), 42);
        assert_eq!(ledger.reserved_durable("team").unwrap(), 0);
        assert_eq!(
            ledger.settle_with_evidence_durable("team", id, 42).unwrap(),
            SettlementOutcome::AlreadyCommitted(original.clone())
        );
        assert!(matches!(
            ledger.settle_with_evidence_durable("team", id, 43),
            Err(EvidenceError::ConflictingCharge)
        ));
        assert!(matches!(
            ledger.settle_with_evidence_durable("other", id, 42),
            Err(EvidenceError::WrongScope)
        ));
        ledger.settle_durable(id, 900).unwrap(); // legacy callers cannot overwrite the receipt
        assert_eq!(ledger.spent_durable("team").unwrap(), 42);
        assert_eq!(outbox_count(&ledger), 1);
        let settled_at: i64 = ledger.scalar(&format!(
            "SELECT settled_at FROM budget_reservation WHERE id = {id}"
        ));
        assert_eq!(settled_at, original.settled_at);
        drop_schema(&schema);
    }

    #[test]
    #[ignore = "requires SANDHI_TEST_PG_DSN"]
    fn missing_wrong_scope_legacy_and_reclaimed_are_explicit() {
        let schema = pg_test::schema("pg-ev-explicit");
        let ledger = pg_ledger("pg-ev-explicit");
        let id = admit(&ledger, "team", 100).id;
        assert!(matches!(
            ledger.settle_with_evidence_durable("other", id, 2),
            Err(EvidenceError::WrongScope)
        ));
        assert!(matches!(
            ledger.settle_with_evidence_durable("team", id + 1, 2),
            Err(EvidenceError::MissingReservation)
        ));
        ledger.settle_durable(id, 2).unwrap();
        assert!(matches!(
            ledger.settle_with_evidence_durable("team", id, 2),
            Err(EvidenceError::LegacySettlement)
        ));
        let reclaimed = admit(&ledger, "team", 100).id;
        ledger
            .reclaim_expired_durable(now() + Duration::hours(2))
            .unwrap();
        assert!(matches!(
            ledger.settle_with_evidence_durable("team", reclaimed, 2),
            Err(EvidenceError::MissingReservation)
        ));
        assert_eq!(outbox_count(&ledger), 0);
        drop_schema(&schema);
    }

    #[test]
    #[ignore = "requires SANDHI_TEST_PG_DSN"]
    fn failed_receipt_insert_rolls_back_charge() {
        let schema = pg_test::schema("pg-ev-receipt-fault");
        let ledger = pg_ledger("pg-ev-receipt-fault");
        let id = admit(&ledger, "team", 100).id;
        // PG fault trigger replaces the SQLite RAISE(ABORT) fixture; the RAISE(IGNORE)
        // variant is pruned (Postgres has no statement-skip: an error aborts the
        // transaction, which is the fail-closed direction).
        ledger.reject_trigger(
            "reject_receipt",
            "BEFORE INSERT ON budget_settlement_outbox",
        );
        assert!(matches!(
            ledger.settle_with_evidence_durable("team", id, 42),
            Err(EvidenceError::Storage(_))
        ));
        assert_eq!(ledger.spent_durable("team").unwrap(), 0);
        assert_eq!(ledger.reserved_durable("team").unwrap(), 100);
        assert_eq!(outbox_count(&ledger), 0);
        ledger.drop_trigger("reject_receipt", "budget_settlement_outbox");
        assert!(matches!(
            ledger.settle_with_evidence_durable("team", id, 42),
            Ok(SettlementOutcome::Committed(_))
        ));
        drop_schema(&schema);
    }

    #[test]
    #[ignore = "requires SANDHI_TEST_PG_DSN"]
    fn bounded_claims_expiry_and_fenced_acknowledgement() {
        let schema = pg_test::schema("pg-ev-claims");
        let ledger = pg_ledger("pg-ev-claims");
        let id = admit(&ledger, "team", 100).id;
        ledger.settle_with_evidence_durable("team", id, 42).unwrap();
        let now = now();
        for (limit, duration) in [(0, 1), (1001, 1), (1, 0), (1, 3601)] {
            assert!(matches!(
                ledger.claim_settlements_durable(limit, now, duration),
                Err(EvidenceError::InvalidInput)
            ));
        }
        let first = ledger
            .claim_settlements_durable(1, now, 10)
            .unwrap()
            .remove(0);
        assert!(ledger
            .claim_settlements_durable(1, now, 10)
            .unwrap()
            .is_empty());
        let expired = now + Duration::seconds(10);
        assert!(!ledger
            .acknowledge_settlement_durable(&first.receipt.receipt_id, &first.claim_token, expired)
            .unwrap());
        let second = ledger
            .claim_settlements_durable(1, expired, 10)
            .unwrap()
            .remove(0);
        assert_eq!(first.receipt, second.receipt);
        assert_ne!(first.claim_token, second.claim_token);
        assert!(!ledger
            .acknowledge_settlement_durable(&first.receipt.receipt_id, &first.claim_token, expired)
            .unwrap());
        assert!(!ledger
            .acknowledge_settlement_durable("unknown", &second.claim_token, expired)
            .unwrap());
        assert!(ledger
            .acknowledge_settlement_durable(
                &second.receipt.receipt_id,
                &second.claim_token,
                expired
            )
            .unwrap());
        assert!(!ledger
            .acknowledge_settlement_durable(
                &second.receipt.receipt_id,
                &second.claim_token,
                expired
            )
            .unwrap());
        assert!(ledger
            .claim_settlements_durable(1, expired + Duration::hours(1), 10)
            .unwrap()
            .is_empty());
        assert_eq!(outbox_count(&ledger), 1);
        drop_schema(&schema);
    }

    #[test]
    #[ignore = "requires SANDHI_TEST_PG_DSN"]
    fn terminal_settlement_binds_frozen_charge_and_blocks_caller_charge_bypasses() {
        let schema = pg_test::schema("pg-ev-bind");
        let ledger = pg_ledger("pg-ev-bind");
        let intent = tracked(&ledger);
        assert!(matches!(
            ledger.settle_with_evidence_durable("team", intent.reservation.id, 42),
            Err(EvidenceError::TrackedSettlementRequired)
        ));
        assert!(ledger.settle_durable(intent.reservation.id, 42).is_err());
        assert!(matches!(
            ledger.settle_terminal_durable("team", &intent.execution_id),
            Err(EvidenceError::MissingObservation)
        ));
        ledger
            .record_terminal_durable("team", &intent.execution_id, &observed_usage(), 10_000)
            .unwrap();
        // A retained snapshot may use an older version's charging formula; settlement
        // must use its frozen value, never derive a new charge.
        ledger.exec("UPDATE budget_terminal_observation SET frozen_charge = 43");
        assert!(matches!(
            ledger.settle_terminal_durable("other", &intent.execution_id),
            Err(EvidenceError::WrongScope)
        ));
        let receipt = unwrap_receipt(
            ledger
                .settle_terminal_durable("team", &intent.execution_id)
                .unwrap(),
        );
        assert_eq!(receipt.charged_tokens, 43);
        assert_eq!(
            ledger
                .settle_terminal_durable("team", &intent.execution_id)
                .unwrap(),
            SettlementOutcome::AlreadyCommitted(receipt.clone())
        );
        let claim = ledger
            .claim_settlements_durable(1, now(), 60)
            .unwrap()
            .pop()
            .unwrap();
        assert!(ledger
            .acknowledge_settlement_durable(&claim.receipt.receipt_id, &claim.claim_token, now())
            .unwrap());
        assert_eq!(
            ledger
                .settle_terminal_durable("team", &intent.execution_id)
                .unwrap(),
            SettlementOutcome::AlreadyCommitted(receipt)
        );
        assert_eq!(ledger.spent_durable("team").unwrap(), 43);
        assert_eq!(ledger.reserved_durable("team").unwrap(), 0);
        assert_eq!(outbox_count(&ledger), 1);
        assert!(matches!(
            ledger.settle_terminal_durable("team", &"0".repeat(64)),
            Err(EvidenceError::MissingIntent)
        ));
        assert!(matches!(
            ledger.settle_terminal_durable("", &intent.execution_id),
            Err(EvidenceError::InvalidInput)
        ));
        assert!(matches!(
            ledger.settle_terminal_durable("team", "bad-id"),
            Err(EvidenceError::InvalidInput)
        ));
        drop_schema(&schema);
    }

    #[test]
    #[ignore = "requires SANDHI_TEST_PG_DSN"]
    fn terminal_settlement_failure_preserves_observation_and_liability() {
        let schema = pg_test::schema("pg-ev-settle-fault");
        let ledger = pg_ledger("pg-ev-settle-fault");
        let intent = tracked(&ledger);
        let observed = ledger
            .record_terminal_durable("team", &intent.execution_id, &observed_usage(), 10_000)
            .unwrap();
        ledger.reject_trigger(
            "reject_receipt",
            "BEFORE INSERT ON budget_settlement_outbox",
        );
        assert!(matches!(
            ledger.settle_terminal_durable("team", &intent.execution_id),
            Err(EvidenceError::Storage(_))
        ));
        assert_eq!(ledger.reserved_durable("team").unwrap(), 100);
        assert_eq!(ledger.spent_durable("team").unwrap(), 0);
        assert_eq!(outbox_count(&ledger), 0);
        assert_eq!(
            ledger
                .terminal_durable("team", &intent.execution_id)
                .unwrap(),
            Some(unwrap_observation(observed))
        );
        ledger.drop_trigger("reject_receipt", "budget_settlement_outbox");
        assert_eq!(
            unwrap_receipt(
                ledger
                    .settle_terminal_durable("team", &intent.execution_id)
                    .unwrap()
            )
            .charged_tokens,
            42
        );
        // Corrupt stored usage fails closed with both liabilities intact.
        let ledger2 = pg_test::ledger_in_schema(&pg_test::schema("pg-ev-settle-fault"));
        let intent2 = tracked(&ledger2);
        ledger2
            .record_terminal_durable("team", &intent2.execution_id, &observed_usage(), 10_000)
            .unwrap();
        ledger2.exec("UPDATE budget_terminal_observation SET usage_json = '{}'");
        assert!(matches!(
            ledger2.settle_terminal_durable("team", &intent2.execution_id),
            Err(EvidenceError::CorruptObservation)
        ));
        assert_eq!(ledger2.reserved_durable("team").unwrap(), 100);
        drop_schema(&schema);
    }

    #[test]
    #[ignore = "requires SANDHI_TEST_PG_DSN"]
    fn recovery_pages_are_scoped_read_only_and_bounded_across_reopen() {
        let schema = pg_test::schema("pg-ev-pages");
        let ledger = pg_ledger("pg-ev-pages");
        let first = tracked(&ledger);
        // Interleaved untracked/other-scope rows cannot consume or escape the page.
        admit(&ledger, "team", 100);
        let foreign = tracked(&ledger);
        ledger.exec(&format!(
            "UPDATE budget_reservation SET scope = 'other' WHERE id = {}",
            foreign.reservation.id
        ));
        let second = tracked(&ledger);
        let third = tracked(&ledger);
        ledger
            .record_terminal_durable("team", &second.execution_id, &observed_usage(), 10_000)
            .unwrap();
        ledger
            .record_terminal_durable("team", &third.execution_id, &observed_usage(), 10_000)
            .unwrap();
        let receipt = unwrap_receipt(
            ledger
                .settle_terminal_durable("team", &third.execution_id)
                .unwrap(),
        );
        let claim = ledger
            .claim_settlements_durable(1, now(), 10)
            .unwrap()
            .remove(0);
        ledger
            .acknowledge_settlement_durable(&receipt.receipt_id, &claim.claim_token, now())
            .unwrap();
        let reopened = pg_test::ledger_in_schema(&schema);
        let mut entries = Vec::new();
        let mut cursor = None;
        loop {
            let page = reopened
                .recovery_page_durable("team", cursor.as_ref(), 1)
                .unwrap();
            assert_eq!(page.entries.len(), 1);
            entries.extend(page.entries);
            cursor = page.next;
            if cursor.is_none() {
                break;
            }
        }
        assert_eq!(
            entries,
            vec![
                RecoveryEntry {
                    execution_id: first.execution_id,
                    reservation_id: first.reservation.id,
                    state: RecoveryState::MissingObservation
                },
                RecoveryEntry {
                    execution_id: second.execution_id,
                    reservation_id: second.reservation.id,
                    state: RecoveryState::ReadyToSettle
                },
                RecoveryEntry {
                    execution_id: third.execution_id,
                    reservation_id: third.reservation.id,
                    state: RecoveryState::Settled(receipt)
                },
            ]
        );
        let empty = reopened.recovery_page_durable("empty", None, 100).unwrap();
        assert!(empty.entries.is_empty() && empty.next.is_none());
        assert_eq!(
            reopened
                .recovery_page_durable("other", None, 100)
                .unwrap()
                .entries[0]
                .execution_id,
            foreign.execution_id
        );
        // The pagers are read-only: no liability moved, no receipt fabricated.
        // The untracked interleave also holds its ceiling in `team`.
        assert_eq!(reopened.reserved_durable("team").unwrap(), 300);
        assert_eq!(reopened.spent_durable("team").unwrap(), 42);
        assert_eq!(outbox_count(&reopened), 1);
        drop_schema(&schema);
    }

    #[test]
    #[ignore = "requires SANDHI_TEST_PG_DSN"]
    fn recovery_cursor_freezes_admission_range_but_rechecks_current_state() {
        let schema = pg_test::schema("pg-ev-cursor");
        let ledger = pg_ledger("pg-ev-cursor");
        let first = tracked(&ledger);
        let second = tracked(&ledger);
        let page = ledger.recovery_page_durable("team", None, 1).unwrap();
        let cursor = page.next.unwrap();
        let writer = pg_test::ledger_in_schema(&schema);
        let later = tracked(&writer);
        for intent in [&first, &second] {
            writer
                .record_terminal_durable("team", &intent.execution_id, &observed_usage(), 10_000)
                .unwrap();
        }
        let next = ledger
            .recovery_page_durable("team", Some(&cursor), 100)
            .unwrap();
        assert_eq!(
            next.entries,
            vec![RecoveryEntry {
                execution_id: second.execution_id.clone(),
                reservation_id: second.reservation.id,
                state: RecoveryState::ReadyToSettle
            }]
        );
        assert!(next.next.is_none());
        // Inventory is not a claim: another connection settles before this consumer does.
        let receipt = unwrap_receipt(
            writer
                .settle_terminal_durable("team", &second.execution_id)
                .unwrap(),
        );
        assert_eq!(
            ledger
                .settle_terminal_durable("team", &second.execution_id)
                .unwrap(),
            SettlementOutcome::AlreadyCommitted(receipt)
        );
        let sweep = ledger.recovery_page_durable("team", None, 100).unwrap();
        assert_eq!(sweep.entries.len(), 3);
        assert_eq!(sweep.entries[0].state, RecoveryState::ReadyToSettle);
        assert_eq!(sweep.entries[2].execution_id, later.execution_id);
        for limit in [0, 101, usize::MAX] {
            assert!(matches!(
                ledger.recovery_page_durable("team", None, limit),
                Err(EvidenceError::InvalidInput)
            ));
        }
        assert!(matches!(
            ledger.recovery_page_durable("", None, 1),
            Err(EvidenceError::InvalidInput)
        ));
        assert!(matches!(
            ledger.recovery_page_durable(&"x".repeat(4097), None, 1),
            Err(EvidenceError::InvalidInput)
        ));
        assert!(matches!(
            ledger.recovery_page_durable("other", Some(&cursor), 1),
            Err(EvidenceError::WrongScope)
        ));
        for (after_id, through_id) in [(0u64, 2u64), (3, 2), (1, u64::MAX)] {
            let bad = RecoveryCursor {
                scope: "team".into(),
                after_id,
                through_id,
            };
            assert!(matches!(
                ledger.recovery_page_durable("team", Some(&bad), 1),
                Err(EvidenceError::InvalidInput)
            ));
        }
        let exhausted = RecoveryCursor {
            scope: "team".into(),
            after_id: second.reservation.id,
            through_id: second.reservation.id,
        };
        let empty = ledger
            .recovery_page_durable("team", Some(&exhausted), 1)
            .unwrap();
        assert!(empty.entries.is_empty() && empty.next.is_none());
        drop_schema(&schema);
    }

    #[test]
    #[ignore = "requires SANDHI_TEST_PG_DSN"]
    fn durable_intent_survives_both_expiry_paths_and_reopen() {
        let schema = pg_test::schema("pg-ev-intent");
        let ledger = pg_ledger("pg-ev-intent");
        let now = now();
        ledger
            .set_limit_durable("team", Some(100), Window::Total, Policy::Block)
            .unwrap();
        let IntentAdmission::Admitted(intent) = ledger
            .reserve_with_intent_durable("team", 100, now, Duration::seconds(1), 10)
            .unwrap()
        else {
            panic!("denied")
        };
        assert_eq!(intent.execution_id.len(), 64);
        let later = now + Duration::seconds(2);
        assert_eq!(ledger.reclaim_expired_durable(later).unwrap(), 0);
        assert!(matches!(
            ledger
                .reserve_durable("team", 1, later, Duration::seconds(1))
                .unwrap(),
            crate::ledger::ReserveOutcome::Denied(_)
        ));
        assert_eq!(ledger.reserved_durable("team").unwrap(), 100);
        assert_eq!(
            ledger
                .intent_durable(&intent.execution_id)
                .unwrap()
                .unwrap()
                .reservation
                .id,
            intent.reservation.id
        );
        ledger
            .record_terminal_durable("team", &intent.execution_id, &observed_usage(), 10_000)
            .unwrap();
        let receipt = ledger
            .settle_terminal_durable("team", &intent.execution_id)
            .unwrap();
        assert!(matches!(receipt, SettlementOutcome::Committed(_)));
        assert!(matches!(
            ledger
                .settle_terminal_durable("team", &intent.execution_id)
                .unwrap(),
            SettlementOutcome::AlreadyCommitted(_)
        ));
        assert_eq!(ledger.spent_durable("team").unwrap(), 42);
        drop_schema(&schema);
    }

    #[test]
    #[ignore = "requires SANDHI_TEST_PG_DSN"]
    fn intent_capacity_is_atomic_and_retains_settled_identities() {
        let schema = pg_test::schema("pg-ev-capacity");
        let ledger = pg_ledger("pg-ev-capacity");
        // Two independent pools contending for the retained-limit COUNT under the
        // dedicated advisory key: exactly one admission may win.
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let workers: Vec<_> = (0..2)
            .map(|_| {
                let ledger = pg_test::ledger_in_schema(&schema);
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    ledger.reserve_with_intent_durable("team", 1, now(), Duration::seconds(1), 1)
                })
            })
            .collect();
        let mut admitted = None;
        let mut exhausted = 0;
        for worker in workers {
            match worker.join().unwrap() {
                Ok(IntentAdmission::Admitted(intent)) => {
                    assert!(admitted.is_none());
                    admitted = Some(intent);
                }
                Err(EvidenceError::IntentCapacity) => exhausted += 1,
                other => panic!("unexpected {other:?}"),
            }
        }
        assert_eq!(exhausted, 1);
        let intent = admitted.unwrap();
        ledger
            .record_terminal_durable(
                "team",
                &intent.execution_id,
                &UsageV2 {
                    tokens_in: 1,
                    completeness: UsageCompleteness::Final,
                    ..UsageV2::default()
                },
                10_000,
            )
            .unwrap();
        ledger
            .settle_terminal_durable("team", &intent.execution_id)
            .unwrap();
        // Settled identities still retain their capacity slot.
        assert!(matches!(
            ledger.reserve_with_intent_durable("team", 1, now(), Duration::seconds(1), 1),
            Err(EvidenceError::IntentCapacity)
        ));
        assert!(ledger
            .intent_durable(&intent.execution_id)
            .unwrap()
            .is_some());
        drop_schema(&schema);
    }

    #[test]
    #[ignore = "requires SANDHI_TEST_PG_DSN"]
    fn correlated_admission_is_atomic_scoped_and_survives_reopen() {
        let schema = pg_test::schema("pg-ev-correlated");
        let ledger = pg_ledger("pg-ev-correlated");
        let IntentAdmission::Admitted(intent) = ledger
            .reserve_correlated_durable(
                "vk:removed",
                100,
                now(),
                Duration::minutes(5),
                10,
                "req-once",
            )
            .unwrap()
        else {
            panic!("denied")
        };
        assert_eq!(
            ledger
                .request_id_durable("vk:removed", &intent.execution_id)
                .unwrap()
                .as_deref(),
            Some("req-once")
        );
        assert!(matches!(
            ledger.request_id_durable("other", &intent.execution_id),
            Err(EvidenceError::WrongScope)
        ));
        let old = tracked(&ledger);
        assert_eq!(
            ledger
                .request_id_durable("team", &old.execution_id)
                .unwrap(),
            None
        );
        let before: i64 = ledger.scalar("SELECT COUNT(*) FROM budget_execution_intent");
        assert!(matches!(
            ledger.reserve_correlated_durable("vk:removed", 1, now(), Duration::minutes(5), 10, ""),
            Err(EvidenceError::InvalidInput)
        ));
        // A failure inserting correlation rolls the reservation and intent back too.
        ledger.reject_trigger(
            "refuse_correlation",
            "BEFORE INSERT ON budget_request_correlation",
        );
        assert!(ledger
            .reserve_correlated_durable(
                "vk:removed",
                1,
                now(),
                Duration::minutes(5),
                10,
                "req-fail"
            )
            .is_err());
        assert_eq!(
            ledger.scalar("SELECT COUNT(*) FROM budget_execution_intent"),
            before
        );
        ledger.drop_trigger("refuse_correlation", "budget_request_correlation");
        drop_schema(&schema);
    }

    #[test]
    #[ignore = "requires SANDHI_TEST_PG_DSN"]
    fn prepared_closure_is_durable_without_provider_usage_or_receipt() {
        let schema = pg_test::schema("pg-ev-closure");
        let ledger = pg_ledger("pg-ev-closure");
        // Fence-insert failure rolls the whole admission back (PG fault trigger).
        ledger.reject_trigger("fail_fence", "BEFORE INSERT ON budget_dispatch_fence");
        assert!(matches!(
            ledger.reserve_prepared_durable("team", 100, now(), Duration::seconds(30), 10),
            Err(EvidenceError::Storage(_))
        ));
        for table in [
            "budget_reservation",
            "budget_execution_intent",
            "budget_dispatch_fence",
        ] {
            assert_eq!(
                ledger.scalar(&format!("SELECT COUNT(*) FROM {table}")),
                0,
                "{table}"
            );
        }
        ledger.drop_trigger("fail_fence", "budget_dispatch_fence");
        let IntentAdmission::Admitted(intent) = ledger
            .reserve_prepared_durable("team", 100, now(), Duration::seconds(30), 10)
            .unwrap()
        else {
            panic!("denied")
        };
        assert!(matches!(
            ledger
                .recovery_page_durable("team", None, 10)
                .unwrap()
                .entries[0]
                .state,
            RecoveryState::Prepared
        ));
        assert!(matches!(
            ledger.record_terminal_durable("team", &intent.execution_id, &observed_usage(), 10_000),
            Err(EvidenceError::DispatchNotAuthorized)
        ));
        assert!(matches!(
            ledger.close_before_dispatch_durable("other", &intent.execution_id),
            Err(EvidenceError::WrongScope)
        ));
        // Closure zero-charge release failure leaves the intent Prepared (re-stampable).
        ledger.reject_trigger("fail_closure", "BEFORE UPDATE ON budget_reservation");
        assert!(matches!(
            ledger.close_before_dispatch_durable("team", &intent.execution_id),
            Err(EvidenceError::Storage(_))
        ));
        assert!(matches!(
            ledger
                .recovery_page_durable("team", None, 10)
                .unwrap()
                .entries[0]
                .state,
            RecoveryState::Prepared
        ));
        ledger.drop_trigger("fail_closure", "budget_reservation");
        let ClosureOutcome::Closed(closure) = ledger
            .close_before_dispatch_durable("team", &intent.execution_id)
            .unwrap()
        else {
            panic!("not closed")
        };
        let reopened = pg_test::ledger_in_schema(&schema);
        assert_eq!(
            reopened
                .close_before_dispatch_durable("team", &intent.execution_id)
                .unwrap(),
            ClosureOutcome::AlreadyClosed(closure.clone())
        );
        assert!(
            matches!(reopened.authorize_dispatch_durable("team", &intent.execution_id).unwrap(), DispatchOutcome::ClosedBeforeDispatch(value) if value == closure)
        );
        assert_eq!(
            reopened
                .recovery_page_durable("team", None, 10)
                .unwrap()
                .entries[0]
                .state,
            RecoveryState::ClosedBeforeDispatch(closure)
        );
        assert!(matches!(
            reopened.record_terminal_durable(
                "team",
                &intent.execution_id,
                &observed_usage(),
                10_000
            ),
            Err(EvidenceError::DispatchNotAuthorized)
        ));
        assert!(matches!(
            reopened.settle_terminal_durable("team", &intent.execution_id),
            Err(EvidenceError::DispatchNotAuthorized)
        ));
        assert!(reopened
            .terminal_durable("team", &intent.execution_id)
            .unwrap()
            .is_none());
        assert!(reopened
            .claim_settlements_durable(10, now(), 30)
            .unwrap()
            .is_empty());
        assert_eq!(
            reopened.scalar(
                "SELECT (COALESCE(SUM(actual), 0) \
                 + COALESCE(SUM(CASE WHEN settled = 0 THEN ceiling ELSE 0 END), 0))::bigint \
                 FROM budget_reservation"
            ),
            0
        );
        drop_schema(&schema);
    }

    #[test]
    #[ignore = "requires SANDHI_TEST_PG_DSN"]
    fn dispatch_authorization_survives_lost_permit_and_retains_liability() {
        let schema = pg_test::schema("pg-ev-permit");
        let ledger = pg_ledger("pg-ev-permit");
        let IntentAdmission::Admitted(intent) = ledger
            .reserve_prepared_durable("team", 100, now(), Duration::seconds(30), 10)
            .unwrap()
        else {
            panic!("denied")
        };
        let DispatchOutcome::Authorized(permit) = ledger
            .authorize_dispatch_durable("team", &intent.execution_id)
            .unwrap()
        else {
            panic!("no permit")
        };
        assert_eq!(permit.execution_id(), intent.execution_id);
        drop(permit);
        let reopened = pg_test::ledger_in_schema(&schema);
        assert!(matches!(
            reopened
                .authorize_dispatch_durable("team", &intent.execution_id)
                .unwrap(),
            DispatchOutcome::MayHaveDispatched
        ));
        assert_eq!(
            reopened
                .close_before_dispatch_durable("team", &intent.execution_id)
                .unwrap(),
            ClosureOutcome::MayHaveDispatched
        );
        assert_eq!(
            reopened
                .recovery_page_durable("team", None, 10)
                .unwrap()
                .entries[0]
                .state,
            RecoveryState::MayHaveDispatched
        );
        // Unknown dispatch keeps BOTH liabilities: the held ceiling survives expiry.
        assert_eq!(
            reopened.scalar("SELECT ceiling FROM budget_reservation WHERE settled = 0"),
            100
        );
        reopened
            .record_terminal_durable("team", &intent.execution_id, &observed_usage(), 10_000)
            .unwrap();
        assert!(matches!(
            reopened
                .settle_terminal_durable("team", &intent.execution_id)
                .unwrap(),
            SettlementOutcome::Committed(_)
        ));
        assert_eq!(
            reopened
                .close_before_dispatch_durable("team", &intent.execution_id)
                .unwrap(),
            ClosureOutcome::MayHaveDispatched
        );
        drop_schema(&schema);
    }

    #[test]
    #[ignore = "requires SANDHI_TEST_PG_DSN"]
    fn dispatch_fence_rejects_expired_and_damaged_evidence() {
        let schema = pg_test::schema("pg-ev-damage");
        // Pruned (SQLite-only): the `phase = 9` arm needed PRAGMA
        // ignore_check_constraints; the CHECK(phase IN (0,1,2)) refuses the UPDATE here,
        // which is itself fail-closed.
        for damage in [
            "expiry",
            "orphan",
            "timestamp",
            "prepared_receipt",
            "closed_observation",
        ] {
            let ledger = pg_ledger("pg-ev-damage");
            let IntentAdmission::Admitted(intent) = ledger
                .reserve_prepared_durable("team", 100, now(), Duration::seconds(30), 10)
                .unwrap()
            else {
                panic!("denied")
            };
            match damage {
                "expiry" => {
                    ledger.exec("UPDATE budget_reservation SET expires_at = 0");
                    assert!(matches!(
                        ledger.authorize_dispatch_durable("team", &intent.execution_id),
                        Err(EvidenceError::ExpiredBeforeDispatch)
                    ));
                    assert!(matches!(
                        ledger
                            .close_before_dispatch_durable("team", &intent.execution_id)
                            .unwrap(),
                        ClosureOutcome::Closed(_)
                    ));
                    continue;
                }
                "orphan" => {
                    ledger.exec("DELETE FROM budget_reservation");
                }
                "timestamp" => {
                    ledger.exec(
                        "UPDATE budget_dispatch_fence SET phase = 1, transitioned_at = 9223372036854775807",
                    );
                }
                "prepared_receipt" => {
                    ledger.exec(
                        "INSERT INTO budget_settlement_outbox(receipt_id, reservation_id, scope, charged_tokens, settled_at) \
                         SELECT 'fake', id, scope, 0, 0 FROM budget_reservation",
                    );
                }
                "closed_observation" => {
                    ledger
                        .close_before_dispatch_durable("team", &intent.execution_id)
                        .unwrap();
                    ledger.exec(&format!(
                        "INSERT INTO budget_terminal_observation(execution_id, version, usage_json, frozen_charge, observed_at) \
                         VALUES ('{}', 1, '{}', 37, 0)",
                        intent.execution_id,
                        encode_usage(&observed_usage()).unwrap()
                    ));
                }
                _ => unreachable!(),
            }
            assert!(
                ledger
                    .authorize_dispatch_durable("team", &intent.execution_id)
                    .is_err(),
                "{damage}"
            );
            assert!(
                ledger
                    .close_before_dispatch_durable("team", &intent.execution_id)
                    .is_err(),
                "{damage}"
            );
            assert!(
                ledger.recovery_page_durable("team", None, 10).is_err(),
                "{damage}"
            );
        }
        drop_schema(&schema);
    }

    #[test]
    #[ignore = "requires SANDHI_TEST_PG_DSN"]
    fn context_admission_is_atomic_and_settlement_cannot_change_its_weight() {
        let schema = pg_test::schema("pg-ev-context");
        let ledger = pg_ledger("pg-ev-context");
        // Context-insert failure rolls reservations, intent, fence and correlation back.
        ledger.reject_trigger("fail_context", "BEFORE INSERT ON budget_execution_context");
        assert!(ledger
            .reserve_contextual_durable(
                "team",
                100,
                now(),
                Duration::seconds(60),
                100,
                "request",
                &paired_context()
            )
            .is_err());
        for table in [
            "budget_reservation",
            "budget_execution_intent",
            "budget_dispatch_fence",
            "budget_request_correlation",
            "budget_execution_context",
        ] {
            assert_eq!(
                ledger.scalar(&format!("SELECT COUNT(*) FROM {table}")),
                0,
                "{table}"
            );
        }
        ledger.drop_trigger("fail_context", "budget_execution_context");
        let desired = paired_context();
        let IntentAdmission::Admitted(intent) = ledger
            .reserve_contextual_durable(
                "team",
                100,
                now(),
                Duration::seconds(60),
                100,
                "request",
                &desired,
            )
            .unwrap()
        else {
            panic!("admission")
        };
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
        // The recorded weight is frozen; a caller-supplied different weight conflicts.
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
        assert!(
            matches!(ledger.settle_terminal_durable("team", &intent.execution_id).unwrap(), SettlementOutcome::Committed(receipt) if receipt.charged_tokens == 6)
        );
        assert!(
            matches!(ledger.settle_terminal_durable("team", &intent.execution_id).unwrap(), SettlementOutcome::AlreadyCommitted(receipt) if receipt.charged_tokens == 6)
        );
        drop_schema(&schema);
    }

    #[test]
    #[ignore = "requires SANDHI_TEST_PG_DSN"]
    fn missing_or_corrupt_required_context_never_degrades_to_legacy() {
        let schema = pg_test::schema("pg-ev-ctx-corrupt");
        // Pruned (SQLite-only): the `version = 2` and `oversized` mutations needed PRAGMA
        // ignore_check_constraints; CHECK(version = 1) / CHECK(octet_length <= 16384)
        // refuse them here, which is itself fail-closed.
        for corrupt in ["missing", "unknown-field", "marker"] {
            let ledger = pg_ledger("pg-ev-ctx-corrupt");
            let IntentAdmission::Admitted(intent) = ledger
                .reserve_contextual_durable(
                    "team",
                    100,
                    now(),
                    Duration::seconds(60),
                    100,
                    "request",
                    &paired_context(),
                )
                .unwrap()
            else {
                panic!("admission")
            };
            match corrupt {
                "missing" => ledger.exec("DELETE FROM budget_execution_context"),
                "unknown-field" => ledger.exec(
                    "UPDATE budget_execution_context \
                     SET context_json = jsonb_set(context_json::jsonb, '{secret}', \
                     '\"forbidden\"')::text",
                ),
                "marker" => ledger.exec("UPDATE budget_dispatch_fence SET context_version = 0"),
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
        drop_schema(&schema);
    }

    #[test]
    #[ignore = "requires SANDHI_TEST_PG_DSN"]
    fn paired_admission_denial_and_link_failure_roll_back_every_row() {
        let schema = pg_test::schema("pg-ev-paired-deny");
        let ledger = pg_ledger("pg-ev-paired-deny");
        ledger
            .set_limit_durable("upstream:team", Some(50), Window::Total, Policy::Block)
            .unwrap();
        // Back-leg denial rolls the primary reservation back too.
        assert!(matches!(
            ledger
                .reserve_paired_contextual_durable(
                    "team",
                    100,
                    now(),
                    Duration::seconds(60),
                    100,
                    "request",
                    &paired_context()
                )
                .unwrap(),
            IntentAdmission::Denied(_)
        ));
        assert_eq!(ledger.reserved_durable("team").unwrap(), 0);
        assert_eq!(ledger.reserved_durable("upstream:team").unwrap(), 0);
        ledger
            .set_limit_durable("upstream:team", Some(1000), Window::Total, Policy::Block)
            .unwrap();
        // Back-leg link failure rolls back reservations, intent, fence and correlation.
        ledger.reject_trigger("fail_link", "BEFORE INSERT ON budget_execution_back_leg");
        assert!(ledger
            .reserve_paired_contextual_durable(
                "team",
                100,
                now(),
                Duration::seconds(60),
                100,
                "request",
                &paired_context()
            )
            .is_err());
        for table in [
            "budget_reservation",
            "budget_execution_intent",
            "budget_dispatch_fence",
            "budget_request_correlation",
            "budget_execution_context",
            "budget_execution_back_leg",
        ] {
            assert_eq!(
                ledger.scalar(&format!("SELECT COUNT(*) FROM {table}")),
                0,
                "{table}"
            );
        }
        ledger.drop_trigger("fail_link", "budget_execution_back_leg");
        drop_schema(&schema);
    }

    #[test]
    #[ignore = "requires SANDHI_TEST_PG_DSN"]
    fn attempt_allocation_rolls_back_with_authorization_and_replay_never_gets_a_permit() {
        let schema = pg_test::schema("pg-ev-attempt");
        let ledger = pg_ledger("pg-ev-attempt");
        let intent = admit_paired(&ledger);
        assert!(ledger
            .physical_attempt_durable("team", &intent.execution_id)
            .unwrap()
            .is_none());
        ledger.reject_trigger("fail_attempt", "BEFORE INSERT ON budget_physical_attempt");
        assert!(ledger
            .authorize_dispatch_durable("team", &intent.execution_id)
            .is_err());
        assert_eq!(
            ledger
                .recovery_page_durable("team", None, 10)
                .unwrap()
                .entries[0]
                .state,
            RecoveryState::Prepared
        );
        ledger.drop_trigger("fail_attempt", "budget_physical_attempt");
        let DispatchOutcome::Authorized(permit) = ledger
            .authorize_dispatch_durable("team", &intent.execution_id)
            .unwrap()
        else {
            panic!("first permit")
        };
        let attempt = ledger
            .physical_attempt_durable("team", &intent.execution_id)
            .unwrap()
            .unwrap();
        assert_eq!(permit.attempt_id(), Some(attempt.attempt_id.as_str()));
        assert!(attempt.evidence.is_none());
        assert!(matches!(
            ledger
                .authorize_dispatch_durable("team", &intent.execution_id)
                .unwrap(),
            DispatchOutcome::MayHaveDispatched
        ));
        assert_eq!(
            ledger
                .physical_attempt_durable("team", &intent.execution_id)
                .unwrap(),
            Some(attempt)
        );
        assert!(matches!(
            ledger.physical_attempt_durable("other", &intent.execution_id),
            Err(EvidenceError::WrongScope)
        ));
        assert!(matches!(
            ledger.record_terminal_durable("team", &intent.execution_id, &usage(), 1000),
            Err(EvidenceError::ConflictingObservation)
        ));
        drop_schema(&schema);
    }

    #[test]
    #[ignore = "requires SANDHI_TEST_PG_DSN"]
    fn attempt_evidence_and_usage_commit_together_and_never_accept_refinements() {
        let schema = pg_test::schema("pg-ev-together");
        let ledger = pg_ledger("pg-ev-together");
        let intent = admit_paired(&ledger);
        ledger
            .authorize_dispatch_durable("team", &intent.execution_id)
            .unwrap();
        let evidence = attempt_evidence(&ledger, &intent);
        // Observation-insert failure rolls the attempt evidence back with it.
        ledger.reject_trigger(
            "fail_terminal",
            "BEFORE INSERT ON budget_terminal_observation",
        );
        assert!(ledger
            .record_terminal_with_attempt_durable(
                "team",
                &intent.execution_id,
                &usage(),
                1000,
                &evidence
            )
            .is_err());
        assert!(ledger
            .physical_attempt_durable("team", &intent.execution_id)
            .unwrap()
            .unwrap()
            .evidence
            .is_none());
        assert!(ledger
            .terminal_durable("team", &intent.execution_id)
            .unwrap()
            .is_none());
        ledger.drop_trigger("fail_terminal", "budget_terminal_observation");
        let first = ledger
            .record_terminal_with_attempt_durable(
                "team",
                &intent.execution_id,
                &usage(),
                1000,
                &evidence,
            )
            .unwrap();
        let ObservationOutcome::Recorded(first) = first else {
            panic!("new observation")
        };
        assert_eq!(
            ledger
                .record_terminal_with_attempt_durable(
                    "team",
                    &intent.execution_id,
                    &usage(),
                    1000,
                    &evidence
                )
                .unwrap(),
            ObservationOutcome::AlreadyRecorded(first)
        );
        let mut changed = evidence.clone();
        changed.terminal.as_mut().unwrap().provider_request_id = Some("different".into());
        assert!(matches!(
            ledger.record_terminal_with_attempt_durable(
                "team",
                &intent.execution_id,
                &usage(),
                1000,
                &changed
            ),
            Err(EvidenceError::ConflictingObservation)
        ));
        ledger
            .settle_terminal_durable("team", &intent.execution_id)
            .unwrap();
        assert_eq!(
            ledger
                .physical_attempt_durable("team", &intent.execution_id)
                .unwrap()
                .unwrap()
                .evidence,
            Some(evidence)
        );
        drop_schema(&schema);
    }

    #[test]
    #[ignore = "requires SANDHI_TEST_PG_DSN"]
    fn paired_settlement_is_atomic_and_recovers_one_original_receipt() {
        let schema = pg_test::schema("pg-ev-paired-settle");
        let ledger = pg_ledger("pg-ev-paired-settle");
        let intent = admit_paired(&ledger);
        let back = ledger
            .execution_back_leg_durable("team", &intent.execution_id)
            .unwrap()
            .unwrap();
        ledger
            .authorize_dispatch_durable("team", &intent.execution_id)
            .unwrap();
        let evidence = attempt_evidence(&ledger, &intent);
        ledger
            .record_terminal_with_attempt_durable(
                "team",
                &intent.execution_id,
                &usage(),
                1000,
                &evidence,
            )
            .unwrap();
        // Back-leg settle failure aborts the WHOLE settlement (PG fault trigger with a
        // WHEN guard on the back-leg row).
        ledger.exec(
            "CREATE OR REPLACE FUNCTION __reject_fixture() RETURNS trigger \
             LANGUAGE plpgsql AS $f$ BEGIN RAISE EXCEPTION 'fixture'; END $f$;",
        );
        ledger.exec(&format!(
            "CREATE TRIGGER fail_back_settle BEFORE UPDATE OF settled ON budget_reservation \
             FOR EACH ROW WHEN (OLD.id = {}) EXECUTE FUNCTION __reject_fixture()",
            back.id
        ));
        assert!(ledger
            .settle_terminal_durable("team", &intent.execution_id)
            .is_err());
        assert_eq!(ledger.spent_durable("team").unwrap(), 0);
        assert_eq!(ledger.spent_durable("upstream:team").unwrap(), 0);
        assert_eq!(outbox_count(&ledger), 0);
        assert_eq!(ledger.reserved_durable("team").unwrap(), 100);
        assert_eq!(ledger.reserved_durable("upstream:team").unwrap(), 100);
        ledger.drop_trigger("fail_back_settle", "budget_reservation");
        let outcome = ledger
            .settle_terminal_durable("team", &intent.execution_id)
            .unwrap();
        assert!(
            matches!(&outcome, SettlementOutcome::Committed(receipt) if receipt.charged_tokens == 6)
        );
        assert_eq!(ledger.spent_durable("team").unwrap(), 6);
        assert_eq!(ledger.spent_durable("upstream:team").unwrap(), 6);
        let SettlementOutcome::AlreadyCommitted(replayed) = ledger
            .settle_terminal_durable("team", &intent.execution_id)
            .unwrap()
        else {
            panic!("receipt replay")
        };
        let SettlementOutcome::Committed(original) = outcome else {
            panic!("first receipt")
        };
        assert_eq!(replayed, original);
        assert_eq!(
            ledger
                .execution_back_leg_durable("team", &intent.execution_id)
                .unwrap()
                .unwrap()
                .id,
            back.id
        );
        drop_schema(&schema);
    }

    #[test]
    #[ignore = "requires SANDHI_TEST_PG_DSN"]
    fn aged_unresolved_liability_releases_hold_and_classifies_terminal() {
        // ADR-0048: a tracked intent admitted past the retention window with no
        // observation, fence or receipt is the incident class — released uncharged,
        // evidence retained, no receipt fabricated.
        let schema = pg_test::schema("pg-ev-aged");
        let ledger = pg_ledger("pg-ev-aged");
        let IntentAdmission::Admitted(intent) = ledger
            .reserve_with_intent_durable(
                "team",
                100,
                now() - Duration::hours(48),
                Duration::seconds(1),
                10,
            )
            .unwrap()
        else {
            panic!("denied")
        };
        let now = now();
        let cutoff = now - Duration::hours(24);
        assert_eq!(
            ledger
                .release_aged_unresolved_liabilities_durable(now, cutoff)
                .unwrap(),
            1
        );
        // Capacity freed; spend untouched; no settlement receipt written.
        assert_eq!(ledger.reserved_durable("team").unwrap(), 0);
        assert_eq!(ledger.spent_durable("team").unwrap(), 0);
        assert_eq!(outbox_count(&ledger), 0);
        // Classification is terminal-by-policy (#516 ordering: BEFORE the receipt
        // machinery), with the stamp recorded on the row.
        let page = ledger.recovery_page_durable("team", None, 10).unwrap();
        assert_eq!(page.entries.len(), 1);
        assert_eq!(page.entries[0].state, RecoveryState::ReleasedAfterRetention);
        let stamp = ledger.scalar(&format!(
            "SELECT COUNT(*) FROM budget_execution_intent \
             WHERE execution_id = '{}' AND liability_released_at IS NOT NULL",
            intent.execution_id
        ));
        assert_eq!(stamp, 1);
        // Idempotent: a second sweep releases nothing further.
        assert_eq!(
            ledger
                .release_aged_unresolved_liabilities_durable(now, cutoff)
                .unwrap(),
            0
        );
        drop_schema(&schema);
    }

    #[test]
    #[ignore = "requires SANDHI_TEST_PG_DSN"]
    fn aged_release_converges_around_a_poisoned_settled_row() {
        // Production shape (DS3 migration): a settled reservation whose stored
        // receipt disagrees with it. The receipt-consistency machinery rejects that
        // row, but a settled reservation is not a liability — the sweep must skip
        // it (never abort) and still release the genuinely aged unresolved intent.
        let schema = pg_test::schema("pg-ev-aged-poison");
        let ledger = pg_ledger("pg-ev-aged-poison");
        let IntentAdmission::Admitted(poison) = ledger
            .reserve_with_intent_durable(
                "team",
                100,
                now() - Duration::hours(48),
                Duration::seconds(1),
                10,
            )
            .unwrap()
        else {
            panic!("denied")
        };
        ledger.exec(&format!(
            "UPDATE budget_reservation SET settled = 1, actual = 40, settled_at = 1795662826 \
             WHERE id = {}",
            poison.reservation.id
        ));
        ledger.exec(&format!(
            "INSERT INTO budget_settlement_outbox(receipt_id, reservation_id, scope, charged_tokens, settled_at) \
             VALUES ('{}', {}, 'team', 40, 1794873147)",
            poison.execution_id, poison.reservation.id
        ));
        let IntentAdmission::Admitted(intent) = ledger
            .reserve_with_intent_durable(
                "other",
                100,
                now() - Duration::hours(48),
                Duration::seconds(1),
                10,
            )
            .unwrap()
        else {
            panic!("denied")
        };
        let now = now();
        let cutoff = now - Duration::hours(24);
        assert_eq!(
            ledger
                .release_aged_unresolved_liabilities_durable(now, cutoff)
                .unwrap(),
            1,
            "the poisoned row must not block the genuine release"
        );
        assert_eq!(ledger.reserved_durable("other").unwrap(), 0);
        // The poisoned row is untouched: still settled with its own numbers, no stamp.
        let untouched: (i64, i64, i64) = ledger.block(async {
            sqlx::query_as(
                "SELECT r.actual, r.settled, \
                        (SELECT COUNT(*) FROM budget_execution_intent \
                         WHERE execution_id = $1 AND liability_released_at IS NOT NULL) \
                 FROM budget_reservation r WHERE r.id = $2",
            )
            .bind(&poison.execution_id)
            .bind(poison.reservation.id as i64)
            .fetch_one(&mut *ledger.pool.acquire().await.unwrap())
            .await
            .unwrap()
        });
        assert_eq!((untouched.0, untouched.1), (40, 1));
        assert_eq!(untouched.2, 0);
        assert_eq!(
            ledger
                .recovery_page_durable("other", None, 10)
                .unwrap()
                .entries[0]
                .state,
            RecoveryState::ReleasedAfterRetention
        );
        drop_schema(&schema);
    }

    #[test]
    #[ignore = "requires SANDHI_TEST_PG_DSN"]
    fn aged_release_closes_the_paired_back_leg_with_the_same_zero_charge() {
        let schema = pg_test::schema("pg-ev-aged-paired");
        let ledger = pg_ledger("pg-ev-aged-paired");
        let intent = admit_paired(&ledger);
        ledger.exec(&format!(
            "UPDATE budget_execution_intent SET created_at = {} WHERE execution_id = '{}'",
            (now() - Duration::hours(48)).unix_timestamp(),
            intent.execution_id
        ));
        let back_id: i64 = ledger.scalar(&format!(
            "SELECT reservation_id FROM budget_execution_back_leg WHERE execution_id = '{}'",
            intent.execution_id
        ));
        let now = now();
        let cutoff = now - Duration::hours(24);
        assert_eq!(
            ledger
                .release_aged_unresolved_liabilities_durable(now, cutoff)
                .unwrap(),
            1
        );
        let legs: Vec<(i64, i64, Option<i64>)> = ledger.block(async {
            sqlx::query_as(
                "SELECT actual, settled, settled_at FROM budget_reservation \
                 WHERE id IN (SELECT reservation_id FROM budget_execution_back_leg \
                              WHERE execution_id = $1) OR id = $2 ORDER BY id",
            )
            .bind(&intent.execution_id)
            .bind(intent.reservation.id as i64)
            .fetch_all(&ledger.pool)
            .await
            .unwrap()
        });
        assert_eq!(legs.len(), 2);
        // Same zero-charge policy release on both legs, same stamp.
        for (actual, settled, at) in &legs {
            assert_eq!((*actual, *settled), (0, 1));
            assert_eq!(*at, Some(now.unix_timestamp()));
        }
        // The released pair satisfies the state-mirror invariant: the back leg is
        // readable through the scope-checked paired view.
        let back = ledger
            .execution_back_leg_durable("team", &intent.execution_id)
            .unwrap()
            .expect("released paired intent keeps a readable back leg");
        assert_eq!(back.id as i64, back_id);
        assert_eq!(back.scope, "upstream:team");
        drop_schema(&schema);
    }

    #[test]
    #[ignore = "requires SANDHI_TEST_PG_DSN"]
    fn aged_release_skips_a_closed_before_dispatch_pair_untouched() {
        let schema = pg_test::schema("pg-ev-aged-closed");
        let ledger = pg_ledger("pg-ev-aged-closed");
        let intent = admit_paired(&ledger);
        ledger.exec(&format!(
            "UPDATE budget_execution_intent SET created_at = {} WHERE execution_id = '{}'",
            (now() - Duration::hours(48)).unix_timestamp(),
            intent.execution_id
        ));
        assert!(matches!(
            ledger.close_before_dispatch_durable("team", &intent.execution_id),
            Ok(ClosureOutcome::Closed(_))
        ));
        let read_legs = |ledger: &PostgresLedger| -> Vec<(i64, i64, Option<i64>)> {
            ledger.block(async {
                sqlx::query_as(
                    "SELECT actual, settled, settled_at FROM budget_reservation \
                     WHERE id = $1 OR id = (SELECT reservation_id \
                                             FROM budget_execution_back_leg \
                                             WHERE execution_id = $2) ORDER BY id",
                )
                .bind(intent.reservation.id as i64)
                .bind(&intent.execution_id)
                .fetch_all(&ledger.pool)
                .await
                .unwrap()
            })
        };
        let legs_before = read_legs(&ledger);
        let now = now();
        let cutoff = now - Duration::hours(24);
        assert_eq!(
            ledger
                .release_aged_unresolved_liabilities_durable(now, cutoff)
                .unwrap(),
            0
        );
        // Both legs exactly as close_before_dispatch_durable left them: the sweep
        // skipped the terminal pair and re-stamped nothing.
        assert_eq!(legs_before[0], (0, 1, legs_before[0].2));
        assert_eq!(legs_before[1], (0, 1, legs_before[0].2));
        assert_eq!(read_legs(&ledger), legs_before);
        drop_schema(&schema);
    }

    #[test]
    #[ignore = "requires SANDHI_TEST_PG_DSN"]
    fn release_sweep_converges_back_legs_stranded_by_older_releases() {
        let schema = pg_test::schema("pg-ev-converge");
        let ledger = pg_ledger("pg-ev-converge");
        let intent = admit_paired(&ledger);
        ledger.exec(&format!(
            "UPDATE budget_execution_intent SET created_at = {} WHERE execution_id = '{}'",
            (now() - Duration::hours(48)).unix_timestamp(),
            intent.execution_id
        ));
        let back_id: i64 = ledger.scalar(&format!(
            "SELECT reservation_id FROM budget_execution_back_leg WHERE execution_id = '{}'",
            intent.execution_id
        ));
        let now = now();
        let cutoff = now - Duration::hours(24);
        assert_eq!(
            ledger
                .release_aged_unresolved_liabilities_durable(now, cutoff)
                .unwrap(),
            1
        );
        assert_eq!(ledger.reserved_durable("team").unwrap(), 0);
        // Recreate the pre-fix residual: intent stamped, back leg unsettled again.
        ledger.exec(&format!(
            "UPDATE budget_reservation SET settled = 0, settled_at = NULL WHERE id = {back_id}"
        ));
        // The stranded hold is withheld from the upstream scope until the convergence
        // pass closes it.
        assert_eq!(ledger.reserved_durable("upstream:team").unwrap(), 100);
        // A LATER sweep clock: production's 60 s cadence guarantees the healing sweep's
        // now differs from the original release stamp, so the convergence must restore
        // the intent's OWN stamp — not the sweep's — for the state mirrors to satisfy
        // the paired read.
        let later = now + Duration::seconds(120);
        assert_eq!(
            ledger
                .release_aged_unresolved_liabilities_durable(later, cutoff)
                .unwrap(),
            0,
            "no new release, only convergence"
        );
        let (back_actual, back_settled, back_at): (i64, i64, Option<i64>) = ledger.block(async {
            sqlx::query_as(
                "SELECT actual, settled, settled_at FROM budget_reservation WHERE id = $1",
            )
            .bind(back_id)
            .fetch_one(&ledger.pool)
            .await
            .unwrap()
        });
        assert_eq!((back_actual, back_settled), (0, 1));
        // The healed mirror carries the ORIGINAL release stamp, not the sweep's.
        assert_eq!(back_at, Some(now.unix_timestamp()));
        // The pair is readable again through the scope-checked paired view, and the
        // upstream capacity hold is freed from the admission view.
        let back = ledger
            .execution_back_leg_durable("team", &intent.execution_id)
            .unwrap()
            .expect("converged paired intent keeps a readable back leg");
        assert_eq!(back.id as i64, back_id);
        assert_eq!(ledger.reserved_durable("upstream:team").unwrap(), 0);
        drop_schema(&schema);
    }

    #[test]
    #[ignore = "requires SANDHI_TEST_PG_DSN"]
    fn fresh_or_terminal_intents_are_never_released() {
        let schema = pg_test::schema("pg-ev-skip");
        let ledger = pg_ledger("pg-ev-skip");
        // Aged MissingObservation (release target)…
        let IntentAdmission::Admitted(aged) = ledger
            .reserve_with_intent_durable(
                "team",
                100,
                now() - Duration::hours(48),
                Duration::seconds(1),
                10,
            )
            .unwrap()
        else {
            panic!("denied")
        };
        // …a fresh unresolved intent (still inside the window)…
        let fresh = tracked(&ledger);
        // …an aged ReadyToSettle intent (final usage exists — recovery settles it).
        let IntentAdmission::Admitted(settleable) = ledger
            .reserve_with_intent_durable(
                "team",
                100,
                now() - Duration::hours(48),
                Duration::seconds(1),
                10,
            )
            .unwrap()
        else {
            panic!("denied")
        };
        ledger
            .record_terminal_durable("team", &settleable.execution_id, &observed_usage(), 1_000)
            .unwrap();
        let now = now();
        let cutoff = now - Duration::hours(24);
        assert_eq!(
            ledger
                .release_aged_unresolved_liabilities_durable(now, cutoff)
                .unwrap(),
            1
        );
        let page = ledger.recovery_page_durable("team", None, 10).unwrap();
        assert!(page
            .entries
            .iter()
            .any(|entry| entry.execution_id == aged.execution_id
                && entry.state == RecoveryState::ReleasedAfterRetention));
        assert!(page
            .entries
            .iter()
            .any(|entry| entry.execution_id == fresh.execution_id
                && entry.state == RecoveryState::MissingObservation));
        assert!(page
            .entries
            .iter()
            .any(|entry| entry.execution_id == settleable.execution_id
                && entry.state == RecoveryState::ReadyToSettle));
        // The settleable intent still settles normally after the sweep skipped it.
        ledger
            .settle_terminal_durable("team", &settleable.execution_id)
            .unwrap();
        assert!(ledger.spent_durable("team").unwrap() > 0);
        drop_schema(&schema);
    }

    #[test]
    #[ignore = "requires SANDHI_TEST_PG_DSN"]
    fn settled_intents_survive_the_release_sweep_untouched() {
        let schema = pg_test::schema("pg-ev-settled");
        let ledger = pg_ledger("pg-ev-settled");
        let intent = tracked(&ledger);
        ledger
            .record_terminal_durable("team", &intent.execution_id, &observed_usage(), 1_000)
            .unwrap();
        ledger
            .settle_terminal_durable("team", &intent.execution_id)
            .unwrap();
        let settled_before = outbox_count(&ledger);
        let now = now();
        assert_eq!(
            ledger
                .release_aged_unresolved_liabilities_durable(now, now)
                .unwrap(),
            0
        );
        let page = ledger.recovery_page_durable("team", None, 10).unwrap();
        assert!(matches!(page.entries[0].state, RecoveryState::Settled(_)));
        assert_eq!(outbox_count(&ledger), settled_before);
        drop_schema(&schema);
    }

    #[test]
    #[ignore = "requires SANDHI_TEST_PG_DSN"]
    fn readiness_inputs_converge_with_a_released_row() {
        // The recovery worker's readiness predicate reads two inputs from this pager:
        // "cursor complete" (a full sweep returned every intent) and "unresolved == 0".
        // A policy-released row must count toward NEITHER fail condition: it classifies
        // terminal-before-receipt-machinery (#516), the page terminates (no cursor), and
        // no entry is unresolved — alongside a normal Settled classification.
        let schema = pg_test::schema("pg-ev-ready");
        let ledger = pg_ledger("pg-ev-ready");
        let IntentAdmission::Admitted(released) = ledger
            .reserve_with_intent_durable(
                "team",
                100,
                now() - Duration::hours(48),
                Duration::seconds(1),
                10,
            )
            .unwrap()
        else {
            panic!("denied")
        };
        let settled = tracked(&ledger);
        ledger
            .record_terminal_durable("team", &settled.execution_id, &observed_usage(), 1_000)
            .unwrap();
        ledger
            .settle_terminal_durable("team", &settled.execution_id)
            .unwrap();
        let now = now();
        let cutoff = now - Duration::hours(24);
        assert_eq!(
            ledger
                .release_aged_unresolved_liabilities_durable(now, cutoff)
                .unwrap(),
            1
        );
        // Cursor complete: one full page, no continuation.
        let page = ledger.recovery_page_durable("team", None, 100).unwrap();
        assert!(page.next.is_none(), "a sub-limit page must end the sweep");
        // Unresolved == 0: the released row is terminal, the other settled.
        assert_eq!(page.entries.len(), 2);
        assert!(page
            .entries
            .iter()
            .any(|entry| entry.execution_id == released.execution_id
                && entry.state == RecoveryState::ReleasedAfterRetention));
        assert!(!page
            .entries
            .iter()
            .any(|entry| matches!(entry.state, RecoveryState::UnresolvedObservation(_))));
        drop_schema(&schema);
    }
}
