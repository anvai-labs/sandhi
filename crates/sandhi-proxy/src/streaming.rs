//! Durable streaming body ownership, independent of downstream polling.
use std::time::Duration;

mod qualified;
pub(super) use qualified::{
    capture_provider_terminal, capture_terminal, qualified_response, OwnedObservation,
};

/// Body-only lifetime after upstream headers; setup and idle transport limits are unchanged.
/// This is not a bound on synchronous parsing, settlement, or origin-side cancellation.
#[derive(Clone, Copy, Debug)]
pub struct StreamBodyLifetime(Duration);

impl StreamBodyLifetime {
    pub fn new(duration: Duration) -> Result<Self, &'static str> {
        let maximum = Duration::from_secs(crate::ledger::RESERVATION_TTL_SECS as u64)
            - Duration::from_millis(crate::deadlines::SETTLEMENT_HEADROOM_MS);
        if duration.is_zero() || duration > maximum {
            return Err("stream body lifetime must be positive and fit the reservation TTL with settlement headroom");
        }
        Ok(Self(duration))
    }

    pub fn duration(self) -> Duration {
        self.0
    }
}

use crate::{AdmissionPermit, RequestAccounting};
use axum::body::{Body, Bytes};
use futures_util::{FutureExt, Stream, StreamExt};
use sandhi_core::{ChatStreamEventV1, ProviderErrorV1, UsageBasis, UsageCompleteness};
use sandhi_dialect::{IngressDialect, TranslatedStream};
use serde_json::Value;
use std::collections::BTreeMap;
use std::{pin::Pin, sync::Arc};
use tokio::sync::{mpsc, watch};

type ByteStream<'a> = Pin<Box<dyn Stream<Item = Result<Bytes, std::io::Error>> + Send + 'a>>;
const QUEUE_FRAMES: usize = 4;
const FRAME_BYTES: usize = 16 * 1024;

#[derive(Clone)]
enum Failure {
    Deadline,
    /// Body-deadline cancellation carrying the rendered per-plane timeout frame:
    /// the one failure mode that surfaces to the client in-band (ADR-0050).
    DeadlineFrame(Bytes),
    Disconnected,
    Shutdown,
    Lease,
    Source,
    Panic,
    Accounting,
}

impl Failure {
    fn message(&self) -> &'static str {
        match self {
            Self::Deadline | Self::DeadlineFrame(_) => "stream body deadline exceeded",
            Self::Disconnected => "stream receiver disconnected",
            Self::Shutdown => "shutdown grace expired",
            Self::Lease => "insufficient reservation lifetime after stream setup",
            Self::Source => "stream body failed",
            Self::Panic => "stream producer panicked",
            Self::Accounting => "accounting unresolved; do not automatically retry",
        }
    }

    fn outcome(&self) -> &'static str {
        match self {
            Self::Deadline | Self::DeadlineFrame(_) => "timeout",
            Self::Disconnected => "cancelled",
            _ => "error",
        }
    }
}

#[derive(Clone)]
enum Terminal {
    Running,
    Complete,
    Failed(Failure),
}

/// Rendered per-plane structured timeout frame (ADR-0050): the error event plus the
/// plane's terminal tail, produced from the final accounting snapshot at deadline time.
pub(crate) type TimeoutFrameRenderer = Arc<dyn Fn(&RequestAccounting) -> Bytes + Send + Sync>;

pub(crate) fn deadline_renderer(dialect: IngressDialect, slug: String) -> TimeoutFrameRenderer {
    Arc::new(move |accounting| {
        let mut details = BTreeMap::from([("timeout_phase".into(), Value::from("body"))]);
        let measured = accounting.usage.as_ref().filter(|usage| {
            usage.completeness != UsageCompleteness::Unavailable
                && usage.basis == UsageBasis::ProviderReported
        });
        if let Some(usage) = measured.and_then(|usage| serde_json::to_value(usage).ok()) {
            details.insert("measured_usage".into(), usage);
        }
        let event = ChatStreamEventV1::Error {
            error: ProviderErrorV1 {
                code: "timeout".into(),
                message: Failure::Deadline.message().into(),
                retryable: false,
                http_status: Some(504),
                provider: Some(slug.clone()),
                request_id: Some(accounting.request_id.clone()),
                details,
            },
        };
        let Ok(frames) = TranslatedStream::new(dialect, true).push(&event) else {
            return Bytes::new();
        };
        let mut out = Vec::new();
        for (name, value) in frames {
            out.extend_from_slice(crate::sse_frame(name, &value).as_bytes());
        }
        if dialect == IngressDialect::OpenAi && !out.is_empty() {
            out.extend_from_slice(b"data: [DONE]\n\n");
        }
        Bytes::from(out)
    })
}

/// One owner for both wire planes; their existing generators retain usage parsing authority.
/// `timeout_frame` (when present) renders the per-plane structured timeout error frame
/// (ADR-0050) at body-deadline time, with access to the final accounting snapshot.
pub(super) fn body<F>(
    mut accounting: RequestAccounting,
    permit: Arc<AdmissionPermit>,
    tail: Option<Bytes>,
    timeout_frame: Option<crate::streaming::TimeoutFrameRenderer>,
    make: F,
) -> Body
where
    F: for<'a> FnOnce(&'a mut RequestAccounting) -> ByteStream<'a> + Send + 'static,
{
    if accounting.owned.is_none() {
        return Body::from_stream(futures_util::stream::once(async {
            Err::<Bytes, _>(std::io::Error::other("durable streaming owner required"))
        }));
    }
    let Some(lifetime) = accounting.stream_body_lifetime else {
        return Body::from_stream(futures_util::stream::once(async {
            Err::<Bytes, _>(std::io::Error::other("bounded streaming owner required"))
        }));
    };
    let deadline = tokio::time::Instant::now() + lifetime.duration();
    let fits_lease = accounting.reservation.as_ref().is_none_or(|reservation| {
        crate::deadlines::fits_lease(
            lifetime.duration(),
            reservation.expires_at,
            time::OffsetDateTime::now_utc(),
        )
    });
    let lifecycle = accounting.state.lifecycle.clone();
    // Leave the existing accounting wait inside the original shutdown grace.
    let settlement_headroom = accounting.owned.as_ref().map(|owned| owned.wait);
    let (tx, rx) = mpsc::channel(QUEUE_FRAMES);
    let (terminal_tx, terminal_rx) = watch::channel(Terminal::Running);
    tokio::spawn(async move {
        let open = accounting.state.metrics.stream_open_guard();
        // Catch producer panics without losing the accounting/operation owner. The borrowed
        // source drops on every select exit BEFORE any synchronous settlement is scheduled.
        let mut result = std::panic::AssertUnwindSafe(async {
            let mut source = make(&mut accounting);
            if !fits_lease {
                return Err(Failure::Lease);
            }
            let grace = async {
                lifecycle.cancelled().await;
                if let Some(at) = lifecycle.deadline() {
                    let at = settlement_headroom.map_or(at, |wait| {
                        at.checked_sub(wait).unwrap_or_else(std::time::Instant::now)
                    });
                    tokio::time::sleep_until(tokio::time::Instant::from_std(at)).await;
                }
            };
            tokio::select! {
                biased;
                _ = tokio::time::sleep_until(deadline) => {
                    if let Some(owned) = accounting.owned.as_ref() {
                        owned.attempts.context.mark_timeout_phase(
                            sandhi_providers::TimeoutPhase::Body,
                        );
                    }
                    Err(Failure::Deadline)
                }
                _ = tx.closed() => Err(Failure::Disconnected),
                _ = grace => Err(Failure::Shutdown),
                result = async {
                    while let Some(bytes) = source.next().await {
                        send_bytes(&tx, bytes.map_err(|_| Failure::Source)?).await?;
                    }
                    drop(source);
                    if let Some(tail) = tail { send_bytes(&tx, tail).await?; }
                    Ok(())
                } => result,
            }
        })
        .catch_unwind()
        .await
        .unwrap_or(Err(Failure::Panic));
        drop(open);
        if let Err(reason) = &result {
            // Measured Final usage remains authoritative even when delivery fails. Only the
            // transport outcome changes; never replace measured counts with an invented zero.
            let outcome = reason.outcome();
            accounting.set_outcome(outcome);
            if let Some(usage) = accounting.usage.as_mut() {
                usage.outcome = Some(outcome.into());
            }
        }
        // Snapshot usage after the source drops. Publish the timeout from that
        // snapshot before waiting for the durable settlement.
        let owner_deadline = matches!(result, Err(Failure::Deadline));
        let can_frame_timeout = owner_deadline && !qualified::source_terminal_seen(&accounting);
        let committed = std::panic::AssertUnwindSafe(accounting.finalize_owned_after_source(
            owner_deadline,
            |accounting| {
                if can_frame_timeout {
                    if let Some(frame) = timeout_frame
                        .map(|render| render(accounting))
                        .filter(|frame| !frame.is_empty())
                    {
                        result = Err(Failure::DeadlineFrame(frame));
                    }
                }
                if let Err(reason) = &result {
                    terminal_tx.send_replace(Terminal::Failed(reason.clone()));
                }
            },
        ))
        .catch_unwind()
        .await
        .unwrap_or(false);
        if !committed && result.is_ok() {
            result = Err(Failure::Accounting);
        }
        drop(tx);
        terminal_tx.send_replace(match result {
            Ok(()) => Terminal::Complete,
            Err(reason) => Terminal::Failed(reason),
        });
        drop(permit);
        drop(accounting);
    });
    receiver_body(rx, terminal_rx)
}

async fn send_bytes(tx: &mpsc::Sender<Bytes>, bytes: Bytes) -> Result<(), Failure> {
    for chunk in bytes.chunks(FRAME_BYTES) {
        // Copy, not Bytes::slice: queued fragments must not retain oversized backing buffers.
        tx.send(Bytes::copy_from_slice(chunk))
            .await
            .map_err(|_| Failure::Disconnected)?;
    }
    Ok(())
}

// HTTP chunks may split any SSE line. Track only the trailing line endings
// of bytes actually delivered, including a CRLF split across chunks.
struct SseBoundary {
    line_endings: u8,
    last_cr: bool,
}

impl SseBoundary {
    fn observe(&mut self, bytes: &[u8]) {
        let suffix =
            if let Some(last_content) = bytes.iter().rposition(|&b| b != b'\r' && b != b'\n') {
                self.line_endings = 0;
                self.last_cr = false;
                &bytes[last_content + 1..]
            } else {
                bytes
            };
        for &byte in suffix {
            if byte == b'\r' || !self.last_cr {
                self.line_endings = (self.line_endings + 1).min(2);
            }
            self.last_cr = byte == b'\r';
        }
    }
}

fn receiver_body(mut rx: mpsc::Receiver<Bytes>, mut terminal: watch::Receiver<Terminal>) -> Body {
    Body::from_stream(async_stream::stream! {
        let mut boundary = SseBoundary { line_endings: 2, last_cr: false };
        loop {
            let status = terminal.borrow_and_update().clone();
            match status {
                // ADR-0050: the owner-origin body timeout delivers its structured frame
                // in-band, then ends the stream cleanly — the frame is a wire-transport
                // terminator, never a settlement receipt (outcome stays "timeout").
                Terminal::Failed(Failure::DeadlineFrame(frame)) if boundary.line_endings >= 2 => {
                    rx.close();
                    yield Ok(frame);
                    break;
                }
                Terminal::Failed(reason) => {
                    rx.close();
                    yield Err::<Bytes, std::io::Error>(std::io::Error::other(reason.message()));
                    break;
                }
                _ => {}
            }
            tokio::select! {
                biased;
                changed = terminal.changed(), if matches!(status, Terminal::Running) => {
                    if changed.is_err() {
                        yield Err(std::io::Error::other("stream owner disappeared"));
                        break;
                    }
                }
                bytes = rx.recv() => {
                    // Terminal failure wins over already queued bytes (including DONE).
                    if matches!(*terminal.borrow(), Terminal::Failed(_)) { continue; }
                    match bytes {
                        Some(bytes) => {
                            boundary.observe(&bytes);
                            yield Ok(bytes);
                        }
                        None if matches!(status, Terminal::Complete) => break,
                        None => {
                            if terminal.changed().await.is_err() {
                                yield Err(std::io::Error::other("stream owner disappeared"));
                                break;
                            }
                        }
                    }
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ProxyLedger, ProxyState};
    use sandhi_core::{ChatRequestV1, InMemorySink, KeyStore};
    use tokio::sync::Semaphore;
    #[tokio::test]
    async fn owner_timeout_frame_requires_a_complete_sse_boundary() {
        for chunks in [
            vec![Bytes::from_static(b"data: {\"partial\":")],
            vec![
                Bytes::from_static(b"data: {}\r\n\r"),
                Bytes::from_static(b"\n"),
            ],
        ] {
            let complete = chunks.len() == 2;
            let (tx, rx) = mpsc::channel(QUEUE_FRAMES);
            let (status, terminal) = watch::channel(Terminal::Running);
            let mut body = receiver_body(rx, terminal).into_data_stream();
            for chunk in chunks {
                tx.send(chunk.clone()).await.unwrap();
                assert_eq!(body.next().await.unwrap().unwrap(), chunk);
            }
            let frame = Bytes::from_static(b"data: {\"error\":{}}\n\n");
            status.send_replace(Terminal::Failed(Failure::DeadlineFrame(frame.clone())));
            match body.next().await.unwrap() {
                Ok(delivered) => {
                    assert!(complete);
                    assert_eq!(delivered, frame);
                }
                Err(_) => assert!(!complete),
            }
            assert!(body.next().await.is_none());
        }
    }

    #[tokio::test]
    async fn unowned_stream_cannot_poll_source_or_emit_success_with_or_without_a_lifetime() {
        for lifetime in [
            None,
            Some(StreamBodyLifetime::new(Duration::from_secs(1)).unwrap()),
        ] {
            let sink = Arc::new(InMemorySink::new());
            let state = Arc::new(ProxyState::new(
                KeyStore::new(),
                ProxyLedger::in_memory(),
                sink.clone(),
                Default::default(),
                None,
            ));
            let request: ChatRequestV1 =
                serde_json::from_value(serde_json::json!({"model":"fixture","messages":[]}))
                    .unwrap();
            let mut accounting = RequestAccounting::new(
                state.clone(),
                "scope".into(),
                None,
                "fixture".into(),
                &request,
                "openai",
                crate::metrics::Plane::Transparent,
            );
            accounting.stream_body_lifetime = lifetime;
            let slots = Arc::new(Semaphore::new(1));
            let permit = Arc::new(AdmissionPermit {
                _permit: slots.clone().acquire_owned().await.unwrap(),
            });
            let response = body(
                accounting,
                permit,
                Some(Bytes::from_static(b"DONE")),
                None,
                |_| panic!("unowned stream must never invoke its source"),
            );
            assert!(axum::body::to_bytes(response, 1000).await.is_err());
            assert!(sink.is_empty());
            assert_eq!(slots.available_permits(), 1);
            assert_eq!(state.lifecycle.active_operations(), 0);
        }
    }
    #[test]
    fn stream_lifetime_bounds_leave_settlement_headroom() {
        assert!(StreamBodyLifetime::new(Duration::ZERO).is_err());
        assert!(StreamBodyLifetime::new(Duration::from_secs(841)).is_err());
        assert_eq!(
            StreamBodyLifetime::new(Duration::from_secs(30))
                .unwrap()
                .duration(),
            Duration::from_secs(30)
        );
    }
}
