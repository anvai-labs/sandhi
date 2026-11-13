//! Qualified HTTP cache availability is independent of numeric zero and receipt status.
use axum::{
    body::{to_bytes, Body},
    http::{Request, StatusCode},
};
use sandhi_core::{CacheReadStatus, InMemorySink, KeyStore, UsageCompleteness, VirtualKey};
use sandhi_providers::{ProviderHandle, ProviderRuntime};
use sandhi_proxy::{build_app, ProxyLedger, ProxyState};
use std::{collections::HashMap, path::PathBuf, sync::Arc};
use tower::ServiceExt;
use wiremock::{matchers::method, Mock, MockServer, ResponseTemplate};

struct Fixture {
    state: Arc<ProxyState>,
    database: PathBuf,
    _directory: tempfile::TempDir,
}
fn fixture(provider: ProviderHandle, sink: Arc<InMemorySink>) -> Fixture {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("original.db");
    let ledger = ProxyLedger::durable(database.to_str().unwrap(), 1).unwrap();
    let keys = KeyStore::new();
    keys.insert(VirtualKey {
        id: "vk_cache".into(),
        upstream_ref: "origin".into(),
        ..Default::default()
    });
    let state = Arc::new(ProxyState::new(
        keys,
        ledger,
        sink,
        HashMap::from([("origin".into(), provider)]),
        None,
    ));
    Fixture {
        state,
        database,
        _directory: directory,
    }
}
fn request(stream: bool) -> Request<Body> {
    Request::builder().method("POST").uri("/v1/chat/completions")
        .header("authorization", "Bearer vk_cache").header("content-type", "application/json")
        .body(Body::from(serde_json::json!({"model":"fixture","max_tokens":16,"messages":[{"role":"user","content":"hi"}],"stream":stream}).to_string())).unwrap()
}
fn openai(server: &MockServer) -> ProviderHandle {
    ProviderRuntime::new().openai_compat(
        "openai",
        server.uri(),
        "synthetic",
        Default::default(),
        Some(0),
        None,
        None,
        None,
    )
}
fn evidence(fixture: &Fixture, settled: bool) {
    let connection = rusqlite::Connection::open(&fixture.database).unwrap();
    assert_eq!(
        connection
            .query_row("SELECT count(*) FROM budget_physical_attempt", [], |row| {
                row.get::<_, i64>(0)
            })
            .unwrap(),
        1
    );
    assert_eq!(
        connection
            .query_row("SELECT count(*) FROM budget_settlement_outbox", [], |row| {
                row.get::<_, i64>(0)
            })
            .unwrap(),
        i64::from(settled)
    );
    let mut query = connection
        .prepare("SELECT settled FROM budget_reservation ORDER BY id")
        .unwrap();
    assert_eq!(
        query
            .query_map([], |row| row.get::<_, i64>(0))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap(),
        vec![i64::from(settled); 2]
    );
}

#[tokio::test]
async fn qualified_raw_zero_and_absent_cache_preserve_exact_response_bytes() {
    for reported_zero in [false, true] {
        for streaming in [false, true] {
            let server = MockServer::start().await;
            let mut usage = serde_json::json!({"prompt_tokens":9,"completion_tokens":2});
            if reported_zero {
                usage["prompt_tokens_details"] = serde_json::json!({"cached_tokens":0});
            }
            let body = serde_json::json!({"id":"fixture","object":"chat.completion","model":"fixture","choices":[{"index":0,"message":{"role":"assistant","content":"hi"},"finish_reason":"stop"}],"usage":usage});
            let wire = if streaming {
                format!("data: {{\"id\":\"fixture\",\"object\":\"chat.completion.chunk\",\"model\":\"fixture\",\"choices\":[{{\"index\":0,\"delta\":{{\"content\":\"hi\"}},\"finish_reason\":null}}]}}\n\ndata: {{\"object\":\"chat.completion.chunk\",\"choices\":[{{\"index\":0,\"delta\":{{}},\"finish_reason\":\"stop\"}}]}}\n\ndata: {}\n\ndata: [DONE]\n\n", serde_json::json!({"object":"chat.completion.chunk","choices":[],"usage":usage}))
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
            let fixture = fixture(openai(&server), sink.clone());
            let response = build_app(fixture.state.clone())
                .oneshot(request(streaming))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(
                to_bytes(response.into_body(), usize::MAX).await.unwrap(),
                wire
            );
            assert_eq!(server.received_requests().await.unwrap().len(), 1);
            let events = sink.events();
            assert_eq!(events.len(), 1);
            assert_eq!(
                events[0].cache_read_observation.unwrap().status,
                if reported_zero {
                    CacheReadStatus::Reported
                } else {
                    CacheReadStatus::Absent
                }
            );
            assert_eq!(events[0].cache_read_tokens, 0);
            assert_eq!((events[0].tokens_in, events[0].tokens_out), (9, 2));
            assert_eq!(events[0].usage_completeness, UsageCompleteness::Final);
            evidence(&fixture, true);
        }
    }
}

#[tokio::test]
async fn malformed_or_missing_usage_retains_paired_liability_without_replay() {
    for usage in [
        None,
        Some(serde_json::Value::Null),
        Some(
            serde_json::json!({"prompt_tokens":9,"completion_tokens":2,"prompt_tokens_details":{"cached_tokens":"bad"}}),
        ),
    ] {
        for streaming in [false, true] {
            let server = MockServer::start().await;
            let mut body = serde_json::json!({"id":"fixture","object":"chat.completion","model":"fixture","choices":[{"index":0,"message":{"role":"assistant","content":"hi"},"finish_reason":"stop"}]});
            if let Some(usage) = &usage {
                body["usage"] = usage.clone();
            }
            let wire = if streaming {
                let mut terminal =
                    serde_json::json!({"object":"chat.completion.chunk","choices":[]});
                if let Some(usage) = &usage {
                    terminal["usage"] = usage.clone();
                }
                format!("data: {{\"object\":\"chat.completion.chunk\",\"choices\":[{{\"index\":0,\"delta\":{{\"content\":\"hi\"}},\"finish_reason\":null}}]}}\n\ndata: {{\"object\":\"chat.completion.chunk\",\"choices\":[{{\"index\":0,\"delta\":{{}},\"finish_reason\":\"stop\"}}]}}\n\ndata: {terminal}\n\ndata: [DONE]\n\n")
            } else {
                body.to_string()
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
                        .set_body_string(wire),
                )
                .mount(&server)
                .await;
            let sink = Arc::new(InMemorySink::new());
            let fixture = fixture(openai(&server), sink.clone());
            let response = build_app(fixture.state.clone())
                .oneshot(request(streaming))
                .await
                .unwrap();
            if streaming {
                assert_eq!(response.status(), StatusCode::OK);
                if let Ok(body) = to_bytes(response.into_body(), usize::MAX).await {
                    assert!(!String::from_utf8_lossy(&body).contains("data: [DONE]"));
                }
            } else {
                assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
                assert_eq!(response.headers()["x-should-retry"], "false");
                to_bytes(response.into_body(), usize::MAX).await.unwrap();
            }
            assert_eq!(server.received_requests().await.unwrap().len(), 1);
            for event in sink.events() {
                assert_ne!(event.usage_completeness, UsageCompleteness::Final);
            }
            evidence(&fixture, false);
        }
    }
}

#[tokio::test]
async fn translated_qualified_stream_has_one_wire_measurement_and_source_cache_evidence() {
    let server = MockServer::start().await;
    let frames = [
        serde_json::json!({"type":"message_start","message":{"id":"fixture","type":"message","model":"fixture","role":"assistant","content":[],"stop_reason":null,"usage":{"input_tokens":5,"output_tokens":0,"cache_read_input_tokens":4,"cache_creation_input_tokens":0}}}),
        serde_json::json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
        serde_json::json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hi"}}),
        serde_json::json!({"type":"content_block_stop","index":0}),
        serde_json::json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":2}}),
        serde_json::json!({"type":"message_stop"}),
    ];
    let wire = frames
        .into_iter()
        .map(|frame| format!("data: {frame}\n\n"))
        .collect::<String>();
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(wire),
        )
        .mount(&server)
        .await;
    let provider = ProviderRuntime::new().anthropic(
        server.uri(),
        "synthetic",
        sandhi_providers::AnthropicAuthScheme::ApiKey,
        Default::default(),
        Some(0),
        None,
        None,
        None,
    );
    let sink = Arc::new(InMemorySink::new());
    let fixture = fixture(provider, sink.clone());
    let response = build_app(fixture.state.clone())
        .oneshot(request(true))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let wire = std::str::from_utf8(&body).unwrap();
    let values = wire
        .lines()
        .filter_map(|line| {
            line.strip_prefix("data: ")
                .and_then(|data| serde_json::from_str::<serde_json::Value>(data).ok())
        })
        .collect::<Vec<_>>();
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
        CacheReadStatus::Reported
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
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
    evidence(&fixture, true);
}

#[tokio::test]
async fn real_transport_failure_before_headers_has_unknown_cache_and_retains_uncertainty() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = format!("http://{}", listener.local_addr().unwrap());
    let origin = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        drop(stream); // One real connection closes before any response header.
    });
    let provider = ProviderRuntime::new().openai_compat(
        "openai",
        address,
        "synthetic",
        Default::default(),
        Some(0),
        None,
        None,
        None,
    );
    let sink = Arc::new(InMemorySink::new());
    let fixture = fixture(provider, sink.clone());
    let response = build_app(fixture.state.clone())
        .oneshot(request(true))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    assert_eq!(response.headers()["x-should-retry"], "false");
    to_bytes(response.into_body(), usize::MAX).await.unwrap();
    for event in sink.events() {
        assert_eq!(event.cache_read_observation, None);
        assert_eq!(event.usage_completeness, UsageCompleteness::Unavailable);
    }
    origin.await.unwrap();
    evidence(&fixture, false);
}
