//! Usage-event sinks. Emission is **best-effort, off the critical path** — a slow or failing
//! sink must never break or delay the model call (AnvaiOps ADR-0047 D7 / ADR-0020 D7).

use crate::event::UsageEvent;
use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, SyncSender, TrySendError};
use std::sync::Arc;
use std::sync::Mutex;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Where finalized usage events go (local JSONL/SQLite, an HTTP collector, …).
pub trait Sink: Send + Sync {
    /// Record one event. Implementations must swallow their own errors (best-effort).
    fn emit(&self, event: &UsageEvent);
}

enum BufferedMessage {
    Event(Box<UsageEvent>),
    Flush(mpsc::Sender<()>),
    Shutdown,
}

/// A bounded, single-writer buffer in front of a potentially blocking [`Sink`].
///
/// `emit` never waits for SQLite, a file, or a collector. Once the fixed-capacity queue is full,
/// new events are dropped and counted instead of allocating without bound or blocking an async
/// request task. The proxy drains the queue during graceful shutdown through [`Self::close`].
///
/// This is deliberately an *observation* primitive. Enforcement ledger writes must remain on
/// their synchronous, linearizable path and must never be routed through a best-effort sink.
pub struct BufferedSink {
    sender: SyncSender<BufferedMessage>,
    dropped: AtomicU64,
    closed: AtomicBool,
    worker: Mutex<Option<JoinHandle<()>>>,
}

impl BufferedSink {
    /// Wrap `inner` with a bounded queue serviced by one dedicated writer thread.
    ///
    /// A zero capacity is promoted to one: callers always get a useful non-rendezvous buffer.
    #[must_use]
    pub fn new(inner: Arc<dyn Sink>, capacity: usize) -> Self {
        let (sender, receiver) = mpsc::sync_channel(capacity.max(1));
        let worker = std::thread::Builder::new()
            .name("sandhi-usage-writer".into())
            .spawn(move || {
                while let Ok(message) = receiver.recv() {
                    match message {
                        BufferedMessage::Event(event) => inner.emit(&event),
                        BufferedMessage::Flush(ack) => {
                            let _ = ack.send(());
                        }
                        BufferedMessage::Shutdown => break,
                    }
                }
            })
            .expect("spawn usage writer");
        Self {
            sender,
            dropped: AtomicU64::new(0),
            closed: AtomicBool::new(false),
            worker: Mutex::new(Some(worker)),
        }
    }

    /// Number of events rejected because the bounded queue was full or already closed.
    #[must_use]
    pub fn dropped_events(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// Drain all events accepted before this call and stop the writer thread.
    ///
    /// Returns `false` when the writer did not drain before `timeout`. In that case the worker is
    /// left running rather than being detached from queued events; the process may choose its own
    /// forced-shutdown policy after reporting the loss risk.
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
        if !self.send_control_until(BufferedMessage::Flush(ack_tx), deadline) {
            return false;
        }
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            return false;
        };
        if ack_rx.recv_timeout(remaining).is_err() {
            return false;
        }
        if !self.send_control_until(BufferedMessage::Shutdown, deadline) {
            return false;
        }

        let worker = self.worker.lock().ok().and_then(|mut worker| worker.take());
        match worker {
            Some(worker) => worker.join().is_ok(),
            None => true,
        }
    }

    fn send_control_until(&self, mut message: BufferedMessage, deadline: Instant) -> bool {
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
        // Log at powers of two: persistent overload stays visible without creating a second
        // overload through one log line per rejected event.
        if dropped.is_power_of_two() {
            tracing::warn!(dropped, "usage event buffer full or closed; event dropped");
        }
    }
}

impl Sink for BufferedSink {
    fn emit(&self, event: &UsageEvent) {
        if self.closed.load(Ordering::Acquire) {
            self.record_drop();
            return;
        }
        match self
            .sender
            .try_send(BufferedMessage::Event(Box::new(event.clone())))
        {
            Ok(()) => {}
            Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => self.record_drop(),
        }
    }
}

/// An in-memory sink — the default for tests and single-process local use.
#[derive(Debug, Default)]
pub struct InMemorySink {
    events: Mutex<Vec<UsageEvent>>,
}

impl InMemorySink {
    pub fn new() -> Self {
        Self::default()
    }

    /// A snapshot of everything emitted so far.
    pub fn events(&self) -> Vec<UsageEvent> {
        self.events.lock().map(|e| e.clone()).unwrap_or_default()
    }

    pub fn len(&self) -> usize {
        self.events.lock().map(|e| e.len()).unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl Sink for InMemorySink {
    fn emit(&self, event: &UsageEvent) {
        if let Ok(mut events) = self.events.lock() {
            events.push(event.clone());
        }
    }
}

/// A JSONL sink — one serialized event per line to any writer (file, stdout, buffer).
pub struct JsonlSink<W: Write + Send> {
    writer: Mutex<W>,
}

impl<W: Write + Send> JsonlSink<W> {
    pub fn new(writer: W) -> Self {
        Self {
            writer: Mutex::new(writer),
        }
    }
}

impl<W: Write + Send> Sink for JsonlSink<W> {
    fn emit(&self, event: &UsageEvent) {
        if let (Ok(mut w), Ok(line)) = (self.writer.lock(), serde_json::to_string(event)) {
            let _ = writeln!(w, "{line}"); // best-effort — never propagate
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{Backend, UsageEvent};

    fn sample() -> UsageEvent {
        UsageEvent::new("r", "t", "openai", "gpt-x", Backend::External).with_tokens(3, 4)
    }

    #[test]
    fn in_memory_collects() {
        let sink = InMemorySink::new();
        assert!(sink.is_empty());
        sink.emit(&sample());
        assert_eq!(sink.len(), 1);
        assert_eq!(sink.events()[0].tokens_out, 4);
    }

    #[test]
    fn jsonl_writes_one_line_per_event() {
        let buf: Vec<u8> = Vec::new();
        let sink = JsonlSink::new(buf);
        sink.emit(&sample());
        sink.emit(&sample());
        let inner = sink.writer.into_inner().unwrap();
        let text = String::from_utf8(inner).unwrap();
        assert_eq!(text.lines().count(), 2);
        assert!(text.lines().all(|l| l.contains("\"schema_version\":\"1\"")));
    }

    #[test]
    fn buffered_sink_flushes_accepted_events_before_close() {
        let inner = Arc::new(InMemorySink::new());
        let buffered = BufferedSink::new(inner.clone(), 8);
        buffered.emit(&sample());
        buffered.emit(&sample());

        assert!(buffered.close(Duration::from_secs(1)));
        assert_eq!(inner.len(), 2);
        assert_eq!(buffered.dropped_events(), 0);
    }

    #[test]
    fn buffered_sink_bounds_memory_and_counts_overflow() {
        struct GateSink {
            entered: mpsc::Sender<()>,
            release: Mutex<mpsc::Receiver<()>>,
            events: Mutex<Vec<UsageEvent>>,
        }

        impl Sink for GateSink {
            fn emit(&self, event: &UsageEvent) {
                let _ = self.entered.send(());
                let _ = self.release.lock().unwrap().recv();
                self.events.lock().unwrap().push(event.clone());
            }
        }

        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let inner = Arc::new(GateSink {
            entered: entered_tx,
            release: Mutex::new(release_rx),
            events: Mutex::new(Vec::new()),
        });
        let buffered = BufferedSink::new(inner.clone(), 1);

        buffered.emit(&sample());
        entered_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("writer entered inner sink");
        buffered.emit(&sample()); // occupies the one queue slot
        buffered.emit(&sample()); // must be rejected, never allocated behind it
        assert_eq!(buffered.dropped_events(), 1);

        release_tx.send(()).unwrap();
        release_tx.send(()).unwrap();
        assert!(buffered.close(Duration::from_secs(1)));
        assert_eq!(inner.events.lock().unwrap().len(), 2);
    }
}
