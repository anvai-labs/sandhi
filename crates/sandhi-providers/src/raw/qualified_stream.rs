//! Response-owned qualification; no task, retry, persistence or settlement owner.
use super::RawChunkStream;
use crate::attempt::AttemptGuard;
use crate::stream_usage::{ProtocolStreamUsageObserver, StreamObservationError};
use crate::{AttemptOutcome, ParsedUsage, ProviderError};
use bytes::Bytes;
use futures_core::Stream;
#[cfg(test)]
use sandhi_core::EndpointFamilyV1;
use sandhi_core::UsageV2;
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

/// Canonical evidence, including partial progress and reported-zero fields.
/// `snapshot` retains the original copyable projection for compatibility.
/// Protocol completion and delivery success do not make partial usage final.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct QualifiedStreamEvidence {
    pub gateway_peer: bool,
    pub gateway_failure: Option<crate::GatewayFailure>,
    /// Field availability is metadata; it neither qualifies counts nor proves a hit.
    pub cache_read_observation: Option<sandhi_core::CacheReadObservation>,
    pub snapshot: QualifiedStreamSnapshot,
    pub usage: Option<UsageV2>,
}

/// Read-only latest snapshot that survives stream drop. Reading copies the facts
/// and never exposes a watch borrow that could block publication. This is bounded
/// in-memory evidence, not durable storage or proof of origin cancellation. A
/// handle does not poll the stream or keep its upstream request alive.
#[derive(Clone)]
pub struct QualifiedStreamObservation(watch::Receiver<QualifiedStreamEvidence>);

impl QualifiedStreamObservation {
    #[must_use]
    pub fn snapshot(&self) -> QualifiedStreamSnapshot {
        self.0.borrow().snapshot
    }

    #[must_use]
    pub fn cache_read_observation(&self) -> Option<sandhi_core::CacheReadObservation> {
        self.0.borrow().cache_read_observation
    }

    #[must_use]
    pub fn evidence(&self) -> QualifiedStreamEvidence {
        self.0.borrow().clone()
    }
}

#[cfg(test)]
pub(super) fn observe(
    inner: RawChunkStream,
    guard: Option<AttemptGuard>,
) -> (RawChunkStream, QualifiedStreamObservation) {
    observe_endpoint(inner, guard, EndpointFamilyV1::OpenaiChatCompletions)
        .expect("Chat stream qualification is supported")
}

#[cfg(test)]
pub(super) fn observe_endpoint(
    inner: RawChunkStream,
    guard: Option<AttemptGuard>,
    endpoint: EndpointFamilyV1,
) -> Result<(RawChunkStream, QualifiedStreamObservation), StreamObservationError> {
    Ok(observe_response(
        inner,
        guard,
        ProtocolStreamUsageObserver::for_endpoint(endpoint)?,
        None,
    ))
}

pub(crate) fn observe_response(
    inner: RawChunkStream,
    guard: Option<AttemptGuard>,
    observer: ProtocolStreamUsageObserver,
    request_id: Option<String>,
) -> (RawChunkStream, QualifiedStreamObservation) {
    let (sender, receiver) = watch::channel(QualifiedStreamEvidence::default());
    (
        Box::pin(QualifiedStream {
            inner,
            observer,
            sender,
            guard,
            outcome: None,
            request_id: request_id.map(|id| crate::attempt::bounded_label(&id, 256)),
        }),
        QualifiedStreamObservation(receiver),
    )
}

struct QualifiedStream {
    inner: RawChunkStream,
    observer: ProtocolStreamUsageObserver,
    sender: watch::Sender<QualifiedStreamEvidence>,
    guard: Option<AttemptGuard>,
    outcome: Option<AttemptOutcome>,
    request_id: Option<String>,
}

impl QualifiedStream {
    fn publish(&self) {
        let mut usage = self.observer.canonical_usage();
        if let Some(usage) = &mut usage {
            // This response-scoped observer represents one physical HTTP
            // stream. Qualified setup forbids inference retries; absence of
            // canonical decoder events cannot turn that response into zero.
            usage.attempts = 1;
            usage.upstream_request_id.clone_from(&self.request_id);
        }
        let gateway_failure = self.observer.gateway_failure().map(|mut failure| {
            if self.request_id.is_some() {
                failure.request_id.clone_from(&self.request_id);
            }
            failure
        });
        self.sender.send_replace(QualifiedStreamEvidence {
            gateway_peer: self.observer.gateway_peer(),
            gateway_failure,
            cache_read_observation: self.observer.cache_read_observation(),
            snapshot: QualifiedStreamSnapshot {
                usage: self.observer.usage(),
                protocol_done: self.observer.is_done(),
                qualification_error: self.observer.error(),
                outcome: self.outcome,
            },
            usage,
        });
    }

    fn finish(&mut self, outcome: AttemptOutcome) {
        if self.outcome.is_some() {
            return;
        }
        self.observer.close_on_abort();
        self.outcome = Some(outcome);
        // Publish first: a full/disconnected diagnostic channel cannot lose the
        // response owner's measurement. Neither channel is a durable ledger.
        self.publish();
        if let Some(mut guard) = self.guard.take() {
            guard.finish_qualified_stream(outcome, self.observer.canonical_usage().as_ref());
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
                let qualified = self.observer.finish().is_ok();
                let outcome = if self.observer.provider_failed() {
                    AttemptOutcome::ProviderRejected
                } else if qualified {
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
        // An already observed provider error is a known rejection even when
        // the consumer stops after surfacing it. It is not an ambiguous local
        // cancellation, and it cannot promote partial counters to final ones.
        let outcome = if self.observer.provider_failed() {
            AttemptOutcome::ProviderRejected
        } else {
            self.guard.as_ref().map_or(
                AttemptOutcome::Cancelled,
                AttemptGuard::cancellation_outcome,
            )
        };
        self.finish(outcome);
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
    async fn gemini_response_owner_requires_eof_and_releases_once_on_every_path() {
        let terminal = b"data: {\"candidates\":[{\"finishReason\":\"STOP\"}],\"usageMetadata\":{\"promptTokenCount\":11,\"cachedContentTokenCount\":8,\"candidatesTokenCount\":4,\"thoughtsTokenCount\":2,\"totalTokenCount\":17}}\n\n";
        for mode in [
            "unpolled",
            "terminal_drop",
            "timeout",
            "transport",
            "truncated",
            "provider_error",
            "eof",
        ] {
            let (context, receiver) =
                AttemptContext::single_attempt("gemini-owner", "b".repeat(64)).unwrap();
            let guard = context.begin_checked("fixture", Some("fixture")).unwrap();
            let mut chunks = vec![Ok(Bytes::from_static(terminal))];
            match mode {
                "transport" => chunks.push(Err(ProviderError::Transport("lost".into()))),
                "truncated" => chunks.push(Ok(Bytes::from_static(b"data: {"))),
                "provider_error" => chunks.push(Ok(Bytes::from_static(
                    b"data: {\"error\":{\"code\":503}}\n\n",
                ))),
                _ => {}
            }
            let drops = Arc::new(AtomicUsize::new(0));
            let (mut stream, observation) = observe_endpoint(
                Box::pin(DropObservedStream(
                    Box::pin(stream::iter(chunks)),
                    drops.clone(),
                )),
                guard,
                EndpointFamilyV1::GeminiGenerateContent,
            )
            .unwrap();
            if mode != "unpolled" {
                assert_eq!(
                    stream.next().await.unwrap().unwrap(),
                    Bytes::from_static(terminal)
                );
                assert!(observation.snapshot().usage.is_none());
                assert!(!observation.snapshot().protocol_done);
                assert_eq!(
                    observation.evidence().usage.unwrap().completeness,
                    UsageCompleteness::Partial
                );
                if !matches!(mode, "terminal_drop" | "timeout") {
                    while stream.next().await.is_some() {}
                    assert_eq!(
                        drops.load(Ordering::SeqCst),
                        1,
                        "release before wrapper drop: {mode}"
                    );
                }
            }
            if mode == "timeout" {
                context.mark_timeout();
            }
            drop(stream);
            assert_eq!(drops.load(Ordering::SeqCst), 1, "{mode}");
            let outcome = match mode {
                "eof" => AttemptOutcome::Success,
                "timeout" => AttemptOutcome::Timeout,
                "transport" => AttemptOutcome::TransportError,
                "truncated" => AttemptOutcome::IncompleteStream,
                "provider_error" => AttemptOutcome::ProviderRejected,
                _ => AttemptOutcome::Cancelled,
            };
            let completeness = match mode {
                "unpolled" => UsageCompleteness::Unavailable,
                "eof" => UsageCompleteness::Final,
                _ => UsageCompleteness::Partial,
            };
            let evidence = observation.evidence();
            assert_eq!(evidence.snapshot.outcome, Some(outcome), "{mode}");
            assert_eq!(evidence.snapshot.protocol_done, mode == "eof", "{mode}");
            assert_eq!(evidence.snapshot.usage.is_some(), mode == "eof", "{mode}");
            if let Some(usage) = &evidence.usage {
                assert_eq!(usage.completeness, completeness, "{mode}");
                assert_eq!(
                    (usage.tokens_in, usage.cache_read_tokens, usage.tokens_out),
                    (3, 8, 4)
                );
                assert_eq!(usage.reasoning_tokens, Some(2));
            } else {
                assert_eq!(mode, "unpolled");
            }
            let events = receiver.drain();
            assert_eq!(
                events.len(),
                2,
                "one attempt start and one terminal: {mode}"
            );
            assert_eq!(events[0].attempt_id, events[1].attempt_id);
            assert!(
                matches!(&events[1].phase, AttemptPhase::Terminal{
                outcome:actual,usage,usage_completeness,..}
                if *actual==outcome && *usage_completeness==completeness
                && *usage==evidence.usage.as_ref().map(Into::into)),
                "{mode}"
            );
        }
    }

    #[tokio::test]
    async fn responses_stream_keeps_canonical_evidence_and_one_terminal_across_drop_paths() {
        let progress = b"data: {\"type\":\"response.in_progress\",\"response\":{\"id\":\"resp\",\"usage\":{\"input_tokens\":10,\"input_tokens_details\":{\"cached_tokens\":6},\"output_tokens\":2,\"output_tokens_details\":{\"reasoning_tokens\":0}}}}\n\n";
        for mode in [
            "unpolled",
            "progress_drop",
            "terminal_drop",
            "partial_tail_drop",
            "comment_tail_drop",
            "eof",
            "failed",
            "error",
            "truncated",
            "timeout",
        ] {
            let (context, receiver) =
                AttemptContext::single_attempt("execution", "a".repeat(64)).unwrap();
            let guard = context.begin_checked("models", Some("fixture")).unwrap();
            let status = if mode == "failed" {
                "failed"
            } else {
                "completed"
            };
            let mut terminal = format!(
                "data: {}\n\n",
                serde_json::json!({"type":format!("response.{status}"),"response":{
                "id":"resp","status":status,"usage":{"input_tokens":10,"input_tokens_details":{"cached_tokens":6},
                "output_tokens":4,"output_tokens_details":{"reasoning_tokens":0}}}})
            );
            if mode == "partial_tail_drop" {
                terminal.push_str("data: {");
            }
            if mode == "comment_tail_drop" {
                terminal.push_str(": keepalive padding");
            }
            let mut chunks = vec![Ok(Bytes::from_static(progress))];
            if !matches!(mode, "unpolled" | "progress_drop" | "timeout") {
                chunks.push(Ok(Bytes::from(terminal)));
            }
            if mode == "error" {
                chunks.push(Err(ProviderError::Transport("lost".into())));
            }
            if mode == "truncated" {
                chunks.push(Ok(Bytes::from_static(b"data: {")));
            }
            let drops = Arc::new(AtomicUsize::new(0));
            let (mut stream, observation) = observe_endpoint(
                Box::pin(DropObservedStream(
                    Box::pin(stream::iter(chunks)),
                    drops.clone(),
                )),
                guard,
                EndpointFamilyV1::OpenaiResponses,
            )
            .unwrap();
            if mode != "unpolled" {
                stream.next().await.unwrap().unwrap();
                assert!(observation.snapshot().usage.is_none());
                assert_eq!(
                    observation.evidence().usage.unwrap().completeness,
                    UsageCompleteness::Partial
                );
                if matches!(
                    mode,
                    "terminal_drop" | "partial_tail_drop" | "comment_tail_drop"
                ) {
                    stream.next().await.unwrap().unwrap();
                } else if !matches!(mode, "progress_drop" | "timeout") {
                    while stream.next().await.is_some() {}
                }
            }
            if mode == "timeout" {
                context.mark_timeout();
            }
            drop(stream);
            assert_eq!(drops.load(Ordering::SeqCst), 1, "{mode}");
            let evidence = observation.evidence();
            let outcome = match mode {
                "eof" => AttemptOutcome::Success,
                "failed" => AttemptOutcome::ProviderRejected,
                "error" => AttemptOutcome::TransportError,
                "truncated" => AttemptOutcome::IncompleteStream,
                "timeout" => AttemptOutcome::Timeout,
                _ => AttemptOutcome::Cancelled,
            };
            assert_eq!(evidence.snapshot.outcome, Some(outcome), "{mode}");
            let completeness = match mode {
                "unpolled" => UsageCompleteness::Unavailable,
                "progress_drop" | "timeout" | "truncated" | "partial_tail_drop" => {
                    UsageCompleteness::Partial
                }
                _ => UsageCompleteness::Final,
            };
            if let Some(usage) = &evidence.usage {
                assert_eq!(usage.completeness, completeness, "{mode}");
                assert_eq!(usage.attempts, 1, "one response even on {mode}");
                assert_eq!(usage.reasoning_tokens, Some(0));
                assert_eq!(usage.cache_read_tokens, 6);
                assert_eq!(
                    usage.tokens_out,
                    if matches!(mode, "progress_drop" | "timeout") {
                        2
                    } else {
                        4
                    }
                );
            } else {
                assert_eq!(mode, "unpolled");
            }
            let events = receiver.drain();
            assert_eq!(events.len(), 2, "{mode}");
            assert_eq!(events[0].attempt_id, events[1].attempt_id);
            assert!(
                matches!(&events[1].phase,AttemptPhase::Terminal{outcome:actual,usage,usage_completeness,..}
                if *actual == outcome && *usage_completeness == completeness && *usage == evidence.usage.as_ref().map(Into::into)),
                "{mode}"
            );
        }
    }

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
                if *outcome == expected && *usage == snapshot.usage && *usage_completeness == if mode == "unpolled" { UsageCompleteness::Unavailable } else if snapshot.qualification_error.is_some() { UsageCompleteness::Partial } else { UsageCompleteness::Final })
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
