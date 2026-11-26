//! The object-safe paired/`tracked` execution surface (Phase 3 PR-D): every method the
//! proxy's durable settlement machinery calls through its ledger-lock guard, defined once
//! so [`crate::ShardedLedger`] (SQLite, default) and [`crate::ledger_pg::PostgresLedger`]
//! (`SANDHI_STORE_DSN`, `postgres` feature) can sit behind one `ProxyLedger` arm without
//! the settlement module branching per backend.
//!
//! Method-for-method the same surface both backends already expose inherently — this
//! trait adds no behavior, it only names it. The `*_for_intent_durable` bridge methods
//! perform the shared original-intent binding check (scope + reservation id/ceiling +
//! expiry at persisted precision) before any transition; neither backend treats a
//! caller-supplied intent as authorization.

use time::OffsetDateTime;

use sandhi_core::UsageV2;

use crate::ledger::evidence::context::ExecutionContext;
use crate::ledger::evidence::{
    AdmissionProgress, AttemptEvidence, ClosureOutcome, DispatchOutcome, EvidenceError,
    IntentAdmission, ObservationOutcome, RecoveryCursor, RecoveryPage, ScopeCursor, ScopePage,
    SettlementOutcome,
};
#[cfg(feature = "postgres")]
use crate::PostgresLedger;
use crate::{LedgerStoreError, ShardedLedger};

/// The durable paired-execution ledger seam (see the module docs).
pub trait DurableExecutionLedger: Send + Sync {
    /// Commit opt-in Prepared admission before dispatch.
    fn reserve_prepared_durable(
        &self,
        scope: &str,
        ceiling: u64,
        now: OffsetDateTime,
        ttl: time::Duration,
        retained_limit: usize,
    ) -> Result<IntentAdmission, EvidenceError>;
    /// Commit the original paired admission with its immutable gateway correlation,
    /// reporting internal stages through `progress`.
    #[allow(clippy::too_many_arguments)] // mirrors the inherent store method shape-for-shape
    fn reserve_paired_contextual_durable_observed(
        &self,
        scope: &str,
        ceiling: u64,
        now: OffsetDateTime,
        ttl: time::Duration,
        retained_limit: usize,
        request_id: &str,
        context: &ExecutionContext,
        progress: Option<&AdmissionProgress>,
    ) -> Result<IntentAdmission, EvidenceError>;
    /// Validate the original admission before its one dispatch permit is issued.
    fn authorize_dispatch_for_intent_durable(
        &self,
        intent: &crate::ledger::evidence::ExecutionIntent,
    ) -> Result<DispatchOutcome, EvidenceError>;
    /// Close only proven never-authorized admission, with full binding checks.
    fn close_before_dispatch_for_intent_durable(
        &self,
        intent: &crate::ledger::evidence::ExecutionIntent,
    ) -> Result<ClosureOutcome, EvidenceError>;
    /// Record immutable terminal usage for the exact original admission.
    fn record_terminal_for_intent_durable(
        &self,
        intent: &crate::ledger::evidence::ExecutionIntent,
        usage: &UsageV2,
        cache_read_bp: u32,
    ) -> Result<ObservationOutcome, EvidenceError>;
    /// Record terminal usage together with qualified source/attempt evidence.
    fn record_terminal_with_attempt_for_intent_durable(
        &self,
        intent: &crate::ledger::evidence::ExecutionIntent,
        usage: &UsageV2,
        cache_read_bp: u32,
        attempt: &AttemptEvidence,
    ) -> Result<ObservationOutcome, EvidenceError>;
    /// Canonical stored-charge settlement on the persisted intent.
    fn settle_terminal_durable(
        &self,
        scope: &str,
        execution_id: &str,
    ) -> Result<SettlementOutcome, EvidenceError>;
    /// Legacy paired settle (lease-scoped, no tracked intent).
    fn settle_with_evidence_durable(
        &self,
        scope: &str,
        reservation_id: u64,
        charged_tokens: u64,
    ) -> Result<SettlementOutcome, EvidenceError>;
    /// Lease settle routed by scope (single-database backends ignore the routing).
    fn settle_durable(
        &self,
        scope: &str,
        reservation_id: u64,
        actual: u64,
    ) -> Result<(), LedgerStoreError>;
    /// The tracked execution's stored paired back leg, if any.
    fn execution_back_leg_durable(
        &self,
        scope: &str,
        execution_id: &str,
    ) -> Result<Option<sandhi_core::Reservation>, EvidenceError>;
    /// Recovery inventory paging (startup convergence).
    fn recovery_scopes_durable(
        &self,
        cursor: Option<&ScopeCursor>,
        limit: usize,
    ) -> Result<ScopePage, EvidenceError>;
    /// Recovery entry paging for one scope.
    fn recovery_page_durable(
        &self,
        scope: &str,
        cursor: Option<&RecoveryCursor>,
        limit: usize,
    ) -> Result<RecoveryPage, EvidenceError>;
}

impl DurableExecutionLedger for ShardedLedger {
    fn reserve_prepared_durable(
        &self,
        scope: &str,
        ceiling: u64,
        now: OffsetDateTime,
        ttl: time::Duration,
        retained_limit: usize,
    ) -> Result<IntentAdmission, EvidenceError> {
        ShardedLedger::reserve_prepared_durable(self, scope, ceiling, now, ttl, retained_limit)
    }
    fn reserve_paired_contextual_durable_observed(
        &self,
        scope: &str,
        ceiling: u64,
        now: OffsetDateTime,
        ttl: time::Duration,
        retained_limit: usize,
        request_id: &str,
        context: &ExecutionContext,
        progress: Option<&AdmissionProgress>,
    ) -> Result<IntentAdmission, EvidenceError> {
        ShardedLedger::reserve_paired_contextual_durable_observed(
            self,
            scope,
            ceiling,
            now,
            ttl,
            retained_limit,
            request_id,
            context,
            progress,
        )
    }
    fn authorize_dispatch_for_intent_durable(
        &self,
        intent: &crate::ledger::evidence::ExecutionIntent,
    ) -> Result<DispatchOutcome, EvidenceError> {
        ShardedLedger::authorize_dispatch_for_intent_durable(self, intent)
    }
    fn close_before_dispatch_for_intent_durable(
        &self,
        intent: &crate::ledger::evidence::ExecutionIntent,
    ) -> Result<ClosureOutcome, EvidenceError> {
        ShardedLedger::close_before_dispatch_for_intent_durable(self, intent)
    }
    fn record_terminal_for_intent_durable(
        &self,
        intent: &crate::ledger::evidence::ExecutionIntent,
        usage: &UsageV2,
        cache_read_bp: u32,
    ) -> Result<ObservationOutcome, EvidenceError> {
        ShardedLedger::record_terminal_for_intent_durable(self, intent, usage, cache_read_bp)
    }
    fn record_terminal_with_attempt_for_intent_durable(
        &self,
        intent: &crate::ledger::evidence::ExecutionIntent,
        usage: &UsageV2,
        cache_read_bp: u32,
        attempt: &AttemptEvidence,
    ) -> Result<ObservationOutcome, EvidenceError> {
        ShardedLedger::record_terminal_with_attempt_for_intent_durable(
            self,
            intent,
            usage,
            cache_read_bp,
            attempt,
        )
    }
    fn settle_terminal_durable(
        &self,
        scope: &str,
        execution_id: &str,
    ) -> Result<SettlementOutcome, EvidenceError> {
        ShardedLedger::settle_terminal_durable(self, scope, execution_id)
    }
    fn settle_with_evidence_durable(
        &self,
        scope: &str,
        reservation_id: u64,
        charged_tokens: u64,
    ) -> Result<SettlementOutcome, EvidenceError> {
        ShardedLedger::settle_with_evidence_durable(self, scope, reservation_id, charged_tokens)
    }
    fn settle_durable(
        &self,
        scope: &str,
        reservation_id: u64,
        actual: u64,
    ) -> Result<(), LedgerStoreError> {
        ShardedLedger::settle_durable(self, scope, reservation_id, actual)
    }
    fn execution_back_leg_durable(
        &self,
        scope: &str,
        execution_id: &str,
    ) -> Result<Option<sandhi_core::Reservation>, EvidenceError> {
        ShardedLedger::execution_back_leg_durable(self, scope, execution_id)
    }
    fn recovery_scopes_durable(
        &self,
        cursor: Option<&ScopeCursor>,
        limit: usize,
    ) -> Result<ScopePage, EvidenceError> {
        ShardedLedger::recovery_scopes_durable(self, cursor, limit)
    }
    fn recovery_page_durable(
        &self,
        scope: &str,
        cursor: Option<&RecoveryCursor>,
        limit: usize,
    ) -> Result<RecoveryPage, EvidenceError> {
        ShardedLedger::recovery_page_durable(self, scope, cursor, limit)
    }
}

#[cfg(feature = "postgres")]
impl DurableExecutionLedger for PostgresLedger {
    fn reserve_prepared_durable(
        &self,
        scope: &str,
        ceiling: u64,
        now: OffsetDateTime,
        ttl: time::Duration,
        retained_limit: usize,
    ) -> Result<IntentAdmission, EvidenceError> {
        PostgresLedger::reserve_prepared_durable(self, scope, ceiling, now, ttl, retained_limit)
    }
    fn reserve_paired_contextual_durable_observed(
        &self,
        scope: &str,
        ceiling: u64,
        now: OffsetDateTime,
        ttl: time::Duration,
        retained_limit: usize,
        request_id: &str,
        context: &ExecutionContext,
        progress: Option<&AdmissionProgress>,
    ) -> Result<IntentAdmission, EvidenceError> {
        PostgresLedger::reserve_paired_contextual_durable_observed(
            self,
            scope,
            ceiling,
            now,
            ttl,
            retained_limit,
            request_id,
            context,
            progress,
        )
    }
    fn authorize_dispatch_for_intent_durable(
        &self,
        intent: &crate::ledger::evidence::ExecutionIntent,
    ) -> Result<DispatchOutcome, EvidenceError> {
        PostgresLedger::authorize_dispatch_for_intent_durable(self, intent)
    }
    fn close_before_dispatch_for_intent_durable(
        &self,
        intent: &crate::ledger::evidence::ExecutionIntent,
    ) -> Result<ClosureOutcome, EvidenceError> {
        PostgresLedger::close_before_dispatch_for_intent_durable(self, intent)
    }
    fn record_terminal_for_intent_durable(
        &self,
        intent: &crate::ledger::evidence::ExecutionIntent,
        usage: &UsageV2,
        cache_read_bp: u32,
    ) -> Result<ObservationOutcome, EvidenceError> {
        PostgresLedger::record_terminal_for_intent_durable(self, intent, usage, cache_read_bp)
    }
    fn record_terminal_with_attempt_for_intent_durable(
        &self,
        intent: &crate::ledger::evidence::ExecutionIntent,
        usage: &UsageV2,
        cache_read_bp: u32,
        attempt: &AttemptEvidence,
    ) -> Result<ObservationOutcome, EvidenceError> {
        PostgresLedger::record_terminal_with_attempt_for_intent_durable(
            self,
            intent,
            usage,
            cache_read_bp,
            attempt,
        )
    }
    fn settle_terminal_durable(
        &self,
        scope: &str,
        execution_id: &str,
    ) -> Result<SettlementOutcome, EvidenceError> {
        PostgresLedger::settle_terminal_durable(self, scope, execution_id)
    }
    fn settle_with_evidence_durable(
        &self,
        scope: &str,
        reservation_id: u64,
        charged_tokens: u64,
    ) -> Result<SettlementOutcome, EvidenceError> {
        PostgresLedger::settle_with_evidence_durable(self, scope, reservation_id, charged_tokens)
    }
    fn settle_durable(
        &self,
        _scope: &str,
        reservation_id: u64,
        actual: u64,
    ) -> Result<(), LedgerStoreError> {
        // One database: no shard routing — the reservation id is unambiguous.
        PostgresLedger::settle_durable(self, reservation_id, actual)
    }
    fn execution_back_leg_durable(
        &self,
        scope: &str,
        execution_id: &str,
    ) -> Result<Option<sandhi_core::Reservation>, EvidenceError> {
        PostgresLedger::execution_back_leg_durable(self, scope, execution_id)
    }
    fn recovery_scopes_durable(
        &self,
        cursor: Option<&ScopeCursor>,
        limit: usize,
    ) -> Result<ScopePage, EvidenceError> {
        PostgresLedger::recovery_scopes_durable(self, cursor, limit)
    }
    fn recovery_page_durable(
        &self,
        scope: &str,
        cursor: Option<&RecoveryCursor>,
        limit: usize,
    ) -> Result<RecoveryPage, EvidenceError> {
        PostgresLedger::recovery_page_durable(self, scope, cursor, limit)
    }
}
