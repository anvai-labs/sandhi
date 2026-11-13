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
use std::{pin::Pin, sync::Arc};
use tokio::sync::{mpsc, watch};

type ByteStream<'a> = Pin<Box<dyn Stream<Item = Result<Bytes, std::io::Error>> + Send + 'a>>;
const QUEUE_FRAMES: usize = 4;
const FRAME_BYTES: usize = 16 * 1024;

#[derive(Clone, Copy)]
enum Failure {
    Deadline,
    Disconnected,
    Shutdown,
    Lease,
    Source,
    Panic,
    Accounting,
}

impl Failure {
    fn message(self) -> &'static str {
        match self {
            Self::Deadline => "stream body deadline exceeded",
            Self::Disconnected => "stream receiver disconnected",
            Self::Shutdown => "shutdown grace expired",
            Self::Lease => "insufficient reservation lifetime after stream setup",
            Self::Source => "stream body failed",
            Self::Panic => "stream producer panicked",
            Self::Accounting => "accounting unresolved; do not automatically retry",
        }
    }

    fn outcome(self) -> &'static str {
        match self {
            Self::Deadline => "timeout",
            Self::Disconnected => "cancelled",
            _ => "error",
        }
    }
}

#[derive(Clone, Copy)]
enum Terminal {
    Running,
    Complete,
    Failed(Failure),
}

/// One owner for both wire planes; their existing generators retain usage parsing authority.
pub(super) fn body<F>(
    mut accounting: RequestAccounting,
    permit: Arc<AdmissionPermit>,
    tail: Option<Bytes>,
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
                _ = tokio::time::sleep_until(deadline) => Err(Failure::Deadline),
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
        if let Err(reason) = result {
            // Measured Final usage remains authoritative even when delivery fails. Only the
            // transport outcome changes; never replace measured counts with an invented zero.
            let outcome = reason.outcome();
            accounting.set_outcome(outcome);
            if let Some(usage) = accounting.usage.as_mut() {
                usage.outcome = Some(outcome.into());
            }
        }
        // A failure is visible promptly; the accounting owner still outlives the wire.
        if let Err(reason) = result {
            terminal_tx.send_replace(Terminal::Failed(reason));
        }
        // Read the snapshot only after the borrowed source has dropped. Retain the
        // permit and lifecycle guard through submission and the bounded settlement wait.
        let committed = std::panic::AssertUnwindSafe(accounting.finalize_owned())
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

fn receiver_body(mut rx: mpsc::Receiver<Bytes>, mut terminal: watch::Receiver<Terminal>) -> Body {
    Body::from_stream(async_stream::stream! {
        loop {
            let status = *terminal.borrow_and_update();
            if let Terminal::Failed(reason) = status {
                rx.close();
                yield Err::<Bytes, std::io::Error>(std::io::Error::other(reason.message()));
                break;
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
                        Some(bytes) => yield Ok(bytes),
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
