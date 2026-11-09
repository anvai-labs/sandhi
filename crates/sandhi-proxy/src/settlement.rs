//! Owned settlement attempts; authoritative HTTP integration remains a separate gate.
//!
//! Legacy attempts freeze caller charge. Tracked attempts own full immutable usage,
//! persist its observation, then settle through the canonical stored-charge API.
//! Failures retain ownership and distinguish an unstored observation from one already
//! recorded. Neither mode retries inference, acknowledges logical events, or schedules
//! recovery. Do not activate HTTP ownership until W05c–e's integration gates pass.
use crate::ProxyLedger;
use sandhi_core::{Reservation, UsageV2};
use sandhi_store::ledger::evidence::{
    ClosureOutcome, DispatchOutcome, DispatchPermit, EvidenceError, ExecutionIntent,
    PreDispatchClosure, SettlementOutcome,
};
use sandhi_store::ShardedLedger;
use std::sync::Mutex;

pub mod admission;
pub mod buffered;
pub mod jobs;

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
    /// Budget cost-basis weight for the tracked frozen charge (basis points;
    /// 10,000 = neutral). Server-admitted config, never caller data (TD-0032).
    cache_read_bp: u32,
    /// ADR-0015 back-leg lease and its actual, settled in the SAME ledger critical section as
    /// the primary, immediately after the primary commits. Carried here so the back leg never
    /// contends with its own call's primary for the proxy mutex, follows the jobs lifecycle,
    /// and is retained with the primary across unresolved retries.
    back_leg: Option<(Reservation, u64)>,
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
    fn snapshot(&self) -> Self {
        Self {
            request_id: self.request_id.clone(),
            reservation: self.reservation.clone(),
            charge: self.charge,
            cache_read_bp: self.cache_read_bp,
            back_leg: self.back_leg.clone(),
            tracked: self.tracked.as_ref().map(|t| {
                Box::new(TrackedObservation {
                    intent: t.intent.clone(),
                    usage: t.usage.clone(),
                    recorded: t.recorded,
                })
            }),
        }
    }

    pub fn new(request_id: String, reservation: Option<Reservation>, usage: &UsageV2) -> Self {
        Self {
            request_id,
            reservation,
            cache_read_bp: sandhi_core::allowance::Weight::default().basis_points(),
            charge: match usage.completeness {
                sandhi_core::UsageCompleteness::Final | sandhi_core::UsageCompleteness::Partial => {
                    Some(sandhi_core::billable(usage))
                }
                sandhi_core::UsageCompleteness::Unavailable => None,
            },
            tracked: None,
            back_leg: None,
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
            cache_read_bp: sandhi_core::allowance::Weight::default().basis_points(),
            tracked: Some(Box::new(TrackedObservation {
                intent,
                usage,
                recorded: false,
            })),
            back_leg: None,
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

    /// Set the budget cost-basis weight applied to the tracked frozen charge
    /// (basis points; 10,000 = neutral). Server-admitted operator config -
    /// callers of the settlement API can never choose a cheaper weight.
    pub fn with_cache_read_weight(mut self, cache_read_bp: u32) -> Self {
        self.cache_read_bp = cache_read_bp;
        self
    }

    /// Attach the ADR-0015 back-leg lease, settled with `actual` once the primary commits.
    pub fn with_back_leg(mut self, reservation: Option<Reservation>, actual: u64) -> Self {
        self.back_leg = reservation.map(|reservation| (reservation, actual));
        self
    }

    /// Detach the back-leg lease when this settlement will never run (refused submission).
    pub fn take_back_leg(&mut self) -> Option<(Reservation, u64)> {
        self.back_leg.take()
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
        let back_leg = &self.back_leg;
        let tracked = &mut self.tracked;
        let reservation = &self.reservation;
        let charge = self.charge;
        let result = with_durable_ledger(ledger, |durable| {
            // ADR-0015 back leg, settled in this critical section. Idempotent by lease id
            // (`settled=0` guard), so retained replays are no-ops; a failure never undoes the
            // primary and leaves the lease to TTL reclaim.
            let settle_back_leg = || {
                if let Some((back, actual)) = back_leg {
                    if let Err(error) = durable.settle_durable(&back.scope, back.id, *actual) {
                        tracing::error!(
                            lease = back.id,
                            actual = *actual,
                            %error,
                            "durable back-leg settle FAILED — capacity stays reserved until the lease expires"
                        );
                    }
                }
            };
            if let Some(tracked) = tracked.as_mut() {
                // Exact record replay is intentional, even after a previous recorded
                // attempt: the current ledger must confirm identical retained evidence.
                match durable.record_terminal_for_intent_durable(
                    &tracked.intent,
                    &tracked.usage,
                    self.cache_read_bp,
                ) {
                    Ok(_) => {
                        tracked.recorded = true;
                        // The terminal observation is durable, so the call's measured actual
                        // (0 when unmeasured) is final for the back leg. Settle it NOW, before
                        // the receipt: a primary that never commits (upstream error, missing or
                        // non-final usage, a failing receipt, a crash before receipt recovery)
                        // must not park the back lease against the provider cap until TTL.
                        settle_back_leg();
                        durable.settle_terminal_durable(
                            &tracked.intent.reservation.scope,
                            &tracked.intent.execution_id,
                        )
                    }
                    // Not durably observed: the back leg stays with the retained primary.
                    Err(error) => Err(error),
                }
            } else {
                let reservation = reservation.as_ref().expect("legacy reservation checked");
                let primary = durable.settle_with_evidence_durable(
                    &reservation.scope,
                    reservation.id,
                    charge.expect("legacy charge checked"),
                );
                if primary.is_ok() {
                    settle_back_leg();
                }
                primary
            }
        });
        match result {
            Ok(outcome) => Attempt::Committed {
                request_id: self.request_id,
                outcome,
            },
            Err(failure) => Attempt::Unresolved {
                pending: self,
                failure,
            },
        }
    }
}

// Shared lock/failure policy. The outer mutex is nonblocking; SQLite and inner
// shard locks can still wait. This does not promise an end-to-end deadline.
fn with_durable_ledger<T>(
    ledger: &Mutex<ProxyLedger>,
    operation: impl FnOnce(&ShardedLedger) -> Result<T, EvidenceError>,
) -> Result<T, Failure> {
    let guard = ledger.try_lock().map_err(|error| match error {
        std::sync::TryLockError::WouldBlock => Failure::LedgerBusy,
        std::sync::TryLockError::Poisoned(_) => Failure::LedgerPoisoned,
    })?;
    let ProxyLedger::Durable(durable) = &*guard else {
        return Err(Failure::NonDurableLedger);
    };
    operation(durable).map_err(Failure::Evidence)
}

/// Own an admitted execution until explicit dispatch authorization or closure.
/// No Drop I/O: abandonment retains durable liability for recovery. Constructing
/// this owner is not proof of Prepared state or permission; storage checks both.
/// Use the original database and fixed topology with a trusted caller. Binding
/// validation is not caller authorization. Synchronous try_* methods can wait
/// inside SQLite/inner shard locks; they provide no end-to-end deadline or cutoff.
#[derive(Debug)]
#[must_use]
pub struct PendingDispatch {
    request_id: String,
    intent: ExecutionIntent,
}

/// Carries the only permit issued by the successful authorization transaction.
/// Not Clone/Deserialize; HTTP dispatch must consume this owner at its boundary.
/// Dropping it retains uncertain liability and never manufactures terminal usage.
#[derive(Debug)]
#[must_use]
pub struct AuthorizedExecution {
    request_id: String,
    intent: ExecutionIntent,
    _permit: DispatchPermit,
}

#[derive(Debug)]
#[must_use]
pub enum DispatchAttempt {
    MayHaveDispatched {
        pending: PendingDispatch,
    },
    Authorized(AuthorizedExecution),
    Closed {
        request_id: String,
        closure: PreDispatchClosure,
    },
    Unresolved {
        pending: PendingDispatch,
        failure: Failure,
    },
}

impl PendingDispatch {
    pub fn new(request_id: String, intent: ExecutionIntent) -> Self {
        Self { request_id, intent }
    }
    pub fn request_id(&self) -> &str {
        &self.request_id
    }
    pub fn intent(&self) -> &ExecutionIntent {
        &self.intent
    }

    /// Only a first committed authorization returns an AuthorizedExecution.
    /// Errors retain the unchanged owner. Replays are uncertain, never a send retry.
    pub fn try_authorize(self, ledger: &Mutex<ProxyLedger>) -> DispatchAttempt {
        match with_durable_ledger(ledger, |durable| {
            durable.authorize_dispatch_for_intent_durable(&self.intent)
        }) {
            Ok(DispatchOutcome::Authorized(permit)) => {
                DispatchAttempt::Authorized(AuthorizedExecution {
                    request_id: self.request_id,
                    intent: self.intent,
                    _permit: permit,
                })
            }
            Ok(DispatchOutcome::ClosedBeforeDispatch(closure)) => DispatchAttempt::Closed {
                request_id: self.request_id,
                closure,
            },
            Ok(DispatchOutcome::MayHaveDispatched) => {
                DispatchAttempt::MayHaveDispatched { pending: self }
            }
            Err(failure) => DispatchAttempt::Unresolved {
                pending: self,
                failure,
            },
        }
    }

    /// Close only proven never-authorized admission; never legacy settle(0).
    pub fn try_close(self, ledger: &Mutex<ProxyLedger>) -> DispatchAttempt {
        match with_durable_ledger(ledger, |durable| {
            durable.close_before_dispatch_for_intent_durable(&self.intent)
        }) {
            Ok(ClosureOutcome::Closed(closure) | ClosureOutcome::AlreadyClosed(closure)) => {
                DispatchAttempt::Closed {
                    request_id: self.request_id,
                    closure,
                }
            }
            Ok(ClosureOutcome::MayHaveDispatched) => {
                DispatchAttempt::MayHaveDispatched { pending: self }
            }
            Err(failure) => DispatchAttempt::Unresolved {
                pending: self,
                failure,
            },
        }
    }
}

impl AuthorizedExecution {
    pub fn request_id(&self) -> &str {
        &self.request_id
    }
    pub fn intent(&self) -> &ExecutionIntent {
        &self.intent
    }

    /// Transfer the original identity and actual terminal observation to the
    /// existing settlement owner. Caller supplies evidence, not inferred zero.
    /// Authorization alone is not evidence that a provider completed a request.
    pub fn into_settlement(self, usage: UsageV2) -> PendingSettlement {
        PendingSettlement::tracked(self.request_id, self.intent, usage)
    }
}
