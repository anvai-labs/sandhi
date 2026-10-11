//! Response-owned qualification; no task, retry, persistence or settlement owner.
use super::RawChunkStream;
use crate::attempt::AttemptGuard;
use crate::stream_usage::{OpenAiStreamUsageObserver, StreamObservationError};
use crate::{AttemptOutcome, ParsedUsage, ProviderError};
use bytes::Bytes;
use futures_core::Stream;
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::sync::watch;

/// Observed evidence is independent of protocol completion and delivery outcome.
/// A qualification error preserves the first measurement for reconciliation;
/// it must not be ignored by a future settlement consumer.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct QualifiedStreamSnapshot {
    pub usage: Option<ParsedUsage>,
    pub protocol_done: bool,
    pub qualification_error: Option<StreamObservationError>,
    pub outcome: Option<AttemptOutcome>,
}

/// Read-only latest snapshot that survives stream drop. Reading copies the facts
/// and never exposes a watch borrow that could block publication. This is bounded
/// in-memory evidence, not durable storage or proof of origin cancellation. A
/// handle does not poll the stream or keep its upstream request alive.
#[derive(Clone)]
pub struct QualifiedStreamObservation(watch::Receiver<QualifiedStreamSnapshot>);

impl QualifiedStreamObservation {
    #[must_use]
    pub fn snapshot(&self) -> QualifiedStreamSnapshot {
        *self.0.borrow()
    }
}

pub(super) fn observe(
    inner: RawChunkStream,
    guard: Option<AttemptGuard>,
) -> (RawChunkStream, QualifiedStreamObservation) {
    let (sender, receiver) = watch::channel(QualifiedStreamSnapshot::default());
    (
        Box::pin(QualifiedStream {
            inner,
            observer: OpenAiStreamUsageObserver::new(),
            sender,
            guard,
            outcome: None,
        }),
        QualifiedStreamObservation(receiver),
    )
}

struct QualifiedStream {
    inner: RawChunkStream,
    observer: OpenAiStreamUsageObserver,
    sender: watch::Sender<QualifiedStreamSnapshot>,
    guard: Option<AttemptGuard>,
    outcome: Option<AttemptOutcome>,
}

impl QualifiedStream {
    fn publish(&self) {
        self.sender.send_replace(QualifiedStreamSnapshot {
            usage: self.observer.usage(),
            protocol_done: self.observer.is_done(),
            qualification_error: self.observer.error(),
            outcome: self.outcome,
        });
    }

    fn finish(&mut self, outcome: AttemptOutcome) {
        if self.outcome.is_some() {
            return;
        }
        self.outcome = Some(outcome);
        // Publish first: a full/disconnected diagnostic channel cannot lose the
        // response owner's measurement. Neither channel is a durable ledger.
        self.publish();
        if let Some(mut guard) = self.guard.take() {
            guard.finish_qualified_stream(outcome, self.observer.usage());
        }
        // Release the upstream immediately, even when the caller retains this
        // fused stream after a terminal error.
        self.inner = Box::pin(futures_util::stream::empty());
    }
}

impl Stream for QualifiedStream {
    type Item = Result<Bytes, ProviderError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.outcome.is_some() {
            return Poll::Ready(None);
        }
        match self.inner.as_mut().poll_next(cx) {
            Poll::Ready(Some(Ok(bytes))) => {
                // Qualification failures are explicit snapshot facts. Forward
                // even malformed bytes verbatim, never turn them into retryable
                // transport errors or silently substitute a second request.
                let _ = self.observer.push(&bytes);
                self.publish();
                Poll::Ready(Some(Ok(bytes)))
            }
            Poll::Ready(Some(Err(error))) => {
                self.finish(if matches!(error, ProviderError::Timeout(_)) {
                    AttemptOutcome::Timeout
                } else {
                    AttemptOutcome::TransportError
                });
                Poll::Ready(Some(Err(error)))
            }
            Poll::Ready(None) => {
                let outcome = if self.observer.finish().is_ok() {
                    AttemptOutcome::Success
                } else {
                    AttemptOutcome::IncompleteStream
                };
                self.finish(outcome);
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

impl Drop for QualifiedStream {
    fn drop(&mut self) {
        self.finish(self.guard.as_ref().map_or(
            AttemptOutcome::Cancelled,
            AttemptGuard::cancellation_outcome,
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AttemptContext, AttemptPhase};
    use futures_util::{stream, StreamExt};
    use sandhi_core::UsageCompleteness;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };
    use std::time::Duration;

    struct DropObservedStream(RawChunkStream, Arc<AtomicUsize>);

    impl Stream for DropObservedStream {
        type Item = Result<Bytes, ProviderError>;
        fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
            self.0.as_mut().poll_next(cx)
        }
    }

    impl Drop for DropObservedStream {
        fn drop(&mut self) {
            self.1.fetch_add(1, Ordering::SeqCst);
        }
    }

    const USAGE: &[u8] = b"id: untrusted\ndata: {\"object\":\"chat.completion.chunk\",\"choices\":[],\"usage\":{\"prompt_tokens\":6,\"completion_tokens\":5}}\n\n";
    const DONE: &[u8] = b"data: [DONE]\n\n";

    #[tokio::test]
    async fn qualified_stream_retains_evidence_across_terminal_paths() {
        for mode in [
            "unpolled",
            "usage_drop",
            "done_drop",
            "timeout_drop",
            "eof",
            "missing_done",
            "conflict",
            "malformed",
            "post_done",
            "transport",
            "idle",
        ] {
            let (context, receiver) = AttemptContext::channel("execution", 4).unwrap();
            let guard = context.begin("inferflux", Some("test")).unwrap();
            let mut chunks = vec![Ok(Bytes::from_static(USAGE))];
            if matches!(mode, "eof" | "done_drop" | "post_done") {
                chunks.push(Ok(Bytes::from_static(DONE)));
            }
            if mode == "conflict" {
                chunks.push(Ok(Bytes::from(
                    String::from_utf8_lossy(USAGE).replace(":6", ":7"),
                )));
            }
            if mode == "malformed" || mode == "post_done" {
                chunks.push(Ok(Bytes::from_static(b"data: nope\n\n")));
            }
            if mode == "transport" {
                chunks.push(Err(ProviderError::Transport("lost body".into())));
            }
            let expected_bytes: Vec<u8> = chunks
                .iter()
                .filter_map(|c| c.as_ref().ok())
                .flat_map(|b| b.iter().copied())
                .collect();
            let inner: RawChunkStream = if mode == "idle" {
                super::super::with_idle_timeout(
                    Box::pin(stream::iter(chunks).chain(stream::pending())),
                    Some(Duration::from_millis(1)),
                )
            } else {
                Box::pin(stream::iter(chunks))
            };
            let drops = Arc::new(AtomicUsize::new(0));
            let (mut stream, observation) = observe(
                Box::pin(DropObservedStream(inner, drops.clone())),
                Some(guard),
            );
            let mut forwarded = Vec::new();
            if mode != "unpolled" {
                let chunk = stream.next().await.unwrap().unwrap();
                forwarded.extend_from_slice(&chunk);
                let snapshot = observation.snapshot();
                assert_eq!(snapshot.usage.unwrap().tokens_in, 6);
                assert_eq!(snapshot.outcome, None, "usage is not delivery success");
                if mode == "done_drop" {
                    forwarded.extend_from_slice(&stream.next().await.unwrap().unwrap());
                    assert!(observation.snapshot().protocol_done);
                    assert_eq!(observation.snapshot().outcome, None);
                } else if !matches!(mode, "usage_drop" | "timeout_drop") {
                    while let Some(chunk) = stream.next().await {
                        if let Ok(chunk) = chunk {
                            forwarded.extend_from_slice(&chunk);
                        }
                    }
                    assert_eq!(forwarded, expected_bytes, "including malformed bytes");
                    assert!(stream.next().await.is_none(), "terminal stream is fused");
                    assert_eq!(
                        drops.load(Ordering::SeqCst),
                        1,
                        "release inner while retaining wrapper: {mode}"
                    );
                }
            }
            if mode == "timeout_drop" {
                context.mark_timeout();
            }
            drop(stream);
            assert_eq!(
                drops.load(Ordering::SeqCst),
                1,
                "exactly one release: {mode}"
            );
            let snapshot = observation.clone().snapshot();
            let expected = match mode {
                "unpolled" | "usage_drop" | "done_drop" => AttemptOutcome::Cancelled,
                "timeout_drop" | "idle" => AttemptOutcome::Timeout,
                "transport" => AttemptOutcome::TransportError,
                "eof" => AttemptOutcome::Success,
                _ => AttemptOutcome::IncompleteStream,
            };
            assert_eq!(snapshot.outcome, Some(expected), "{mode}");
            assert_eq!(snapshot.usage.is_some(), mode != "unpolled");
            assert_eq!(
                snapshot.qualification_error.is_some(),
                matches!(
                    mode,
                    "missing_done" | "conflict" | "malformed" | "post_done"
                )
            );
            let observations = receiver.drain();
            assert_eq!(observations.len(), 2, "one terminal: {mode}");
            assert_eq!(observations[0].attempt_id, observations[1].attempt_id);
            assert_eq!(observations[1].execution_id, "execution");
            assert!(
                matches!(&observations[1].phase, AttemptPhase::Terminal { outcome, usage, usage_completeness, .. }
                if *outcome == expected && *usage == snapshot.usage && *usage_completeness == if mode == "unpolled" { UsageCompleteness::Unavailable } else { UsageCompleteness::Final })
            );
        }
    }

    #[tokio::test]
    async fn qualified_snapshot_is_independent_of_diagnostic_delivery() {
        for disconnected in [false, true] {
            let (context, receiver) = AttemptContext::channel("execution", 1).unwrap();
            let guard = context.begin("openai", None).unwrap();
            if disconnected {
                drop(receiver);
            }
            let (mut stream, observation) = observe(
                Box::pin(stream::iter([Ok(Bytes::from_static(USAGE))])),
                Some(guard),
            );
            stream.next().await.unwrap().unwrap();
            drop(stream);
            assert_eq!(observation.snapshot().usage.unwrap().tokens_out, 5);
            assert_eq!(
                observation.snapshot().outcome,
                Some(AttemptOutcome::Cancelled)
            );
            assert_eq!(context.dropped_observations(), 1);
        }
        let (mut stream, observation) =
            observe(Box::pin(stream::iter([Ok(Bytes::from_static(DONE))])), None);
        while stream.next().await.is_some() {}
        assert_eq!(observation.snapshot().usage, None);
        assert_eq!(
            observation.snapshot().outcome,
            Some(AttemptOutcome::Success)
        );
    }
}
