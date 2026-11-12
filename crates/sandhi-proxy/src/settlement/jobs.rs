//! Bounded process-local ownership for blocking settlement work.
//!
//! Keep this manager alive through drain and explicit result collection. Slots include
//! queued, running and unclaimed results; this is a count bound, not a byte bound.
//! Retained RAM evidence is not durable. HTTP wiring/recovery scheduling remain gated.
use super::admission::{AdmissionOutcome, PreparedAdmission};
use super::{Attempt, DispatchAttempt, Failure, PendingDispatch, PendingSettlement};
use crate::{
    lifecycle::{Lifecycle, OperationGuard},
    ProxyLedger,
};
use std::collections::BTreeMap;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use tokio::sync::watch;
mod timing;

// Only LedgerBusy proves that the outer lock was never acquired and no SQL ran.
// Retry that accounting work within one existing wait budget; never retry storage
// ambiguity, reconstruct a permit, change evidence, or execute inference here.
fn run_owned(
    mut input: Input,
    lifecycle: &Lifecycle,
    until: Option<std::time::Instant>,
    mut run: impl FnMut(Input) -> ResultValue,
) -> ResultValue {
    let mut delay = std::time::Duration::from_millis(1);
    loop {
        let close = matches!(&input, Input::Work(Work::Close(_)));
        let dispatching = matches!(
            &input,
            Input::Admission(_) | Input::Work(Work::Authorize(_))
        );
        let outcome = run(input);
        let Some(until) = until else {
            return outcome;
        };
        let now = std::time::Instant::now();
        let remaining = until
            .saturating_duration_since(now)
            .min(lifecycle.remaining().unwrap_or(std::time::Duration::MAX));
        if remaining.is_zero() || (dispatching && !lifecycle.is_running()) {
            return outcome;
        }
        let busy = matches!(
            &outcome,
            ResultValue::Admission(AdmissionOutcome::Failed {
                failure: Failure::LedgerBusy,
                ..
            }) | ResultValue::Work(Outcome::Settlement(Attempt::Unresolved {
                failure: Failure::LedgerBusy,
                ..
            })) | ResultValue::Work(Outcome::Dispatch(DispatchAttempt::Unresolved {
                failure: Failure::LedgerBusy,
                ..
            }))
        );
        if !busy {
            return outcome;
        }
        std::thread::sleep(delay.min(remaining));
        if std::time::Instant::now() >= until
            || lifecycle.remaining().is_some_and(|left| left.is_zero())
            || (dispatching && !lifecycle.is_running())
        {
            return outcome;
        }
        input = match outcome {
            ResultValue::Admission(AdmissionOutcome::Failed {
                evidence,
                failure: Failure::LedgerBusy,
            }) => Input::Admission(PreparedAdmission { evidence }),
            ResultValue::Work(Outcome::Settlement(Attempt::Unresolved {
                pending,
                failure: Failure::LedgerBusy,
            })) => Input::Work(Work::Settle(pending)),
            ResultValue::Work(Outcome::Dispatch(DispatchAttempt::Unresolved {
                pending,
                failure: Failure::LedgerBusy,
            })) => Input::Work(if close {
                Work::Close(pending)
            } else {
                Work::Authorize(pending)
            }),
            other => return other,
        };
        delay = (delay * 2).min(std::time::Duration::from_millis(10));
    }
}

/// Only accounting transitions. No inference callback or network dispatch authority.
#[derive(Debug)]
pub enum Work {
    Authorize(PendingDispatch),
    Close(PendingDispatch),
    Settle(PendingSettlement),
}

impl Work {
    // Private, inert evidence only. Never copy AuthorizedExecution or DispatchPermit.
    fn snapshot(&self) -> Self {
        match self {
            Self::Authorize(p) => {
                Self::Authorize(PendingDispatch::new(p.request_id.clone(), p.intent.clone()))
            }
            Self::Close(p) => {
                Self::Close(PendingDispatch::new(p.request_id.clone(), p.intent.clone()))
            }
            Self::Settle(p) => Self::Settle(p.snapshot()),
        }
    }
    fn run(self, ledger: &Mutex<ProxyLedger>) -> Outcome {
        match self {
            Self::Authorize(p) => Outcome::Dispatch(p.try_authorize(ledger)),
            Self::Close(p) => Outcome::Dispatch(p.try_close(ledger)),
            Self::Settle(p) => Outcome::Settlement(p.try_commit(ledger)),
        }
    }
}

#[derive(Debug)]
pub enum Outcome {
    Dispatch(DispatchAttempt),
    Settlement(Attempt),
    /// Durable progress is unknown; snapshot flags may predate a committed transaction.
    /// Reconcile the original ledger. Never infer rollback or recreate a send permit.
    Interrupted(Work),
}

#[derive(Debug, PartialEq, Eq)]
pub enum Refusal {
    Full,
    Closed,
    NoRuntime,
}

#[derive(Debug)]
pub struct Rejected {
    pub work: Work,
    pub reason: Refusal,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Work,
    Admission,
}
enum Input {
    Work(Work),
    Admission(PreparedAdmission),
}
impl Input {
    fn snapshot(&self) -> Self {
        match self {
            Self::Work(work) => Self::Work(work.snapshot()),
            Self::Admission(input) => Self::Admission(PreparedAdmission {
                evidence: input.evidence.clone(),
            }),
        }
    }
    fn interrupted(self) -> ResultValue {
        match self {
            Self::Work(work) => ResultValue::Work(Outcome::Interrupted(work)),
            Self::Admission(input) => {
                ResultValue::Admission(AdmissionOutcome::Interrupted(input.evidence))
            }
        }
    }
}
enum ResultValue {
    Work(Outcome),
    Admission(AdmissionOutcome),
}
impl ResultValue {
    fn kind(&self) -> Kind {
        match self {
            Self::Work(_) => Kind::Work,
            Self::Admission(_) => Kind::Admission,
        }
    }
    fn interrupted(&self) -> bool {
        matches!(
            self,
            Self::Work(Outcome::Interrupted(_)) | Self::Admission(AdmissionOutcome::Interrupted(_))
        )
    }
}
struct InputRejected {
    input: Input,
    reason: Refusal,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Empty,
    Admitting,
    Prepared,
    Authorizing,
    Authorized,
    Closing,
    Terminal,
    Resolved,
    Uncertain,
}

struct Slot {
    snapshot: Option<Input>,
    outcome: Option<ResultValue>,
    held: bool,
    running: bool,
    epoch: u64,
    releasable: bool,
    consumed: bool,
    phase: Phase,
}
struct State {
    slots: BTreeMap<u64, Slot>,
    next: u64,
    closed: bool,
}
struct Inner {
    state: Mutex<State>,
    changed: watch::Sender<()>,
    capacity: usize,
    lifecycle: Arc<Lifecycle>,
}
impl Inner {
    fn lock(&self) -> MutexGuard<'_, State> {
        match self.state.lock() {
            Ok(state) => state,
            Err(error) => {
                let mut state = error.into_inner();
                state.closed = true;
                state
            }
        }
    }
    fn finish(&self, id: u64, epoch: u64, outcome: Option<ResultValue>) {
        let mut state = self.lock();
        let interrupted = outcome.as_ref().is_none_or(ResultValue::interrupted);
        if interrupted {
            state.closed = true;
        }
        if let Some(slot) = state
            .slots
            .get_mut(&id)
            .filter(|slot| slot.epoch == epoch && slot.running)
        {
            slot.running = false;
            if slot.held {
                slot.phase = match &outcome {
                    Some(ResultValue::Admission(AdmissionOutcome::Prepared(_))) => Phase::Prepared,
                    Some(ResultValue::Admission(
                        AdmissionOutcome::Denied { .. } | AdmissionOutcome::Cutoff(_),
                    )) => Phase::Resolved,
                    Some(ResultValue::Work(Outcome::Dispatch(DispatchAttempt::Authorized(_)))) => {
                        Phase::Authorized
                    }
                    Some(ResultValue::Work(Outcome::Dispatch(DispatchAttempt::Closed {
                        ..
                    })))
                    | Some(ResultValue::Work(Outcome::Settlement(Attempt::Committed { .. }))) => {
                        Phase::Resolved
                    }
                    _ if slot.phase == Phase::Terminal => Phase::Terminal,
                    _ => Phase::Uncertain,
                };
            }
            slot.releasable = matches!(
                &outcome,
                Some(ResultValue::Admission(
                    AdmissionOutcome::Denied { .. } | AdmissionOutcome::Cutoff(_)
                )) | Some(ResultValue::Work(Outcome::Settlement(
                    Attempt::Committed { .. }
                ))) | Some(ResultValue::Work(Outcome::Dispatch(
                    DispatchAttempt::Closed { .. }
                )))
            );
            if slot.held {
                if let Some(ResultValue::Admission(AdmissionOutcome::Prepared(pending))) = &outcome
                {
                    slot.snapshot = Some(Input::Work(Work::Close(PendingDispatch::new(
                        pending.request_id.clone(),
                        pending.intent.clone(),
                    ))));
                }
            }
            // Only the worker guard can finish; consumers cannot take a pending slot.
            slot.outcome = outcome.or_else(|| {
                slot.snapshot
                    .as_ref()
                    .map(Input::snapshot)
                    .map(Input::interrupted)
            });
            if !slot.held {
                slot.snapshot = None;
            }
        } else {
            state.closed = true;
        }
        self.changed.send_replace(());
    }
    fn take(&self, id: u64, epoch: u64, kind: Kind) -> Option<ResultValue> {
        let mut state = self.lock();
        if state.slots.get(&id)?.epoch != epoch
            || state.slots.get(&id)?.consumed
            || state.slots.get(&id)?.outcome.as_ref()?.kind() != kind
        {
            return None;
        }
        let outcome = if state.slots.get(&id)?.held {
            let slot = state.slots.get_mut(&id)?;
            slot.consumed = true;
            slot.outcome.take()
        } else {
            state.slots.remove(&id)?.outcome
        };
        self.changed.send_replace(());
        outcome
    }
    fn take_ready(&self, kind: Kind) -> Option<ResultValue> {
        let mut state = self.lock();
        let id = state.slots.iter().find_map(|(id, slot)| {
            if slot.held {
                return None;
            }
            slot.outcome
                .as_ref()
                .filter(|result| result.kind() == kind)
                .map(|_| *id)
        })?;
        let outcome = state.slots.remove(&id)?.outcome;
        self.changed.send_replace(());
        outcome
    }
}

// Created BEFORE spawn_blocking. Aborting a queued task before execution drops
// this guard too, publishing uncertainty before its operation guard is released.
struct Worker {
    inner: Arc<Inner>,
    id: u64,
    finished: bool,
    epoch: u64,
    timing: Arc<timing::Timing>,
    _operation: Arc<OperationGuard>,
}
impl Worker {
    fn finish(mut self, outcome: ResultValue) {
        self.timing.returned();
        self.inner.finish(self.id, self.epoch, Some(outcome));
        self.timing.published();
        self.finished = true;
    }
}
impl Drop for Worker {
    fn drop(&mut self) {
        if !self.finished {
            self.timing.interrupted();
            self.inner.finish(self.id, self.epoch, None);
            self.timing.published();
        }
    }
}

// Access the same authoritative mutex. The HTTP bridge is weak because its
// owner belongs to ProxyState; admitted async tasks hold the strong state.
#[derive(Clone)]
enum LedgerAccess {
    Shared(Arc<Mutex<ProxyLedger>>),
    Proxy(Weak<crate::ProxyState>),
}
impl LedgerAccess {
    fn with<T>(&self, run: impl FnOnce(&Mutex<ProxyLedger>) -> T) -> Option<T> {
        match self {
            Self::Shared(ledger) => Some(run(ledger)),
            Self::Proxy(state) => state.upgrade().map(|state| run(&state.ledger)),
        }
    }
}

/// Trusted process-local supervisor. Keep alive until all retained owners are handed off.
/// Dropping the last manager/ticket/worker loses RAM evidence, never durable liability.
pub struct Jobs {
    inner: Arc<Inner>,
    ledger: LedgerAccess,
    retry_wait: Option<std::time::Duration>,
}
/// A notification/explicit transfer handle. Dropping it does not remove the registry slot.
pub struct Ticket {
    inner: Arc<Inner>,
    id: u64,
    epoch: u64,
    timing: Arc<timing::Timing>,
    #[cfg(test)]
    abort: tokio::task::AbortHandle,
}
/// Same bounded registry and notification semantics as Ticket, with typed transfer.
pub struct AdmissionTicket {
    ticket: Ticket,
}
#[derive(Debug)]
pub struct AdmissionRejected {
    pub input: PreparedAdmission,
    pub reason: Refusal,
}

/// One private HTTP obligation reserves capacity through provider execution.
/// Drop retains unresolved evidence; it performs no I/O and grants no retry authority.
pub(crate) struct Obligation {
    inner: Arc<Inner>,
    ledger: LedgerAccess,
    id: u64,
    operation: Arc<OperationGuard>,
    retry_wait: Option<std::time::Duration>,
}
impl Obligation {
    pub(crate) fn submit(&mut self, work: Work) -> Result<Ticket, Rejected> {
        self.submit_input(Input::Work(work)).map_err(|error| {
            let Input::Work(work) = error.input else {
                unreachable!("work refusal")
            };
            Rejected {
                work,
                reason: error.reason,
            }
        })
    }
    pub(crate) fn admit(
        &mut self,
        input: PreparedAdmission,
    ) -> Result<AdmissionTicket, AdmissionRejected> {
        self.submit_input(Input::Admission(input))
            .map(|ticket| AdmissionTicket { ticket })
            .map_err(|error| {
                let Input::Admission(input) = error.input else {
                    unreachable!("admission refusal")
                };
                AdmissionRejected {
                    input,
                    reason: error.reason,
                }
            })
    }
    fn submit_input(&mut self, input: Input) -> Result<Ticket, InputRejected> {
        let authorization = matches!(input, Input::Admission(_) | Input::Work(Work::Authorize(_)));
        let mut state = self.inner.lock();
        let closed = state.closed;
        let Some(slot) = state.slots.get_mut(&self.id) else {
            return Err(InputRejected {
                input,
                reason: Refusal::Closed,
            });
        };
        if slot.running || slot.outcome.is_some() {
            return Err(InputRejected {
                input,
                reason: Refusal::Full,
            });
        }
        // Validate the one admission, original binding and monotonic phase before
        // allowing a caller to replace retained evidence. No second intent or permit.
        let binding_matches =
            |request: &str, intent: &sandhi_store::ledger::evidence::ExecutionIntent| match slot
                .snapshot
                .as_ref()
            {
                Some(Input::Work(Work::Authorize(p) | Work::Close(p))) => {
                    p.request_id == request && &p.intent == intent
                }
                Some(Input::Work(Work::Settle(p))) => {
                    p.request_id == request
                        && p.tracked.as_ref().is_some_and(|t| &t.intent == intent)
                }
                _ => false,
            };
        let next_phase = match &input {
            Input::Admission(_) if slot.phase == Phase::Empty => Some(Phase::Admitting),
            Input::Work(Work::Authorize(p))
                if slot.phase == Phase::Prepared && binding_matches(&p.request_id, &p.intent) =>
            {
                Some(Phase::Authorizing)
            }
            Input::Work(Work::Close(p))
                if slot.phase == Phase::Prepared && binding_matches(&p.request_id, &p.intent) =>
            {
                Some(Phase::Closing)
            }
            Input::Work(Work::Settle(p))
                if matches!(slot.phase, Phase::Authorized | Phase::Terminal) =>
            {
                p.tracked
                    .as_ref()
                    .filter(|t| {
                        binding_matches(&p.request_id, &t.intent)
                            && match slot.snapshot.as_ref() {
                                Some(Input::Work(Work::Settle(previous))) => {
                                    previous.terminal_usage() == p.terminal_usage()
                                }
                                _ => slot.phase == Phase::Authorized,
                            }
                    })
                    .map(|_| Phase::Terminal)
            }
            _ => None,
        };
        let Some(next_phase) = next_phase else {
            return Err(InputRejected {
                input,
                reason: Refusal::Closed,
            });
        };
        // Capture terminal metadata BEFORE scheduling can fail (including absent
        // runtime or elapsed drain deadline). This is inert evidence, never a permit.
        if next_phase == Phase::Terminal {
            slot.phase = next_phase;
            slot.snapshot = Some(input.snapshot());
            slot.releasable = false;
        }
        if (authorization && (closed || !self.inner.lifecycle.is_running()))
            || self
                .inner
                .lifecycle
                .remaining()
                .is_some_and(|left| left.is_zero())
        {
            return Err(InputRejected {
                input,
                reason: Refusal::Closed,
            });
        }
        let runtime = match tokio::runtime::Handle::try_current() {
            Ok(runtime) => runtime,
            Err(_) => {
                return Err(InputRejected {
                    input,
                    reason: Refusal::NoRuntime,
                })
            }
        };
        let Some(epoch) = slot.epoch.checked_add(1) else {
            return Err(InputRejected {
                input,
                reason: Refusal::Closed,
            });
        };
        slot.phase = next_phase;
        slot.snapshot = Some(input.snapshot());
        slot.releasable = false;
        slot.epoch = epoch;
        slot.consumed = false;
        slot.running = true;
        let timing = Arc::new(timing::Timing::new());
        let worker = Worker {
            inner: self.inner.clone(),
            id: self.id,
            finished: false,
            epoch,
            timing: timing.clone(),
            _operation: self.operation.clone(),
        };
        drop(state);
        let ledger = self.ledger.clone();
        let lifecycle = self.inner.lifecycle.clone();
        let until = self.retry_wait.map(|wait| std::time::Instant::now() + wait);
        let _task = runtime.spawn_blocking(move || {
            worker.timing.start();
            if let Ok(outcome) = catch_unwind(AssertUnwindSafe(|| {
                run_owned(input, &lifecycle, until, |input| {
                    ledger
                        .with(|ledger| match input {
                            Input::Work(work) => ResultValue::Work(work.run(ledger)),
                            Input::Admission(input) => {
                                ResultValue::Admission(input.run(ledger, &lifecycle))
                            }
                        })
                        .expect("owned proxy state")
                })
            })) {
                worker.finish(outcome);
            }
        });
        Ok(Ticket {
            inner: self.inner.clone(),
            id: self.id,
            epoch,
            timing,
            #[cfg(test)]
            abort: _task.abort_handle(),
        })
    }
}
impl Drop for Obligation {
    fn drop(&mut self) {
        let mut state = self.inner.lock();
        if let Some(slot) = state.slots.get_mut(&self.id) {
            slot.held = false;
            if !slot.running && slot.outcome.is_none() {
                if slot.releasable || slot.snapshot.is_none() {
                    state.slots.remove(&self.id);
                } else {
                    slot.outcome = slot.snapshot.take().map(Input::interrupted);
                }
            }
        }
        self.inner.changed.send_replace(());
    }
}
impl AdmissionTicket {
    pub(crate) fn warn_timeout(
        &self,
        request_id: &str,
        wait: std::time::Duration,
        started: std::time::Instant,
    ) {
        self.ticket
            .warn_timeout(request_id, "admission", wait, started);
    }
    pub async fn wait(&self) {
        self.ticket.wait().await;
    }
    pub fn take(&self) -> Option<AdmissionOutcome> {
        match self
            .ticket
            .inner
            .take(self.ticket.id, self.ticket.epoch, Kind::Admission)?
        {
            ResultValue::Admission(result) => Some(result),
            ResultValue::Work(_) => unreachable!("typed registry transfer"),
        }
    }
}
#[derive(Debug)]
pub struct DrainReport {
    /// All admitted operations in the shared lifecycle, not just this manager's jobs.
    pub active: usize,
    /// Queued, running and unclaimed completed/uncertain owners in this manager.
    pub retained: usize,
    pub timed_out: bool,
}
impl Jobs {
    /// Reconcile one abandoned terminal snapshot on the original ledger. The
    /// registry slot remains owned until canonical settlement confirms completion.
    /// Cursor advances past uncertainty; no authorization or inference is repeated.
    pub(crate) fn retry_terminal(&self, after: &mut Option<u64>) {
        let candidate = {
            let mut state = self.inner.lock();
            let candidate = state
                .slots
                .range((
                    after.map_or(std::ops::Bound::Unbounded, std::ops::Bound::Excluded),
                    std::ops::Bound::Unbounded,
                ))
                .find(|(_, slot)| !slot.held && !slot.running)
                .map(|(id, _)| *id);
            let Some(id) = candidate else {
                *after = None;
                return;
            };
            *after = Some(id);
            let slot = state.slots.get_mut(&id).expect("existing slot");
            if slot.releasable {
                state.slots.remove(&id);
                return;
            }
            let work = match slot.snapshot.as_ref() {
                Some(Input::Work(Work::Settle(p))) => Some(Work::Settle(p.snapshot())),
                _ => match slot.outcome.as_ref() {
                    Some(ResultValue::Work(Outcome::Settlement(Attempt::Unresolved {
                        pending,
                        ..
                    }))) => Some(Work::Settle(pending.snapshot())),
                    Some(ResultValue::Work(Outcome::Interrupted(Work::Settle(p)))) => {
                        Some(Work::Settle(p.snapshot()))
                    }
                    _ => None,
                },
            };
            let Some(work) = work else {
                return;
            };
            let Some(epoch) = slot.epoch.checked_add(1) else {
                return;
            };
            slot.epoch = epoch;
            slot.running = true;
            slot.snapshot = Some(Input::Work(work.snapshot()));
            slot.outcome = None;
            (id, epoch, work)
        };
        let (id, epoch, work) = candidate;
        let outcome = catch_unwind(AssertUnwindSafe(|| {
            self.ledger
                .with(|ledger| ResultValue::Work(work.run(ledger)))
        }))
        .ok()
        .flatten();
        self.inner.finish(id, epoch, outcome);
        let mut state = self.inner.lock();
        if state
            .slots
            .get(&id)
            .is_some_and(|s| !s.held && !s.running && s.releasable)
        {
            state.slots.remove(&id);
        }
    }

    pub(crate) fn retained(&self) -> usize {
        self.inner.lock().slots.len()
    }
    pub(crate) fn obligation(&self) -> Result<Obligation, Refusal> {
        let mut state = self.inner.lock();
        if state.closed || !self.inner.lifecycle.is_running() {
            return Err(Refusal::Closed);
        }
        if state.slots.len() >= self.inner.capacity {
            return Err(Refusal::Full);
        }
        let Some(next) = state.next.checked_add(1) else {
            return Err(Refusal::Closed);
        };
        let operation = self
            .inner
            .lifecycle
            .try_operation()
            .ok_or(Refusal::Closed)?;
        let id = state.next;
        state.next = next;
        state.slots.insert(
            id,
            Slot {
                snapshot: None,
                outcome: None,
                held: true,
                running: false,
                epoch: 0,
                releasable: false,
                consumed: false,
                phase: Phase::Empty,
            },
        );
        Ok(Obligation {
            inner: self.inner.clone(),
            ledger: self.ledger.clone(),
            id,
            operation: Arc::new(operation),
            retry_wait: self.retry_wait,
        })
    }
    pub fn submit_admission(
        &self,
        input: PreparedAdmission,
    ) -> Result<AdmissionTicket, AdmissionRejected> {
        let ledger = self.ledger.clone();
        let lifecycle = self.inner.lifecycle.clone();
        self.submit_admission_with(input, move |input| {
            ledger
                .with(|ledger| input.run(ledger, &lifecycle))
                .expect("owned proxy state")
        })
    }
    fn submit_admission_with(
        &self,
        input: PreparedAdmission,
        run: impl FnOnce(PreparedAdmission) -> AdmissionOutcome + Send + 'static,
    ) -> Result<AdmissionTicket, AdmissionRejected> {
        self.submit_input(Input::Admission(input), move |input| {
            let Input::Admission(input) = input else {
                unreachable!("admission input")
            };
            ResultValue::Admission(run(input))
        })
        .map(|ticket| AdmissionTicket { ticket })
        .map_err(|error| {
            let Input::Admission(input) = error.input else {
                unreachable!("admission refusal")
            };
            AdmissionRejected {
                input,
                reason: error.reason,
            }
        })
    }
    /// Transfer one ready admission without consuming existing Work outcomes.
    /// Uncertainty is inert evidence, never permission to reserve again.
    pub fn take_admission_ready(&self) -> Option<AdmissionOutcome> {
        match self.inner.take_ready(Kind::Admission)? {
            ResultValue::Admission(result) => Some(result),
            ResultValue::Work(_) => unreachable!("typed registry transfer"),
        }
    }
    /// Zero capacity intentionally refuses all submissions. Inputs must be size-limited upstream.
    pub fn new(
        ledger: Arc<Mutex<ProxyLedger>>,
        lifecycle: Arc<Lifecycle>,
        capacity: usize,
    ) -> Self {
        Self::with_access(LedgerAccess::Shared(ledger), lifecycle, capacity)
    }
    pub(crate) fn for_proxy(
        state: &Arc<crate::ProxyState>,
        capacity: usize,
        wait: std::time::Duration,
    ) -> Self {
        let mut jobs = Self::with_access(
            LedgerAccess::Proxy(Arc::downgrade(state)),
            state.lifecycle.clone(),
            capacity,
        );
        jobs.retry_wait = Some(wait);
        jobs
    }
    fn with_access(ledger: LedgerAccess, lifecycle: Arc<Lifecycle>, capacity: usize) -> Self {
        Self {
            inner: Arc::new(Inner {
                state: Mutex::new(State {
                    slots: BTreeMap::new(),
                    next: 0,
                    closed: false,
                }),
                changed: watch::channel(()).0,
                capacity,
                lifecycle,
            }),
            ledger,
            retry_wait: None,
        }
    }
    pub fn submit(&self, work: Work) -> Result<Ticket, Rejected> {
        let ledger = self.ledger.clone();
        self.submit_with(work, move |work| {
            ledger
                .with(|ledger| work.run(ledger))
                .expect("owned proxy state")
        })
    }
    fn submit_with(
        &self,
        work: Work,
        run: impl FnOnce(Work) -> Outcome + Send + 'static,
    ) -> Result<Ticket, Rejected> {
        self.submit_input(Input::Work(work), move |input| {
            let Input::Work(work) = input else {
                unreachable!("work input")
            };
            ResultValue::Work(run(work))
        })
        .map_err(|error| {
            let Input::Work(work) = error.input else {
                unreachable!("work refusal")
            };
            Rejected {
                work,
                reason: error.reason,
            }
        })
    }
    fn submit_input(
        &self,
        input: Input,
        run: impl FnOnce(Input) -> ResultValue + Send + 'static,
    ) -> Result<Ticket, InputRejected> {
        let runtime = match tokio::runtime::Handle::try_current() {
            Ok(runtime) => runtime,
            Err(_) => {
                return Err(InputRejected {
                    input,
                    reason: Refusal::NoRuntime,
                })
            }
        };
        let mut state = self.inner.lock();
        if state.closed || !self.inner.lifecycle.is_running() {
            return Err(InputRejected {
                input,
                reason: Refusal::Closed,
            });
        }
        if state.slots.len() >= self.inner.capacity {
            return Err(InputRejected {
                input,
                reason: Refusal::Full,
            });
        }
        let Some(next) = state.next.checked_add(1) else {
            state.closed = true;
            return Err(InputRejected {
                input,
                reason: Refusal::Closed,
            });
        };
        let Some(operation) = self.inner.lifecycle.try_operation() else {
            return Err(InputRejected {
                input,
                reason: Refusal::Closed,
            });
        };
        let id = state.next;
        state.next = next;
        // Reserve under the one registry lock before copying/scheduling any work.
        state.slots.insert(
            id,
            Slot {
                snapshot: Some(input.snapshot()),
                outcome: None,
                held: false,
                running: true,
                epoch: 0,
                releasable: false,
                consumed: false,
                phase: Phase::Empty,
            },
        );
        let timing = Arc::new(timing::Timing::new());
        let worker = Worker {
            inner: self.inner.clone(),
            id,
            finished: false,
            epoch: 0,
            timing: timing.clone(),
            _operation: Arc::new(operation),
        };
        drop(state);
        // The closure owns both completion publication and lifecycle progress. Its
        // JoinHandle is not the result owner and dropping it cannot discard a result.
        let _task = runtime.spawn_blocking(move || {
            worker.timing.start();
            if let Ok(outcome) = catch_unwind(AssertUnwindSafe(|| run(input))) {
                worker.finish(outcome);
            }
        });
        Ok(Ticket {
            inner: self.inner.clone(),
            id,
            epoch: 0,
            timing,
            #[cfg(test)]
            abort: _task.abort_handle(),
        })
    }
    #[cfg(test)]
    pub(crate) fn interrupt_after_commit(&self, work: Work) -> Result<Ticket, Rejected> {
        let ledger = self.ledger.clone();
        self.submit_with(work, move |work| {
            let outcome = ledger
                .with(|ledger| work.run(ledger))
                .expect("owned proxy state");
            assert!(matches!(
                outcome,
                Outcome::Dispatch(DispatchAttempt::Authorized(_))
                    | Outcome::Settlement(Attempt::Committed { .. })
            ));
            drop(outcome);
            panic!("injected interruption after commit, before publication");
        })
    }
    #[cfg(test)]
    pub(crate) fn after_admission_commit(
        &self,
        input: PreparedAdmission,
        after: impl FnOnce() + Send + 'static,
    ) -> Result<AdmissionTicket, AdmissionRejected> {
        let ledger = self.ledger.clone();
        let lifecycle = self.inner.lifecycle.clone();
        self.submit_admission_with(input, move |input| {
            let outcome = ledger
                .with(|ledger| input.run(ledger, &lifecycle))
                .expect("owned proxy state");
            assert!(matches!(outcome, AdmissionOutcome::Prepared(_)));
            after();
            outcome
        })
    }
    /// Recover an abandoned ticket's result; exactly one consumer wins under the lock.
    pub fn take_ready(&self) -> Option<Outcome> {
        match self.inner.take_ready(Kind::Work)? {
            ResultValue::Work(result) => Some(result),
            ResultValue::Admission(_) => unreachable!("typed registry transfer"),
        }
    }
    /// None until the shared lifecycle establishes cutoff. Does not extend its deadline
    /// or claim accounting is resolved merely because all workers have finished.
    pub async fn drain(&self) -> Option<DrainReport> {
        let deadline = self.inner.lifecycle.deadline()?;
        let timed_out = tokio::time::timeout_at(deadline.into(), self.inner.lifecycle.wait_idle())
            .await
            .is_err();
        Some(DrainReport {
            active: self.inner.lifecycle.active_operations(),
            retained: self.inner.lock().slots.len(),
            timed_out,
        })
    }
}
impl Ticket {
    // A nonblocking diagnostic read. Even an unavailable registry lock is useful
    // evidence; logging a timeout must not wait on the lock being diagnosed.
    fn readiness(&self) -> &'static str {
        match self.inner.state.try_lock() {
            Ok(state) => match state.slots.get(&self.id) {
                Some(slot) if slot.epoch == self.epoch && !slot.consumed => {
                    if slot.outcome.is_some() {
                        "available"
                    } else {
                        "pending"
                    }
                }
                _ => "stale_or_collected",
            },
            Err(std::sync::TryLockError::WouldBlock) => "registry_busy",
            Err(std::sync::TryLockError::Poisoned(_)) => "registry_poisoned",
        }
    }
    pub(crate) fn warn_timeout(
        &self,
        request_id: &str,
        transition: &'static str,
        wait: std::time::Duration,
        started: std::time::Instant,
    ) {
        let timing = self.timing.snapshot();
        tracing::warn!(
            request_id, transition, reason = "wait_timeout",
            wait_ms = wait.as_millis().min(u128::from(u64::MAX)) as u64,
            wait_started_us = ?self.timing.offset(started),
            wait_elapsed_us = started.elapsed().as_micros().min(u128::from(u64::MAX)) as u64,
            job_stage = timing.stage, elapsed_us = timing.elapsed_us,
            queue_us = timing.queue_us, run_us = ?timing.run_us,
            publication_us = ?timing.publication_us, published_at_us = ?timing.published_at_us,
            interrupted = timing.interrupted,
            readiness = self.readiness(), "tracked accounting wait unresolved"
        );
    }
    /// Level-triggered readiness only. Cancellation cannot consume a result.
    /// Returns also when another trusted consumer has already taken the result.
    pub async fn wait(&self) {
        let mut changed = self.inner.changed.subscribe();
        loop {
            {
                let state = self.inner.lock();
                if state.slots.get(&self.id).is_none_or(|slot| {
                    slot.epoch != self.epoch || slot.consumed || slot.outcome.is_some()
                }) {
                    return;
                }
            }
            if changed.changed().await.is_err() {
                return;
            }
        }
    }
    /// Synchronously transfer a completed result once. Pending work is never exposed.
    pub fn take(&self) -> Option<Outcome> {
        match self.inner.take(self.id, self.epoch, Kind::Work)? {
            ResultValue::Work(result) => Some(result),
            ResultValue::Admission(_) => unreachable!("typed registry transfer"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sandhi_core::{Reservation, UsageBasis, UsageCompleteness, UsageV2};
    use sandhi_store::ledger::evidence::ExecutionIntent;
    use std::time::Duration;
    use time::OffsetDateTime;

    #[test]
    fn diagnostics_distinguish_pool_queue_from_execution_without_consuming_admission() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .max_blocking_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let blocker = runtime.spawn_blocking(move || {
            started_tx.send(()).unwrap();
            release_rx.recv().unwrap();
        });
        started_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        runtime.block_on(async {
            let dir = tempfile::tempdir().unwrap();
            let lifecycle = Arc::new(Lifecycle::new());
            let ledger = Arc::new(Mutex::new(
                ProxyLedger::durable(dir.path().join("queue.db").to_str().unwrap(), 1).unwrap(),
            ));
            let jobs = Jobs::new(ledger, lifecycle.clone(), 1);
            let ticket = jobs.submit_admission(admission()).unwrap();
            let queued = ticket.ticket.timing.snapshot();
            assert_eq!(queued.stage, "queued");
            assert!(queued.run_us.is_none() && queued.publication_us.is_none());
            assert_eq!(ticket.ticket.readiness(), "pending");
            assert!(tokio::time::timeout(Duration::ZERO, ticket.wait())
                .await
                .is_err());
            assert!(ticket.take().is_none());
            assert_eq!(jobs.retained(), 1);
            release_tx.send(()).unwrap();
            blocker.await.unwrap();
            tokio::time::timeout(Duration::from_secs(2), lifecycle.wait_idle())
                .await
                .unwrap();
            let completed = ticket.ticket.timing.snapshot();
            assert_eq!(completed.stage, "published");
            assert!(!completed.interrupted);
            assert!(completed.run_us.is_some() && completed.publication_us.is_some());
            assert_eq!(ticket.ticket.readiness(), "available");
            assert!(matches!(ticket.take(), Some(AdmissionOutcome::Prepared(_))));
            assert!(ticket.take().is_none());
            assert_eq!(ticket.ticket.readiness(), "stale_or_collected");
            let connection = rusqlite::Connection::open(dir.path().join("queue.db")).unwrap();
            assert_eq!(
                connection
                    .query_row("SELECT count(*) FROM budget_execution_intent", [], |r| r
                        .get::<_, i64>(0))
                    .unwrap(),
                1
            );
        });
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // Hold the registry only to observe blocked publication.
    async fn diagnostics_distinguish_running_work_from_blocked_publication() {
        #[derive(Clone)]
        struct Captured(Arc<Mutex<Vec<u8>>>);
        impl std::io::Write for Captured {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Captured {
            type Writer = Self;
            fn make_writer(&'a self) -> Self::Writer {
                self.clone()
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let lifecycle = Arc::new(Lifecycle::new());
        let ledger = Arc::new(Mutex::new(
            ProxyLedger::durable(dir.path().join("running.db").to_str().unwrap(), 1).unwrap(),
        ));
        let jobs = Jobs::new(ledger.clone(), lifecycle.clone(), 1);
        let (committed_tx, committed_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let worker_lifecycle = lifecycle.clone();
        let ticket = jobs
            .submit_admission_with(admission(), move |input| {
                let outcome = input.run(&ledger, &worker_lifecycle);
                assert!(matches!(outcome, AdmissionOutcome::Prepared(_)));
                committed_tx.send(()).unwrap();
                release_rx.recv().unwrap();
                outcome
            })
            .unwrap();
        committed_rx.await.unwrap();
        let running = ticket.ticket.timing.snapshot();
        assert_eq!(running.stage, "running");
        assert!(running.run_us.is_some() && running.publication_us.is_none());
        assert!(ticket.take().is_none());
        let registry = jobs.inner.lock();
        release_tx.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            while ticket.ticket.timing.snapshot().stage != "publishing" {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let publishing = ticket.ticket.timing.snapshot();
        assert!(publishing.run_us.is_some() && publishing.publication_us.is_some());
        assert_eq!(ticket.ticket.readiness(), "registry_busy");
        // Timeout diagnostics must not wait on the registry they diagnose.
        let captured = Captured(Arc::new(Mutex::new(vec![])));
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .without_time()
            .with_writer(captured.clone())
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            ticket.warn_timeout(
                "diagnostic-request",
                Duration::ZERO,
                std::time::Instant::now(),
            )
        });
        let log = String::from_utf8(captured.0.lock().unwrap().clone()).unwrap();
        assert_eq!(log.lines().count(), 1);
        for expected in [
            "request_id=",
            "diagnostic-request",
            "transition=",
            "admission",
            "reason=",
            "wait_timeout",
            "job_stage=",
            "publishing",
            "queue_us=",
            "run_us=",
            "publication_us=",
            "published_at_us=",
            "wait_started_us=",
            "wait_elapsed_us=",
            "readiness=",
            "registry_busy",
        ] {
            assert!(log.contains(expected), "missing {expected}: {log}");
        }
        drop(registry);
        tokio::time::timeout(Duration::from_secs(2), lifecycle.wait_idle())
            .await
            .unwrap();
        let completed = ticket.ticket.timing.snapshot();
        assert_eq!(completed.stage, "published");
        assert!(matches!(ticket.take(), Some(AdmissionOutcome::Prepared(_))));
        assert!(
            completed.queue_us + completed.run_us.unwrap() + completed.publication_us.unwrap()
                <= completed.elapsed_us
        );
    }

    #[tokio::test]
    async fn http_obligation_keeps_cleanup_capacity_across_cutoff() {
        let (_dir, jobs, mut obligation, pending) = prepared_obligation().await;
        let lifecycle = jobs.inner.lifecycle.clone();
        assert!(matches!(jobs.obligation(), Err(Refusal::Full)));
        let first = obligation.submit(Work::Authorize(pending)).unwrap();
        first.wait().await;
        let Some(Outcome::Dispatch(DispatchAttempt::Authorized(authorized))) = first.take() else {
            panic!("authorized")
        };
        let cleanup_work = Work::Settle(authorized.into_settlement(UsageV2::default()));
        assert!(
            matches!(jobs.obligation(), Err(Refusal::Full)),
            "taking a transition must not release finalization capacity"
        );
        lifecycle.begin_quiesce(Duration::from_secs(1));
        assert!(matches!(jobs.obligation(), Err(Refusal::Closed)));
        let cleanup = obligation
            .submit(cleanup_work)
            .expect("owned cleanup after cutoff");
        cleanup.wait().await;
        assert!(
            first.take().is_none(),
            "a stale transition ticket cannot consume later cleanup"
        );
        assert!(matches!(
            cleanup.take(),
            Some(Outcome::Settlement(Attempt::Unresolved { .. }))
        ));
        drop(obligation);
        assert!(
            cleanup.take().is_none(),
            "taken ticket must not regain abandonment evidence"
        );
        lifecycle.wait_idle().await;

        let (_dir, jobs, mut obligation, pending) = prepared_obligation().await;
        jobs.inner.lifecycle.begin_quiesce(Duration::from_secs(1));
        let rejected = obligation.submit(Work::Authorize(pending)).err().unwrap();
        assert_eq!(rejected.reason, Refusal::Closed);
        let Work::Authorize(pending) = rejected.work else {
            panic!("unchanged owner")
        };
        let cleanup = obligation
            .submit(Work::Close(pending))
            .expect("refused authorization must still allow safe closure");
        cleanup.wait().await;
        assert!(matches!(
            cleanup.take(),
            Some(Outcome::Dispatch(DispatchAttempt::Closed { .. }))
        ));
        tokio::time::timeout(Duration::from_millis(100), cleanup.wait())
            .await
            .expect("consumed ticket is ready without another transition");
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // Deliberately inject a busy ledger in the blocking worker.
    async fn recovery_retries_the_first_retained_terminal_slot() {
        let (_dir, jobs, mut obligation, pending) = prepared_obligation().await;
        let ticket = obligation.submit(Work::Authorize(pending)).unwrap();
        ticket.wait().await;
        let Some(Outcome::Dispatch(DispatchAttempt::Authorized(execution))) = ticket.take() else {
            panic!("authorization");
        };
        let LedgerAccess::Shared(ledger) = &jobs.ledger else {
            panic!("fixture");
        };
        let guard = ledger.lock().unwrap();
        let ticket = obligation
            .submit(Work::Settle(execution.into_settlement(UsageV2 {
                tokens_in: 3,
                completeness: UsageCompleteness::Final,
                ..Default::default()
            })))
            .unwrap();
        ticket.wait().await;
        assert!(matches!(
            ticket.take(),
            Some(Outcome::Settlement(Attempt::Unresolved { .. }))
        ));
        drop(guard);
        drop(obligation);
        assert_eq!(jobs.retained(), 1);
        let mut cursor = None;
        jobs.retry_terminal(&mut cursor);
        assert_eq!(jobs.retained(), 0);
    }

    async fn prepared_obligation() -> (tempfile::TempDir, Jobs, Obligation, PendingDispatch) {
        let dir = tempfile::tempdir().unwrap();
        let ledger =
            ProxyLedger::durable(dir.path().join("owned.db").to_str().unwrap(), 1).unwrap();
        let jobs = Jobs::new(Arc::new(Mutex::new(ledger)), Arc::new(Lifecycle::new()), 1);
        let mut obligation = jobs.obligation().unwrap();
        let ticket = obligation.admit(admission()).unwrap();
        ticket.wait().await;
        let Some(AdmissionOutcome::Prepared(pending)) = ticket.take() else {
            panic!("durable admission")
        };
        (dir, jobs, obligation, pending)
    }

    #[allow(clippy::await_holding_lock)] // Synchronize after real lock contention, never by timing alone.
    async fn run_contended(jobs: &Jobs, input: Input, cutoff: bool) -> ResultValue {
        let LedgerAccess::Shared(ledger) = &jobs.ledger else {
            panic!("fixture")
        };
        let guard = ledger.lock().unwrap();
        let ledger = ledger.clone();
        let lifecycle = jobs.inner.lifecycle.clone();
        let (tx, rx) = tokio::sync::oneshot::channel();
        let worker = tokio::task::spawn_blocking(move || {
            let mut signal = Some(tx);
            run_owned(
                input,
                &lifecycle,
                Some(std::time::Instant::now() + std::time::Duration::from_secs(1)),
                |input| {
                    let outcome = match input {
                        Input::Work(work) => ResultValue::Work(work.run(&ledger)),
                        Input::Admission(input) => {
                            ResultValue::Admission(input.run(&ledger, &lifecycle))
                        }
                    };
                    if let Some(signal) = signal.take() {
                        let _ = signal.send(());
                    }
                    outcome
                },
            )
        });
        tokio::time::timeout(std::time::Duration::from_secs(2), rx)
            .await
            .unwrap()
            .unwrap();
        if cutoff {
            jobs.inner
                .lifecycle
                .begin_quiesce(std::time::Duration::from_secs(1));
            let result = worker.await.unwrap();
            drop(guard);
            result
        } else {
            drop(guard);
            worker.await.unwrap()
        }
    }

    #[tokio::test]
    async fn owned_accounting_busy_admission_and_dispatch_preserve_one_transition() {
        for close in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let jobs = Jobs::new(
                Arc::new(Mutex::new(
                    ProxyLedger::durable(dir.path().join("owned.db").to_str().unwrap(), 1).unwrap(),
                )),
                Arc::new(Lifecycle::new()),
                1,
            );
            let ResultValue::Admission(AdmissionOutcome::Prepared(pending)) =
                run_contended(&jobs, Input::Admission(admission()), false).await
            else {
                panic!("one admission")
            };
            let original = pending.intent.clone();
            let request = pending.request_id.clone();
            let action = if close {
                Work::Close(pending)
            } else {
                Work::Authorize(pending)
            };
            let result = run_contended(&jobs, Input::Work(action), false).await;
            let LedgerAccess::Shared(ledger) = &jobs.ledger else {
                panic!("fixture")
            };
            if close {
                assert!(matches!(
                    result,
                    ResultValue::Work(Outcome::Dispatch(DispatchAttempt::Closed { .. }))
                ));
                assert!(matches!(
                    PendingDispatch::new(request, original).try_authorize(ledger),
                    DispatchAttempt::Closed { .. }
                ));
            } else {
                assert!(matches!(
                    result,
                    ResultValue::Work(Outcome::Dispatch(DispatchAttempt::Authorized(_)))
                ));
                assert!(matches!(
                    PendingDispatch::new(request, original).try_authorize(ledger),
                    DispatchAttempt::MayHaveDispatched { .. }
                ));
            }
            let connection = rusqlite::Connection::open(dir.path().join("owned.db")).unwrap();
            assert_eq!(
                connection
                    .query_row("SELECT count(*) FROM budget_execution_intent", [], |r| r
                        .get::<_, i64>(0))
                    .unwrap(),
                1
            );
        }
    }

    #[tokio::test]
    async fn owned_accounting_busy_dispatch_never_retries_across_cutoff() {
        let (dir, jobs, _obligation, pending) = prepared_obligation().await;
        let outcome = run_contended(&jobs, Input::Work(Work::Authorize(pending)), true).await;
        assert!(matches!(
            outcome,
            ResultValue::Work(Outcome::Dispatch(DispatchAttempt::Unresolved {
                failure: Failure::LedgerBusy,
                ..
            }))
        ));
        let connection = rusqlite::Connection::open(dir.path().join("owned.db")).unwrap();
        assert_eq!(
            connection
                .query_row("SELECT phase FROM budget_dispatch_fence", [], |r| r
                    .get::<_, i64>(0))
                .unwrap(),
            0
        );
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // Deliberately prove contention before releasing the ledger.
    async fn owned_accounting_retries_only_known_busy_within_one_budget() {
        for expire in [false, true] {
            let (_dir, jobs, mut obligation, pending) = prepared_obligation().await;
            let ticket = obligation.submit(Work::Authorize(pending)).unwrap();
            ticket.wait().await;
            let Some(Outcome::Dispatch(DispatchAttempt::Authorized(execution))) = ticket.take()
            else {
                panic!("permit")
            };
            let expected_id = execution.intent().reservation.id;
            let pending = execution.into_settlement(UsageV2 {
                tokens_in: 3,
                completeness: UsageCompleteness::Final,
                ..Default::default()
            });
            let LedgerAccess::Shared(ledger) = &jobs.ledger else {
                panic!("fixture")
            };
            let guard = ledger.lock().unwrap();
            let worker_ledger = ledger.clone();
            let lifecycle = jobs.inner.lifecycle.clone();
            let (tx, rx) = tokio::sync::oneshot::channel();
            let worker = tokio::task::spawn_blocking(move || {
                let mut signal = Some(tx);
                let until = std::time::Instant::now()
                    + std::time::Duration::from_millis(if expire { 20 } else { 1000 });
                run_owned(
                    Input::Work(Work::Settle(pending)),
                    &lifecycle,
                    Some(until),
                    |input| {
                        let Input::Work(work) = input else {
                            panic!("settlement")
                        };
                        let result = ResultValue::Work(work.run(&worker_ledger));
                        if matches!(
                            &result,
                            ResultValue::Work(Outcome::Settlement(Attempt::Unresolved {
                                failure: Failure::LedgerBusy,
                                ..
                            }))
                        ) {
                            if let Some(signal) = signal.take() {
                                let _ = signal.send(());
                            }
                        }
                        result
                    },
                )
            });
            // This signal comes AFTER a real try_commit failed before acquiring
            // the held mutex, so the success case cannot pass without a retry.
            rx.await.unwrap();
            if expire {
                let ResultValue::Work(Outcome::Settlement(Attempt::Unresolved {
                    pending,
                    failure: Failure::LedgerBusy,
                })) = worker.await.unwrap()
                else {
                    panic!("budget must expire")
                };
                assert_eq!(pending.reservation().unwrap().id, expected_id);
                assert_eq!(pending.terminal_usage().unwrap().tokens_in, 3);
                assert_eq!(pending.observation_recorded(), Some(false));
                drop(guard);
                assert!(matches!(
                    pending.try_commit(ledger),
                    Attempt::Committed { .. }
                ));
            } else {
                drop(guard);
                let ResultValue::Work(Outcome::Settlement(Attempt::Committed { outcome, .. })) =
                    worker.await.unwrap()
                else {
                    panic!("unchanged accounting must commit after release")
                };
                let sandhi_store::ledger::evidence::SettlementOutcome::Committed(receipt) = outcome
                else {
                    panic!("first receipt")
                };
                assert_eq!(receipt.reservation_id, expected_id);
                assert_eq!(receipt.charged_tokens, 3);
            }
        }
    }

    #[tokio::test]
    async fn owned_accounting_does_not_loop_storage_failure() {
        let (dir, jobs, mut obligation, pending) = prepared_obligation().await;
        let ticket = obligation.submit(Work::Authorize(pending)).unwrap();
        ticket.wait().await;
        let Some(Outcome::Dispatch(DispatchAttempt::Authorized(execution))) = ticket.take() else {
            panic!("permit")
        };
        let connection = rusqlite::Connection::open(dir.path().join("owned.db")).unwrap();
        connection.execute_batch("CREATE TRIGGER fail_receipt BEFORE INSERT ON budget_settlement_outbox BEGIN SELECT RAISE(ABORT,'fixture'); END").unwrap();
        let pending = execution.into_settlement(UsageV2 {
            tokens_in: 3,
            completeness: UsageCompleteness::Final,
            ..Default::default()
        });
        let mut calls = 0;
        let outcome = run_owned(
            Input::Work(Work::Settle(pending)),
            &jobs.inner.lifecycle,
            Some(std::time::Instant::now() + std::time::Duration::from_secs(1)),
            |input| {
                calls += 1;
                let Input::Work(work) = input else {
                    panic!("settlement")
                };
                ResultValue::Work(jobs.ledger.with(|ledger| work.run(ledger)).unwrap())
            },
        );
        assert_eq!(calls, 1);
        let ResultValue::Work(Outcome::Settlement(Attempt::Unresolved {
            pending,
            failure: Failure::Evidence(_),
        })) = outcome
        else {
            panic!("retain storage uncertainty")
        };
        assert_eq!(pending.observation_recorded(), Some(true));
        assert_eq!(
            connection
                .query_row("SELECT count(*) FROM budget_settlement_outbox", [], |r| r
                    .get::<_, i64>(
                    0
                ))
                .unwrap(),
            0
        );
    }

    #[tokio::test]
    async fn http_obligation_rejects_second_admission_and_wrong_binding() {
        let (_dir, jobs, mut obligation, pending) = prepared_obligation().await;
        assert_eq!(
            obligation
                .admit(admission())
                .err()
                .expect("one admission only")
                .reason,
            Refusal::Closed
        );
        let mut wrong = pending.intent.clone();
        wrong.reservation.scope = "other".into();
        assert!(obligation
            .submit(Work::Authorize(PendingDispatch::new(
                pending.request_id.clone(),
                wrong
            )))
            .is_err());
        let ticket = obligation.submit(Work::Close(pending)).unwrap();
        ticket.wait().await;
        assert!(matches!(
            ticket.take(),
            Some(Outcome::Dispatch(DispatchAttempt::Closed { .. }))
        ));
        drop(obligation);
        assert!(jobs.take_ready().is_none());
    }

    #[tokio::test]
    async fn http_terminal_without_runtime_survives_refusal_and_downgrade() {
        let (_dir, jobs, mut obligation, pending) = prepared_obligation().await;
        let ticket = obligation.submit(Work::Authorize(pending)).unwrap();
        ticket.wait().await;
        let Some(Outcome::Dispatch(DispatchAttempt::Authorized(authorized))) = ticket.take() else {
            panic!("owned authorization")
        };
        let downgrade = Work::Close(PendingDispatch::new(
            authorized.request_id.clone(),
            authorized.intent.clone(),
        ));
        let usage = UsageV2 {
            tokens_in: 11,
            tokens_out: 7,
            completeness: UsageCompleteness::Final,
            basis: UsageBasis::ProviderReported,
            ..Default::default()
        };
        let pending = authorized.into_settlement(usage.clone());
        let mut obligation = std::thread::spawn(move || {
            let rejected = obligation.submit(Work::Settle(pending)).err().unwrap();
            assert_eq!(rejected.reason, Refusal::NoRuntime);
            drop(rejected);
            obligation
        })
        .join()
        .unwrap();
        assert!(
            obligation.submit(downgrade).is_err(),
            "terminal evidence cannot be replaced by Close"
        );
        drop(obligation);
        let Some(Outcome::Interrupted(Work::Settle(retained))) = jobs.take_ready() else {
            panic!("terminal usage must remain owned without a runtime")
        };
        assert_eq!(retained.terminal_usage(), Some(&usage));
    }

    fn work() -> Work {
        Work::Settle(PendingSettlement::tracked(
            "request".into(),
            ExecutionIntent {
                execution_id: "a".repeat(64),
                reservation: Reservation {
                    id: 1,
                    scope: "scope".into(),
                    ceiling: 100,
                    expires_at: OffsetDateTime::now_utc(),
                },
            },
            UsageV2 {
                tokens_in: 11,
                tokens_out: 7,
                cache_read_tokens: 3,
                completeness: UsageCompleteness::Final,
                basis: UsageBasis::ProviderReported,
                ..Default::default()
            },
        ))
    }
    fn admission() -> PreparedAdmission {
        PreparedAdmission::new(
            "admission".into(),
            "scope".into(),
            100,
            time::Duration::seconds(30),
            10,
        )
        .unwrap()
    }

    #[tokio::test]
    async fn admission_and_work_share_capacity_without_cross_kind_consumption() {
        let (jobs, lifecycle) = jobs(2);
        let admitted = jobs.submit_admission(admission()).unwrap();
        admitted.wait().await;
        // A completed unclaimed result still occupies its slot. Serialize the
        // transitions so an unrelated legitimate LedgerBusy cannot race this assertion.
        let existing = jobs.submit(work()).unwrap();
        existing.wait().await;
        assert_eq!(jobs.submit(work()).err().unwrap().reason, Refusal::Full);
        assert_eq!(
            jobs.submit_admission(admission()).err().unwrap().reason,
            Refusal::Full
        );
        assert!(jobs.take_ready().is_some());
        assert!(jobs.take_ready().is_none());
        assert!(existing.take().is_none());
        let Some(AdmissionOutcome::Failed {
            evidence,
            failure: super::super::Failure::NonDurableLedger,
        }) = admitted.take()
        else {
            panic!("admission result must stay owned by its own consumer")
        };
        assert_eq!(evidence.request_id, "admission");
        assert!(admitted.take().is_none());
        // Reverse ordering: the admission collector cannot take an existing Work.
        let existing = jobs.submit(work()).unwrap();
        existing.wait().await;
        let admitted = jobs.submit_admission(admission()).unwrap();
        admitted.wait().await;
        let winners = std::thread::scope(|s| {
            let a = s.spawn(|| usize::from(admitted.take().is_some()));
            let b = s.spawn(|| usize::from(jobs.take_admission_ready().is_some()));
            a.join().unwrap() + b.join().unwrap()
        });
        assert_eq!(winners, 1);
        assert!(jobs.take_admission_ready().is_none());
        assert!(existing.take().is_some());
        lifecycle.wait_idle().await;
    }

    #[test]
    fn admission_without_runtime_and_oversized_metadata_fail_explicitly() {
        let (jobs, _) = jobs(1);
        let rejected = jobs.submit_admission(admission()).err().unwrap();
        assert_eq!(rejected.reason, Refusal::NoRuntime);
        assert_eq!(rejected.input.evidence().request_id, "admission");
        for (request, scope) in [
            ("x".repeat(4097), "scope".into()),
            ("request".into(), "s".repeat(4097)),
        ] {
            assert!(
                PreparedAdmission::new(request, scope, 100, time::Duration::seconds(30), 10)
                    .is_err()
            );
        }
    }

    #[tokio::test]
    async fn queued_admission_observes_cutoff_before_calling_storage() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("queued.db");
        let path = path.to_str().unwrap();
        let lifecycle = Arc::new(Lifecycle::new());
        let jobs = Jobs::new(
            Arc::new(Mutex::new(ProxyLedger::durable(path, 1).unwrap())),
            lifecycle.clone(),
            1,
        );
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let ledger = jobs.ledger.clone();
        let worker_lifecycle = lifecycle.clone();
        let ticket = jobs
            .submit_admission_with(admission(), move |input| {
                started_tx.send(()).unwrap();
                release_rx.recv().unwrap();
                ledger
                    .with(|ledger| input.run(ledger, &worker_lifecycle))
                    .expect("owned proxy state")
            })
            .unwrap();
        started_rx.await.unwrap();
        lifecycle.begin_quiesce(Duration::ZERO);
        assert!(jobs.drain().await.unwrap().timed_out);
        release_tx.send(()).unwrap();
        ticket.wait().await;
        assert!(matches!(ticket.take(), Some(AdmissionOutcome::Cutoff(_))));
        let mut inspector = sandhi_store::ledger::SqliteLedger::open(path).unwrap();
        assert_eq!(inspector.reserved_durable("scope").unwrap(), 0);
        assert!(inspector
            .recovery_page_durable("scope", None, 10)
            .unwrap()
            .entries
            .is_empty());
    }
    fn jobs(capacity: usize) -> (Jobs, Arc<Lifecycle>) {
        let lifecycle = Arc::new(Lifecycle::new());
        (
            Jobs::new(
                Arc::new(Mutex::new(ProxyLedger::in_memory())),
                lifecycle.clone(),
                capacity,
            ),
            lifecycle,
        )
    }
    fn assert_input(work: &Work) {
        let Work::Settle(pending) = work else {
            panic!("original settlement")
        };
        assert_eq!(pending.request_id(), "request");
        assert_eq!(pending.terminal_usage().unwrap().tokens_in, 11);
        assert_eq!(pending.terminal_usage().unwrap().tokens_out, 7);
        assert_eq!(pending.terminal_usage().unwrap().cache_read_tokens, 3);
    }

    #[tokio::test]
    async fn cancelled_waiter_retains_running_and_completed_slots() {
        let (jobs, lifecycle) = jobs(1);
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let ticket = jobs
            .submit_with(work(), move |work| {
                started_tx.send(()).unwrap();
                release_rx.recv().unwrap();
                let Work::Settle(pending) = work else {
                    unreachable!()
                };
                Outcome::Settlement(pending.try_commit(&Mutex::new(ProxyLedger::in_memory())))
            })
            .unwrap();
        started_rx.await.unwrap();
        let waiter = tokio::spawn(async move { ticket.wait().await });
        waiter.abort();
        assert!(waiter.await.unwrap_err().is_cancelled());
        let rejected = jobs.submit(work()).err().unwrap();
        assert_eq!(rejected.reason, Refusal::Full);
        assert_input(&rejected.work);
        assert_eq!(lifecycle.active_operations(), 1);
        release_tx.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(2), lifecycle.wait_idle())
            .await
            .unwrap();
        assert_eq!(jobs.submit(work()).err().unwrap().reason, Refusal::Full);
        let Some(Outcome::Settlement(Attempt::Unresolved { pending, .. })) = jobs.take_ready()
        else {
            panic!("retained result")
        };
        assert_eq!(pending.terminal_usage().unwrap().tokens_out, 7);
        assert!(jobs.take_ready().is_none());
        let ticket = jobs.submit(work()).unwrap();
        ticket.wait().await;
        assert!(ticket.take().is_some());
        assert!(ticket.take().is_none());
    }

    #[tokio::test]
    async fn panic_retains_full_input_and_closes_admission() {
        let (jobs, lifecycle) = jobs(1);
        let ticket = jobs
            .submit_with(work(), |work| {
                drop(work); // Consuming code has lost its original owner before unwinding.
                panic!("injected worker failure")
            })
            .unwrap();
        ticket.wait().await;
        let Some(Outcome::Interrupted(input)) = ticket.take() else {
            panic!("retained input")
        };
        assert_input(&input);
        let rejected = jobs.submit(work()).err().unwrap();
        assert_eq!(rejected.reason, Refusal::Closed);
        assert_input(&rejected.work);
        lifecycle.wait_idle().await;
    }

    #[tokio::test]
    async fn shutdown_uses_original_deadline_and_reports_blocked_work() {
        let (jobs, lifecycle) = jobs(1);
        assert!(jobs.drain().await.is_none());
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let ticket = jobs
            .submit_with(work(), move |work| {
                started_tx.send(()).unwrap();
                release_rx.recv().unwrap();
                Outcome::Interrupted(work)
            })
            .unwrap();
        started_rx.await.unwrap();
        let deadline = lifecycle.begin_quiesce(Duration::ZERO);
        assert_eq!(deadline, lifecycle.begin_quiesce(Duration::from_secs(30)));
        assert_eq!(jobs.submit(work()).err().unwrap().reason, Refusal::Closed);
        let report = jobs.drain().await.unwrap();
        assert!(report.timed_out);
        assert_eq!((report.active, report.retained), (1, 1));
        assert!(ticket.take().is_none());
        release_tx.send(()).unwrap();
        lifecycle.wait_idle().await;
        let report = jobs.drain().await.unwrap();
        assert!(!report.timed_out);
        assert_eq!((report.active, report.retained), (0, 1));
        assert!(ticket.take().is_some());
    }

    #[test]
    fn admission_without_runtime_preserves_input() {
        let (jobs, _) = jobs(1);
        let rejected = jobs.submit(work()).err().unwrap();
        assert_eq!(rejected.reason, Refusal::NoRuntime);
        assert_input(&rejected.work);
    }
    #[tokio::test]
    async fn competing_ticket_and_manager_transfer_once() {
        for _ in 0..32 {
            let (jobs, _) = jobs(2);
            let first = jobs.submit(work()).unwrap();
            let second = jobs.submit(work()).unwrap();
            first.wait().await;
            second.wait().await;
            let winners = std::thread::scope(|scope| {
                let ticket = scope.spawn(|| usize::from(first.take().is_some()));
                let manager = scope.spawn(|| {
                    let mut count = 0;
                    while jobs.take_ready().is_some() {
                        count += 1;
                    }
                    count
                });
                ticket.join().unwrap() + manager.join().unwrap()
            });
            assert_eq!(winners, 2); // No false-empty while a second ready result exists.
            first.wait().await;
            assert!(second.take().is_none());
        }
    }

    #[test]
    fn queued_job_cancellation_retains_input_before_idle() {
        for admission_kind in [false, true] {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .max_blocking_threads(1)
                .enable_all()
                .build()
                .unwrap();
            let (started_tx, started_rx) = std::sync::mpsc::channel();
            let (release_tx, release_rx) = std::sync::mpsc::channel();
            runtime.spawn_blocking(move || {
                started_tx.send(()).unwrap();
                release_rx.recv().unwrap();
            });
            started_rx.recv_timeout(Duration::from_secs(2)).unwrap();
            let (jobs, lifecycle) = jobs(1);
            let executed = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let ticket = {
                let _entered = runtime.enter();
                let executed = executed.clone();
                jobs.submit_input(
                    if admission_kind {
                        Input::Admission(admission())
                    } else {
                        Input::Work(work())
                    },
                    move |input| {
                        executed.store(true, std::sync::atomic::Ordering::SeqCst);
                        input.interrupted()
                    },
                )
                .unwrap_or_else(|_| panic!("queued input admitted"))
            };
            assert_eq!(lifecycle.active_operations(), 1);
            // Runtime shutdown alone drains queued blocking work; explicitly cancel a
            // job before it starts to exercise the closure-owned completion guard.
            ticket.abort.abort();
            runtime.shutdown_background();
            release_tx.send(()).unwrap();
            // A separate runtime observes cleanup after the submission runtime is gone.
            let observer = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            observer.block_on(async {
                tokio::time::timeout(Duration::from_secs(2), lifecycle.wait_idle())
                    .await
                    .unwrap();
                let timing = ticket.timing.snapshot();
                assert_eq!(timing.stage, "published");
                assert!(timing.interrupted);
                assert!(timing.run_us.is_none());
                assert!(timing.publication_us.is_some());
                let kind = if admission_kind {
                    Kind::Admission
                } else {
                    Kind::Work
                };
                match ticket.inner.take(ticket.id, ticket.epoch, kind) {
                    Some(ResultValue::Work(Outcome::Interrupted(input))) => assert_input(&input),
                    Some(ResultValue::Admission(AdmissionOutcome::Interrupted(evidence))) => {
                        assert_eq!(evidence.request_id, "admission");
                        assert_eq!(evidence.scope, "scope");
                    }
                    _ => panic!("retained queued input"),
                }
                assert!(!executed.load(std::sync::atomic::Ordering::SeqCst));
                assert_eq!(jobs.submit(work()).err().unwrap().reason, Refusal::Closed);
            });
        }
    }
}
