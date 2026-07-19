//! Resilience decorator (AnvaiOps ADR-0047 D10): wrap any [`Provider`] with **retry** (on
//! transient failures, with exponential backoff) and a **circuit breaker** (fast-fail while an
//! upstream is failing). Because it implements [`Provider`], it composes transparently — the
//! proxy and bindings use a `ResilientProvider` exactly like a bare adapter.

use std::sync::Arc;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use tokio::time::sleep;

use crate::{ByteStream, Provider, ProviderError, ProviderRequest, ProviderResponse};

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

/// A [`Provider`] wrapped with retry + circuit-breaking.
pub struct ResilientProvider {
    inner: Arc<dyn Provider>,
    retry: RetryConfig,
    breaker: CircuitBreaker,
}

impl ResilientProvider {
    /// Sensible defaults: 2 retries (200ms base backoff), circuit opens after 5 consecutive
    /// failures, 30s cooldown.
    pub fn new(inner: Arc<dyn Provider>) -> Self {
        Self {
            inner,
            retry: RetryConfig::default(),
            breaker: CircuitBreaker::new(5, Duration::from_secs(30)),
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
        self.breaker = CircuitBreaker::new(threshold, cooldown);
        self
    }
}

/// Retry only failures that might succeed on a repeat: network blips, 429, and 5xx. Auth,
/// 4xx client errors, and an open circuit are terminal.
fn is_retryable(e: &ProviderError) -> bool {
    match e {
        ProviderError::Transport(_) | ProviderError::RateLimited => true,
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
            match self.inner.complete(req.clone()).await {
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
            match self.inner.stream(req.clone()).await {
                Ok(stream) => {
                    self.breaker.record_success();
                    return Ok(stream);
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
