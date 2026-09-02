//! Usage-event sinks. Emission is **best-effort, off the critical path** — a slow or failing
//! sink must never break or delay the model call (AnvaiOps ADR-0047 D7 / ADR-0020 D7).

use crate::event::UsageEvent;
use std::collections::VecDeque;
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

/// Default event capacity of [`InMemorySink`] when no explicit bound is given.
pub const DEFAULT_MEMORY_SINK_CAPACITY: usize = 10_000;

/// An in-memory sink — the default for tests and single-process local use.
///
/// Bounded by design (design audit A3): the standalone proxy uses this when `SANDHI_STORE` is
/// unset, where an unbounded `Vec` made the default no-config deployment a monotonic memory
/// leak. Once full it evicts the OLDEST event (a local recent-usage view wants the newest),
/// counts the eviction, and logs at powers of two exactly like [`BufferedSink`] — no bound
/// ships unobservable (TD-0014's rule). The proxy's default is
/// [`DEFAULT_MEMORY_SINK_CAPACITY`], tunable via `SANDHI_MEMORY_SINK_MAX`.
#[derive(Debug)]
pub struct InMemorySink {
    events: Mutex<VecDeque<UsageEvent>>,
    capacity: usize,
    dropped: AtomicU64,
}

impl InMemorySink {
    /// A bounded sink retaining the newest [`DEFAULT_MEMORY_SINK_CAPACITY`] events.
    #[must_use]
    pub fn new() -> Self {
        Self::with_capacity(DEFAULT_MEMORY_SINK_CAPACITY)
    }

    /// A bounded sink retaining (at most) the newest `capacity` events; zero is promoted to one.
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            events: Mutex::new(VecDeque::new()),
            capacity: capacity.max(1),
            dropped: AtomicU64::new(0),
        }
    }

    /// A snapshot of the retained events, oldest-first.
    pub fn events(&self) -> Vec<UsageEvent> {
        self.events
            .lock()
            .map(|events| events.iter().cloned().collect())
            .unwrap_or_default()
    }

    pub fn len(&self) -> usize {
        self.events.lock().map(|events| events.len()).unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Number of oldest events evicted after the ring filled.
    #[must_use]
    pub fn dropped_events(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    fn record_drop(&self) {
        let dropped = self.dropped.fetch_add(1, Ordering::Relaxed) + 1;
        // Log at powers of two: persistent overload stays visible without creating a second
        // overload through one log line per evicted event.
        if dropped.is_power_of_two() {
            tracing::warn!(dropped, "in-memory usage sink full; oldest event evicted");
        }
    }
}

impl Default for InMemorySink {
    fn default() -> Self {
        Self::new()
    }
}

impl Sink for InMemorySink {
    fn emit(&self, event: &UsageEvent) {
        let mut evicted = false;
        if let Ok(mut events) = self.events.lock() {
            if events.len() >= self.capacity {
                events.pop_front();
                evicted = true;
            }
            events.push_back(event.clone());
        }
        if evicted {
            self.record_drop();
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
    fn in_memory_sink_evicts_oldest_when_full_and_counts_it() {
        // Design audit A3: the no-SANDHI_STORE default must be bounded, keep the NEWEST events,
        // and make the eviction observable — the same three properties BufferedSink guarantees.
        let sink = InMemorySink::with_capacity(2);
        sink.emit(&sample().with_tokens(1, 1));
        sink.emit(&sample().with_tokens(2, 2));
        sink.emit(&sample().with_tokens(3, 3));
        assert_eq!(sink.len(), 2, "capacity respected");
        assert_eq!(sink.dropped_events(), 1, "eviction counted");
        let kept: Vec<u64> = sink.events().iter().map(|e| e.tokens_out).collect();
        assert_eq!(kept, vec![2, 3], "newest retained, oldest-first order");
    }

    #[test]
    fn in_memory_sink_promotes_zero_capacity_to_one() {
        let sink = InMemorySink::with_capacity(0);
        sink.emit(&sample().with_tokens(1, 1));
        sink.emit(&sample().with_tokens(2, 2));
        assert_eq!(sink.len(), 1);
        assert_eq!(sink.dropped_events(), 1);
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
