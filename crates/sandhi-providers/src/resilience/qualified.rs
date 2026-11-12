//! Retry-free qualified SSE setup and delivery-specific bounds.
use super::{bounded_until, IdleTimeout, ResilientProvider};
use crate::{
    AttemptContext, BufferedDeadline, ByteStream, ProviderError, ProviderRequest,
    QualifiedProviderStream, QualifiedStreamDelivery, StreamChunk,
};
use futures_util::Stream;
use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};

pub(super) async fn stream(
    provider: &ResilientProvider,
    mut request: ProviderRequest,
    delivery: QualifiedStreamDelivery,
) -> Result<QualifiedProviderStream, ProviderError> {
    if provider.retry.max_retries != 0 {
        return Err(ProviderError::InvalidRequest(
            "qualified streaming requires retries disabled".into(),
        ));
    }
    if !provider.breaker.allow() {
        return Err(ProviderError::CircuitOpen);
    }
    request.attempt_context = request
        .attempt_context
        .as_ref()
        .map(AttemptContext::fresh_call);
    let context = request.attempt_context.clone();
    let policy = crate::StreamingDeadline::current();
    let whole = (delivery == QualifiedStreamDelivery::Buffered).then(|| {
        BufferedDeadline::current()
            .unwrap_or_else(|| BufferedDeadline::new(provider.timeouts.complete))
    });
    let mut setup = policy
        .map(|policy| policy.setup)
        .unwrap_or_else(|| BufferedDeadline::new(provider.timeouts.stream_setup));
    if let Some(whole) = whole.filter(|whole| whole.at < setup.at) {
        setup = whole;
    }
    match bounded_until(
        setup.timeout,
        setup.at,
        context.as_ref(),
        provider.inner.stream_qualified(request, delivery),
    )
    .await
    {
        Ok(mut result) => {
            provider.breaker.record_success();
            if let Some(idle) = policy.map(|policy| policy.idle).or(provider.timeouts.idle) {
                result.stream = Box::pin(IdleTimeout::new(result.stream, idle, context.clone()));
            }
            if let Some(deadline) = whole {
                result.stream = Box::pin(AbsoluteTimeout {
                    inner: Some(result.stream),
                    deadline,
                    sleep: Box::pin(tokio::time::sleep_until(deadline.at)),
                    context,
                });
            }
            Ok(result)
        }
        Err(error) => {
            provider.breaker.record_failure();
            Err(error)
        }
    }
}

/// The complete buffered operation shares its original bound, including draining
/// bytes after a protocol terminal. Heartbeats cannot extend or reset it.
struct AbsoluteTimeout {
    inner: Option<ByteStream>,
    deadline: BufferedDeadline,
    sleep: Pin<Box<tokio::time::Sleep>>,
    context: Option<AttemptContext>,
}

impl Stream for AbsoluteTimeout {
    type Item = Result<StreamChunk, ProviderError>;
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.inner.is_none() {
            return Poll::Ready(None);
        }
        if tokio::time::Instant::now() >= self.deadline.at
            || self.sleep.as_mut().poll(cx).is_ready()
        {
            if let Some(context) = &self.context {
                context.mark_timeout();
            }
            self.inner.take();
            return Poll::Ready(Some(Err(ProviderError::Timeout(self.deadline.timeout))));
        }
        match self.inner.as_mut().unwrap().as_mut().poll_next(cx) {
            Poll::Ready(None) => {
                self.inner.take();
                Poll::Ready(None)
            }
            other => other,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        AttemptOutcome, AttemptPhase, Provider, ProviderResponse, ResilientProvider, TimeoutConfig,
    };
    use futures_util::StreamExt;
    use std::{
        sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        },
        time::Duration,
    };

    struct Heartbeats(Arc<AtomicUsize>, bool);
    #[async_trait::async_trait]
    impl Provider for Heartbeats {
        fn slug(&self) -> &str {
            "fixture"
        }
        async fn complete(&self, _: ProviderRequest) -> Result<ProviderResponse, ProviderError> {
            unreachable!()
        }
        async fn stream(&self, _: ProviderRequest) -> Result<ByteStream, ProviderError> {
            unreachable!()
        }
        async fn stream_qualified(
            &self,
            req: ProviderRequest,
            _: QualifiedStreamDelivery,
        ) -> Result<QualifiedProviderStream, ProviderError> {
            crate::attempt::observe_qualified_stream(req.attempt_context,self.slug(),Some(&req.model),
                sandhi_core::EndpointFamilyV1::OpenaiResponses, |facts| async move {
                    self.0.fetch_add(1,Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(5)).await;
                    let mut headers=http::HeaderMap::new();
                    headers.insert("x-request-id",http::HeaderValue::from_static("origin"));
                    facts.record_headers(200,&headers,None);
                    let kind=if self.1 {"response.completed"}else{"response.in_progress"};
                    let first=format!("data: {}\n\n",serde_json::json!({"type":kind,"response":{"usage":{"input_tokens":10,"output_tokens":2}}}));
                    let raw:crate::raw::RawChunkStream=Box::pin(futures_util::stream::once(async {Ok(bytes::Bytes::from(first))})
                        .chain(futures_util::stream::unfold((),|()|async {
                            tokio::time::sleep(Duration::from_millis(5)).await;
                            Some((Ok(bytes::Bytes::from_static(b": heartbeat\n\n")),()))
                        })));
                    Ok((raw,Some("origin".into())))
                }).await
        }
    }

    #[tokio::test(start_paused = true)]
    async fn qualified_buffered_deadline_includes_setup_and_cannot_be_reset_by_heartbeats() {
        for final_usage in [false, true] {
            for scoped in [false, true] {
                let sends = Arc::new(AtomicUsize::new(0));
                let provider =
                    ResilientProvider::new(Arc::new(Heartbeats(sends.clone(), final_usage)))
                        .with_timeouts(TimeoutConfig {
                            complete: Duration::from_millis(20),
                            stream_setup: Duration::from_millis(100),
                            idle: Some(Duration::from_millis(50)),
                        });
                let (context, receiver) =
                    AttemptContext::single_attempt("execution", "c".repeat(64)).unwrap();
                let started = tokio::time::Instant::now();
                let deadline = BufferedDeadline::new(Duration::from_millis(12));
                let run = async {
                    let mut response = provider
                        .stream_qualified(
                            ProviderRequest::new("fixture", serde_json::json!({}))
                                .with_attempt_context(context.clone()),
                            QualifiedStreamDelivery::Buffered,
                        )
                        .await
                        .unwrap();
                    let mut observed = 0;
                    while let Some(item) = response.stream.next().await {
                        match item {
                            Ok(_) => observed += 1,
                            Err(error) => {
                                assert!(
                                    matches!(error,ProviderError::Timeout(bound) if bound==Duration::from_millis(if scoped {12}else{20}))
                                );
                                break;
                            }
                        }
                    }
                    assert!(observed >= 2, "heartbeats flowed after usage");
                    assert!(response.stream.next().await.is_none());
                };
                if scoped {
                    deadline.scope(run).await;
                } else {
                    run.await;
                }
                assert_eq!(
                    tokio::time::Instant::now() - started,
                    Duration::from_millis(if scoped { 12 } else { 20 })
                );
                assert_eq!(sends.load(Ordering::SeqCst), 1);
                let evidence = context.qualified_stream_evidence().unwrap();
                assert_eq!(evidence.snapshot.outcome, Some(AttemptOutcome::Timeout));
                assert_eq!(
                    evidence.usage.unwrap().completeness,
                    if final_usage {
                        sandhi_core::UsageCompleteness::Final
                    } else {
                        sandhi_core::UsageCompleteness::Partial
                    }
                );
                let events = receiver.drain();
                assert_eq!(events.len(), 2);
                assert!(matches!(
                    events[1].phase,
                    AttemptPhase::Terminal {
                        outcome: AttemptOutcome::Timeout,
                        provider_status: Some(200),
                        ..
                    }
                ));
            }
        }
    }
}
