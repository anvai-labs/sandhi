//! Bounded process-local admission timing. Never durable or authorization evidence.
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

#[derive(Clone, Copy)]
pub(crate) enum AdmissionStage {
    ShardLock = 1,
    ValidateContext,
    ValidateAdmission,
    GenerateIdentity,
    BeginTransaction,
    ReadCapacity,
    ReservePrimary,
    ReserveBackLeg,
    WriteIntent,
    WriteFence,
    WriteCorrelation,
    WriteContext,
    WriteBackLeg,
    Commit,
    Returned,
}

/// One admission worker's diagnostic clock; no identifiers, secrets or counters.
/// Individual atomic reads may straddle a transition. No decisions use this state.
pub struct AdmissionProgress {
    origin: Instant,
    stage: AtomicU64,
    stage_at: AtomicU64,
}

pub struct AdmissionSnapshot {
    pub stage: &'static str,
    pub stage_us: Option<u64>,
}

impl AdmissionProgress {
    pub fn new(origin: Instant) -> Self {
        Self {
            origin,
            stage: AtomicU64::new(0),
            stage_at: AtomicU64::new(0),
        }
    }
    pub fn reset(&self) {
        self.stage.store(0, Ordering::Release);
        self.stage_at.store(0, Ordering::Release);
    }
    fn elapsed(&self) -> u64 {
        self.origin
            .elapsed()
            .as_micros()
            .min(u128::from(u64::MAX - 1)) as u64
    }
    pub(crate) fn mark(&self, stage: AdmissionStage) {
        self.stage_at.store(self.elapsed() + 1, Ordering::Release);
        self.stage.store(stage as u64, Ordering::Release);
    }
    pub fn snapshot(&self) -> AdmissionSnapshot {
        let stage = match self.stage.load(Ordering::Acquire) {
            1 => "shard_lock",
            2 => "validate_context",
            3 => "validate_admission",
            4 => "generate_identity",
            5 => "begin_transaction",
            6 => "read_capacity",
            7 => "reserve_primary",
            8 => "reserve_back_leg",
            9 => "write_intent",
            10 => "write_fence",
            11 => "write_correlation",
            12 => "write_context",
            13 => "write_back_leg",
            14 => "commit",
            15 => "returned",
            _ => "unobserved",
        };
        AdmissionSnapshot {
            stage,
            stage_us: self
                .stage_at
                .load(Ordering::Acquire)
                .checked_sub(1)
                .map(|at| self.elapsed().saturating_sub(at)),
        }
    }
}

pub(crate) fn mark(progress: Option<&AdmissionProgress>, stage: AdmissionStage) {
    if let Some(progress) = progress {
        progress.mark(stage);
    }
}
