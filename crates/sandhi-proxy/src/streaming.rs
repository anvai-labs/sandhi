//! Opt-in streaming body ownership, independent of downstream polling.
use std::time::Duration;

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
    let Some(lifetime) = accounting.stream_body_lifetime else {
        return Body::from_stream(async_stream::stream! {
            let _permit = permit;
            let _open = accounting.state.metrics.stream_open_guard();
            let mut source = make(&mut accounting);
            while let Some(bytes) = source.next().await { yield bytes; }
            drop(source);
            accounting.finalize();
            if let Some(tail) = tail { yield Ok(tail); }
        });
    };
    let deadline = tokio::time::Instant::now() + lifetime.duration();
    let fits_lease = accounting.reservation.as_ref().map_or(true, |reservation| {
        crate::deadlines::fits_lease(
            lifetime.duration(),
            reservation.expires_at,
            time::OffsetDateTime::now_utc(),
        )
    });
    let lifecycle = accounting.state.lifecycle.clone();
    let (tx, rx) = mpsc::channel(QUEUE_FRAMES);
    let (terminal_tx, terminal_rx) = watch::channel(Terminal::Running);
    tokio::spawn(async move {
        let open = accounting.state.metrics.stream_open_guard();
        // Catch producer panics without losing the accounting/operation owner. The borrowed
        // source drops on every select exit BEFORE any synchronous settlement is scheduled.
        let result = std::panic::AssertUnwindSafe(async {
            let mut source = make(&mut accounting);
            if !fits_lease {
                return Err(Failure::Lease);
            }
            let grace = async {
                lifecycle.cancelled().await;
                if let Some(at) = lifecycle.deadline() {
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
        drop(tx);
        terminal_tx.send_replace(match result {
            Ok(()) => Terminal::Complete,
            Err(reason) => Terminal::Failed(reason),
        });
        // This worker is NOT cancellable. Retain admission capacity and lifecycle ownership
        // until it finishes, so contention cannot create an unbounded queue or claim idle.
        if let Err(error) = tokio::task::spawn_blocking(move || {
            accounting.finalize();
            drop(permit);
            drop(accounting);
        })
        .await
        {
            tracing::error!(%error, "stream settlement worker failed; persistence is not confirmed");
        }
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
    use crate::{Admission, ProxyLedger, ProxyState};
    use sandhi_core::{ChatRequestV1, InMemorySink, KeyStore, Policy, UsageCompleteness, UsageV2};
    use tokio::sync::{oneshot, Semaphore};

    struct Fixture {
        accounting: RequestAccounting,
        state: Arc<ProxyState>,
        sink: Arc<InMemorySink>,
        permit: Arc<AdmissionPermit>,
        slots: Arc<Semaphore>,
    }

    impl Fixture {
        async fn new(duration: Duration) -> Self {
            let sink = Arc::new(InMemorySink::new());
            let mut state = ProxyState::new(
                KeyStore::new(),
                ProxyLedger::in_memory(),
                sink.clone(),
                Default::default(),
                None,
            );
            state.stream_body_lifetime = Some(StreamBodyLifetime::new(duration).unwrap());
            let state = Arc::new(state);
            let request: ChatRequestV1 =
                serde_json::from_value(serde_json::json!({"model":"fixture","messages":[]}))
                    .unwrap();
            let Admission::Leased(lease) = state.ledger.lock().unwrap().reserve(
                "scope",
                100,
                time::OffsetDateTime::now_utc(),
                Policy::Block,
            ) else {
                panic!("lease");
            };
            let mut accounting = RequestAccounting::new(
                state.clone(),
                "scope".into(),
                Some(lease),
                "openai".into(),
                &request,
                "openai",
                crate::metrics::Plane::Transparent,
            );
            accounting.operation = state.lifecycle.try_operation();
            let slots = Arc::new(Semaphore::new(1));
            let permit = Arc::new(AdmissionPermit {
                _permit: slots.clone().acquire_owned().await.unwrap(),
            });
            Self {
                accounting,
                state,
                sink,
                permit,
                slots,
            }
        }
    }

    struct Closed(Option<oneshot::Sender<()>>);
    impl Drop for Closed {
        fn drop(&mut self) {
            if let Some(tx) = self.0.take() {
                let _ = tx.send(());
            }
        }
    }

    async fn within<F: std::future::Future>(future: F) -> F::Output {
        tokio::time::timeout(Duration::from_secs(2), future)
            .await
            .expect("bounded completion")
    }

    #[tokio::test]
    async fn full_queue_is_bounded_and_disconnect_or_deadline_closes_source() {
        for disconnect in [false, true] {
            let Fixture {
                accounting,
                state,
                sink,
                permit,
                slots,
            } = Fixture::new(if disconnect {
                Duration::from_secs(30)
            } else {
                Duration::from_millis(50)
            })
            .await;
            let (closed_tx, closed_rx) = oneshot::channel();
            let (entered_tx, entered_rx) = oneshot::channel();
            let response = body(
                accounting,
                permit,
                Some(Bytes::from_static(b"DONE")),
                move |accounting| {
                    Box::pin(async_stream::stream! {
                        let _closed = Closed(Some(closed_tx));
                        // Terminal counts can race a timeout blocked on downstream delivery.
                        accounting.observe(&UsageV2 { tokens_in: 10, tokens_out: 7, completeness: UsageCompleteness::Final, ..Default::default() });
                        entered_tx.send(()).unwrap();
                        yield Ok(Bytes::from(vec![b'x'; FRAME_BYTES * (QUEUE_FRAMES + 2)]));
                        panic!("full queue must prevent a second source poll");
                    })
                },
            );
            within(entered_rx).await.unwrap();
            if disconnect {
                drop(response);
            } else {
                within(closed_rx).await.unwrap();
                let mut response = response.into_data_stream();
                assert!(within(response.next()).await.unwrap().is_err());
                assert!(within(response.next()).await.is_none());
                within(state.lifecycle.wait_idle()).await;
                assert_eq!(sink.events()[0].outcome.as_deref(), Some("timeout"));
                assert_eq!(slots.available_permits(), 1);
                assert_eq!(state.ledger.lock().unwrap().spent("scope"), 17);
                assert_eq!(sink.events().len(), 1);
                assert_eq!(
                    sink.events()[0].usage_completeness,
                    UsageCompleteness::Final
                );
                continue;
            }
            within(closed_rx).await.unwrap();
            within(state.lifecycle.wait_idle()).await;
            assert_eq!(slots.available_permits(), 1);
            assert_eq!(sink.events().len(), 1);
            assert_eq!(
                sink.events()[0].usage_completeness,
                UsageCompleteness::Final
            );
            assert_eq!(state.ledger.lock().unwrap().spent("scope"), 17);
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn shutdown_waits_for_grace_and_blocked_settlement_keeps_ownership() {
        let Fixture {
            accounting,
            state,
            sink,
            permit,
            slots,
        } = Fixture::new(Duration::from_secs(30)).await;
        let (locked_tx, locked_rx) = oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let holder_state = state.clone();
        let holder = std::thread::spawn(move || {
            let _ledger = holder_state.ledger.lock().unwrap();
            locked_tx.send(()).unwrap();
            let _ = release_rx.recv();
        });
        locked_rx.await.unwrap();
        let (closed_tx, mut closed_rx) = oneshot::channel();
        let (entered_tx, entered_rx) = oneshot::channel();
        let response = body(accounting, permit, None, move |_| {
            Box::pin(async_stream::stream! {
                let _closed = Closed(Some(closed_tx));
                entered_tx.send(()).unwrap();
                std::future::pending::<()>().await;
                yield Ok(Bytes::new());
            })
        });
        within(entered_rx).await.unwrap();
        state.lifecycle.begin_quiesce(Duration::from_millis(100));
        assert!(
            tokio::time::timeout(Duration::from_millis(20), &mut closed_rx)
                .await
                .is_err(),
            "quiescence must allow admitted streams their grace"
        );
        within(closed_rx).await.unwrap();
        let mut response = response.into_data_stream();
        assert!(
            within(response.next()).await.unwrap().is_err(),
            "settlement must not delay the transport error"
        );
        assert_eq!(state.lifecycle.active_operations(), 1);
        assert_eq!(slots.available_permits(), 0);
        assert!(sink.is_empty());
        release_tx.send(()).unwrap();
        within(state.lifecycle.wait_idle()).await;
        holder.join().unwrap();
        assert_eq!(sink.events().len(), 1);
        assert_eq!(slots.available_permits(), 1);
    }

    #[tokio::test]
    async fn success_drains_all_bytes_and_tail_then_settles_once() {
        let Fixture {
            accounting,
            state,
            sink,
            permit,
            ..
        } = Fixture::new(Duration::from_secs(2)).await;
        let payload = vec![b'x'; FRAME_BYTES * (QUEUE_FRAMES + 1) + 5];
        let expected = [payload.as_slice(), b"DONE"].concat();
        let response = body(
            accounting,
            permit,
            Some(Bytes::from_static(b"DONE")),
            move |accounting| {
                Box::pin(async_stream::stream! {
                    yield Ok(Bytes::from(payload));
                    accounting.set_outcome("success");
                })
            },
        );
        let bytes = within(axum::body::to_bytes(response, usize::MAX))
            .await
            .unwrap();
        assert_eq!(bytes.as_ref(), expected);
        within(state.lifecycle.wait_idle()).await;
        assert_eq!(sink.events().len(), 1);
    }

    #[tokio::test]
    async fn producer_panic_and_post_setup_lease_failure_are_explicit_once() {
        for expired in [false, true] {
            let Fixture {
                mut accounting,
                state,
                sink,
                permit,
                ..
            } = Fixture::new(Duration::from_secs(2)).await;
            if expired {
                accounting.reservation.as_mut().unwrap().expires_at =
                    time::OffsetDateTime::now_utc();
            }
            let response = body(accounting, permit, None, move |_| {
                Box::pin(async_stream::stream! {
                    assert!(!expired, "unfittable lease must not poll the source");
                    panic!("injected producer panic");
                    #[allow(unreachable_code)]
                    { yield Ok(Bytes::new()); }
                })
            });
            let mut response = response.into_data_stream();
            let error = within(response.next())
                .await
                .unwrap()
                .unwrap_err()
                .to_string();
            assert!(
                error.contains(if expired {
                    "insufficient reservation"
                } else {
                    "producer panicked"
                }),
                "{error}"
            );
            within(state.lifecycle.wait_idle()).await;
            assert_eq!(sink.events().len(), 1);
            assert_eq!(
                sink.events()[0].usage_completeness,
                UsageCompleteness::Unavailable
            );
        }
        assert!(StreamBodyLifetime::new(Duration::ZERO).is_err());
        assert!(StreamBodyLifetime::new(Duration::from_secs(841)).is_err());
    }
}
