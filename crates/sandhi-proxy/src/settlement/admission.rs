//! Prepared admission inputs and inert recovery evidence. No HTTP activation.
use super::{with_durable_ledger, Failure, PendingDispatch};
use crate::{lifecycle::Lifecycle, ProxyLedger};
use sandhi_core::Denied;
use sandhi_store::ledger::evidence::{EvidenceError, IntentAdmission};
use std::sync::Mutex;

/// Bounded input for one new admission, not an idempotency key or dispatch permit.
/// Never resubmit after uncertain completion: storage generates the execution ID.
#[derive(Debug)]
pub struct PreparedAdmission {
    pub(super) evidence: AdmissionEvidence,
}

/// Metadata for reconciliation against the supervisor's ORIGINAL ledger. It does
/// not identify a committed intent or authorize matching records by request ID.
#[derive(Debug, Clone)]
pub struct AdmissionEvidence {
    pub request_id: String,
    pub scope: String,
    pub ceiling: u64,
    pub ttl: time::Duration,
    pub retained_limit: usize,
}

impl PreparedAdmission {
    pub fn new(
        request_id: String,
        scope: String,
        ceiling: u64,
        ttl: time::Duration,
        retained_limit: usize,
    ) -> Result<Self, EvidenceError> {
        // Bound retained RAM before the registry snapshots input. Storage remains
        // the authority for numeric policy, topology and durable capacity checks.
        if request_id.is_empty()
            || request_id.len() > 4096
            || scope.is_empty()
            || scope.len() > 4096
        {
            return Err(EvidenceError::InvalidInput);
        }
        Ok(Self {
            evidence: AdmissionEvidence {
                request_id,
                scope,
                ceiling,
                ttl,
                retained_limit,
            },
        })
    }

    pub fn evidence(&self) -> &AdmissionEvidence {
        &self.evidence
    }

    pub(super) fn run(
        self,
        ledger: &Mutex<ProxyLedger>,
        lifecycle: &Lifecycle,
    ) -> AdmissionOutcome {
        let evidence = self.evidence;
        let result = with_durable_ledger(ledger, |store| {
            // This check follows acquisition of the outer ledger lock. An inner
            // shard/SQLite wait may still cross cutoff: retain that late Prepared
            // result, never authorize or dispatch here.
            if !lifecycle.is_running() {
                return Ok(None);
            }
            store
                .reserve_prepared_durable(
                    &evidence.scope,
                    evidence.ceiling,
                    time::OffsetDateTime::now_utc(),
                    evidence.ttl,
                    evidence.retained_limit,
                )
                .map(Some)
        });
        match result {
            Ok(Some(IntentAdmission::Admitted(intent))) => {
                AdmissionOutcome::Prepared(PendingDispatch::new(evidence.request_id, intent))
            }
            Ok(Some(IntentAdmission::Denied(denial))) => {
                AdmissionOutcome::Denied { evidence, denial }
            }
            Ok(None) => AdmissionOutcome::Cutoff(evidence),
            Err(failure) => AdmissionOutcome::Failed { evidence, failure },
        }
    }
}

#[derive(Debug)]
#[must_use]
pub enum AdmissionOutcome {
    /// Committed Prepared intent only; authorization is a separate consuming step.
    Prepared(PendingDispatch),
    Denied {
        evidence: AdmissionEvidence,
        denial: Denied,
    },
    /// No store call was attempted because cutoff was observed.
    Cutoff(AdmissionEvidence),
    /// Preserve evidence; storage errors do not prove no commit happened.
    Failed {
        evidence: AdmissionEvidence,
        failure: Failure,
    },
    /// The worker could have committed before losing publication. No automatic retry.
    Interrupted(AdmissionEvidence),
}
