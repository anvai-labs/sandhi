//! Resilience decorator (AnvaiOps ADR-0047 D10): wrap any [`Provider`] with **retry** (on
//! transient failures, with exponential backoff) and a **circuit breaker** (fast-fail while an
//! upstream is failing). Because it implements [`Provider`], it composes transparently — the
//! proxy and bindings use a `ResilientProvider` exactly like a bare adapter.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use futures_util::Stream;
use tokio::time::sleep;

use crate::{ByteStream, Provider, ProviderError, ProviderRequest, ProviderResponse, StreamChunk};

/// Retry policy for transient failures.
#[derive(Debug, Clone)]
pub struct RetryConfig {
    /// Extra attempts after the first (0 = no retries).
    pub max_retries: u32,
    /// Base backoff; the delay before retry `n` is `base * 2^(n-1)`, capped at 30s.
    pub base_backoff: Duration,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_retries: 2,
            base_backoff: Duration::from_millis(200),
        }
    }
}

/// Per-call time bounds, applied uniformly by the decorator (the only seam covering every
/// adapter, including host-language ones). Orthogonal to [`RetryConfig`]: each retry attempt
/// gets a fresh bound, and a timed-out attempt is retryable.
#[derive(Debug, Clone)]
pub struct TimeoutConfig {
    /// Whole-call bound for `complete()`, per attempt. Deliberately tighter than SDK client
    /// defaults — a gateway should fail visibly and retryably; raise it via config if needed.
    pub complete: Duration,
    /// Bound on stream *setup* (POST → response headers), per attempt. Generation streams
    /// after headers, so this only limits time-to-first-byte, never a long generation.
    pub stream_setup: Duration,
    /// Max gap between stream items; `None` disables. Providers emit SSE keepalives every
    /// ~15–30s, so the default (90s) is several missed keepalives — a strong stall signal.
    /// A mid-stream idle timeout surfaces as an in-stream error and is never retried.
    pub idle: Option<Duration>,
}

impl Default for TimeoutConfig {
    fn default() -> Self {
        Self {
            complete: Duration::from_secs(120),
            stream_setup: Duration::from_secs(30),
            idle: Some(Duration::from_secs(90)),
        }
    }
}

/// A consecutive-failure circuit breaker. After `threshold` consecutive failures it **opens**
/// (fast-fail) for `cooldown`, then allows a single **half-open** trial: success closes it, a
/// failure re-opens it.
#[derive(Debug)]
pub struct CircuitBreaker {
    threshold: u32,
    cooldown: Duration,
    state: Mutex<BreakerState>,
}

#[derive(Debug, Default)]
struct BreakerState {
    consecutive_failures: u32,
    opened_at: Option<Instant>,
}

impl CircuitBreaker {
    pub fn new(threshold: u32, cooldown: Duration) -> Self {
        Self {
            threshold: threshold.max(1),
            cooldown,
            state: Mutex::new(BreakerState::default()),
        }
    }

    /// Is a call allowed right now? Closed → yes; open & within cooldown → no; open & cooldown
    /// elapsed → yes (a half-open trial).
    pub fn allow(&self) -> bool {
        let state = self.state.lock().unwrap();
        match state.opened_at {
            Some(opened) => opened.elapsed() >= self.cooldown,
            None => true,
        }
    }

    pub fn record_success(&self) {
        let mut state = self.state.lock().unwrap();
        state.consecutive_failures = 0;
        state.opened_at = None;
    }

    pub fn record_failure(&self) {
        let mut state = self.state.lock().unwrap();
        state.consecutive_failures += 1;
        if state.consecutive_failures >= self.threshold {
            state.opened_at = Some(Instant::now());
        }
    }

    /// True while the breaker is open and still within its cooldown window.
    pub fn is_open(&self) -> bool {
        !self.allow()
    }
}

/// A [`Provider`] wrapped with retry + circuit-breaking + per-call timeouts.
pub struct ResilientProvider {
    inner: Arc<dyn Provider>,
    retry: RetryConfig,
    breaker: Arc<CircuitBreaker>,
    timeouts: TimeoutConfig,
}

impl ResilientProvider {
    /// Sensible defaults: 2 retries (200ms base backoff), circuit opens after 5 consecutive
    /// failures, 30s cooldown, timeouts per [`TimeoutConfig::default`].
    pub fn new(inner: Arc<dyn Provider>) -> Self {
        Self {
            inner,
            retry: RetryConfig::default(),
            breaker: Arc::new(CircuitBreaker::new(5, Duration::from_secs(30))),
            timeouts: TimeoutConfig::default(),
        }
    }

    #[must_use]
    pub fn with_retry(mut self, max_retries: u32, base_backoff: Duration) -> Self {
        self.retry = RetryConfig {
            max_retries,
            base_backoff,
        };
        self
    }

    #[must_use]
    pub fn with_circuit(mut self, threshold: u32, cooldown: Duration) -> Self {
        self.breaker = Arc::new(CircuitBreaker::new(threshold, cooldown));
        self
    }

    /// Share a breaker across provider instances (e.g. a binding that constructs a provider
    /// per call: a per-call breaker is stateless theater — the shared one carries the state).
    #[must_use]
    pub fn with_shared_breaker(mut self, breaker: Arc<CircuitBreaker>) -> Self {
        self.breaker = breaker;
        self
    }

    #[must_use]
    pub fn with_timeouts(mut self, timeouts: TimeoutConfig) -> Self {
        self.timeouts = timeouts;
        self
    }
}

/// Run one attempt under a bound, mapping elapsed → `ProviderError::Timeout(bound)`.
async fn bounded<T>(
    bound: Duration,
    fut: impl std::future::Future<Output = Result<T, ProviderError>>,
) -> Result<T, ProviderError> {
    match tokio::time::timeout(bound, fut).await {
        Ok(result) => result,
        Err(_elapsed) => Err(ProviderError::Timeout(bound)),
    }
}

/// Wraps a [`ByteStream`], yielding `Err(Timeout(idle))` then terminating when the gap
/// between items exceeds `idle`. Lives in the resilience decorator (time policy), NOT in
/// `metered_passthrough` (the metering primitive must not carry resilience policy).
struct IdleTimeout {
    inner: ByteStream,
    idle: Duration,
    sleep: Pin<Box<tokio::time::Sleep>>,
    expired: bool,
}

impl IdleTimeout {
    fn new(inner: ByteStream, idle: Duration) -> Self {
        Self {
            inner,
            idle,
            sleep: Box::pin(tokio::time::sleep(idle)),
            expired: false,
        }
    }
}

impl Stream for IdleTimeout {
    type Item = Result<StreamChunk, ProviderError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.expired {
            return Poll::Ready(None);
        }
        match self.inner.as_mut().poll_next(cx) {
            Poll::Ready(item) => {
                let idle = self.idle;
                self.sleep
                    .as_mut()
                    .reset(tokio::time::Instant::now() + idle);
                Poll::Ready(item)
            }
            Poll::Pending => match self.sleep.as_mut().poll(cx) {
                Poll::Ready(()) => {
                    self.expired = true;
                    Poll::Ready(Some(Err(ProviderError::Timeout(self.idle))))
                }
                Poll::Pending => Poll::Pending,
            },
        }
    }
}

/// Retry only failures that might succeed on a repeat: network blips, 429, and 5xx. Auth,
/// 4xx client errors, and an open circuit are terminal.
fn is_retryable(e: &ProviderError) -> bool {
    match e {
        ProviderError::Transport(_) | ProviderError::RateLimited | ProviderError::Timeout(_) => {
            true
        }
        ProviderError::Upstream(status) => *status >= 500,
        ProviderError::Auth | ProviderError::CircuitOpen => false,
    }
}

fn backoff(base: Duration, attempt: u32) -> Duration {
    let factor = 2u32.saturating_pow(attempt.saturating_sub(1));
    base.saturating_mul(factor).min(Duration::from_secs(30))
}

#[async_trait]
impl Provider for ResilientProvider {
    fn slug(&self) -> &str {
        self.inner.slug()
    }

    async fn complete(&self, req: ProviderRequest) -> Result<ProviderResponse, ProviderError> {
        if !self.breaker.allow() {
            return Err(ProviderError::CircuitOpen);
        }
        let mut attempt = 0u32;
        loop {
            match bounded(self.timeouts.complete, self.inner.complete(req.clone())).await {
                Ok(resp) => {
                    self.breaker.record_success();
                    return Ok(resp);
                }
                Err(e) => {
                    if is_retryable(&e) && attempt < self.retry.max_retries {
                        attempt += 1;
                        sleep(backoff(self.retry.base_backoff, attempt)).await;
                        continue;
                    }
                    self.breaker.record_failure();
                    return Err(e);
                }
            }
        }
    }

    async fn stream(&self, req: ProviderRequest) -> Result<ByteStream, ProviderError> {
        // Only the stream *setup* is retried; once bytes flow, in-stream errors surface to the
        // caller unchanged (retrying mid-stream would double-send tokens).
        if !self.breaker.allow() {
            return Err(ProviderError::CircuitOpen);
        }
        let mut attempt = 0u32;
        loop {
            match bounded(self.timeouts.stream_setup, self.inner.stream(req.clone())).await {
                Ok(stream) => {
                    self.breaker.record_success();
                    return Ok(match self.timeouts.idle {
                        Some(idle) => Box::pin(IdleTimeout::new(stream, idle)),
                        None => stream,
                    });
                }
                Err(e) => {
                    if is_retryable(&e) && attempt < self.retry.max_retries {
                        attempt += 1;
                        sleep(backoff(self.retry.base_backoff, attempt)).await;
                        continue;
                    }
                    self.breaker.record_failure();
                    return Err(e);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ParsedUsage;
    use futures_util::stream;
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// A test double: returns queued results in order and counts how many times it was called.
    struct Flaky {
        queue: Mutex<VecDeque<Result<ProviderResponse, ProviderError>>>,
        calls: AtomicU32,
    }

    impl Flaky {
        fn new(results: Vec<Result<ProviderResponse, ProviderError>>) -> Arc<Self> {
            Arc::new(Self {
                queue: Mutex::new(results.into()),
                calls: AtomicU32::new(0),
            })
        }
        fn calls(&self) -> u32 {
            self.calls.load(Ordering::Relaxed)
        }
    }

    fn ok_resp() -> ProviderResponse {
        ProviderResponse {
            status: 200,
            body: serde_json::json!({}),
            usage: ParsedUsage::default(),
        }
    }

    #[async_trait]
    impl Provider for Flaky {
        fn slug(&self) -> &str {
            "flaky"
        }
        async fn complete(&self, _req: ProviderRequest) -> Result<ProviderResponse, ProviderError> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            self.queue
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(Err(ProviderError::Transport("exhausted".into())))
        }
        async fn stream(&self, _req: ProviderRequest) -> Result<ByteStream, ProviderError> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            match self.queue.lock().unwrap().pop_front() {
                Some(Ok(_)) => Ok(Box::pin(stream::empty())),
                Some(Err(e)) => Err(e),
                None => Err(ProviderError::Transport("exhausted".into())),
            }
        }
    }

    fn req() -> ProviderRequest {
        ProviderRequest::new("m", serde_json::json!({}))
    }

    #[test]
    fn timeout_error_is_retryable() {
        // A timeout is definitionally a transient bet — same class as a 503.
        assert!(is_retryable(&ProviderError::Timeout(Duration::from_secs(
            30
        ))));
    }

    /// Hangs forever for the first `hang_calls` calls, then succeeds.
    struct Hang {
        hang_calls: u32,
        calls: AtomicU32,
    }

    impl Hang {
        fn new(hang_calls: u32) -> Arc<Self> {
            Arc::new(Self {
                hang_calls,
                calls: AtomicU32::new(0),
            })
        }
        fn calls(&self) -> u32 {
            self.calls.load(Ordering::Relaxed)
        }
    }

    #[async_trait]
    impl Provider for Hang {
        fn slug(&self) -> &str {
            "hang"
        }
        async fn complete(&self, _req: ProviderRequest) -> Result<ProviderResponse, ProviderError> {
            let n = self.calls.fetch_add(1, Ordering::Relaxed);
            if n < self.hang_calls {
                std::future::pending::<()>().await;
            }
            Ok(ok_resp())
        }
        async fn stream(&self, _req: ProviderRequest) -> Result<ByteStream, ProviderError> {
            let n = self.calls.fetch_add(1, Ordering::Relaxed);
            if n < self.hang_calls {
                std::future::pending::<()>().await;
            }
            Ok(Box::pin(stream::empty()))
        }
    }

    fn tight_timeouts() -> TimeoutConfig {
        TimeoutConfig {
            complete: Duration::from_millis(50),
            stream_setup: Duration::from_millis(50),
            idle: None,
        }
    }

    #[tokio::test]
    async fn complete_times_out_with_timeout_error() {
        let hang = Hang::new(u32::MAX);
        let p = ResilientProvider::new(hang.clone())
            .with_retry(0, Duration::from_millis(1))
            .with_timeouts(tight_timeouts());
        let start = Instant::now();
        let out = p.complete(req()).await;
        assert!(matches!(out, Err(ProviderError::Timeout(_))));
        assert!(start.elapsed() < Duration::from_secs(1));
    }

    #[tokio::test]
    async fn timed_out_attempt_is_retried_then_succeeds() {
        let hang = Hang::new(1); // first attempt hangs, second succeeds
        let p = ResilientProvider::new(hang.clone())
            .with_retry(1, Duration::from_millis(1))
            .with_timeouts(tight_timeouts());
        assert!(p.complete(req()).await.is_ok());
        assert_eq!(hang.calls(), 2);
    }

    #[tokio::test]
    async fn stream_setup_times_out() {
        let hang = Hang::new(u32::MAX);
        let p = ResilientProvider::new(hang.clone())
            .with_retry(0, Duration::from_millis(1))
            .with_timeouts(tight_timeouts());
        assert!(matches!(
            p.stream(req()).await,
            Err(ProviderError::Timeout(_))
        ));
    }

    #[tokio::test]
    async fn consecutive_timeouts_open_the_circuit() {
        let hang = Hang::new(u32::MAX);
        let p = ResilientProvider::new(hang.clone())
            .with_retry(0, Duration::from_millis(1))
            .with_circuit(2, Duration::from_secs(60))
            .with_timeouts(tight_timeouts());
        assert!(p.complete(req()).await.is_err()); // timeout 1
        assert!(p.complete(req()).await.is_err()); // timeout 2 → opens
        assert!(matches!(
            p.complete(req()).await,
            Err(ProviderError::CircuitOpen)
        ));
        assert_eq!(hang.calls(), 2); // fast-failed without touching the upstream
    }

    fn chunk(data: &str) -> StreamChunk {
        StreamChunk {
            data: bytes::Bytes::copy_from_slice(data.as_bytes()),
            usage: None,
        }
    }

    fn terminal_chunk() -> StreamChunk {
        StreamChunk {
            data: bytes::Bytes::new(),
            usage: Some(ParsedUsage {
                tokens_in: 1,
                tokens_out: 2,
                ..Default::default()
            }),
        }
    }

    #[tokio::test]
    async fn idle_timeout_fires_between_chunks() {
        use futures_util::StreamExt;
        let stalled: ByteStream = Box::pin(
            stream::iter(vec![Ok(chunk("first"))])
                .chain(stream::pending::<Result<StreamChunk, ProviderError>>()),
        );
        struct One(Mutex<Option<ByteStream>>);
        #[async_trait]
        impl Provider for One {
            fn slug(&self) -> &str {
                "one"
            }
            async fn complete(
                &self,
                _req: ProviderRequest,
            ) -> Result<ProviderResponse, ProviderError> {
                unreachable!()
            }
            async fn stream(&self, _req: ProviderRequest) -> Result<ByteStream, ProviderError> {
                Ok(self.0.lock().unwrap().take().unwrap())
            }
        }
        let p = ResilientProvider::new(Arc::new(One(Mutex::new(Some(stalled)))))
            .with_retry(0, Duration::from_millis(1))
            .with_timeouts(TimeoutConfig {
                complete: Duration::from_secs(5),
                stream_setup: Duration::from_secs(5),
                idle: Some(Duration::from_millis(30)),
            });
        let mut s = p.stream(req()).await.unwrap();
        let first = s.next().await.unwrap().unwrap();
        assert_eq!(&first.data[..], b"first");
        // The gap exceeds the idle bound → a Timeout error, then termination.
        assert!(matches!(
            s.next().await,
            Some(Err(ProviderError::Timeout(_)))
        ));
        assert!(s.next().await.is_none());
    }

    #[tokio::test]
    async fn idle_none_disables_idle_timeout() {
        use futures_util::StreamExt;
        let stalled: ByteStream = Box::pin(
            stream::iter(vec![Ok(chunk("first"))])
                .chain(stream::pending::<Result<StreamChunk, ProviderError>>()),
        );
        struct One(Mutex<Option<ByteStream>>);
        #[async_trait]
        impl Provider for One {
            fn slug(&self) -> &str {
                "one"
            }
            async fn complete(
                &self,
                _req: ProviderRequest,
            ) -> Result<ProviderResponse, ProviderError> {
                unreachable!()
            }
            async fn stream(&self, _req: ProviderRequest) -> Result<ByteStream, ProviderError> {
                Ok(self.0.lock().unwrap().take().unwrap())
            }
        }
        let p = ResilientProvider::new(Arc::new(One(Mutex::new(Some(stalled)))))
            .with_retry(0, Duration::from_millis(1))
            .with_timeouts(TimeoutConfig {
                complete: Duration::from_secs(5),
                stream_setup: Duration::from_secs(5),
                idle: None,
            });
        let mut s = p.stream(req()).await.unwrap();
        let _ = s.next().await.unwrap().unwrap();
        // idle disabled → the stalled stream stays pending (the TEST times out, not the stream)
        let second = tokio::time::timeout(Duration::from_millis(100), s.next()).await;
        assert!(
            second.is_err(),
            "no idle timeout should fire when idle=None"
        );
    }

    #[tokio::test]
    async fn mid_stream_idle_timeout_is_not_retried() {
        use futures_util::StreamExt;
        let hang = Hang::new(0); // setup succeeds immediately with an empty stream
        let stalled: ByteStream = Box::pin(stream::pending::<Result<StreamChunk, ProviderError>>());
        struct One(Mutex<Option<ByteStream>>, AtomicU32);
        #[async_trait]
        impl Provider for One {
            fn slug(&self) -> &str {
                "one"
            }
            async fn complete(
                &self,
                _req: ProviderRequest,
            ) -> Result<ProviderResponse, ProviderError> {
                unreachable!()
            }
            async fn stream(&self, _req: ProviderRequest) -> Result<ByteStream, ProviderError> {
                self.1.fetch_add(1, Ordering::Relaxed);
                Ok(self.0.lock().unwrap().take().unwrap())
            }
        }
        let one = Arc::new(One(Mutex::new(Some(stalled)), AtomicU32::new(0)));
        let p = ResilientProvider::new(one.clone())
            .with_retry(3, Duration::from_millis(1))
            .with_timeouts(TimeoutConfig {
                complete: Duration::from_secs(5),
                stream_setup: Duration::from_secs(5),
                idle: Some(Duration::from_millis(20)),
            });
        let mut s = p.stream(req()).await.unwrap();
        assert!(matches!(
            s.next().await,
            Some(Err(ProviderError::Timeout(_)))
        ));
        assert_eq!(
            one.1.load(Ordering::Relaxed),
            1,
            "mid-stream timeout must not retry"
        );
        drop(hang);
    }

    #[tokio::test]
    async fn fast_stream_unaffected_by_idle_timeout() {
        use futures_util::StreamExt;
        let fast: ByteStream = Box::pin(stream::iter(vec![
            Ok(chunk("a")),
            Ok(chunk("b")),
            Ok(terminal_chunk()),
        ]));
        struct One(Mutex<Option<ByteStream>>);
        #[async_trait]
        impl Provider for One {
            fn slug(&self) -> &str {
                "one"
            }
            async fn complete(
                &self,
                _req: ProviderRequest,
            ) -> Result<ProviderResponse, ProviderError> {
                unreachable!()
            }
            async fn stream(&self, _req: ProviderRequest) -> Result<ByteStream, ProviderError> {
                Ok(self.0.lock().unwrap().take().unwrap())
            }
        }
        let p = ResilientProvider::new(Arc::new(One(Mutex::new(Some(fast)))))
            .with_retry(0, Duration::from_millis(1))
            .with_timeouts(TimeoutConfig {
                complete: Duration::from_secs(5),
                stream_setup: Duration::from_secs(5),
                idle: Some(Duration::from_millis(50)),
            });
        let mut s = p.stream(req()).await.unwrap();
        let items: Vec<_> = (&mut s).collect::<Vec<_>>().await;
        assert_eq!(items.len(), 3);
        let last = items.last().unwrap().as_ref().unwrap();
        assert!(
            last.usage.is_some(),
            "terminal usage must survive the wrapper"
        );
    }

    #[tokio::test]
    async fn retries_transient_failures_then_succeeds() {
        let flaky = Flaky::new(vec![
            Err(ProviderError::Transport("blip".into())),
            Err(ProviderError::Upstream(503)),
            Ok(ok_resp()),
        ]);
        let p = ResilientProvider::new(flaky.clone()).with_retry(3, Duration::from_millis(1));
        let out = p.complete(req()).await;
        assert!(out.is_ok());
        assert_eq!(flaky.calls(), 3); // two failures + one success
    }

    #[tokio::test]
    async fn does_not_retry_auth() {
        let flaky = Flaky::new(vec![Err(ProviderError::Auth), Ok(ok_resp())]);
        let p = ResilientProvider::new(flaky.clone()).with_retry(5, Duration::from_millis(1));
        assert!(matches!(p.complete(req()).await, Err(ProviderError::Auth)));
        assert_eq!(flaky.calls(), 1); // Auth is terminal — no retry
    }

    #[tokio::test]
    async fn circuit_opens_after_threshold_and_fast_fails() {
        let flaky = Flaky::new(vec![
            Err(ProviderError::Transport("x".into())),
            Err(ProviderError::Transport("x".into())),
            Ok(ok_resp()), // would succeed, but the circuit should be open by now
        ]);
        let p = ResilientProvider::new(flaky.clone())
            .with_retry(0, Duration::from_millis(1))
            .with_circuit(2, Duration::from_secs(60));

        assert!(p.complete(req()).await.is_err()); // failure 1
        assert!(p.complete(req()).await.is_err()); // failure 2 → opens
                                                   // Third call is fast-failed without touching the upstream.
        assert!(matches!(
            p.complete(req()).await,
            Err(ProviderError::CircuitOpen)
        ));
        assert_eq!(flaky.calls(), 2);
    }

    #[tokio::test]
    async fn circuit_half_opens_after_cooldown() {
        let flaky = Flaky::new(vec![
            Err(ProviderError::Transport("x".into())),
            Ok(ok_resp()),
        ]);
        let p = ResilientProvider::new(flaky.clone())
            .with_retry(0, Duration::from_millis(1))
            .with_circuit(1, Duration::from_millis(20));

        assert!(p.complete(req()).await.is_err()); // failure → opens
        assert!(matches!(
            p.complete(req()).await,
            Err(ProviderError::CircuitOpen)
        )); // still within cooldown
        assert_eq!(flaky.calls(), 1);

        sleep(Duration::from_millis(30)).await; // cooldown elapses

        assert!(p.complete(req()).await.is_ok()); // half-open trial succeeds → closes
        assert_eq!(flaky.calls(), 2);
    }
}
