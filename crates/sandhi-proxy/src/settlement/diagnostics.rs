//! Bounded process-local observations, never accounting or dispatch evidence.
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

#[derive(Clone, Copy)]
pub(super) enum Stage {
    Call = 1,
    OuterTryLock,
    LedgerReady,
    ReserveCall,
    AuthorizeCall,
    CloseCall,
    BackLegLookupCall,
    RecordTerminalCall,
    CompatibilityBackLegCall,
    SettleReceiptCall,
    LegacySettleCall,
    Returned,
    Backoff,
}
impl Stage {
    fn label(value: u64) -> &'static str {
        match value {
            1 => "work_call",
            2 => "outer_try_lock",
            3 => "ledger_ready",
            4 => "reserve_call",
            5 => "authorize_call",
            6 => "close_call",
            7 => "back_leg_lookup_call",
            8 => "record_terminal_call",
            9 => "compatibility_back_leg_call",
            10 => "settle_receipt_call",
            11 => "legacy_settle_call",
            12 => "work_returned",
            13 => "known_busy_backoff",
            _ => "unobserved",
        }
    }
}
pub(super) struct Progress {
    origin: Instant,
    stage: AtomicU64,
    stage_at: AtomicU64,
    calls: AtomicU64,
    busy_returns: AtomicU64,
    backoff_us: AtomicU64,
    backoff_started: AtomicU64,
}
pub(super) struct Snapshot {
    pub stage: &'static str,
    pub stage_us: Option<u64>,
    pub calls: u64,
    pub busy_returns: u64,
    pub backoff_us: u64,
}
fn increment(value: &AtomicU64, extra: u64) {
    let _ = value.try_update(Ordering::AcqRel, Ordering::Acquire, |prior| {
        Some(prior.saturating_add(extra))
    });
}
impl Progress {
    pub(super) fn new(origin: Instant) -> Self {
        Self {
            origin,
            stage: AtomicU64::new(0),
            stage_at: AtomicU64::new(0),
            calls: AtomicU64::new(0),
            busy_returns: AtomicU64::new(0),
            backoff_us: AtomicU64::new(0),
            backoff_started: AtomicU64::new(0),
        }
    }
    fn elapsed(&self) -> u64 {
        self.origin
            .elapsed()
            .as_micros()
            .min(u128::from(u64::MAX - 1)) as u64
    }
    pub(super) fn mark(&self, stage: Stage) {
        self.stage_at.store(self.elapsed() + 1, Ordering::Release);
        self.stage.store(stage as u64, Ordering::Release);
    }
    pub(super) fn call(&self) {
        increment(&self.calls, 1);
        self.mark(Stage::Call);
    }
    pub(super) fn returned(&self, busy: bool) {
        if busy {
            increment(&self.busy_returns, 1);
        }
        self.mark(Stage::Returned);
    }
    pub(super) fn begin_backoff(&self) {
        self.backoff_started
            .store(self.elapsed() + 1, Ordering::Release);
        self.mark(Stage::Backoff);
    }
    pub(super) fn end_backoff(&self) {
        if let Some(start) = self
            .backoff_started
            .swap(0, Ordering::AcqRel)
            .checked_sub(1)
        {
            increment(&self.backoff_us, self.elapsed().saturating_sub(start));
        }
        self.mark(Stage::Returned);
    }
    pub(super) fn snapshot(&self) -> Snapshot {
        let stage = self.stage.load(Ordering::Acquire);
        let at = self.stage_at.load(Ordering::Acquire).checked_sub(1);
        let completed_backoff = self.backoff_us.load(Ordering::Acquire);
        let backoff = self.backoff_started.load(Ordering::Acquire).checked_sub(1);
        let elapsed = self.elapsed();
        let busy_returns = self.busy_returns.load(Ordering::Acquire);
        let calls = self.calls.load(Ordering::Acquire);
        Snapshot {
            stage: Stage::label(stage),
            stage_us: at.map(|at| elapsed.saturating_sub(at)),
            calls,
            busy_returns,
            backoff_us: completed_backoff
                .saturating_add(backoff.map_or(0, |start| elapsed.saturating_sub(start))),
        }
    }
}
pub(super) fn mark(progress: Option<&Progress>, stage: Stage) {
    if let Some(progress) = progress {
        progress.mark(stage);
    }
}
