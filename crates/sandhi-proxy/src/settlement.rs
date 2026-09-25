//! Owned settlement attempts; authoritative HTTP integration remains a separate gate.
//!
//! Legacy attempts freeze caller charge. Tracked attempts own full immutable usage,
//! persist its observation, then settle through the canonical stored-charge API.
//! Failures retain ownership and distinguish an unstored observation from one already
//! recorded. Neither mode retries inference, acknowledges logical events, or schedules
//! recovery. Do not activate HTTP ownership until W05c–e's integration gates pass.
use crate::ProxyLedger;
use sandhi_core::{Reservation, UsageV2};
use sandhi_store::ledger::evidence::{EvidenceError, ExecutionIntent, SettlementOutcome};
use std::sync::Mutex;

#[derive(Debug)]
struct TrackedObservation {
    intent: ExecutionIntent,
    usage: UsageV2,
    recorded: bool,
}

#[derive(Debug)]
#[must_use]
pub struct PendingSettlement {
    request_id: String,
    reservation: Option<Reservation>,
    charge: Option<u64>,
    tracked: Option<Box<TrackedObservation>>,
}

#[derive(Debug)]
pub enum Failure {
    LedgerBusy,
    LedgerPoisoned,
    NoReservation,
    NonDurableLedger,
    UnknownUsage,
    Evidence(EvidenceError),
}

#[derive(Debug)]
#[must_use]
pub enum Attempt {
    Committed {
        request_id: String,
        outcome: SettlementOutcome,
    },
    Unresolved {
        pending: PendingSettlement,
        failure: Failure,
    },
}

impl PendingSettlement {
    pub fn new(request_id: String, reservation: Option<Reservation>, usage: &UsageV2) -> Self {
        Self {
            request_id,
            reservation,
            charge: match usage.completeness {
                sandhi_core::UsageCompleteness::Final | sandhi_core::UsageCompleteness::Partial => {
                    Some(sandhi_core::billable(usage))
                }
                sandhi_core::UsageCompleteness::Unavailable => None,
            },
            tracked: None,
        }
    }
    /// Own the exact terminal usage for an existing durable execution intent.
    /// Storage validates the original binding before recording anything. Usage is
    /// never amended or converted into caller-supplied charge on this path.
    pub fn tracked(request_id: String, intent: ExecutionIntent, usage: UsageV2) -> Self {
        Self {
            request_id,
            reservation: Some(intent.reservation.clone()),
            charge: None,
            tracked: Some(Box::new(TrackedObservation {
                intent,
                usage,
                recorded: false,
            })),
        }
    }
    pub fn terminal_usage(&self) -> Option<&UsageV2> {
        self.tracked.as_ref().map(|tracked| &tracked.usage)
    }
    /// None for legacy attempts. True means a record/replay succeeded on the
    /// original ledger, not that settlement committed or usage was independently verified.
    pub fn observation_recorded(&self) -> Option<bool> {
        self.tracked.as_ref().map(|tracked| tracked.recorded)
    }
    pub fn request_id(&self) -> &str {
        &self.request_id
    }
    pub fn reservation(&self) -> Option<&Reservation> {
        self.reservation.as_ref()
    }
    /// Legacy caller charge only. Tracked charge belongs exclusively to storage;
    /// use the committed receipt to learn the charge after settlement succeeds.
    pub fn charge(&self) -> Option<u64> {
        self.charge
    }
    /// Try the existing receipt path without waiting for the proxy mutex. SQLite
    /// and the inner shard mutex may still wait: this is not an end-to-end deadline.
    /// An unresolved result owns the unchanged usage and must be retained.
    /// Always use the original ledger and unchanged topology. Scope matching is
    /// not caller authorization; request IDs are caller metadata, not execution IDs.
    pub fn try_commit(mut self, ledger: &Mutex<ProxyLedger>) -> Attempt {
        if self.tracked.is_none() {
            if self.charge.is_none() {
                return Attempt::Unresolved {
                    pending: self,
                    failure: Failure::UnknownUsage,
                };
            }
            if self.reservation.is_none() {
                return Attempt::Unresolved {
                    pending: self,
                    failure: Failure::NoReservation,
                };
            }
        }
        let guard = match ledger.try_lock() {
            Ok(guard) => guard,
            Err(std::sync::TryLockError::WouldBlock) => {
                return Attempt::Unresolved {
                    pending: self,
                    failure: Failure::LedgerBusy,
                };
            }
            Err(std::sync::TryLockError::Poisoned(_)) => {
                return Attempt::Unresolved {
                    pending: self,
                    failure: Failure::LedgerPoisoned,
                };
            }
        };
        let ProxyLedger::Durable(durable) = &*guard else {
            return Attempt::Unresolved {
                pending: self,
                failure: Failure::NonDurableLedger,
            };
        };
        let result = if let Some(tracked) = self.tracked.as_mut() {
            // Exact record replay is intentional, even after a previous recorded
            // attempt: the current ledger must confirm identical retained evidence.
            match durable.record_terminal_for_intent_durable(&tracked.intent, &tracked.usage) {
                Ok(_) => {
                    tracked.recorded = true;
                    durable.settle_terminal_durable(
                        &tracked.intent.reservation.scope,
                        &tracked.intent.execution_id,
                    )
                }
                Err(error) => Err(error),
            }
        } else {
            let reservation = self
                .reservation
                .as_ref()
                .expect("legacy reservation checked");
            durable.settle_with_evidence_durable(
                &reservation.scope,
                reservation.id,
                self.charge.expect("legacy charge checked"),
            )
        };
        match result {
            Ok(outcome) => Attempt::Committed {
                request_id: self.request_id,
                outcome,
            },
            Err(error) => Attempt::Unresolved {
                pending: self,
                failure: Failure::Evidence(error),
            },
        }
    }
}
