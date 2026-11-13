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
    if profile == "gemini" {
        return gemini_wire(mode);
    }
    if profile.starts_with("messages") {
        return messages_wire(profile, mode);
    }
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

fn written(profile: &str) -> u64 {
    if matches!(profile, "messages_implicit" | "gemini") {
        0
    } else {
        2
    }
}
fn charge(profile: &str) -> u64 {
    12 + written(profile) + if profile == "gemini" { 2 } else { 0 }
}
fn gemini_wire(mode: &str) -> String {
    let mut initial = json!({"promptTokenCount":7,"cachedContentTokenCount":6,"candidatesTokenCount":1,
        "thoughtsTokenCount":1,"toolUsePromptTokenCount":1,"totalTokenCount":10});
    let mut final_counts = json!({"promptTokenCount":7,"cachedContentTokenCount":6,"candidatesTokenCount":4,
        "thoughtsTokenCount":2,"toolUsePromptTokenCount":1,"totalTokenCount":14});
    if mode == "zero" {
        for counts in [&mut initial, &mut final_counts] {
            for value in counts.as_object_mut().unwrap().values_mut() {
                *value = 0.into();
            }
        }
    }
    if mode == "missing" {
        initial = Value::Null;
        final_counts = Value::Null;
    }
    if mode == "missing_input" {
        final_counts
            .as_object_mut()
            .unwrap()
            .remove("promptTokenCount");
    }
    if mode == "missing_output" {
        final_counts
            .as_object_mut()
            .unwrap()
            .remove("candidatesTokenCount");
    }
    if mode == "malformed" {
        final_counts["candidatesTokenCount"] = "4".into();
    }
    if mode == "cache_conflict" {
        final_counts["cachedContentTokenCount"] = 5.into();
    }
    let mut wire = format!(
        "data: {}\n\n",
        json!({"responseId":"gemini_fixture","modelVersion":"gpt-4o-mini",
        "candidates":[{"index":0,"content":{"role":"model","parts":[{"text":"reply"}]}}],
        "usageMetadata":initial})
    );
    if mode == "provider_error" {
        return wire
            + &format!(
                "data: {}\n\n",
                json!({"error":{"code":503,"message":"synthetic"}})
            );
    }
    let mut end = json!({"candidates":[{"index":0}],"usageMetadata":final_counts});
    if mode != "missing_terminal" {
        end["candidates"][0]["finishReason"] = "STOP".into();
    }
    wire += &format!("data: {end}\n\n");
    if mode == "post_terminal" {
        wire+="data: {\"candidates\":[{\"index\":0,\"content\":{\"parts\":[{\"text\":\"late\"}]}}]}\n\n";
    }
    if mode == "truncated" {
        wire += "data: {";
    }
    wire
}
fn message_frame(value: Value) -> String {
    format!(
        "event: {}\ndata: {value}\n\n",
        value["type"].as_str().unwrap()
    )
}
fn messages_wire(profile: &str, mode: &str) -> String {
    let mut usage = json!({"input_tokens":2,"output_tokens":1,"cache_read_input_tokens":6,"cache_creation_input_tokens":written(profile),"output_tokens_details":{"thinking_tokens":0}});
    if mode == "zero" {
        usage = json!({"input_tokens":0,"output_tokens":0,"cache_read_input_tokens":0,"cache_creation_input_tokens":0,"output_tokens_details":{"thinking_tokens":0}});
    }
    if profile == "messages_implicit" {
        usage
            .as_object_mut()
            .unwrap()
            .remove("cache_creation_input_tokens");
    }
    if mode == "missing_input" {
        usage.as_object_mut().unwrap().remove("input_tokens");
    }
    let mut wire = message_frame(
        json!({"type":"message_start","message":{"id":"msg_fixture","type":"message","role":"assistant","content":[],"model":"gpt-4o-mini","usage":if mode=="missing" {Value::Null} else {usage}}}),
    );
    wire += &message_frame(
        json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
    );
    wire += &message_frame(
        json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"reply"}}),
    );
    wire += &message_frame(json!({"type":"content_block_stop","index":0}));
    if mode == "provider_error" {
        wire += &message_frame(
            json!({"type":"error","error":{"type":"overloaded_error","message":"synthetic"}}),
        );
        return wire;
    }
    let mut usage = match mode {
        "missing" => Value::Null,
        "missing_output" => json!({}),
        "malformed" => json!({"output_tokens":"4"}),
        "zero" => json!({"output_tokens":0}),
        _ => json!({"output_tokens":4}),
    };
    if mode == "cache_conflict" {
        usage["cache_read_input_tokens"] = json!(5);
    }
    wire += &message_frame(
        json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":usage}),
    );
    if mode != "missing_terminal" {
        wire += &message_frame(json!({"type":"message_stop"}));
    }
    if mode == "post_terminal" {
        wire += &message_frame(json!({"type":"ping"}));
    }
    if mode == "truncated" {
        wire += "data: {";
    }
    wire
}

fn provider(profile: &str, uri: String) -> ProviderHandle {
    let runtime = ProviderRuntime::new();
    let mut headers = axum::http::HeaderMap::new();
    headers.insert("chatgpt-account-id", "synthetic-account".parse().unwrap());
    let handle = match profile {
        "gemini" => runtime.gemini(
            uri,
            "synthetic-access",
            sandhi_providers::GeminiAuthScheme::ApiKey,
            Default::default(),
            Some(0),
            None,
            None,
            None,
        ),
        "messages" | "messages_implicit" => runtime.anthropic_with_usage_profile(
            uri,
            "synthetic-access",
            sandhi_providers::AnthropicAuthScheme::ApiKey,
            Default::default(),
            Some(0),
            None,
            None,
            None,
            if profile == "messages_implicit" {
                sandhi_core::usage::AnthropicUsageProfile::ImplicitCache
            } else {
                sandhi_core::usage::AnthropicUsageProfile::SeparateCacheWrites
            },
        ),
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
        } else if profile.starts_with("messages") {
            sandhi_core::EndpointFamilyV1::AnthropicMessages
        } else if profile == "gemini" {
            sandhi_core::EndpointFamilyV1::GeminiGenerateContent
        } else {
            sandhi_core::EndpointFamilyV1::OpenaiResponses
        },
        endpoint_profile: if profile == "codex" {
            "chatgpt_codex"
        } else if profile == "terminal_chat" {
            "openai_terminal_chunk_usage"
        } else if profile == "messages_implicit" {
            "anthropic_implicit_cache"
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
    for profile in [
        "chat",
        "terminal_chat",
        "responses",
        "codex",
        "messages",
        "messages_implicit",
        "gemini",
    ] {
        for flavor in ["chat", "messages", "responses", "gemini"] {
            for mode in [
                "final",
                "zero",
                "failed",
                "missing",
                "missing_terminal",
                "post_terminal",
                "truncated",
                "missing_input",
                "missing_output",
                "malformed",
                "provider_error",
                "cache_conflict",
            ] {
                if (matches!(
                    profile,
                    "chat" | "terminal_chat" | "messages" | "messages_implicit" | "gemini"
                ) && mode == "failed")
                    || (!(profile.starts_with("messages") || profile == "gemini")
                        && matches!(
                            mode,
                            "missing_input"
                                | "missing_output"
                                | "malformed"
                                | "provider_error"
                                | "cache_conflict"
                        ))
                {
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
                        || (flavor == "messages" && profile.starts_with("messages"))
                        || (flavor == "gemini" && profile == "gemini")
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
                        if mode == "zero" { 0 } else { written(profile) }
                    );
                    assert_eq!(usage["tokens_out"], if mode == "zero" { 0 } else { 4 });
                    assert_eq!(
                        usage["reasoning_tokens"],
                        if profile == "gemini" && mode != "zero" {
                            2
                        } else {
                            0
                        }
                    );
                    assert_eq!(usage["reasoning_included"], profile != "gemini");
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
                            (
                                1,
                                if mode == "zero" {
                                    0
                                } else {
                                    charge(profile) as i64
                                },
                            )
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
                if profile == "gemini" {
                    assert!(sent.get("stream").is_none());
                    assert!(calls[0]
                        .url
                        .path()
                        .ends_with("/models/gpt-4o-mini:streamGenerateContent"));
                    assert_eq!(calls[0].url.query(), Some("alt=sse"));
                } else {
                    assert_eq!(sent["stream"], true);
                }
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
    for profile in [
        "chat",
        "terminal_chat",
        "responses",
        "codex",
        "messages",
        "messages_implicit",
        "gemini",
    ] {
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
            if !String::from_utf8_lossy(&first).contains("reply") {
                tokio::time::timeout(Duration::from_secs(2), async {
                    loop {
                        let next = body.next().await.unwrap().unwrap();
                        if String::from_utf8_lossy(&next).contains("reply") {
                            break;
                        }
                    }
                })
                .await
                .unwrap_or_else(|_| panic!("source reply was not observed: {profile}/{case}"));
            }
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
                // Complete native events may already be queued before the
                // terminal transport error; their chunk boundaries are not API semantics.
                let saw_error = tokio::time::timeout(Duration::from_secs(3), async {
                    while let Some(chunk) = body.next().await {
                        if chunk.is_err() {
                            return true;
                        }
                    }
                    false
                })
                .await
                .unwrap();
                assert!(saw_error, "{profile}/{case}");
            }
            tokio::time::timeout(Duration::from_secs(3), state.lifecycle.wait_idle())
                .await
                .unwrap();
            assert_eq!(calls.load(Ordering::SeqCst), 1);
            assert_eq!(sink.events().len(), 1);
            assert_eq!(
                sink.events()[0].usage_completeness,
                if profile == "gemini" {
                    sandhi_core::UsageCompleteness::Partial
                } else {
                    sandhi_core::UsageCompleteness::Final
                },
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
                if case == "expired_shutdown" || profile == "gemini" {
                    0
                } else {
                    1
                },
                "{profile}/{case}"
            );
            assert_eq!(
                state.ledger.lock().unwrap().spent("group:platform"),
                if case == "expired_shutdown" || profile == "gemini" {
                    0
                } else {
                    charge(profile)
                },
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
    static TRACE: std::sync::Once = std::sync::Once::new();
    TRACE.call_once(|| {
        let _ = tracing_subscriber::fmt()
            .with_env_filter("sandhi_proxy=warn")
            .try_init();
    });
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

fn tool_wire(profile: &str, reason: &str, args: &str) -> String {
    if profile == "gemini" {
        return format!(
            "data: {}\n\n",
            json!({"responseId":"gemini_tool","modelVersion":"gpt-4o-mini",
            "candidates":[{"index":0,"content":{"role":"model","parts":[{"functionCall":{
                "id":"call1","name":"inspect","args":serde_json::from_str::<Value>(args).unwrap_or_else(|_|json!(args))}}]},
                "finishReason":match reason {"length"=>"MAX_TOKENS","content_filter"=>"SAFETY",_=>"STOP"}}],
            "usageMetadata":{"promptTokenCount":7,"cachedContentTokenCount":6,"toolUsePromptTokenCount":1,
                "candidatesTokenCount":4,"thoughtsTokenCount":2,"totalTokenCount":14}})
        );
    }
    if profile.starts_with("messages") {
        let prefix = messages_wire(profile, "final");
        let start = prefix.split("event: content_block_start").next().unwrap();
        return start.to_owned()
            + &message_frame(
                json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"call_1","name":"inspect","input":{}}}),
            )
            + &message_frame(
                json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":args}}),
            )
            + &message_frame(json!({"type":"content_block_stop","index":0}))
            + &message_frame(
                json!({"type":"message_delta","delta":{"stop_reason":if reason=="tool_calls" {"tool_use"} else {"max_tokens"}},"usage":{"output_tokens":4}}),
            )
            + &message_frame(json!({"type":"message_stop"}));
    }
    let chat = matches!(profile, "chat" | "terminal_chat");
    let usage = if chat {
        json!({"prompt_tokens":10,"completion_tokens":4,"prompt_tokens_details":{"cached_tokens":6,"cache_write_tokens":2},"completion_tokens_details":{"reasoning_tokens":0}})
    } else {
        json!({"input_tokens":10,"output_tokens":4,"input_tokens_details":{"cached_tokens":6,"cache_write_tokens":2},"output_tokens_details":{"reasoning_tokens":0}})
    };
    if chat {
        let mut chunk = json!({"id":"chat_tool","object":"chat.completion.chunk","model":"gpt-4o-mini","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"inspect","arguments":args}}]},"finish_reason":reason}]});
        let mut result = String::new();
        if profile == "terminal_chat" {
            chunk["usage"] = usage.clone();
        }
        result.push_str(&format!("data: {chunk}\n\n"));
        if profile != "terminal_chat" {
            result.push_str(&format!(
                "data: {}\n\n",
                json!({"object":"chat.completion.chunk","choices":[],"usage":usage})
            ));
        }
        result.push_str("data: [DONE]\n\n");
        result
    } else {
        let status = if reason == "tool_calls" {
            "completed"
        } else {
            "incomplete"
        };
        let call = json!({"id":"fc_1","type":"function_call","call_id":"call_1","name":"inspect","arguments":args,"status":"completed"});
        let mut response = json!({"id":"resp_tool","status":status,"model":"gpt-4o-mini","output":[call.clone()],"usage":usage});
        if status == "incomplete" {
            response["incomplete_details"] =
                json!({"reason":if reason=="length" {"max_output_tokens"} else {"content_filter"}});
        }
        [json!({"type":"response.created","response":{"id":"resp_tool","status":"in_progress","model":"gpt-4o-mini","output":[],"usage":null}}),
         json!({"type":"response.output_item.added","output_index":0,"item":{"id":"fc_1","type":"function_call","call_id":"call_1","name":"inspect","arguments":""}}),
         json!({"type":"response.function_call_arguments.delta","output_index":0,"item_id":"fc_1","delta":args}),
         json!({"type":"response.output_item.done","output_index":0,"item":call}),
         json!({"type":format!("response.{status}"),"response":response})]
        .into_iter().map(|event|format!("data: {event}\n\n")).collect()
    }
}

async fn tool_delivery(profile: &str, flavor: &str, reason: &str, args: &str) -> (bool, String) {
    source_delivery(profile, flavor, tool_wire(profile, reason, args)).await
}

async fn source_delivery(profile: &str, flavor: &str, frames: String) -> (bool, String) {
    use futures_util::StreamExt;
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(frames, "text/event-stream"))
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
    assert_eq!(response.status(), StatusCode::OK, "{profile}/{flavor}");
    let mut stream = response.into_body().into_data_stream();
    let mut bytes = Vec::new();
    let mut clean = true;
    while let Some(item) = stream.next().await {
        match item {
            Ok(chunk) => bytes.extend_from_slice(&chunk),
            Err(_) => {
                clean = false;
                break;
            }
        }
    }
    drop(stream);
    tokio::time::timeout(Duration::from_secs(3), state.lifecycle.wait_idle())
        .await
        .unwrap();
    assert_eq!(upstream.received_requests().await.unwrap().len(), 1);
    assert_eq!(sink.events().len(), 1);
    assert_eq!(
        sink.events()[0].usage_completeness,
        sandhi_core::UsageCompleteness::Final,
        "{profile}/{flavor}"
    );
    let conn = rusqlite::Connection::open(&database).unwrap();
    assert_eq!(
        conn.query_row(
            "SELECT charged_tokens FROM budget_settlement_outbox",
            [],
            |row| row.get::<_, i64>(0)
        )
        .unwrap(),
        charge(profile) as i64
    );
    assert_eq!(
        conn.query_row(
            "SELECT count(*) FROM budget_reservation WHERE settled=1 AND actual=?1",
            [charge(profile) as i64],
            |row| row.get::<_, i64>(0)
        )
        .unwrap(),
        2
    );
    (clean, String::from_utf8(bytes).unwrap())
}

#[tokio::test]
async fn messages_native_output_is_preserved_or_refused_without_losing_source_accounting() {
    for profile in ["messages", "messages_implicit"] {
        for scenario in [
            "signature",
            "opaque_block",
            "future_event",
            "tool_caller",
            "initial_tool",
            "empty_tool",
        ] {
            let ordinary = messages_wire(profile, "final");
            let mut frames = ordinary
                .split("event: content_block_start")
                .next()
                .unwrap()
                .to_owned();
            match scenario {
                "signature" => {
                    frames += &message_frame(
                        json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}}),
                    );
                    frames += &message_frame(
                        json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"synthetic thinking"}}),
                    );
                    frames += &message_frame(
                        json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"synthetic-signature"}}),
                    );
                    frames += &message_frame(json!({"type":"content_block_stop","index":0}));
                }
                "opaque_block" => {
                    frames += &message_frame(
                        json!({"type":"content_block_start","index":0,"content_block":{"type":"redacted_thinking","data":"synthetic-native-state"}}),
                    );
                    frames += &message_frame(json!({"type":"content_block_stop","index":0}));
                }
                "future_event" => {
                    frames += &message_frame(
                        json!({"type":"future_native_event","state":"synthetic-native-state"}),
                    )
                }
                _ => {
                    let mut block = json!({"type":"tool_use","id":"call_1","name":"inspect","input":if scenario=="empty_tool" {json!({})} else {json!({"target":"a"})}});
                    if scenario == "tool_caller" {
                        block["caller"] = json!({"type":"code_execution","tool_id":"issuer-bound"});
                    }
                    frames += &message_frame(
                        json!({"type":"content_block_start","index":0,"content_block":block}),
                    );
                    frames += &message_frame(json!({"type":"content_block_stop","index":0}));
                }
            }
            frames += &message_frame(
                json!({"type":"message_delta","delta":{"stop_reason":if scenario.ends_with("tool")||scenario=="tool_caller" {"tool_use"} else {"end_turn"}},"usage":{"output_tokens":4}}),
            );
            frames += &message_frame(json!({"type":"message_stop"}));
            for flavor in ["chat", "messages", "responses", "gemini"] {
                let (clean, output) = source_delivery(profile, flavor, frames.clone()).await;
                let portable = matches!(scenario, "initial_tool" | "empty_tool");
                assert_eq!(
                    clean,
                    flavor == "messages" || portable,
                    "{profile}/{scenario}/{flavor}: {output}"
                );
                if flavor == "messages" {
                    assert_eq!(output, frames, "transparent native bytes");
                }
                if portable {
                    assert!(output.contains("inspect"));
                }
                if flavor != "messages" && !portable {
                    assert!(
                        !output.contains("synthetic-signature")
                            && !output.contains("synthetic-native-state")
                            && !output.contains("issuer-bound")
                    );
                }
            }
        }
    }
}

#[tokio::test]
async fn gemini_native_output_and_tools_preserve_source_accounting_across_projection_refusals() {
    for (scenario, portable) in [
        ("signature", false),
        ("media", false),
        ("metadata", false),
        ("multiple", false),
        ("missing_args", false),
        ("string_args", false),
        ("null_args", false),
        ("array_args", false),
        ("partial_args", false),
        ("duplicate_id", false),
        ("empty_object", true),
        ("two_calls", true),
        ("continuation", false),
    ] {
        let mut candidate = json!({"index":0,"content":{"role":"model","parts":[{"functionCall":{
            "name":"lookup","args":{}}}]}});
        match scenario {
            "signature" => {
                candidate["content"]["parts"] =
                    json!([{"text":"reply","thoughtSignature":"opaque-signature"}])
            }
            "media" => {
                candidate["content"]["parts"] =
                    json!([{"inlineData":{"mimeType":"image/png","data":"opaque-media"}}])
            }
            "metadata" => {
                candidate["content"]["parts"] = json!([{"text":"reply"}]);
                candidate["groundingMetadata"] = json!({"opaque":"provenance"});
            }
            "missing_args" => {
                candidate["content"]["parts"][0]["functionCall"]
                    .as_object_mut()
                    .unwrap()
                    .remove("args");
            }
            "string_args" => candidate["content"]["parts"][0]["functionCall"]["args"] = "{}".into(),
            "null_args" => candidate["content"]["parts"][0]["functionCall"]["args"] = Value::Null,
            "array_args" => candidate["content"]["parts"][0]["functionCall"]["args"] = json!([]),
            "partial_args" => {
                candidate["content"]["parts"][0]["functionCall"]["partialArgs"] =
                    json!([{"value":"fragment"}])
            }
            "duplicate_id" => {
                candidate["content"]["parts"][0]["functionCall"]["id"] = "same-id".into()
            }
            _ => {}
        }
        let counts = json!({"promptTokenCount":7,"cachedContentTokenCount":6,"toolUsePromptTokenCount":1,
            "candidatesTokenCount":4,"thoughtsTokenCount":2,"totalTokenCount":14});
        let mut frames = format!(
            "data: {}\n\n",
            json!({"responseId":"gemini-native","modelVersion":"gpt-4o-mini","candidates":[candidate.clone()]})
        );
        if matches!(scenario, "two_calls" | "duplicate_id") {
            let id = if scenario == "two_calls" {
                "call-b"
            } else {
                "same-id"
            };
            frames += &format!(
                "data: {}\n\n",
                json!({"candidates":[{"index":0,"content":{"role":"model","parts":[
                {"functionCall":{"name":"lookup","id":id,"args":{"target":"b"}}}]}}]})
            );
        }
        let mut terminal =
            json!({"candidates":[{"index":0,"finishReason":"STOP"}],"usageMetadata":counts});
        if scenario == "continuation" {
            terminal["candidates"][0]["finishReason"] = "CONTINUATION".into();
            terminal["continuationToken"] = "opaque-continuation".into();
        }
        if scenario == "multiple" {
            terminal["candidates"]
                .as_array_mut()
                .unwrap()
                .push(json!({"index":1,"finishReason":"STOP"}));
        }
        frames += &format!("data: {terminal}\n\n");
        for flavor in ["chat", "messages", "responses", "gemini"] {
            let (clean, output) = source_delivery("gemini", flavor, frames.clone()).await;
            assert_eq!(
                clean,
                flavor == "gemini" || portable,
                "{scenario}/{flavor}: {output}"
            );
            if flavor == "gemini" {
                assert!(
                    output.contains(&candidate.to_string()),
                    "native candidate changed: {scenario}"
                );
                assert!(
                    output.contains(&terminal.to_string()),
                    "native terminal changed: {scenario}"
                );
            } else if scenario == "two_calls" {
                assert!(
                    output.contains("gemini_call_") && output.contains("call-b"),
                    "{flavor}: {output}"
                );
            } else if !portable {
                assert!(
                    !output.contains("opaque-signature")
                        && !output.contains("opaque-media")
                        && !output.contains("opaque-continuation")
                );
            }
        }
    }
}

#[tokio::test]
async fn gemini_tool_projection_refuses_invalid_arguments_without_losing_final_accounting() {
    for profile in [
        "chat",
        "terminal_chat",
        "responses",
        "codex",
        "messages",
        "messages_implicit",
    ] {
        for args in [
            r#"{"target":"a"}"#,
            r#"{"target":""#,
            "",
            "[1]",
            "null",
            r#"{"target":1,"target":2}"#,
        ] {
            let (clean, wire) = tool_delivery(profile, "gemini", "length", args).await;
            let actual_empty = profile.starts_with("messages") && args.is_empty();
            let valid = args == r#"{"target":"a"}"# || actual_empty;
            assert_eq!(clean, valid, "{profile}/{args}: {wire}");
            if valid {
                assert!(
                    wire.contains("inspect") && wire.contains("MAX_TOKENS"),
                    "{profile}: {wire}"
                );
                let parts: Vec<Value> = wire
                    .lines()
                    .filter_map(|line| line.strip_prefix("data: "))
                    .map(|data| serde_json::from_str(data).unwrap())
                    .collect();
                assert!(parts.iter().any(|event| event
                    .pointer("/candidates/0/content/parts/0/functionCall/args")
                    == Some(&if actual_empty {
                        json!({})
                    } else {
                        json!({"target":"a"})
                    })));
            } else {
                assert!(
                    !wire.contains("functionCall"),
                    "invented tool: {profile}/{args}: {wire}"
                );
            }
        }
    }
}

#[tokio::test]
async fn chat_native_output_or_portable_projection_keeps_source_counts_and_both_receipts() {
    for profile in ["chat", "terminal_chat"] {
        for scenario in [
            "text",
            "reasoning",
            "audio",
            "opaque",
            "multiple",
            "fragments",
            "invalid_args",
            "duplicate_ids",
            "after_tool",
            "id_drift",
            "model_drift",
        ] {
            let mut delta = json!({"role":"assistant","content":"reply"});
            match scenario {
                "reasoning" => delta["reasoning_content"] = "materialized".into(),
                "audio" => delta["audio"] = json!({"id":"opaque-audio"}),
                "opaque" => delta["vendor_state"] = json!({"signature":"opaque-state"}),
                _ => {}
            }
            let mut values = vec![
                json!({"id":"chat-fixture","object":"chat.completion.chunk","model":"gpt-4o-mini",
                "choices":[{"index":0,"delta":delta,"finish_reason":null}]}),
            ];
            let tools = matches!(
                scenario,
                "fragments" | "invalid_args" | "duplicate_ids" | "after_tool"
            );
            if tools {
                values[0]["choices"][0]["delta"] = json!({"tool_calls":[{"index":0,"id":"call_","type":"function","function":{"name":"in","arguments":"{"}}]});
                values.push(json!({"object":"chat.completion.chunk","choices":[{"index":0,"delta":{"tool_calls":[
                    {"index":0,"id":"xyz","function":{"name":"spect","arguments":"}"}}]},"finish_reason":null}]}));
                if scenario == "invalid_args" {
                    values[0]["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"] =
                        "[".into();
                    values[1]["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"] =
                        "]".into();
                }
                if scenario == "duplicate_ids" {
                    values[1]["choices"][0]["delta"]["tool_calls"].as_array_mut().unwrap().push(json!({"index":1,"id":"call_xyz","function":{"name":"other","arguments":"{}"}}));
                }
            }
            if scenario == "multiple" {
                values[0]["choices"]
                    .as_array_mut()
                    .unwrap()
                    .push(json!({"index":1,"delta":{"content":"other"},"finish_reason":"stop"}));
            }
            if matches!(scenario, "id_drift" | "model_drift") {
                let mut next = json!({"object":"chat.completion.chunk","choices":[{"index":0,"delta":{"content":"other"},"finish_reason":null}]});
                next[if scenario == "id_drift" {
                    "id"
                } else {
                    "model"
                }] = "different".into();
                values.push(next);
            }
            if scenario == "after_tool" {
                values.push(json!({"object":"chat.completion.chunk","choices":[{"index":0,"delta":{"content":"late prose"},"finish_reason":null}]}));
            }
            let counts = json!({"prompt_tokens":10,"completion_tokens":4,"prompt_tokens_details":{"cached_tokens":6,"cache_write_tokens":2},"completion_tokens_details":{"reasoning_tokens":0}});
            let mut terminal = json!({"object":"chat.completion.chunk","choices":[{"index":0,"delta":{},"finish_reason":if tools {"tool_calls"} else {"stop"}}]});
            if profile == "terminal_chat" {
                terminal["usage"] = counts.clone();
            }
            values.push(terminal);
            if profile == "chat" {
                values.push(json!({"object":"chat.completion.chunk","choices":[],"usage":counts}));
            }
            let frames = values
                .iter()
                .map(|value| format!("data: {value}\n\n"))
                .collect::<String>()
                + "data: [DONE]\n\n";
            for flavor in ["chat", "messages", "responses", "gemini"] {
                let (clean, output) = source_delivery(profile, flavor, frames.clone()).await;
                assert_eq!(
                    clean,
                    flavor == "chat" || matches!(scenario, "text" | "reasoning" | "fragments"),
                    "{profile}/{scenario}/{flavor}: {output}"
                );
                if flavor == "chat" {
                    for value in &values {
                        assert!(
                            output.contains(&value.to_string()),
                            "native changed: {scenario}"
                        );
                    }
                } else if scenario == "fragments" {
                    assert!(
                        output.contains("call_xyz") && output.contains("inspect"),
                        "{profile}/{flavor}: {output}"
                    );
                } else if scenario == "reasoning" {
                    assert!(
                        output.contains("materialized"),
                        "{profile}/{flavor}: {output}"
                    );
                }
            }
        }
    }
}

#[tokio::test]
async fn translated_tool_terminals_preserve_output_limit_and_filter_reasons() {
    for profile in [
        "chat",
        "terminal_chat",
        "responses",
        "codex",
        "messages",
        "messages_implicit",
        "gemini",
    ] {
        for flavor in ["chat", "messages", "responses", "gemini"] {
            for reason in ["tool_calls", "length", "content_filter"] {
                // Messages cannot represent this filter terminal. Refuse after
                // source completion rather than relabel it as end_turn.
                if flavor == "messages"
                    && !profile.starts_with("messages")
                    && reason == "content_filter"
                {
                    let (clean, _) =
                        tool_delivery(profile, flavor, reason, r#"{"target":"a"}"#).await;
                    assert!(
                        !clean,
                        "filter silently became a completed Messages turn: {profile}"
                    );
                    continue;
                }
                if profile.starts_with("messages") && reason == "content_filter" {
                    continue;
                }
                let (clean, wire) =
                    tool_delivery(profile, flavor, reason, r#"{"target":"a"}"#).await;
                assert!(clean, "{profile}/{flavor}/{reason}: {wire}");
                assert!(
                    wire.contains("inspect"),
                    "{profile}/{flavor}/{reason}: {wire}"
                );
                let expected = match (flavor, reason) {
                    ("chat", "tool_calls") => "tool_calls",
                    ("chat", other) => other,
                    ("messages", "tool_calls") => "tool_use",
                    ("messages", _) => "max_tokens",
                    ("responses", "tool_calls") => "response.completed",
                    ("responses", _) => "response.incomplete",
                    (_, "length") => "MAX_TOKENS",
                    (_, "content_filter") => "SAFETY",
                    _ => "STOP",
                };
                assert!(
                    wire.contains(expected),
                    "{profile}/{flavor}/{reason}: {wire}"
                );
            }
        }
    }
}

#[tokio::test]
async fn buffered_gemini_tool_projection_has_the_same_strict_argument_boundary() {
    for profile in ["chat", "terminal_chat", "responses", "codex"] {
        for args in [
            r#"{"target":"a"}"#,
            r#"{"target":""#,
            "",
            "[1]",
            "null",
            r#"{"target":1,"target":2}"#,
        ] {
            let upstream = MockServer::start().await;
            let template = if profile == "codex" {
                ResponseTemplate::new(200)
                    .set_body_raw(tool_wire(profile, "length", args), "text/event-stream")
            } else {
                let body = if matches!(profile, "chat" | "terminal_chat") {
                    json!({"id":"chat_buffered_tool","object":"chat.completion","model":"gpt-4o-mini",
                        "choices":[{"index":0,"message":{"role":"assistant","tool_calls":[{"id":"call_1","type":"function","function":{"name":"inspect","arguments":args}}]},"finish_reason":"length"}],
                        "usage":{"prompt_tokens":10,"completion_tokens":4,"prompt_tokens_details":{"cached_tokens":6,"cache_write_tokens":2},"completion_tokens_details":{"reasoning_tokens":0}}})
                } else {
                    json!({"id":"resp_buffered_tool","object":"response","status":"incomplete","incomplete_details":{"reason":"max_output_tokens"},"model":"gpt-4o-mini",
                        "output":[{"type":"function_call","call_id":"call_1","name":"inspect","arguments":args}],
                        "usage":{"input_tokens":10,"output_tokens":4,"input_tokens_details":{"cached_tokens":6,"cache_write_tokens":2},"output_tokens_details":{"reasoning_tokens":0}}})
                };
                ResponseTemplate::new(200).set_body_json(body)
            };
            Mock::given(method("POST"))
                .respond_with(template)
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
            let (route, request) = ingress("gemini");
            let route = route.replace(":streamGenerateContent", ":generateContent");
            let response = build_app(state.clone())
                .oneshot(owned_request(&route, request))
                .await
                .unwrap();
            let valid = args == r#"{"target":"a"}"#;
            assert_eq!(
                response.status(),
                if valid {
                    StatusCode::OK
                } else {
                    StatusCode::BAD_GATEWAY
                },
                "{profile}/{args}"
            );
            if !valid {
                assert_eq!(response.headers()["x-should-retry"], "false");
            }
            let body = axum::body::to_bytes(response.into_body(), 100_000)
                .await
                .unwrap();
            let body: Value = serde_json::from_slice(&body).unwrap();
            if valid {
                assert_eq!(
                    body["candidates"][0]["content"]["parts"][0]["functionCall"]["args"],
                    json!({"target":"a"})
                );
                assert_eq!(body["candidates"][0]["finishReason"], "MAX_TOKENS");
            } else {
                assert!(body.get("candidates").is_none());
            }
            assert_eq!(upstream.received_requests().await.unwrap().len(), 1);
            assert_eq!(sink.events().len(), 1);
            assert_eq!(
                sink.events()[0].usage_completeness,
                sandhi_core::UsageCompleteness::Final
            );
            let conn = rusqlite::Connection::open(&database).unwrap();
            assert_eq!(
                conn.query_row(
                    "SELECT charged_tokens FROM budget_settlement_outbox",
                    [],
                    |row| row.get::<_, i64>(0)
                )
                .unwrap(),
                14
            );
            assert_eq!(
                conn.query_row(
                    "SELECT count(*) FROM budget_reservation WHERE settled=1 AND actual=14",
                    [],
                    |row| row.get::<_, i64>(0)
                )
                .unwrap(),
                2
            );
        }
    }
}
