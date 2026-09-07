//! Bounded background persistence for best-effort alert dedup state.
//!
//! Budget reserve/settle is intentionally absent: hard-cap enforcement remains synchronous and
//! linearizable. Only the observational `last_fired_at` mirror may leave the request task.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use sandhi_core::sink::BufferSnapshot;
use sandhi_store::AlertStore;

/// Sender-free view of the alert writer's process-local bookkeeping.
#[derive(Clone, Debug)]
pub struct AlertBufferObserver {
    counters: Arc<Mutex<BufferSnapshot>>,
}

impl AlertBufferObserver {
    #[must_use]
    pub fn snapshot(&self) -> BufferSnapshot {
        *self.counters.lock().unwrap_or_else(|e| e.into_inner())
    }
}

enum Message {
    MarkFired(String),
    Flush(mpsc::Sender<()>),
    Shutdown,
}

/// A bounded single-writer queue for `AlertStore::mark_fired` updates.
pub struct BufferedAlertStore {
    sender: SyncSender<Message>,
    observer: AlertBufferObserver,
    closed: AtomicBool,
    worker: Mutex<Option<JoinHandle<()>>>,
}

impl BufferedAlertStore {
    #[must_use]
    pub fn new(store: Arc<AlertStore>, capacity: usize) -> Self {
        Self::with_writer(capacity, move |rule_id| {
            if let Err(error) = store.mark_fired(rule_id) {
                tracing::warn!(%error, %rule_id, "could not persist alert fire");
            }
        })
    }

    fn with_writer(capacity: usize, write: impl Fn(&str) + Send + 'static) -> Self {
        let capacity = capacity.max(1);
        let (sender, receiver) = mpsc::sync_channel(capacity);
        let observer = AlertBufferObserver {
            counters: Arc::new(Mutex::new(BufferSnapshot {
                capacity,
                ..BufferSnapshot::default()
            })),
        };
        let counters = observer.counters.clone();
        let worker = std::thread::Builder::new()
            .name("sandhi-alert-writer".into())
            .spawn(move || {
                while let Ok(message) = receiver.recv() {
                    match message {
                        Message::MarkFired(rule_id) => {
                            {
                                let mut counts = counters.lock().unwrap_or_else(|e| e.into_inner());
                                counts.queued -= 1;
                                counts.in_flight += 1;
                            }
                            let result =
                                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                    write(&rule_id);
                                }));
                            let mut counts = counters.lock().unwrap_or_else(|e| e.into_inner());
                            counts.in_flight -= 1;
                            if let Err(panic) = result {
                                counts.dropped += counts.queued as u64;
                                counts.queued = 0;
                                drop(receiver);
                                drop(counts);
                                std::panic::resume_unwind(panic);
                            }
                        }
                        Message::Flush(ack) => {
                            let _ = ack.send(());
                        }
                        Message::Shutdown => break,
                    }
                }
            })
            .expect("spawn alert writer");
        Self {
            sender,
            observer,
            closed: AtomicBool::new(false),
            worker: Mutex::new(Some(worker)),
        }
    }

    /// Queue a durable dedup update without blocking the response task.
    pub fn mark_fired(&self, rule_id: String) {
        let mut counts = self
            .observer
            .counters
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if self.closed.load(Ordering::Acquire) || counts.queued >= counts.capacity {
            counts.dropped += 1;
            let dropped = counts.dropped;
            drop(counts);
            Self::record_drop(dropped);
            return;
        }
        match self.sender.try_send(Message::MarkFired(rule_id)) {
            Ok(()) => counts.queued += 1,
            Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => {
                counts.dropped += 1;
                let dropped = counts.dropped;
                drop(counts);
                Self::record_drop(dropped);
            }
        }
    }

    #[must_use]
    pub fn dropped_updates(&self) -> u64 {
        self.snapshot().dropped
    }

    #[must_use]
    pub fn observer(&self) -> AlertBufferObserver {
        self.observer.clone()
    }

    /// Callback completion is not proof of a committed update: the rule may be missing,
    /// or SQLite may have rejected the write. This snapshot reports buffer activity only.
    #[must_use]
    pub fn snapshot(&self) -> BufferSnapshot {
        self.observer.snapshot()
    }

    /// Drain accepted updates and stop the writer before the supplied deadline.
    pub fn close(&self, timeout: Duration) -> bool {
        let already_closed = {
            let _counts = self
                .observer
                .counters
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            self.closed.swap(true, Ordering::AcqRel)
        };
        if already_closed {
            return self
                .worker
                .lock()
                .map(|worker| worker.is_none())
                .unwrap_or(false);
        }
        let deadline = Instant::now() + timeout;
        let (ack_tx, ack_rx) = mpsc::channel();
        if !self.send_until(Message::Flush(ack_tx), deadline) {
            return false;
        }
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            return false;
        };
        if ack_rx.recv_timeout(remaining).is_err() {
            return false;
        }
        if !self.send_until(Message::Shutdown, deadline) {
            return false;
        }
        let worker = self.worker.lock().ok().and_then(|mut worker| worker.take());
        match worker {
            Some(worker) => worker.join().is_ok(),
            None => true,
        }
    }

    fn send_until(&self, mut message: Message, deadline: Instant) -> bool {
        loop {
            match self.sender.try_send(message) {
                Ok(()) => return true,
                Err(TrySendError::Disconnected(_)) => return false,
                Err(TrySendError::Full(returned)) => {
                    message = returned;
                    if Instant::now() >= deadline {
                        return false;
                    }
                    std::thread::sleep(Duration::from_millis(1));
                }
            }
        }
    }

    fn record_drop(dropped: u64) {
        if dropped.is_power_of_two() {
            tracing::warn!(
                dropped,
                "alert persistence buffer full or closed; update dropped"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sandhi_core::AlertChannel;
    use sandhi_store::CreateAlertRequest;

    #[test]
    fn close_drains_fired_markers() {
        let store = Arc::new(AlertStore::in_memory().unwrap());
        let record = store
            .create(CreateAlertRequest {
                scope: "group:test".into(),
                threshold_pct: 80,
                channel: AlertChannel::Log,
            })
            .unwrap();
        let writer = BufferedAlertStore::new(store.clone(), 4);
        writer.mark_fired(record.id.clone());

        assert!(writer.close(Duration::from_secs(1)));
        assert!(store
            .find_by_id(&record.id)
            .unwrap()
            .unwrap()
            .last_fired_at
            .is_some());
        assert_eq!(writer.dropped_updates(), 0);
        assert_eq!(
            writer.snapshot(),
            BufferSnapshot {
                capacity: 4,
                ..BufferSnapshot::default()
            }
        );
    }

    #[test]
    fn blocked_alert_writer_exposes_queue_execution_and_drops() {
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let writer = BufferedAlertStore::with_writer(1, move |_| {
            entered_tx.send(()).unwrap();
            release_rx.recv().unwrap();
        });
        let observer = writer.observer();
        writer.mark_fired("first".into());
        entered_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        writer.mark_fired("second".into());
        writer.mark_fired("overflow".into());
        assert_eq!(
            observer.snapshot(),
            BufferSnapshot {
                capacity: 1,
                queued: 1,
                in_flight: 1,
                dropped: 1
            }
        );
        release_tx.send(()).unwrap();
        release_tx.send(()).unwrap();
        assert!(writer.close(Duration::from_secs(1)));
        assert_eq!(
            observer.snapshot(),
            BufferSnapshot {
                capacity: 1,
                dropped: 1,
                ..BufferSnapshot::default()
            }
        );
        writer.mark_fired("closed".into());
        assert_eq!(observer.snapshot().dropped, 2);
    }

    #[test]
    fn alert_observer_does_not_keep_worker_alive() {
        struct DropSignal(mpsc::Sender<()>);
        impl Drop for DropSignal {
            fn drop(&mut self) {
                let _ = self.0.send(());
            }
        }
        let (tx, rx) = mpsc::channel();
        let signal = DropSignal(tx);
        let writer = BufferedAlertStore::with_writer(0, move |_| {
            let _ = &signal;
        });
        let observer = writer.observer();
        writer.mark_fired("one".into());
        drop(writer);
        rx.recv_timeout(Duration::from_secs(1))
            .expect("observer must not retain sender");
        assert_eq!(
            observer.snapshot(),
            BufferSnapshot {
                capacity: 1,
                ..BufferSnapshot::default()
            }
        );
    }

    #[test]
    fn alert_worker_panic_clears_activity_and_counts_abandoned_queue() {
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let writer = BufferedAlertStore::with_writer(1, move |_| {
            entered_tx.send(()).unwrap();
            release_rx.recv().unwrap();
            panic!("synthetic alert writer failure");
        });
        writer.mark_fired("first".into());
        entered_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        writer.mark_fired("abandoned".into());
        release_tx.send(()).unwrap();
        assert!(writer
            .worker
            .lock()
            .unwrap()
            .take()
            .unwrap()
            .join()
            .is_err());
        assert_eq!(
            writer.snapshot(),
            BufferSnapshot {
                capacity: 1,
                dropped: 1,
                ..BufferSnapshot::default()
            }
        );
        writer.mark_fired("disconnected".into());
        assert_eq!(writer.snapshot().dropped, 2);
    }

    #[test]
    fn completed_alert_callback_is_not_proof_a_rule_was_updated() {
        let store = Arc::new(AlertStore::in_memory().unwrap());
        let writer = BufferedAlertStore::new(store.clone(), 1);
        writer.mark_fired("nonexistent".into());
        assert!(writer.close(Duration::from_secs(1)));
        assert!(store.find_by_id("nonexistent").unwrap().is_none());
        assert_eq!(
            writer.snapshot(),
            BufferSnapshot {
                capacity: 1,
                ..BufferSnapshot::default()
            }
        );
    }
}
