//! Timeout integration tests over a real adapter + wiremock delayed responders: the
//! decorator's per-attempt bound fires against a genuinely slow upstream and maps to
//! `ProviderError::Timeout` (retryable), while sub-bound latency is unaffected.

use std::sync::Arc;
use std::time::Duration;

use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use sandhi_providers::{
    OpenAiCompat, Provider, ProviderError, ProviderRequest, ResilientProvider, TimeoutConfig,
};

fn req() -> ProviderRequest {
    ProviderRequest::new(
        "gpt-x",
        serde_json::json!({"model": "gpt-x", "messages": []}),
    )
}

fn ok_body() -> serde_json::Value {
    serde_json::json!({
        "choices": [{ "message": { "content": "hi" } }],
        "usage": { "prompt_tokens": 1, "completion_tokens": 1 }
    })
}

#[tokio::test]
async fn delayed_upstream_complete_times_out() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(ok_body())
                .set_delay(Duration::from_secs(10)),
        )
        .mount(&upstream)
        .await;

    let adapter = Arc::new(OpenAiCompat::new("openai", upstream.uri(), "k"));
    let p = ResilientProvider::new(adapter)
        .with_retry(0, Duration::from_millis(1))
        .with_timeouts(TimeoutConfig {
            complete: Duration::from_millis(100),
            stream_setup: Duration::from_millis(100),
            idle: None,
        });

    let start = std::time::Instant::now();
    let out = p.complete(req()).await;
    assert!(
        matches!(out, Err(ProviderError::Timeout(_))),
        "got: {out:?}"
    );
    assert!(start.elapsed() < Duration::from_secs(5));
}

#[tokio::test]
async fn delay_under_timeout_succeeds() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(ok_body())
                .set_delay(Duration::from_millis(50)),
        )
        .mount(&upstream)
        .await;

    let adapter = Arc::new(OpenAiCompat::new("openai", upstream.uri(), "k"));
    let p = ResilientProvider::new(adapter)
        .with_retry(0, Duration::from_millis(1))
        .with_timeouts(TimeoutConfig {
            complete: Duration::from_secs(2),
            stream_setup: Duration::from_secs(2),
            idle: None,
        });

    let out = p
        .complete(req())
        .await
        .expect("under-bound delay must succeed");
    assert_eq!(out.status, 200);
    assert_eq!(out.usage.tokens_in, 1);
}

#[tokio::test]
async fn delayed_stream_setup_times_out() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string("data: [DONE]\n\n")
                .insert_header("content-type", "text/event-stream")
                .set_delay(Duration::from_secs(10)),
        )
        .mount(&upstream)
        .await;

    let adapter = Arc::new(OpenAiCompat::new("openai", upstream.uri(), "k"));
    let p = ResilientProvider::new(adapter)
        .with_retry(0, Duration::from_millis(1))
        .with_timeouts(TimeoutConfig {
            complete: Duration::from_millis(100),
            stream_setup: Duration::from_millis(100),
            idle: None,
        });

    assert!(matches!(
        p.stream(req()).await,
        Err(ProviderError::Timeout(_))
    ));
}
