//! Owned settlement attempts; authoritative proxy integration remains a separate gate.
//!
//! This opt-in library primitive freezes observed charge once and preserves ownership
//! on failure. It does not persist pending intent, renew leases, recover reclaimed
//! liability, acknowledge logical events, export receipts, or retry model inference.
//! The existing HTTP accounting path is unchanged. Do not activate this in a
//! production hot path until W05c–e's integration and retention gates are satisfied.
use crate::ProxyLedger;
use sandhi_core::{Reservation, UsageV2};
use sandhi_store::ledger::evidence::{EvidenceError, SettlementOutcome};
use std::sync::Mutex;

#[derive(Debug)]
#[must_use]
pub struct PendingSettlement {
    request_id: String,
    reservation: Option<Reservation>,
    charge: Option<u64>,
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
        }
    }
    pub fn request_id(&self) -> &str {
        &self.request_id
    }
    pub fn reservation(&self) -> Option<&Reservation> {
        self.reservation.as_ref()
    }
    pub fn charge(&self) -> Option<u64> {
        self.charge
    }
    /// Attempt the existing atomic receipt transaction without waiting for the
    /// proxy mutex. SQLite itself may still wait up to its configured busy timeout.
    /// An unresolved result owns the unchanged attempt; callers must retain it.
    /// Always use the original admission ledger and unchanged shard topology:
    /// reservation IDs are ledger-local, and this primitive does not bind them
    /// to a database identity or authenticate the caller's request ID.
    pub fn try_commit(self, ledger: &Mutex<ProxyLedger>) -> Attempt {
        let Some(charge) = self.charge else {
            return Attempt::Unresolved {
                pending: self,
                failure: Failure::UnknownUsage,
            };
        };
        let Some(reservation) = self.reservation.as_ref() else {
            return Attempt::Unresolved {
                pending: self,
                failure: Failure::NoReservation,
            };
        };
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
        match durable.settle_with_evidence_durable(&reservation.scope, reservation.id, charge) {
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
