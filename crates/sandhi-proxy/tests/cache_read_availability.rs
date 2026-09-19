//! ADR-0010 end-to-end accounting coverage, with real adapters and byte-exact raw egress.
use axum::{
    body::{to_bytes, Body},
    http::{Request, StatusCode},
};
use sandhi_core::{
    CacheReadObservation, CacheReadStatus, ChatRequestV1, ChatResponseV1, ChatStreamEventV1,
    InMemorySink, KeyStore, UsageCompleteness, UsageV2, VirtualKey,
};
use sandhi_providers::{
    ChatEventStream, ChatProvider, ProviderError, ProviderHandle, ProviderRuntime,
};
use sandhi_proxy::{build_app, ProxyLedger, ProxyState};
use std::{collections::HashMap, sync::Arc};
use tower::ServiceExt;
use wiremock::{matchers::method, Mock, MockServer, ResponseTemplate};

fn state(provider: ProviderHandle, sink: Arc<InMemorySink>) -> Arc<ProxyState> {
    let keys = KeyStore::new();
    keys.insert(VirtualKey {
        id: "vk_cache".into(),
        upstream_ref: "origin".into(),
        ..Default::default()
    });
    Arc::new(ProxyState::new(
        keys,
        ProxyLedger::in_memory(),
        sink,
        HashMap::from([("origin".into(), provider)]),
        None,
    ))
}

fn request(stream: bool) -> Request<Body> {
    Request::builder().method("POST").uri("/v1/chat/completions")
        .header("authorization", "Bearer vk_cache").header("content-type", "application/json")
        .body(Body::from(serde_json::json!({"model":"fixture","messages":[{"role":"user","content":"hi"}],"stream":stream}).to_string())).unwrap()
}

#[tokio::test]
async fn raw_buffered_and_streamed_cache_availability_preserve_wire_bytes() {
    let cases = [
        (
            Some(
                serde_json::json!({"prompt_tokens":9,"completion_tokens":2,"prompt_tokens_details":{"cached_tokens":0}}),
            ),
            CacheReadStatus::Reported,
        ),
        (
            Some(serde_json::json!({"prompt_tokens":9,"completion_tokens":2})),
            CacheReadStatus::Absent,
        ),
        (
            Some(
                serde_json::json!({"prompt_tokens":9,"completion_tokens":2,"prompt_tokens_details":{"cached_tokens":"bad"}}),
            ),
            CacheReadStatus::Malformed,
        ),
        (None, CacheReadStatus::Absent),
        (Some(serde_json::Value::Null), CacheReadStatus::Malformed),
    ];
    for (usage, expected) in cases {
        for streaming in [false, true] {
            let server = MockServer::start().await;
            let mut body = serde_json::json!({"id":"fixture","choices":[]});
            if let Some(usage) = &usage {
                body["usage"] = usage.clone();
            }
            let wire = if streaming {
                format!("data: {{\"choices\":[{{\"delta\":{{\"content\":\"hi\"}}}}],\"usage\":null}}\n\ndata: {body}\n\ndata: [DONE]\n\n")
            } else {
                format!("\n{body}\n")
            };
            Mock::given(method("POST"))
                .respond_with(
                    ResponseTemplate::new(200)
                        .insert_header(
                            "content-type",
                            if streaming {
                                "text/event-stream"
                            } else {
                                "application/json"
                            },
                        )
                        .set_body_string(wire.clone()),
                )
                .mount(&server)
                .await;
            let sink = Arc::new(InMemorySink::new());
            let provider = ProviderRuntime::new().openai_compat(
                "openai",
                server.uri(),
                "key",
                Default::default(),
                Some(0),
                None,
                None,
            );
            let response = build_app(state(provider, sink.clone()))
                .oneshot(request(streaming))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(
                to_bytes(response.into_body(), usize::MAX).await.unwrap(),
                wire
            );
            let events = sink.events();
            assert_eq!(events.len(), 1);
            assert_eq!(
                events[0].cache_read_observation.unwrap().status,
                expected,
                "streaming={streaming},usage={usage:?}"
            );
            assert_eq!(events[0].cache_read_tokens, 0);
            if streaming && usage.as_ref().is_none_or(serde_json::Value::is_null) {
                assert_eq!(events[0].usage_completeness, UsageCompleteness::Partial);
                assert!(
                    events[0].tokens_out > 0,
                    "metadata must not overwrite byte estimate with final zero"
                );
            }
        }
    }
}

struct CanonicalStream {
    fail_setup: bool,
}

#[async_trait::async_trait]
impl ChatProvider for CanonicalStream {
    fn slug(&self) -> &str {
        "fixture"
    }
    async fn complete(
        &self,
        _: ChatRequestV1,
        _: axum::http::HeaderMap,
    ) -> Result<ChatResponseV1, ProviderError> {
        unreachable!()
    }
    async fn stream(
        &self,
        _: ChatRequestV1,
        _: axum::http::HeaderMap,
    ) -> Result<ChatEventStream, ProviderError> {
        if self.fail_setup {
            return Err(ProviderError::Transport("no response".into()));
        }
        let events = vec![
            ChatStreamEventV1::ResponseStart {
                id: Some("fixture".into()),
                model: "fixture".into(),
            },
            ChatStreamEventV1::TextDelta { delta: "hi".into() },
            ChatStreamEventV1::Usage {
                usage: UsageV2 {
                    tokens_in: 5,
                    tokens_out: 2,
                    cache_read_tokens: 4,
                    completeness: UsageCompleteness::Final,
                    cache_read_observation: Some(CacheReadObservation::origin(
                        CacheReadStatus::Reported,
                    )),
                    ..Default::default()
                },
            },
            ChatStreamEventV1::Usage {
                usage: UsageV2 {
                    cache_read_observation: Some(CacheReadObservation::origin(
                        CacheReadStatus::Malformed,
                    )),
                    ..Default::default()
                },
            },
            ChatStreamEventV1::Finish {
                reason: sandhi_core::FinishReasonV1::Stop,
            },
        ];
        Ok(Box::pin(futures_util::stream::iter(
            events.into_iter().map(Ok),
        )))
    }
}

#[tokio::test]
async fn translated_metadata_correction_changes_accounting_not_terminal_wire_usage() {
    let sink = Arc::new(InMemorySink::new());
    let provider = ProviderHandle::new(Arc::new(CanonicalStream { fail_setup: false }));
    let response = build_app(state(provider, sink.clone()))
        .oneshot(request(true))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let wire = std::str::from_utf8(&body).unwrap();
    let values: Vec<serde_json::Value> = wire
        .lines()
        .filter_map(|line| {
            line.strip_prefix("data: ")
                .and_then(|data| serde_json::from_str(data).ok())
        })
        .collect();
    assert_eq!(
        values
            .iter()
            .filter(|value| value.get("usage").is_some())
            .count(),
        1
    );
    assert!(!wire.contains("cache_read_observation"));
    let events = sink.events();
    assert_eq!(events.len(), 1);
    assert_eq!(
        events[0].cache_read_observation.unwrap().status,
        CacheReadStatus::Malformed
    );
    assert_eq!(
        (
            events[0].tokens_in,
            events[0].tokens_out,
            events[0].cache_read_tokens
        ),
        (5, 2, 4)
    );
    assert_eq!(events[0].usage_completeness, UsageCompleteness::Final);
}

#[tokio::test]
async fn setup_failure_has_unknown_cache_availability() {
    let sink = Arc::new(InMemorySink::new());
    let provider = ProviderHandle::new(Arc::new(CanonicalStream { fail_setup: true }));
    let response = build_app(state(provider, sink.clone()))
        .oneshot(request(true))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    let events = sink.events();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].cache_read_observation, None);
    assert_eq!(events[0].usage_completeness, UsageCompleteness::Unavailable);
}
