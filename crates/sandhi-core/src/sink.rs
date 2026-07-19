//! Usage-event sinks. Emission is **best-effort, off the critical path** — a slow or failing
//! sink must never break or delay the model call (AnvaiOps ADR-0047 D7 / ADR-0020 D7).

use crate::event::UsageEvent;
use std::io::Write;
use std::sync::Mutex;

/// Where finalized usage events go (local JSONL/SQLite, an HTTP collector, …).
pub trait Sink: Send + Sync {
    /// Record one event. Implementations must swallow their own errors (best-effort).
    fn emit(&self, event: &UsageEvent);
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
}
