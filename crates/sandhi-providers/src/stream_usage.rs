//! Opt-in OpenAI Chat usage observation from complete, bounded SSE events.
//!
//! Create one observer per upstream HTTP response and keep it with the existing
//! request/attempt owner. SSE `id` never becomes Sandhi attribution. This observer
//! performs no I/O, retries, persistence or settlement and does not enable tracked
//! streaming. Legacy byte forwarding and metering are unchanged.

use crate::{linesplit::LineSplitter, ParsedUsage, MAX_STREAM_LINE_BYTES};
use sandhi_core::usage::{qualify_openai_stream_usage, StreamUsageError};

// Bound the extra copy when a caller supplies an arbitrarily large HTTP chunk.
const INPUT_SLICE_BYTES: usize = 8192;

/// Static failure facts only: no upstream content, identifiers or credentials.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamObservationError {
    LineTooLong,
    EventTooLong,
    InvalidUtf8,
    InvalidJson,
    UnexpectedEventType,
    UnexpectedBom,
    InvalidUsage(StreamUsageError),
    ConflictingUsage,
    TruncatedEvent,
    MissingTerminator,
    DataAfterTerminator,
    Closed,
}

/// Response-scoped measurement, independent of delivery success or settlement.
///
/// Usage is accepted at a complete empty-choices usage event, before `[DONE]`.
/// Identical observations are idempotent; conflicts and malformed/truncated later
/// events preserve the first observation and set a sticky error. Inspect both
/// [`usage`](Self::usage) and [`error`](Self::error); neither erases the other.
/// Complete post-DONE data events fail explicitly and cannot change usage.
///
/// SSE lines and events each have the existing 8 MiB ceiling, including comments
/// and ignored fields. Incoming chunks are inspected in bounded slices. CR/LF/CRLF,
/// one initial BOM, multiline data and ordinary message events are supported. EOF
/// never dispatches a partial event. This is not an EventSource/reconnection client.
pub struct OpenAiStreamUsageObserver {
    splitter: LineSplitter,
    limit: usize,
    first_line: bool,
    event_bytes: usize,
    data: Vec<u8>,
    has_data: bool,
    message_event: bool,
    usage: Option<ParsedUsage>,
    error: Option<StreamObservationError>,
    done: bool,
    closed: bool,
    #[cfg(test)]
    peak_buffered: usize,
}

impl Default for OpenAiStreamUsageObserver {
    fn default() -> Self {
        Self::new()
    }
}

impl OpenAiStreamUsageObserver {
    pub fn new() -> Self {
        Self::with_limit(MAX_STREAM_LINE_BYTES)
    }

    fn with_limit(limit: usize) -> Self {
        Self {
            splitter: LineSplitter::new_sse(limit),
            limit,
            first_line: true,
            event_bytes: 0,
            data: Vec::new(),
            has_data: false,
            message_event: true,
            usage: None,
            error: None,
            done: false,
            closed: false,
            #[cfg(test)]
            peak_buffered: 0,
        }
    }

    pub fn usage(&self) -> Option<ParsedUsage> {
        self.usage
    }

    pub fn error(&self) -> Option<StreamObservationError> {
        self.error
    }

    /// A complete `[DONE]` event was observed; this does not imply usage or success.
    pub fn is_done(&self) -> bool {
        self.done
    }

    /// Observe response bytes without modifying them. Errors stop qualification
    /// permanently; the first accepted measurement remains available for recovery.
    pub fn push(&mut self, chunk: &[u8]) -> Result<(), StreamObservationError> {
        if let Some(error) = self.error {
            return Err(error);
        }
        if self.closed {
            return Err(StreamObservationError::Closed);
        }
        for part in chunk.chunks(INPUT_SLICE_BYTES) {
            self.splitter.push(part);
            #[cfg(test)]
            {
                self.peak_buffered = self.peak_buffered.max(self.splitter.buffered_len());
            }
            while let Some(line) = self.splitter.next_line() {
                if let Err(error) = self.line(&line) {
                    return self.fail(error);
                }
            }
            if self.splitter.over_budget() {
                return self.fail(StreamObservationError::LineTooLong);
            }
        }
        Ok(())
    }

    /// Record EOF without manufacturing a blank line or measurement. Idempotent.
    /// Cancellation/transport failure remains the caller's separate outcome fact.
    pub fn finish(&mut self) -> Result<(), StreamObservationError> {
        self.closed = true;
        if let Some(error) = self.error {
            return Err(error);
        }
        if !self.splitter.remainder().is_empty() || self.event_bytes != 0 {
            return self.fail(StreamObservationError::TruncatedEvent);
        }
        if !self.done {
            return self.fail(StreamObservationError::MissingTerminator);
        }
        Ok(())
    }

    fn fail(&mut self, error: StreamObservationError) -> Result<(), StreamObservationError> {
        self.error = Some(error);
        self.splitter.reset();
        self.data.clear();
        Err(error)
    }

    fn line(&mut self, line: &[u8]) -> Result<(), StreamObservationError> {
        if line.len() > self.limit {
            return Err(StreamObservationError::LineTooLong);
        }
        self.event_bytes += line.len();
        if self.event_bytes > self.limit {
            return Err(StreamObservationError::EventTooLong);
        }
        let mut content = &line[..line.len() - 1]; // Splitter includes exactly one terminator.
        if self.first_line {
            content = content.strip_prefix(b"\xef\xbb\xbf").unwrap_or(content);
            self.first_line = false;
        }
        if content.starts_with(b"\xef\xbb\xbf") {
            return Err(StreamObservationError::UnexpectedBom);
        }
        let content =
            std::str::from_utf8(content).map_err(|_| StreamObservationError::InvalidUtf8)?;
        if content.is_empty() {
            self.event()?;
            self.event_bytes = 0;
            self.data.clear();
            self.has_data = false;
            self.message_event = true;
        } else if !content.starts_with(':') {
            let (field, value) = content.split_once(':').unwrap_or((content, ""));
            let value = value.strip_prefix(' ').unwrap_or(value);
            match field {
                "data" => {
                    if self.has_data {
                        self.data.push(b'\n');
                    }
                    self.data.extend_from_slice(value.as_bytes());
                    self.has_data = true;
                }
                "event" => self.message_event = value.is_empty() || value == "message",
                _ => {} // id/retry are not request correlation or replay permission.
            }
        }
        Ok(())
    }

    fn event(&mut self) -> Result<(), StreamObservationError> {
        if !self.has_data {
            return Ok(());
        }
        if self.done {
            return Err(StreamObservationError::DataAfterTerminator);
        }
        if !self.message_event {
            return Err(StreamObservationError::UnexpectedEventType);
        }
        if self.data == b"[DONE]" {
            self.done = true;
            return Ok(());
        }
        let event =
            serde_json::from_slice(&self.data).map_err(|_| StreamObservationError::InvalidJson)?;
        if let Some(next) =
            qualify_openai_stream_usage(&event).map_err(StreamObservationError::InvalidUsage)?
        {
            match self.usage {
                None => self.usage = Some(next),
                Some(first) if first == next => {}
                Some(_) => return Err(StreamObservationError::ConflictingUsage),
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn event(tokens: u64) -> String {
        format!(
            "data: {}\n\n",
            json!({"object":"chat.completion.chunk","choices":[],
            "usage":{"prompt_tokens":tokens,"completion_tokens":4}})
        )
    }

    #[test]
    fn stream_observer_multiline_framing_is_invariant_at_every_byte_boundary() {
        for newline in ["\n", "\r\n", "\r"] {
            let wire = concat!(
                "\u{feff}: keepalive\n\n",
                "id: untrusted-id\nevent: message\nunknown: ignored\n",
                "data: {\"object\":\"chat.completion.chunk\",\"choices\":[{\"delta\":{\"content\":\"雪\"}}],\"usage\":null}\n\n",
                "data: {\"object\":\"chat.completion.chunk\",\"choices\":[],\n",
                "data: \"usage\":{\"prompt_tokens\":10,\"completion_tokens\":4}}\n\n",
                "data: [DONE]\n\n"
            )
            .replace('\n', newline);
            for split in 0..=wire.len() {
                let mut observer = OpenAiStreamUsageObserver::new();
                observer.push(&wire.as_bytes()[..split]).unwrap();
                observer.push(&wire.as_bytes()[split..]).unwrap();
                assert!(observer.is_done());
                assert_eq!(observer.usage().unwrap().tokens_in, 10);
                assert_eq!(observer.error(), None);
                assert_eq!(observer.finish(), Ok(()));
                assert_eq!(observer.finish(), Ok(()));
                assert_eq!(
                    observer.push(b"new response"),
                    Err(StreamObservationError::Closed)
                );
            }
            let mut observer = OpenAiStreamUsageObserver::new();
            for byte in wire.bytes() {
                observer.push(&[byte]).unwrap();
            }
            assert!(observer.is_done());
            assert!(observer.usage().is_some());
        }
    }

    #[test]
    fn stream_observer_never_promotes_partial_events_or_done_to_usage() {
        let final_event = event(10);
        for trim in [1, 2, 10] {
            let mut observer = OpenAiStreamUsageObserver::new();
            observer
                .push(&final_event.as_bytes()[..final_event.len() - trim])
                .unwrap();
            assert_eq!(observer.usage(), None);
            assert_eq!(
                observer.finish(),
                Err(StreamObservationError::TruncatedEvent)
            );
            assert_eq!(observer.usage(), None);
        }
        let mut observer = OpenAiStreamUsageObserver::new();
        observer.push(final_event.as_bytes()).unwrap();
        let usage = observer.usage();
        assert!(usage.is_some());
        assert!(!observer.is_done());
        assert_eq!(
            observer.finish(),
            Err(StreamObservationError::MissingTerminator)
        );
        assert_eq!(observer.usage(), usage);

        let mut observer = OpenAiStreamUsageObserver::new();
        observer.push(b"data: [DONE]\r\r").unwrap();
        assert!(
            observer.is_done(),
            "CR dispatch must not wait for another byte"
        );
        assert_eq!(observer.usage(), None);
        assert_eq!(observer.finish(), Ok(()));
    }

    #[test]
    fn stream_observer_retains_first_usage_and_reports_conflicts_or_later_failure() {
        let cache_changed = format!(
            "data: {}\n\n",
            json!({"object":"chat.completion.chunk","choices":[],
            "usage":{"prompt_tokens":10,"completion_tokens":4,"prompt_tokens_details":{"cached_tokens":0}}})
        );
        for (suffix, expected) in [
            (event(11), StreamObservationError::ConflictingUsage),
            (cache_changed, StreamObservationError::ConflictingUsage),
            (
                "data: invalid\n\n".into(),
                StreamObservationError::InvalidJson,
            ),
            (
                "data: partial".into(),
                StreamObservationError::TruncatedEvent,
            ),
        ] {
            let mut observer = OpenAiStreamUsageObserver::new();
            observer.push(event(10).as_bytes()).unwrap();
            let first = observer.usage();
            observer.push(event(10).as_bytes()).unwrap(); // Identical observation is idempotent.
            let error = observer
                .push(suffix.as_bytes())
                .err()
                .or_else(|| observer.finish().err())
                .unwrap();
            assert_eq!(error, expected);
            assert_eq!(observer.error(), Some(error));
            assert_eq!(observer.usage(), first);
            assert_eq!(observer.push(event(12).as_bytes()), Err(error));
            assert_eq!(observer.finish(), Err(error));
            assert_eq!(observer.usage(), first);
        }
    }

    #[test]
    fn stream_observer_rejects_invalid_events_and_post_done_data() {
        for (wire, error) in [
            (
                b"data: \xff\n\n".to_vec(),
                StreamObservationError::InvalidUtf8,
            ),
            (
                b"data: invalid\n\n".to_vec(),
                StreamObservationError::InvalidJson,
            ),
            (
                b"data: [DONE]\ndata: more\n\n".to_vec(),
                StreamObservationError::InvalidJson,
            ),
            (
                [b"event: error\n".as_slice(), event(10).as_bytes()].concat(),
                StreamObservationError::UnexpectedEventType,
            ),
            (
                [b"event: usage\n".as_slice(), event(10).as_bytes()].concat(),
                StreamObservationError::UnexpectedEventType,
            ),
            (
                [b": first\n\n\xef\xbb\xbf".as_slice(), event(10).as_bytes()].concat(),
                StreamObservationError::UnexpectedBom,
            ),
            (
                b"data: {}\n\n".to_vec(),
                StreamObservationError::InvalidUsage(
                    sandhi_core::usage::StreamUsageError::InvalidChunk,
                ),
            ),
        ] {
            let mut observer = OpenAiStreamUsageObserver::new();
            assert_eq!(observer.push(&wire), Err(error));
            assert_eq!(observer.usage(), None);
            assert_eq!(observer.error(), Some(error));
        }
        let mut observer = OpenAiStreamUsageObserver::new();
        observer.push(event(10).as_bytes()).unwrap();
        let first = observer.usage();
        observer.push(b"data: [DONE]\n\n").unwrap();
        assert_eq!(
            observer.push(event(11).as_bytes()),
            Err(StreamObservationError::DataAfterTerminator)
        );
        assert_eq!(observer.usage(), first);
    }

    #[test]
    fn stream_observer_bounds_complete_pending_and_aggregate_events_without_salvage() {
        for wire in [
            vec![b'x'; 20_000],
            [vec![b'x'; 20_000], b"\n\n".to_vec()].concat(),
            [vec![b'x'; 600], b"\n\n".to_vec()].concat(),
        ] {
            let mut observer = OpenAiStreamUsageObserver::with_limit(512);
            observer.push(event(10).as_bytes()).unwrap();
            let first = observer.usage();
            assert_eq!(
                observer.push(&wire),
                Err(StreamObservationError::LineTooLong)
            );
            assert!(observer.peak_buffered <= 512 + INPUT_SLICE_BYTES);
            assert_eq!(
                observer.push(event(11).as_bytes()),
                Err(StreamObservationError::LineTooLong)
            );
            assert_eq!(observer.usage(), first);
        }
        for line in [
            ": comment padding\n",
            "unknown: ignored padding\n",
            "data: lots of padding\n",
        ] {
            let mut observer = OpenAiStreamUsageObserver::with_limit(512);
            assert_eq!(
                observer.push(line.repeat(100).as_bytes()),
                Err(StreamObservationError::EventTooLong)
            );
            assert_eq!(observer.usage(), None);
            assert!(observer.data.len() <= 512);
        }
    }
}
