//! Metering decorator (ADR-0001 §1): wrap any [`Provider`] so every call emits exactly one
//! neutral [`UsageEvent`] to a [`Sink`], assembled from the usage the adapter parsed **at the
//! source**. Composition order is `Metered(Resilient(Adapter))` — one event per *logical*
//! call (retries are transport internals), and a circuit-open or failed attempt emits nothing:
//! the meter reports **measured** usage only, never estimated.
//!
//! Streams are metered by a Drop-guarded wrapper: exactly one event across every termination —
//! clean end (terminal usage), mid-stream error (partial usage seen so far, matching the
//! proxy's precedent), caller abandonment (client disconnect), and never-polled drop.

use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};

use async_trait::async_trait;
use futures_util::Stream;
use sandhi_core::{Backend, Sink, UsageEvent};

use crate::{
    ByteStream, ParsedUsage, Provider, ProviderError, ProviderRequest, ProviderResponse,
    StreamChunk,
};

/// A [`Provider`] wrapped with usage-event emission. Attribution is read per-call from
/// [`ProviderRequest::attribution`].
pub struct MeteredProvider {
    inner: Arc<dyn Provider>,
    sink: Arc<dyn Sink>,
    backend: Backend,
}

impl MeteredProvider {
    pub fn new(inner: Arc<dyn Provider>, sink: Arc<dyn Sink>) -> Self {
        Self {
            inner,
            sink,
            backend: Backend::External,
        }
    }

    /// Mark events as self-hosted (vLLM / Ollama fleets) instead of the default `External`.
    #[must_use]
    pub fn with_backend(mut self, backend: Backend) -> Self {
        self.backend = backend;
        self
    }

    fn event_base(&self, req: &ProviderRequest) -> UsageEvent {
        UsageEvent::new(
            next_request_id(),
            now_rfc3339(),
            self.inner.slug(),
            &req.model,
            self.backend,
        )
        .with_attribution(
            req.attribution.virtual_key_id.clone(),
            req.attribution.subject_id.clone(),
            req.attribution.group_id.clone(),
        )
        .with_route(req.attribution.route.clone())
        .with_session(req.session_id.clone())
    }
}

#[async_trait]
impl Provider for MeteredProvider {
    fn slug(&self) -> &str {
        self.inner.slug()
    }

    async fn complete(&self, req: ProviderRequest) -> Result<ProviderResponse, ProviderError> {
        let base = self.event_base(&req);
        // On Err: no event — no trustworthy counts exist ("measured, never estimated").
        let resp = self.inner.complete(req).await?;
        self.sink.emit(&resp.usage.apply(base).with_measurement(
            sandhi_core::UsageCompleteness::Final,
            resp.attempts,
            Some("success".into()),
            None,
        ));
        Ok(resp)
    }

    async fn stream(&self, req: ProviderRequest) -> Result<ByteStream, ProviderError> {
        let base = self.event_base(&req);
        let inner = self.inner.stream(req).await?; // setup failure: no event
        Ok(Box::pin(MeteredStream {
            inner,
            pending: Some(PendingEvent {
                base,
                usage: ParsedUsage::default(),
                usage_seen: false,
                attempts: 1,
            }),
            sink: Arc::clone(&self.sink),
        }))
    }
}

struct PendingEvent {
    base: UsageEvent,
    usage: ParsedUsage,
    usage_seen: bool,
    attempts: u32,
}

/// Passes items through verbatim while capturing the (terminal) usage; emits exactly once via
/// take-semantics on `pending` — on clean end, first in-stream error, or `Drop`, whichever
/// comes first. `Sink::emit` is sync and best-effort (sink.rs), so calling it from `Drop` is
/// safe.
struct MeteredStream {
    inner: ByteStream,
    pending: Option<PendingEvent>,
    sink: Arc<dyn Sink>,
}

impl MeteredStream {
    fn emit_once(&mut self, outcome: &str) {
        if let Some(pending) = self.pending.take() {
            let completeness = if pending.usage_seen && outcome == "success" {
                sandhi_core::UsageCompleteness::Final
            } else if pending.usage_seen {
                sandhi_core::UsageCompleteness::Partial
            } else {
                sandhi_core::UsageCompleteness::Unavailable
            };
            self.sink
                .emit(&pending.usage.apply(pending.base).with_measurement(
                    completeness,
                    pending.attempts,
                    Some(outcome.into()),
                    None,
                ));
        }
    }
}

impl Stream for MeteredStream {
    type Item = Result<StreamChunk, ProviderError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match self.inner.as_mut().poll_next(cx) {
            Poll::Ready(Some(Ok(chunk))) => {
                if let Some(pending) = self.pending.as_mut() {
                    pending.attempts = chunk.attempts;
                    if let Some(usage) = &chunk.usage {
                        pending.usage = *usage;
                        pending.usage_seen = true;
                    }
                }
                Poll::Ready(Some(Ok(chunk)))
            }
            Poll::Ready(Some(Err(e))) => {
                // Mid-stream failure: whatever usage we saw is still metered (partial counts
                // are measured counts — e.g. Anthropic's tokens_in arrives on message_start).
                self.emit_once("error");
                Poll::Ready(Some(Err(e)))
            }
            Poll::Ready(None) => {
                self.emit_once("success");
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

impl Drop for MeteredStream {
    fn drop(&mut self) {
        // Caller abandonment (client disconnect) or never-polled drop: still exactly one event.
        self.emit_once("cancelled");
    }
}

fn now_rfc3339() -> String {
    use time::format_description::well_known::Rfc3339;
    time::OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_default()
}

fn next_request_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    format!("req_{millis}_{n}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Attribution, ResilientProvider};
    use futures_util::{stream, StreamExt};
    use sandhi_core::InMemorySink;
    use std::collections::VecDeque;
    use std::sync::Mutex;
    use std::time::Duration;

    struct Scripted {
        queue: Mutex<VecDeque<Result<ProviderResponse, ProviderError>>>,
    }

    impl Scripted {
        fn new(results: Vec<Result<ProviderResponse, ProviderError>>) -> Arc<Self> {
            Arc::new(Self {
                queue: Mutex::new(results.into()),
            })
        }
    }

    fn ok_resp() -> ProviderResponse {
        ProviderResponse {
            status: 200,
            body: serde_json::json!({}),
            usage: ParsedUsage {
                tokens_in: 100,
                tokens_out: 20,
                cache_creation_tokens: 5,
                cache_read_tokens: 60,
            },
            attempts: 1,
        }
    }

    #[async_trait]
    impl Provider for Scripted {
        fn slug(&self) -> &str {
            "scripted"
        }
        async fn complete(&self, _req: ProviderRequest) -> Result<ProviderResponse, ProviderError> {
            self.queue
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(Err(ProviderError::Transport("exhausted".into())))
        }
        async fn stream(&self, _req: ProviderRequest) -> Result<ByteStream, ProviderError> {
            unreachable!("stream tests use StreamOnce")
        }
    }

    /// Serves one pre-built stream, then panics if reused.
    struct StreamOnce(Mutex<Option<Result<ByteStream, ProviderError>>>);

    #[async_trait]
    impl Provider for StreamOnce {
        fn slug(&self) -> &str {
            "stream-once"
        }
        async fn complete(&self, _req: ProviderRequest) -> Result<ProviderResponse, ProviderError> {
            unreachable!()
        }
        async fn stream(&self, _req: ProviderRequest) -> Result<ByteStream, ProviderError> {
            self.0.lock().unwrap().take().unwrap()
        }
    }

    fn attributed_req() -> ProviderRequest {
        ProviderRequest::new("m1", serde_json::json!({}))
            .with_session(Some("conv_9".into()))
            .with_attribution(Attribution {
                virtual_key_id: Some("vk_1".into()),
                subject_id: Some("alice".into()),
                group_id: Some("platform".into()),
                route: Some("/v1/chat/completions".into()),
            })
    }

    fn chunk_with(data: &str, usage: Option<ParsedUsage>) -> StreamChunk {
        StreamChunk {
            data: bytes::Bytes::copy_from_slice(data.as_bytes()),
            usage,
            attempts: 1,
        }
    }

    #[tokio::test]
    async fn complete_emits_exactly_one_event_with_attribution_and_usage() {
        let sink = Arc::new(InMemorySink::new());
        let p = MeteredProvider::new(Scripted::new(vec![Ok(ok_resp())]), sink.clone());
        p.complete(attributed_req()).await.unwrap();

        let events = sink.events();
        assert_eq!(events.len(), 1);
        let ev = &events[0];
        assert_eq!(ev.provider, "scripted");
        assert_eq!(ev.model, "m1");
        assert_eq!(ev.virtual_key_id.as_deref(), Some("vk_1"));
        assert_eq!(ev.subject_id.as_deref(), Some("alice"));
        assert_eq!(ev.group_id.as_deref(), Some("platform"));
        assert_eq!(ev.route.as_deref(), Some("/v1/chat/completions"));
        assert_eq!(ev.session_id.as_deref(), Some("conv_9"));
        assert_eq!(ev.tokens_in, 100);
        assert_eq!(ev.tokens_out, 20);
        assert_eq!(ev.cache_creation_tokens, 5);
        assert_eq!(ev.cache_read_tokens, 60);
    }

    #[tokio::test]
    async fn failed_complete_emits_no_event() {
        let sink = Arc::new(InMemorySink::new());
        let p = MeteredProvider::new(
            Scripted::new(vec![Err(ProviderError::Upstream(500))]),
            sink.clone(),
        );
        assert!(p.complete(attributed_req()).await.is_err());
        assert_eq!(sink.len(), 0, "no measured usage => no event");
    }

    #[tokio::test]
    async fn retried_then_success_emits_one_event() {
        // Metered(Resilient(...)): retries are transport internals — one event per logical call.
        let sink = Arc::new(InMemorySink::new());
        let scripted = Scripted::new(vec![
            Err(ProviderError::Transport("blip".into())),
            Ok(ok_resp()),
        ]);
        let resilient =
            Arc::new(ResilientProvider::new(scripted).with_retry(2, Duration::from_millis(1)));
        let p = MeteredProvider::new(resilient, sink.clone());
        assert!(p.complete(attributed_req()).await.is_ok());
        assert_eq!(sink.len(), 1);
        assert_eq!(sink.events()[0].attempts, 2);
    }

    #[tokio::test]
    async fn circuit_open_emits_no_event() {
        let sink = Arc::new(InMemorySink::new());
        let scripted = Scripted::new(vec![
            Err(ProviderError::Transport("x".into())),
            Ok(ok_resp()),
        ]);
        let resilient = Arc::new(
            ResilientProvider::new(scripted)
                .with_retry(0, Duration::from_millis(1))
                .with_circuit(1, Duration::from_secs(60)),
        );
        let p = MeteredProvider::new(resilient, sink.clone());
        assert!(p.complete(attributed_req()).await.is_err()); // opens the circuit; no event
        assert!(matches!(
            p.complete(attributed_req()).await,
            Err(ProviderError::CircuitOpen)
        ));
        assert_eq!(sink.len(), 0, "zero tokens moved => zero usage events");
    }

    #[tokio::test]
    async fn unattributed_request_still_emits() {
        let sink = Arc::new(InMemorySink::new());
        let p = MeteredProvider::new(Scripted::new(vec![Ok(ok_resp())]), sink.clone());
        p.complete(ProviderRequest::new("m1", serde_json::json!({})))
            .await
            .unwrap();
        let ev = &sink.events()[0];
        assert_eq!(sink.len(), 1);
        assert!(ev.virtual_key_id.is_none() && ev.subject_id.is_none() && ev.group_id.is_none());
    }

    fn stream_provider(items: Vec<Result<StreamChunk, ProviderError>>) -> Arc<StreamOnce> {
        let s: ByteStream = Box::pin(stream::iter(items));
        Arc::new(StreamOnce(Mutex::new(Some(Ok(s)))))
    }

    #[tokio::test]
    async fn stream_emits_one_event_with_terminal_usage() {
        let sink = Arc::new(InMemorySink::new());
        let terminal = ParsedUsage {
            tokens_in: 10,
            tokens_out: 7,
            ..Default::default()
        };
        let p = MeteredProvider::new(
            stream_provider(vec![
                Ok(chunk_with("a", None)),
                Ok(chunk_with("", Some(terminal))),
            ]),
            sink.clone(),
        );
        let mut s = p.stream(attributed_req()).await.unwrap();
        while s.next().await.is_some() {}
        assert_eq!(sink.len(), 1);
        let ev = &sink.events()[0];
        assert_eq!((ev.tokens_in, ev.tokens_out), (10, 7));
        assert_eq!(ev.subject_id.as_deref(), Some("alice"));
    }

    #[tokio::test]
    async fn mid_stream_error_emits_one_event_with_partial_usage() {
        let sink = Arc::new(InMemorySink::new());
        let partial = ParsedUsage {
            tokens_in: 42, // e.g. Anthropic message_start already delivered input counts
            ..Default::default()
        };
        let p = MeteredProvider::new(
            stream_provider(vec![
                Ok(chunk_with("a", Some(partial))),
                Err(ProviderError::Transport("cut".into())),
            ]),
            sink.clone(),
        );
        let mut s = p.stream(attributed_req()).await.unwrap();
        assert!(s.next().await.unwrap().is_ok());
        assert!(s.next().await.unwrap().is_err());
        assert_eq!(sink.len(), 1, "partial usage is measured usage");
        assert_eq!(sink.events()[0].tokens_in, 42);
        drop(s);
        assert_eq!(
            sink.len(),
            1,
            "drop after in-stream emit must not double-emit"
        );
    }

    #[tokio::test]
    async fn dropped_stream_emits_exactly_one_event() {
        let sink = Arc::new(InMemorySink::new());
        let p = MeteredProvider::new(
            stream_provider(vec![Ok(chunk_with("a", None)), Ok(chunk_with("b", None))]),
            sink.clone(),
        );
        let mut s = p.stream(attributed_req()).await.unwrap();
        let _ = s.next().await; // client reads one chunk, then disconnects
        drop(s);
        assert_eq!(sink.len(), 1, "abandonment must still meter (Drop guard)");
    }

    #[tokio::test]
    async fn fully_drained_stream_emits_exactly_once() {
        let sink = Arc::new(InMemorySink::new());
        let p = MeteredProvider::new(
            stream_provider(vec![Ok(chunk_with("a", None))]),
            sink.clone(),
        );
        let mut s = p.stream(attributed_req()).await.unwrap();
        while s.next().await.is_some() {}
        assert!(s.next().await.is_none()); // poll past the end
        drop(s);
        assert_eq!(sink.len(), 1);
    }

    #[tokio::test]
    async fn never_polled_stream_still_emits_on_drop() {
        let sink = Arc::new(InMemorySink::new());
        let p = MeteredProvider::new(stream_provider(vec![]), sink.clone());
        let s = p.stream(attributed_req()).await.unwrap();
        drop(s);
        assert_eq!(sink.len(), 1);
    }
}
