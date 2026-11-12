//! Tracked HTTP delivery across client dialects uses source evidence, never egress estimates.
use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use sandhi_core::{InMemorySink, KeyStore, VirtualKey};
use sandhi_providers::{ProviderHandle, ProviderRuntime};
use sandhi_proxy::{build_app, ProxyLedger, ProxyState};
use serde_json::{json, Value};
use std::{collections::HashMap, sync::Arc, time::Duration};
use tower::ServiceExt;
use wiremock::{matchers::method, Mock, MockServer, ResponseTemplate};

fn ingress(flavor: &str) -> (&str, Value) {
    match flavor {
        "chat" => (
            "/v1/chat/completions",
            json!({"model":"gpt-4o-mini","max_tokens":64,"stream":true,"messages":[{"role":"system","content":"Code."},{"role":"user","content":"hello"}]}),
        ),
        "messages" => (
            "/v1/messages",
            json!({"model":"gpt-4o-mini","max_tokens":64,"stream":true,"system":"Code.","messages":[{"role":"user","content":"hello"}]}),
        ),
        "responses" => (
            "/v1/responses",
            json!({"model":"gpt-4o-mini","max_output_tokens":64,"stream":true,"instructions":"Code.","input":"hello"}),
        ),
        _ => (
            "/v1beta/models/gpt-4o-mini:streamGenerateContent",
            json!({"systemInstruction":{"parts":[{"text":"Code."}]},"contents":[{"role":"user","parts":[{"text":"hello"}]}],"generationConfig":{"maxOutputTokens":64}}),
        ),
    }
}

fn wire(profile: &str, mode: &str) -> String {
    let chat = matches!(profile, "chat" | "terminal_chat");
    let mut usage = if chat {
        json!({"prompt_tokens":10,"completion_tokens":4,"prompt_tokens_details":{"cached_tokens":6,"cache_write_tokens":2},"completion_tokens_details":{"reasoning_tokens":0}})
    } else {
        json!({"input_tokens":10,"output_tokens":4,"input_tokens_details":{"cached_tokens":6,"cache_write_tokens":2},"output_tokens_details":{"reasoning_tokens":0}})
    };
    if mode == "zero" {
        usage = if chat {
            json!({"prompt_tokens":0,"completion_tokens":0,"prompt_tokens_details":{"cached_tokens":0,"cache_write_tokens":0},"completion_tokens_details":{"reasoning_tokens":0}})
        } else {
            json!({"input_tokens":0,"output_tokens":0,"input_tokens_details":{"cached_tokens":0,"cache_write_tokens":0},"output_tokens_details":{"reasoning_tokens":0}})
        };
    }
    let mut result = if chat {
        format!(
            "data: {}\n\n",
            json!({"id":"chat_fixture","object":"chat.completion.chunk","model":"gpt-4o-mini","choices":[{"index":0,"delta":{"content":"reply"},"finish_reason":if profile=="terminal_chat" {Value::Null} else {json!("stop")}}]})
        )
    } else {
        format!(
            "data: {}\n\ndata: {}\n\n",
            json!({"type":"response.in_progress","response":{"id":"resp_fixture","status":"in_progress","usage": if mode=="zero" { Value::Null } else { json!({"input_tokens":10,"output_tokens":1,"input_tokens_details":{"cached_tokens":6}}) }}}),
            json!({"type":"response.output_text.delta","response_id":"resp_fixture","delta":"reply"})
        )
    };
    if chat {
        if mode != "missing" {
            result.push_str(&format!(
                "data: {}\n\n",
                json!({"object":"chat.completion.chunk","choices":if profile=="terminal_chat" {json!([{"index":0,"delta":{},"finish_reason":"stop"}])} else {json!([])},"usage":usage})
            ));
        }
        if mode != "missing_terminal" {
            result.push_str("data: [DONE]\n\n");
        }
    } else if mode != "missing_terminal" {
        let mut response = json!({"id":"resp_fixture","status": if mode=="failed" {"failed"} else {"completed"},"model":"gpt-4o-mini","output":[],"usage":usage});
        if mode == "missing" {
            response.as_object_mut().unwrap().remove("usage");
        }
        result.push_str(&format!(
            "data: {}\n\n",
            json!({"type":if mode=="failed" {"response.failed"} else {"response.completed"},"response":response})
        ));
    }
    if mode == "post_terminal" {
        result.push_str("data: {}\n\n");
    }
    if mode == "truncated" {
        result.push_str("data: {");
    }
    result
}

fn provider(profile: &str, uri: String) -> ProviderHandle {
    let runtime = ProviderRuntime::new();
    let mut headers = axum::http::HeaderMap::new();
    headers.insert("chatgpt-account-id", "synthetic-account".parse().unwrap());
    let handle = match profile {
        "chat" | "terminal_chat" => runtime.openai_compat_with_stream_usage_profile(
            "models",
            uri,
            "synthetic-access",
            Default::default(),
            Some(0),
            None,
            None,
            None,
            if profile == "terminal_chat" {
                sandhi_core::usage::OpenAiStreamUsageProfile::TerminalChunk
            } else {
                sandhi_core::usage::OpenAiStreamUsageProfile::Standard
            },
        ),
        "responses" => runtime.openai_responses(
            "models",
            uri,
            "synthetic-access",
            Default::default(),
            Some(0),
            None,
            None,
            None,
        ),
        _ => runtime
            .chatgpt_responses(
                "models",
                uri,
                "synthetic-access",
                headers,
                Some(0),
                None,
                None,
                None,
            )
            .with_credential_expiry(4_000_000_000),
    };
    handle.with_backend_binding(sandhi_providers::BackendBinding {
        id: "matrix-source".into(),
        revision: 1,
        provider: "models".into(),
        credential_ref: "openai:fixture".into(),
        credential_generation: 1,
        endpoint_family: if matches!(profile, "chat" | "terminal_chat") {
            sandhi_core::EndpointFamilyV1::OpenaiChatCompletions
        } else {
            sandhi_core::EndpointFamilyV1::OpenaiResponses
        },
        endpoint_profile: if profile == "codex" {
            "chatgpt_codex"
        } else if profile == "terminal_chat" {
            "openai_terminal_chunk_usage"
        } else {
            "standard"
        }
        .into(),
        models: vec![serde_json::from_value(json!({"id":"gpt-4o-mini"})).unwrap()],
        cache_policy: None,
    })
}

#[tokio::test]
async fn tracked_typed_streams_join_all_ingress_formats_to_source_usage_and_paired_receipts() {
    for profile in ["chat", "terminal_chat", "responses", "codex"] {
        for flavor in ["chat", "messages", "responses", "gemini"] {
            for mode in [
                "final",
                "zero",
                "failed",
                "missing",
                "missing_terminal",
                "post_terminal",
                "truncated",
            ] {
                if matches!(profile, "chat" | "terminal_chat") && mode == "failed" {
                    continue;
                }
                let upstream = MockServer::start().await;
                Mock::given(method("POST"))
                    .respond_with(
                        ResponseTemplate::new(200)
                            .insert_header("x-request-id", "synthetic-origin")
                            .set_body_raw(wire(profile, mode), "text/event-stream"),
                    )
                    .expect(1)
                    .mount(&upstream)
                    .await;
                let dir = tempfile::tempdir().unwrap();
                let database = dir.path().join("owned.db");
                let sink = Arc::new(InMemorySink::new());
                let state = state(
                    profile,
                    upstream.uri(),
                    &database,
                    sink.clone(),
                    json!({"setup_ms":2000,"idle_ms":2000,"body_ms":5000}),
                );
                let (route, request) = ingress(flavor);
                let response = build_app(state.clone())
                    .oneshot(owned_request(route, request))
                    .await
                    .unwrap();
                assert_eq!(
                    response.status(),
                    StatusCode::OK,
                    "{profile}/{flavor}/{mode}"
                );
                let request_id = response.headers()["x-sandhi-request-id"]
                    .to_str()
                    .unwrap()
                    .to_owned();
                let output = axum::body::to_bytes(response.into_body(), 100_000).await;
                let clean_delivery = matches!(mode, "final" | "zero");
                let final_usage = clean_delivery || mode == "failed";
                assert_eq!(output.is_ok(), clean_delivery, "{profile}/{flavor}/{mode}");
                if clean_delivery {
                    assert!(String::from_utf8_lossy(&output.unwrap()).contains("reply"));
                }
                tokio::time::timeout(
                    std::time::Duration::from_secs(3),
                    state.lifecycle.wait_idle(),
                )
                .await
                .unwrap();
                let conn = rusqlite::Connection::open(&database).unwrap();
                let (context, attempt, usage):(String,String,String) = conn.query_row(
                    "SELECT c.context_json,a.evidence_json,o.usage_json FROM budget_execution_context c JOIN budget_physical_attempt a USING(execution_id) JOIN budget_terminal_observation o USING(execution_id) JOIN budget_request_correlation r USING(execution_id) WHERE r.request_id=?1",
                    [&request_id], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?))).unwrap();
                let context: Value = serde_json::from_str(&context).unwrap();
                let attempt: Value = serde_json::from_str(&attempt).unwrap();
                let usage: Value = serde_json::from_str(&usage).unwrap();
                assert_eq!(context["delivery"], "streaming");
                assert_eq!(
                    context["plane"],
                    if (flavor == "chat" && matches!(profile, "chat" | "terminal_chat"))
                        || (flavor == "responses" && profile == "responses")
                    {
                        "transparent"
                    } else {
                        "translation"
                    }
                );
                assert_eq!(attempt["attempt_ordinal"], 1);
                assert!(attempt["dispatch_observed_at_ms"].is_number());
                assert!(attempt["terminal"].is_object());
                assert_eq!(usage["attempts"], 1);
                if final_usage {
                    assert_eq!(usage["completeness"], "final");
                    assert_eq!(usage["tokens_in"], if mode == "zero" { 0 } else { 2 });
                    assert_eq!(
                        usage["cache_read_tokens"],
                        if mode == "zero" { 0 } else { 6 }
                    );
                    assert_eq!(
                        usage["cache_creation_tokens"],
                        if mode == "zero" { 0 } else { 2 }
                    );
                    assert_eq!(usage["tokens_out"], if mode == "zero" { 0 } else { 4 });
                    assert_eq!(usage["reasoning_tokens"], 0);
                } else {
                    assert_ne!(usage["completeness"], "final");
                }
                let receipts: i64 = conn
                    .query_row("SELECT count(*) FROM budget_settlement_outbox", [], |row| {
                        row.get(0)
                    })
                    .unwrap();
                assert_eq!(receipts, i64::from(final_usage));
                let leases: Vec<(i64, i64)> = conn
                    .prepare("SELECT settled,actual FROM budget_reservation")
                    .unwrap()
                    .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
                    .unwrap()
                    .map(Result::unwrap)
                    .collect();
                assert_eq!(
                    leases,
                    vec![
                        if final_usage {
                            (1, if mode == "zero" { 0 } else { 14 })
                        } else {
                            (0, 0)
                        };
                        2
                    ]
                );
                assert_eq!(sink.events().len(), 1);
                assert_eq!(sink.events()[0].request_id, request_id);
                let calls = upstream.received_requests().await.unwrap();
                assert_eq!(calls.len(), 1);
                let sent: Value = serde_json::from_slice(&calls[0].body).unwrap();
                assert_eq!(sent["stream"], true);
                if matches!(profile, "chat" | "terminal_chat") {
                    assert_eq!(sent["stream_options"]["include_usage"], true);
                }
                if profile == "codex" {
                    assert_eq!(sent["store"], false);
                    assert!(sent.get("max_output_tokens").is_none());
                    assert!(calls[0].headers.contains_key("chatgpt-account-id"));
                }
            }
        }
    }
}

#[tokio::test]
async fn translated_stream_cancellation_keeps_source_final_counts_without_another_send() {
    use futures_util::StreamExt;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;
    for profile in ["chat", "terminal_chat", "responses", "codex"] {
        for case in ["disconnect", "deadline", "shutdown", "expired_shutdown"] {
            let calls = Arc::new(AtomicUsize::new(0));
            let observed = calls.clone();
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let mut frames = wire(profile, "final");
            // Keep source transport open after final usage. Comments are not
            // emitted by typed decoders and cannot invent another usage event.
            frames.push_str(&format!(":{}\n\n", "x".repeat(256 * 1024)));
            let app = axum::Router::new().fallback(axum::routing::post(move || {
                let calls = observed.clone();
                let frames = frames.clone();
                async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    Body::from_stream(async_stream::stream! {
                        yield Ok::<_,std::io::Error>(axum::body::Bytes::from(frames));
                        std::future::pending::<()>().await;
                    })
                }
            }));
            let server = tokio::spawn(async move {
                axum::serve(listener, app).await.unwrap();
            });
            let dir = tempfile::tempdir().unwrap();
            let database = dir.path().join("owned.db");
            let sink = Arc::new(InMemorySink::new());
            let state = state(
                profile,
                format!("http://{address}"),
                &database,
                sink.clone(),
                json!({"setup_ms":1000,"idle_ms":3000,"body_ms":if case=="deadline" {200} else {3000}}),
            );
            let (route, request) = ingress("messages");
            let response = build_app(state.clone())
                .oneshot(owned_request(route, request))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK, "{profile}/{case}");
            let mut body = response.into_body().into_data_stream();
            let first = tokio::time::timeout(Duration::from_secs(2), body.next())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            assert!(!first.is_empty());
            // Join the source, not the projected message_start. The decoder can
            // deliver that start before its next poll has observed upstream bytes.
            tokio::time::timeout(Duration::from_secs(2), async {
                loop {
                    let next = body.next().await.unwrap().unwrap();
                    if String::from_utf8_lossy(&next).contains("reply") {
                        break;
                    }
                }
            })
            .await
            .unwrap();
            if case.ends_with("shutdown") {
                state
                    .lifecycle
                    .begin_quiesce(if case == "expired_shutdown" {
                        Duration::ZERO
                    } else {
                        Duration::from_secs(2)
                    });
            }
            if case == "disconnect" {
                drop(body);
            } else {
                tokio::time::timeout(Duration::from_secs(3), state.lifecycle.wait_idle())
                    .await
                    .unwrap();
                assert!(body.next().await.unwrap().is_err(), "{profile}/{case}");
            }
            tokio::time::timeout(Duration::from_secs(3), state.lifecycle.wait_idle())
                .await
                .unwrap();
            assert_eq!(calls.load(Ordering::SeqCst), 1);
            assert_eq!(sink.events().len(), 1);
            assert_eq!(
                sink.events()[0].usage_completeness,
                sandhi_core::UsageCompleteness::Final,
                "{profile}/{case}"
            );
            let conn = rusqlite::Connection::open(&database).unwrap();
            let receipts: i64 = conn
                .query_row("SELECT count(*) FROM budget_settlement_outbox", [], |row| {
                    row.get(0)
                })
                .unwrap();
            assert_eq!(
                receipts,
                if case == "expired_shutdown" { 0 } else { 1 },
                "{profile}/{case}"
            );
            assert_eq!(
                state.ledger.lock().unwrap().spent("group:platform"),
                if case == "expired_shutdown" { 0 } else { 14 },
                "{profile}/{case}"
            );
            let before = sink.events().len();
            if state.lifecycle.is_running() {
                sandhi_proxy::settlement::buffered::recover(state.clone())
                    .await
                    .unwrap();
            } else {
                assert_eq!(
                    sandhi_proxy::settlement::buffered::recover(state.clone()).await,
                    Err("accounting closed")
                );
            }
            assert_eq!(sink.events().len(), before);
            assert_eq!(calls.load(Ordering::SeqCst), 1);
            server.abort();
        }
    }
}

fn state(
    profile: &str,
    uri: String,
    path: &std::path::Path,
    sink: Arc<InMemorySink>,
    limits: Value,
) -> Arc<ProxyState> {
    let keys = KeyStore::new();
    keys.insert(VirtualKey {
        id: "vk_demo".into(),
        group_id: Some("platform".into()),
        upstream_ref: "up1".into(),
        ..Default::default()
    });
    let mut state = ProxyState::new(
        keys,
        ProxyLedger::Durable(
            sandhi_store::ShardedLedger::open_sharded(path.to_str().unwrap(), 1).unwrap(),
        ),
        sink,
        HashMap::from([("up1".into(), provider(profile, uri))]),
        None,
    );
    state.streaming_deadlines =
        Some(serde_json::from_value(json!({"ceiling_ms":10000,"default":limits})).unwrap());
    let state = Arc::new(state);
    sandhi_proxy::settlement::buffered::enable(
        &state,
        sandhi_proxy::settlement::buffered::Config::new(
            4,
            Duration::from_secs(2),
            Duration::from_secs(2),
        )
        .unwrap(),
    )
    .unwrap();
    state
}
fn owned_request(route: &str, body: Value) -> Request<Body> {
    Request::post(route)
        .header("authorization", "Bearer vk_demo")
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

#[tokio::test]
async fn translated_stream_projection_is_bounded_and_cannot_settle_estimated_output() {
    let upstream = MockServer::start().await;
    let chunk = format!(
        "data: {}\n\n",
        json!({"object":"chat.completion.chunk","choices":[{"index":0,"delta":{"content":"x".repeat(16*1024)},"finish_reason":null}]})
    );
    let mut frames = chunk.repeat(600);
    frames.push_str(&wire("chat", "final"));
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(frames, "text/event-stream"))
        .expect(1)
        .mount(&upstream)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("owned.db");
    let sink = Arc::new(InMemorySink::new());
    let state = state(
        "chat",
        upstream.uri(),
        &database,
        sink.clone(),
        json!({"setup_ms":2000,"idle_ms":2000,"body_ms":5000}),
    );
    let (route, request) = ingress("responses");
    let response = build_app(state.clone())
        .oneshot(owned_request(route, request))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(axum::body::to_bytes(response.into_body(), 16 * 1024 * 1024)
        .await
        .is_err());
    tokio::time::timeout(Duration::from_secs(3), state.lifecycle.wait_idle())
        .await
        .unwrap();
    assert_eq!(upstream.received_requests().await.unwrap().len(), 1);
    assert_eq!(sink.events().len(), 1);
    assert_ne!(
        sink.events()[0].usage_completeness,
        sandhi_core::UsageCompleteness::Final
    );
    let conn = rusqlite::Connection::open(&database).unwrap();
    assert_eq!(
        conn.query_row("SELECT count(*) FROM budget_settlement_outbox", [], |row| {
            row.get::<_, i64>(0)
        })
        .unwrap(),
        0
    );
    assert_eq!(
        conn.query_row(
            "SELECT count(*) FROM budget_reservation WHERE settled=0",
            [],
            |row| row.get::<_, i64>(0)
        )
        .unwrap(),
        2
    );
    assert_eq!(state.ledger.lock().unwrap().spent("group:platform"), 0);
}
