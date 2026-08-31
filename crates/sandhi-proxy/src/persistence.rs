//! Bounded background persistence for best-effort alert dedup state.
//!
//! Budget reserve/settle is intentionally absent: hard-cap enforcement remains synchronous and
//! linearizable. Only the observational `last_fired_at` mirror may leave the request task.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use sandhi_store::AlertStore;

enum Message {
    MarkFired(String),
    Flush(mpsc::Sender<()>),
    Shutdown,
}

/// A bounded single-writer queue for `AlertStore::mark_fired` updates.
pub struct BufferedAlertStore {
    sender: SyncSender<Message>,
    dropped: AtomicU64,
    closed: AtomicBool,
    worker: Mutex<Option<JoinHandle<()>>>,
}

impl BufferedAlertStore {
    #[must_use]
    pub fn new(store: Arc<AlertStore>, capacity: usize) -> Self {
        let (sender, receiver) = mpsc::sync_channel(capacity.max(1));
        let worker = std::thread::Builder::new()
            .name("sandhi-alert-writer".into())
            .spawn(move || {
                while let Ok(message) = receiver.recv() {
                    match message {
                        Message::MarkFired(rule_id) => {
                            if let Err(error) = store.mark_fired(&rule_id) {
                                tracing::warn!(%error, %rule_id, "could not persist alert fire");
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
            dropped: AtomicU64::new(0),
            closed: AtomicBool::new(false),
            worker: Mutex::new(Some(worker)),
        }
    }

    /// Queue a durable dedup update without blocking the response task.
    pub fn mark_fired(&self, rule_id: String) {
        if self.closed.load(Ordering::Acquire) {
            self.record_drop();
            return;
        }
        match self.sender.try_send(Message::MarkFired(rule_id)) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => self.record_drop(),
        }
    }

    #[must_use]
    pub fn dropped_updates(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// Drain accepted updates and stop the writer before the supplied deadline.
    pub fn close(&self, timeout: Duration) -> bool {
        if self.closed.swap(true, Ordering::AcqRel) {
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

    fn record_drop(&self) {
        let dropped = self.dropped.fetch_add(1, Ordering::Relaxed) + 1;
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
    }
}
