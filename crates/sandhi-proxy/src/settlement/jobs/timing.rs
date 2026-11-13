//! Process-local diagnostics for one existing job. Never authorization evidence.
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Instant;

pub(super) struct Timing {
    submitted: Instant,
    pub(super) progress: super::super::diagnostics::Progress,
    started: AtomicU64,
    returned: AtomicU64,
    published: AtomicU64,
    interrupted: AtomicBool,
}

#[derive(Debug)]
pub(super) struct Snapshot {
    pub stage: &'static str,
    pub elapsed_us: u64,
    pub queue_us: u64,
    pub run_us: Option<u64>,
    pub publication_us: Option<u64>,
    pub published_at_us: Option<u64>,
    pub interrupted: bool,
}

impl Timing {
    pub(super) fn offset(&self, instant: Instant) -> Option<u64> {
        instant
            .checked_duration_since(self.submitted)
            .map(|elapsed| elapsed.as_micros().min(u128::from(u64::MAX - 1)) as u64)
    }
    pub(super) fn new() -> Self {
        let submitted = Instant::now();
        Self {
            submitted,
            progress: super::super::diagnostics::Progress::new(submitted),
            started: AtomicU64::new(0),
            returned: AtomicU64::new(0),
            published: AtomicU64::new(0),
            interrupted: AtomicBool::new(false),
        }
    }
    fn elapsed(&self) -> u64 {
        self.submitted
            .elapsed()
            .as_micros()
            .min(u128::from(u64::MAX - 1)) as u64
    }
    fn mark(&self, field: &AtomicU64) {
        // Zero is absent; timestamps are relative to this transition only.
        field.store(self.elapsed() + 1, Ordering::Release);
    }
    pub(super) fn start(&self) {
        self.mark(&self.started);
    }
    pub(super) fn returned(&self) {
        self.mark(&self.returned);
    }
    pub(super) fn published(&self) {
        self.mark(&self.published);
    }
    pub(super) fn interrupted(&self) {
        self.interrupted.store(true, Ordering::Release);
        self.returned();
    }
    pub(super) fn snapshot(&self) -> Snapshot {
        let read = |field: &AtomicU64| field.load(Ordering::Acquire).checked_sub(1);
        // Read terminal marks first: observing publication also observes all
        // preceding worker marks. This is diagnostic, not an atomic store view.
        let published = read(&self.published);
        let returned = read(&self.returned);
        let started = read(&self.started);
        let elapsed_us = self.elapsed();
        Snapshot {
            stage: if published.is_some() {
                "published"
            } else if returned.is_some() {
                "publishing"
            } else if started.is_some() {
                "running"
            } else {
                "queued"
            },
            elapsed_us,
            queue_us: started.or(returned).unwrap_or(elapsed_us),
            run_us: started.map(|start| returned.unwrap_or(elapsed_us).saturating_sub(start)),
            publication_us: returned.map(|end| published.unwrap_or(elapsed_us).saturating_sub(end)),
            published_at_us: published,
            interrupted: self.interrupted.load(Ordering::Acquire),
        }
    }
}
