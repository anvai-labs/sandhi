//! Process-local dispatch authorization and shutdown progress. No provider/ledger health policy.
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::watch;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Running,
    Quiescing,
    Draining,
    Stopped,
}

#[derive(Debug)]
struct State {
    phase: Phase,
    started: Option<Instant>,
    deadline: Option<Instant>,
    stopped: Option<Instant>,
    active: usize,
}

/// A single-use lifecycle shared by the listener, request admission and binary coordinator.
/// Cutoff and operation admission use the same lock. Already-authorized operations may finish;
/// this is not a claim that no previously authorized network bytes can leave after cutoff.
#[derive(Debug)]
pub struct Lifecycle {
    state: Mutex<State>,
    changed: watch::Sender<Phase>,
}

impl Default for Lifecycle {
    fn default() -> Self {
        Self::new()
    }
}

impl Lifecycle {
    #[must_use]
    pub fn new() -> Self {
        Self {
            state: Mutex::new(State {
                phase: Phase::Running,
                started: None,
                deadline: None,
                stopped: None,
                active: 0,
            }),
            changed: watch::channel(Phase::Running).0,
        }
    }

    #[must_use]
    pub fn is_running(&self) -> bool {
        self.phase() == Phase::Running
    }

    #[must_use]
    pub fn phase(&self) -> Phase {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).phase
    }

    /// Establish the immutable monotonic shutdown deadline once. Repeated calls never extend it.
    pub fn begin_quiesce(&self, grace: Duration) -> Instant {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(deadline) = state.deadline {
            return deadline;
        }
        let now = Instant::now();
        // A pathological embedder duration must not panic or disable bounded shutdown.
        let deadline = now.checked_add(grace).unwrap_or(now);
        state.started = Some(now);
        state.deadline = Some(deadline);
        state.phase = Phase::Quiescing;
        self.changed.send_replace(state.phase);
        deadline
    }

    pub fn start_draining(&self) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.phase == Phase::Quiescing {
            state.phase = Phase::Draining;
            self.changed.send_replace(state.phase);
        }
    }

    /// Mark completed cleanup; callers must not call this while admitted work remains.
    pub fn stop(&self) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.phase == Phase::Running || state.active != 0 {
            return;
        }
        state.phase = Phase::Stopped;
        state.stopped.get_or_insert_with(Instant::now);
        self.changed.send_replace(state.phase);
    }

    #[must_use]
    pub fn deadline(&self) -> Option<Instant> {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .deadline
    }

    #[must_use]
    pub fn remaining(&self) -> Option<Duration> {
        self.deadline()
            .map(|d| d.saturating_duration_since(Instant::now()))
    }

    #[must_use]
    pub fn elapsed(&self) -> Duration {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.started.map_or(Duration::ZERO, |start| {
            state
                .stopped
                .unwrap_or_else(Instant::now)
                .saturating_duration_since(start)
        })
    }

    #[must_use]
    pub fn active_operations(&self) -> usize {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).active
    }

    /// Authorize one operation at the cutoff boundary. Move the guard into detached work,
    /// never leave it in the awaiting HTTP future when that work survives cancellation.
    #[must_use]
    pub fn try_operation(self: &Arc<Self>) -> Option<OperationGuard> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.phase != Phase::Running {
            return None;
        }
        state.active += 1;
        Some(OperationGuard {
            lifecycle: self.clone(),
        })
    }

    /// Wake all queued callers on cutoff, including callers subscribed after it happened.
    pub async fn cancelled(&self) {
        let mut changed = self.changed.subscribe();
        while self.is_running() {
            if changed.changed().await.is_err() {
                return;
            }
        }
    }

    /// Wait for admitted operations, including detached blocking work, to finish cleanup.
    pub async fn wait_idle(&self) {
        let mut changed = self.changed.subscribe();
        while self.active_operations() != 0 {
            if changed.changed().await.is_err() {
                return;
            }
        }
    }
}

#[derive(Debug)]
pub struct OperationGuard {
    lifecycle: Arc<Lifecycle>,
}

impl Drop for OperationGuard {
    fn drop(&mut self) {
        let mut state = self
            .lifecycle
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        state.active -= 1;
        self.lifecycle.changed.send_replace(state.phase);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn cutoff_is_final_and_waiters_observe_owned_work() {
        let lifecycle = Arc::new(Lifecycle::new());
        let operation = lifecycle.try_operation().unwrap();
        let deadline = lifecycle.begin_quiesce(Duration::from_secs(1));
        assert_eq!(deadline, lifecycle.begin_quiesce(Duration::from_secs(30)));
        assert!(lifecycle.try_operation().is_none());
        lifecycle.cancelled().await;
        lifecycle.stop();
        assert_eq!(lifecycle.phase(), Phase::Quiescing);
        assert!(
            tokio::time::timeout(Duration::from_millis(5), lifecycle.wait_idle())
                .await
                .is_err()
        );
        drop(operation);
        lifecycle.wait_idle().await;
        lifecycle.start_draining();
        lifecycle.stop();
        assert_eq!(lifecycle.phase(), Phase::Stopped);
        assert!(lifecycle.remaining().is_some());
    }

    #[tokio::test]
    async fn cutoff_wakes_an_existing_subscriber() {
        let lifecycle = Arc::new(Lifecycle::new());
        let waiter = tokio::spawn({
            let lifecycle = lifecycle.clone();
            async move { lifecycle.cancelled().await }
        });
        tokio::task::yield_now().await;
        lifecycle.begin_quiesce(Duration::ZERO);
        tokio::time::timeout(Duration::from_secs(1), waiter)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(lifecycle.remaining(), Some(Duration::ZERO));
    }

    #[test]
    fn operation_admission_and_cutoff_are_serialized() {
        let lifecycle = Arc::new(Lifecycle::new());
        std::thread::scope(|scope| {
            for _ in 0..8 {
                let lifecycle = lifecycle.clone();
                scope.spawn(move || {
                    for _ in 0..100 {
                        drop(lifecycle.try_operation());
                    }
                });
            }
            lifecycle.begin_quiesce(Duration::from_secs(1));
        });
        assert_eq!(lifecycle.active_operations(), 0);
        assert!(lifecycle.try_operation().is_none());
    }
}
