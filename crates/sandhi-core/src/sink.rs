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

/// One coherent, process-local view of a best-effort writer buffer.
///
/// `queued` counts accepted items not yet claimed by the writer; `in_flight` counts
/// executing callbacks. Neither callback return nor an empty buffer proves persistence.
/// Flush/shutdown control messages do not contribute to these counts.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BufferSnapshot {
    /// Configured maximum queued callbacks; at most one additional callback executes.
    pub capacity: usize,
    pub queued: usize,
    pub in_flight: usize,
    /// Rejected items and queued callbacks abandoned after worker panic. Excludes
    /// callback/storage failures, whose persistence outcome cannot be inferred.
    pub dropped: u64,
}

/// Sender-free observer: retaining this handle cannot keep a writer channel alive.
#[derive(Clone, Debug)]
pub struct BufferedSinkObserver {
    counters: Arc<Mutex<BufferSnapshot>>,
}

impl BufferedSinkObserver {
    #[must_use]
    pub fn snapshot(&self) -> BufferSnapshot {
        *self.counters.lock().unwrap_or_else(|e| e.into_inner())
    }
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
    observer: BufferedSinkObserver,
    closed: AtomicBool,
    worker: Mutex<Option<JoinHandle<()>>>,
}

impl BufferedSink {
    /// Wrap `inner` with a bounded queue serviced by one dedicated writer thread.
    ///
    /// A zero capacity is promoted to one: callers always get a useful non-rendezvous buffer.
    #[must_use]
    pub fn new(inner: Arc<dyn Sink>, capacity: usize) -> Self {
        let capacity = capacity.max(1);
        let (sender, receiver) = mpsc::sync_channel(capacity);
        let observer = BufferedSinkObserver {
            counters: Arc::new(Mutex::new(BufferSnapshot {
                capacity,
                ..BufferSnapshot::default()
            })),
        };
        let counters = observer.counters.clone();
        let worker = std::thread::Builder::new()
            .name("sandhi-usage-writer".into())
            .spawn(move || {
                while let Ok(message) = receiver.recv() {
                    match message {
                        BufferedMessage::Event(event) => {
                            {
                                let mut counts = counters.lock().unwrap_or_else(|e| e.into_inner());
                                counts.queued -= 1;
                                counts.in_flight += 1;
                            }
                            let result =
                                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                    inner.emit(&event);
                                }));
                            let mut counts = counters.lock().unwrap_or_else(|e| e.into_inner());
                            counts.in_flight -= 1;
                            if let Err(panic) = result {
                                // The callback's persistence outcome is unknown. Only queued
                                // items abandoned before invocation count as dropped here.
                                counts.dropped += counts.queued as u64;
                                counts.queued = 0;
                                drop(receiver);
                                drop(counts);
                                std::panic::resume_unwind(panic);
                            }
                        }
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
            observer,
            closed: AtomicBool::new(false),
            worker: Mutex::new(Some(worker)),
        }
    }

    /// Events rejected by a full/closed/disconnected queue or abandoned before callback
    /// invocation when the worker panics. Does not count or prove persistence failures.
    #[must_use]
    pub fn dropped_events(&self) -> u64 {
        self.snapshot().dropped
    }

    #[must_use]
    pub fn observer(&self) -> BufferedSinkObserver {
        self.observer.clone()
    }

    #[must_use]
    pub fn snapshot(&self) -> BufferSnapshot {
        self.observer.snapshot()
    }

    /// Drain all events accepted before this call and stop the writer thread.
    ///
    /// Returns `false` when the writer did not drain before `timeout`. In that case the worker is
    /// left running rather than being detached from queued events; the process may choose its own
    /// forced-shutdown policy after reporting the loss risk.
    pub fn close(&self, timeout: Duration) -> bool {
        // Serialize admission closure with try_send: an emitter cannot enqueue behind Shutdown.
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

    fn record_drop(dropped: u64) {
        // Log at powers of two: persistent overload stays visible without creating a second
        // overload through one log line per rejected event.
        if dropped.is_power_of_two() {
            tracing::warn!(dropped, "usage event buffer full or closed; event dropped");
        }
    }
}

impl Sink for BufferedSink {
    fn emit(&self, event: &UsageEvent) {
        let event = Box::new(event.clone());
        // The receiver takes this same lock before claiming an item, preventing a fast
        // callback from decrementing queued before a successful send is counted.
        let mut counts = self
            .observer
            .counters
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        // A received message still counts as queued until the worker claims it under
        // this lock. Enforce the logical capacity as well as the channel's capacity.
        if self.closed.load(Ordering::Acquire) || counts.queued >= counts.capacity {
            counts.dropped += 1;
            let dropped = counts.dropped;
            drop(counts);
            Self::record_drop(dropped);
            return;
        }
        match self.sender.try_send(BufferedMessage::Event(event)) {
            Ok(()) => counts.queued += 1,
            Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => {
                counts.dropped += 1;
                let dropped = counts.dropped;
                drop(counts);
                Self::record_drop(dropped);
            }
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
        let observer = buffered.observer();
        assert_eq!(
            observer.snapshot(),
            BufferSnapshot {
                capacity: 1,
                ..BufferSnapshot::default()
            }
        );

        buffered.emit(&sample());
        entered_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("writer entered inner sink");
        buffered.emit(&sample()); // occupies the one queue slot
        buffered.emit(&sample()); // must be rejected, never allocated behind it
        assert_eq!(buffered.dropped_events(), 1);
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
        assert!(buffered.close(Duration::from_secs(1)));
        assert_eq!(inner.events.lock().unwrap().len(), 2);
        assert_eq!(
            observer.snapshot(),
            BufferSnapshot {
                capacity: 1,
                dropped: 1,
                ..BufferSnapshot::default()
            }
        );
        buffered.emit(&sample());
        assert_eq!(observer.snapshot().dropped, 2);
    }

    #[test]
    fn buffered_observer_does_not_keep_worker_alive() {
        struct DropSink(mpsc::Sender<()>);
        impl Sink for DropSink {
            fn emit(&self, _: &UsageEvent) {}
        }
        impl Drop for DropSink {
            fn drop(&mut self) {
                let _ = self.0.send(());
            }
        }
        let (dropped_tx, dropped_rx) = mpsc::channel();
        let buffered = BufferedSink::new(Arc::new(DropSink(dropped_tx)), 0);
        let observer = buffered.observer();
        buffered.emit(&sample());
        drop(buffered);
        dropped_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("sender-free observer must not retain worker");
        assert_eq!(
            observer.snapshot(),
            BufferSnapshot {
                capacity: 1,
                ..BufferSnapshot::default()
            }
        );
    }

    #[test]
    fn panicked_worker_clears_activity_and_counts_abandoned_queue() {
        struct PanicSink {
            entered: mpsc::Sender<()>,
            release: Mutex<mpsc::Receiver<()>>,
        }
        impl Sink for PanicSink {
            fn emit(&self, _: &UsageEvent) {
                self.entered.send(()).unwrap();
                self.release.lock().unwrap().recv().unwrap();
                panic!("synthetic sink failure");
            }
        }
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let buffered = BufferedSink::new(
            Arc::new(PanicSink {
                entered: entered_tx,
                release: Mutex::new(release_rx),
            }),
            1,
        );
        buffered.emit(&sample());
        entered_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        buffered.emit(&sample());
        release_tx.send(()).unwrap();
        assert!(buffered
            .worker
            .lock()
            .unwrap()
            .take()
            .unwrap()
            .join()
            .is_err());
        assert_eq!(
            buffered.snapshot(),
            BufferSnapshot {
                capacity: 1,
                dropped: 1,
                ..BufferSnapshot::default()
            }
        );
        buffered.emit(&sample());
        assert_eq!(buffered.snapshot().dropped, 2);
    }

    #[test]
    fn concurrent_buffer_admission_and_close_balance_activity() {
        let inner = Arc::new(InMemorySink::with_capacity(10000));
        let buffered = Arc::new(BufferedSink::new(inner.clone(), 32));
        let barrier = Arc::new(std::sync::Barrier::new(5));
        let writers: Vec<_> = (0..4)
            .map(|_| {
                let buffered = buffered.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    for _ in 0..250 {
                        buffered.emit(&sample());
                        let snapshot = buffered.snapshot();
                        assert!(snapshot.queued <= snapshot.capacity);
                        assert!(snapshot.in_flight <= 1);
                    }
                })
            })
            .collect();
        barrier.wait();
        assert!(buffered.close(Duration::from_secs(2)));
        for writer in writers {
            writer.join().unwrap();
        }
        let snapshot = buffered.snapshot();
        assert_eq!(snapshot.queued, 0);
        assert_eq!(snapshot.in_flight, 0);
        assert_eq!(inner.len() as u64 + snapshot.dropped, 1000);
    }
}
