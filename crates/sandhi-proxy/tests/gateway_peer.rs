//! Explicit gateway peers switch source conventions without importing backend authority.
use futures_util::StreamExt;
use sandhi_core::{
    ChatRequestV1, ChatStreamEventV1, InMemorySink, KeyStore, UsageCompleteness, VirtualKey,
};
use sandhi_providers::{ProviderHandle, ProviderOptions, ProviderRuntime};
use sandhi_proxy::{build_app, ProxyLedger, ProxyState};
use serde_json::{json, Value};
use std::{collections::HashMap, sync::Arc, time::Duration};
use wiremock::{matchers::method, Mock, MockServer, ResponseTemplate};

struct TestDirectory(Option<tempfile::TempDir>);
impl TestDirectory {
    fn new() -> Self {
        Self(Some(tempfile::tempdir().unwrap()))
    }
    fn path(&self) -> &std::path::Path {
        self.0.as_ref().unwrap().path()
    }
}
impl Drop for TestDirectory {
    fn drop(&mut self) {
        if std::thread::panicking() {
            if let Some(dir) = self.0.take() {
                eprintln!("retained synthetic peer fixture: {}", dir.keep().display());
            }
        }
    }
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
fn frame(value: Value) -> String {
    format!("data: {value}\n\n")
}
fn wire(profile: &str) -> String {
    match profile {
        "chat" | "terminal_chat" => {
            frame(
                json!({"id":"chat_fixture","object":"chat.completion.chunk","model":"gpt-4o-mini","choices":[{"index":0,"delta":{"content":"reply"},"finish_reason":if profile=="terminal_chat" {Value::Null}else{json!("stop")}}]}),
            ) + &frame(
                json!({"object":"chat.completion.chunk","choices":if profile=="terminal_chat" {json!([{"index":0,"delta":{},"finish_reason":"stop"}])}else{json!([])},"usage":buffered(profile)["usage"]}),
            ) + "data: [DONE]\n\n"
        }
        "messages" | "messages_implicit" => {
            let mut start = buffered(profile);
            start["content"] = json!([]);
            start["stop_reason"] = Value::Null;
            start["usage"]["output_tokens"] = json!(1);
            [json!({"type":"message_start","message":start}),
             json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
             json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"reply"}}),
             json!({"type":"content_block_stop","index":0}),
             json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":4}}),
             json!({"type":"message_stop"})].into_iter().map(frame).collect()
        }
        "gemini" => {
            frame(
                json!({"responseId":"gemini_fixture","modelVersion":"gpt-4o-mini","candidates":[{"index":0,"content":{"role":"model","parts":[{"text":"reply"}]}}],"usageMetadata":{"promptTokenCount":7,"cachedContentTokenCount":6,"candidatesTokenCount":1,"thoughtsTokenCount":1,"toolUsePromptTokenCount":1,"totalTokenCount":10}}),
            ) + &frame(
                json!({"candidates":[{"index":0,"finishReason":"STOP"}],"usageMetadata":buffered(profile)["usageMetadata"]}),
            )
        }
        _ => codex_wire(),
    }
}
fn provider(profile: &str, uri: String) -> ProviderHandle {
    let (protocol, endpoint_profile) = match profile {
        "chat" => ("chat_completions", "standard"),
        "terminal_chat" => ("chat_completions", "openai_terminal_chunk_usage"),
        "messages" => ("anthropic_messages", "standard"),
        "messages_implicit" => ("anthropic_messages", "anthropic_implicit_cache"),
        "gemini" => ("gemini_generate_content", "standard"),
        "codex" => ("chatgpt_responses", "chatgpt_codex"),
        _ => ("responses", "standard"),
    };
    let label = format!("vendor-{profile}");
    let mut headers = axum::http::HeaderMap::new();
    if profile == "codex" {
        headers.insert("chatgpt-account-id", "synthetic-account".parse().unwrap());
    }
    let mut handle = ProviderRuntime::new()
        .configured_provider(ProviderOptions {
            provider: label.clone(),
            model: "gpt-4o-mini".into(),
            api_key: "synthetic-access".into(),
            base_url: Some(uri),
            headers,
            max_retries: Some(0),
            timeout_secs: None,
            stream_setup_timeout_secs: None,
            stream_idle_timeout_secs: None,
            auth_scheme: None,
            protocol: Some(protocol.into()),
            endpoint_profile: Some(endpoint_profile.into()),
            peer_role: None,
        })
        .unwrap()
        .handle;
    if profile == "codex" {
        handle = handle.with_credential_expiry(4_000_000_000);
    }
    let family = handle.family().endpoint_family();
    handle.with_backend_binding(sandhi_providers::BackendBinding {
        id: format!("matrix-source-{profile}"),
        revision: 1,
        provider: label,
        credential_ref: "synthetic:fixture".into(),
        credential_generation: 1,
        endpoint_family: family,
        endpoint_profile: endpoint_profile.into(),
        models: vec![serde_json::from_value(json!({"id":"gpt-4o-mini"})).unwrap()],
        cache_policy: None,
    })
}

fn buffered(profile: &str) -> Value {
    match profile {
        "chat" | "terminal_chat" => {
            json!({"id":"chat_fixture","model":"gpt-4o-mini","choices":[{"index":0,"message":{"role":"assistant","content":"reply"},"finish_reason":"stop"}],"usage":{"prompt_tokens":10,"completion_tokens":4,"prompt_tokens_details":{"cached_tokens":6,"cache_write_tokens":2},"completion_tokens_details":{"reasoning_tokens":0}}})
        }
        "messages" | "messages_implicit" => {
            let mut body = json!({"id":"msg_fixture","type":"message","model":"gpt-4o-mini","role":"assistant","content":[{"type":"text","text":"reply"}],"stop_reason":"end_turn","usage":{"input_tokens":2,"output_tokens":4,"cache_read_input_tokens":6,"cache_creation_input_tokens":written(profile),"output_tokens_details":{"thinking_tokens":0}}});
            if profile == "messages_implicit" {
                body["usage"]
                    .as_object_mut()
                    .unwrap()
                    .remove("cache_creation_input_tokens");
            }
            body
        }
        "gemini" => {
            json!({"responseId":"gemini_fixture","modelVersion":"gpt-4o-mini","candidates":[{"index":0,"content":{"role":"model","parts":[{"text":"reply"}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":7,"cachedContentTokenCount":6,"candidatesTokenCount":4,"thoughtsTokenCount":2,"toolUsePromptTokenCount":1,"totalTokenCount":14}})
        }
        _ => {
            json!({"id":"resp_fixture","object":"response","model":"gpt-4o-mini","status":"completed","output":[{"id":"message_fixture","type":"message","role":"assistant","status":"completed","content":[{"type":"output_text","text":"reply"}]}],"usage":{"input_tokens":10,"output_tokens":4,"input_tokens_details":{"cached_tokens":6,"cache_write_tokens":2},"output_tokens_details":{"reasoning_tokens":0}}})
        }
    }
}
fn codex_wire() -> String {
    let response = buffered("responses");
    let item = response["output"][0].clone();
    [
        json!({"type":"response.in_progress","response":{"id":"resp_fixture","model":"gpt-4o-mini","status":"in_progress","output":[]}}),
        json!({"type":"response.output_item.added","output_index":0,"item":{"id":"message_fixture","type":"message","role":"assistant","status":"in_progress","content":[]}}),
        json!({"type":"response.content_part.added","output_index":0,"item_id":"message_fixture","content_index":0,"part":{"type":"output_text","text":""}}),
        json!({"type":"response.output_text.delta","output_index":0,"item_id":"message_fixture","content_index":0,"delta":"reply"}),
        json!({"type":"response.output_text.done","output_index":0,"item_id":"message_fixture","content_index":0,"text":"reply"}),
        json!({"type":"response.content_part.done","output_index":0,"item_id":"message_fixture","content_index":0,"part":{"type":"output_text","text":"reply"}}),
        json!({"type":"response.output_item.done","output_index":0,"item":item}),
        json!({"type":"response.completed","response":response}),
    ].into_iter().map(|value|format!("data: {value}\n\n")).collect()
}
fn client(root: &str, dialect: &str) -> ProviderHandle {
    let base = match dialect {
        "anthropic_messages" => root.to_owned(),
        "gemini_generate_content" => format!("{root}/v1beta"),
        _ => format!("{root}/v1"),
    };
    ProviderRuntime::new()
        .configured_provider(ProviderOptions {
            provider: "neutral-agent".into(),
            model: "gpt-4o-mini".into(),
            api_key: "vk_demo".into(),
            base_url: Some(base),
            headers: Default::default(),
            max_retries: Some(0),
            timeout_secs: Some(5.0),
            stream_setup_timeout_secs: Some(3.0),
            stream_idle_timeout_secs: Some(3.0),
            auth_scheme: Some("bearer".into()),
            protocol: Some(dialect.into()),
            endpoint_profile: None,
            peer_role: Some("sandhi_gateway_v1".into()),
        })
        .unwrap()
        .handle
}
fn request(native: bool) -> ChatRequestV1 {
    serde_json::from_value(json!({"model":"gpt-4o-mini","max_output_tokens":64,
        "messages":[{"role":"system","content":"Code."},{"role":"user","content":"hello"}],
        "include_native_response":native,"include_native_stream":native,
        "metadata":{"session_id":"one-session"}}))
    .unwrap()
}
async fn gateway(
    handle: ProviderHandle,
    path: &std::path::Path,
    sink: Arc<InMemorySink>,
) -> (Arc<ProxyState>, String, tokio::task::JoinHandle<()>) {
    let keys = KeyStore::new();
    keys.insert(VirtualKey {
        id: "vk_demo".into(),
        group_id: Some("team".into()),
        upstream_ref: "source".into(),
        ..Default::default()
    });
    let mut state = ProxyState::new(
        keys,
        ProxyLedger::durable(path.to_str().unwrap(), 1).unwrap(),
        sink,
        HashMap::from([("source".into(), handle)]),
        None,
    );
    state.streaming_deadlines = Some(
        serde_json::from_value(
            json!({"ceiling_ms":10000,"default":{"setup_ms":2000,"idle_ms":2000,"body_ms":5000}}),
        )
        .unwrap(),
    );
    let state = Arc::new(state);
    sandhi_proxy::settlement::buffered::enable(
        &state,
        sandhi_proxy::settlement::buffered::Config::new(
            16,
            Duration::from_secs(2),
            Duration::from_secs(5),
        )
        .unwrap(),
    )
    .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let app = build_app(state.clone());
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (state, url, task)
}

#[tokio::test]
async fn persistent_gateway_clients_switch_profiles_and_join_source_receipts() {
    let initial = MockServer::start().await;
    let dir = TestDirectory::new();
    let database = dir.path().join("gateway.db");
    let sink = Arc::new(InMemorySink::new());
    let (state, url, server) =
        gateway(provider("chat", initial.uri()), &database, sink.clone()).await;
    let clients: Vec<_> = [
        "chat_completions",
        "anthropic_messages",
        "responses",
        "gemini_generate_content",
    ]
    .into_iter()
    .map(|dialect| (dialect, client(&url, dialect)))
    .collect();
    let mut completed = 0;
    for profile in [
        "chat",
        "terminal_chat",
        "messages",
        "messages_implicit",
        "responses",
        "gemini",
        "codex",
    ] {
        let origin = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(move |request: &wiremock::Request| {
                let body: Value = serde_json::from_slice(&request.body).unwrap();
                let streaming =
                    body["stream"] == true || request.url.path().contains("streamGenerateContent");
                let template = ResponseTemplate::new(200).insert_header(
                    sandhi_core::USAGE_CONVENTION_HEADER,
                    "v1-messages-implicit-cache",
                );
                if profile == "codex" {
                    template.set_body_raw(codex_wire(), "text/event-stream")
                } else if streaming {
                    template.set_body_raw(wire(profile), "text/event-stream")
                } else {
                    template.set_body_json(buffered(profile))
                }
            })
            .mount(&origin)
            .await;
        state
            .providers
            .lock()
            .unwrap()
            .insert("source".into(), provider(profile, origin.uri()));
        for (dialect, client) in &clients {
            for native in [false, true] {
                for streaming in [false, true] {
                    let request = request(native);
                    let usage = if streaming {
                        let events: Vec<_> = client.stream(request).await.unwrap().collect().await;
                        let mut final_usage = None;
                        let mut text = String::new();
                        let mut native_text = false;
                        for event in events {
                            match event.unwrap_or_else(|error| {
                                panic!("{profile}/{dialect}/{native}/{streaming}: {error}")
                            }) {
                                ChatStreamEventV1::TextDelta { delta } => text.push_str(&delta),
                                ChatStreamEventV1::NativeFrame { frame } => {
                                    native_text |= frame.sse.contains("reply");
                                }
                                ChatStreamEventV1::Usage { usage }
                                    if usage.completeness == UsageCompleteness::Final =>
                                {
                                    assert!(final_usage.is_none());
                                    final_usage = Some(usage);
                                }
                                _ => {}
                            }
                        }
                        assert!(
                            text == "reply" || native && native_text,
                            "{profile}/{dialect}/{native}/{streaming}"
                        );
                        final_usage.unwrap()
                    } else {
                        let result = client.complete(request).await.unwrap_or_else(|error| {
                            panic!("{profile}/{dialect}/{native}/{streaming}: {error}")
                        });
                        assert_eq!(
                            result.output.content,
                            Some(sandhi_core::MessageContent::Text("reply".into()))
                        );
                        result.usage
                    };
                    assert_eq!(
                        usage.completeness,
                        UsageCompleteness::Final,
                        "{profile}/{dialect}/{native}/{streaming}"
                    );
                    assert_eq!(
                        (
                            usage.tokens_in,
                            usage.cache_read_tokens,
                            usage.cache_creation_tokens
                        ),
                        (
                            if *dialect == "gemini_generate_content" {
                                2 + written(profile)
                            } else {
                                2
                            },
                            6,
                            if *dialect == "gemini_generate_content" {
                                0
                            } else {
                                written(profile)
                            }
                        ),
                        "{profile}/{dialect}/{native}/{streaming}"
                    );
                    // Gemini's separate thoughts become part of the inclusive output in other client dialects.
                    assert_eq!(
                        usage.tokens_out,
                        if profile == "gemini" && *dialect != "gemini_generate_content" {
                            6
                        } else {
                            4
                        }
                    );
                    assert_eq!(usage.attempts, 1);
                    tokio::time::timeout(Duration::from_secs(3), state.lifecycle.wait_idle())
                        .await
                        .unwrap();
                    completed += 1;
                    let events = sink.events();
                    assert_eq!(events.len(), completed);
                    let id = &events.last().unwrap().request_id;
                    let conn = rusqlite::Connection::open(&database).unwrap();
                    let (source,attempt,receipt):(String,String,i64)=conn.query_row("SELECT o.usage_json,a.evidence_json,s.charged_tokens FROM budget_request_correlation r JOIN budget_terminal_observation o USING(execution_id) JOIN budget_physical_attempt a USING(execution_id) JOIN budget_execution_intent i USING(execution_id) JOIN budget_settlement_outbox s ON s.reservation_id=i.reservation_id WHERE r.request_id=?1",[id],|row|Ok((row.get(0)?,row.get(1)?,row.get(2)?))).unwrap();
                    let source: Value = serde_json::from_str(&source).unwrap();
                    let attempt: Value = serde_json::from_str(&attempt).unwrap();
                    assert_eq!(source["tokens_in"], 2);
                    assert_eq!(source["cache_read_tokens"], 6);
                    assert_eq!(source["cache_creation_tokens"], written(profile));
                    assert_eq!(source["tokens_out"], 4);
                    assert_eq!(
                        source["reasoning_tokens"],
                        if profile == "gemini" { 2 } else { 0 }
                    );
                    assert_eq!(source["reasoning_included"], profile != "gemini");
                    assert_eq!(attempt["attempt_ordinal"], 1);
                    assert_eq!(receipt, charge(profile) as i64);
                    let (context,primary,back,primary_scope,back_scope,attempt_id):(String,i64,i64,String,String,String)=conn.query_row(
                        "SELECT c.context_json,i.reservation_id,b.reservation_id,p.scope,q.scope,a.attempt_id FROM budget_request_correlation r JOIN budget_execution_context c USING(execution_id) JOIN budget_execution_intent i USING(execution_id) JOIN budget_execution_back_leg b USING(execution_id) JOIN budget_physical_attempt a USING(execution_id) JOIN budget_reservation p ON p.id=i.reservation_id JOIN budget_reservation q ON q.id=b.reservation_id WHERE r.request_id=?1",[id],|row|Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?,row.get(4)?,row.get(5)?))).unwrap();
                    let context: Value = serde_json::from_str(&context).unwrap();
                    assert_ne!(primary, back);
                    assert_eq!(primary_scope, "group:team");
                    assert_eq!(context["provider"], format!("vendor-{profile}"));
                    assert_eq!(context["backend"]["id"], format!("matrix-source-{profile}"));
                    assert_eq!(context["upstream_budget_scope"], back_scope);
                    // Existing family slugs remain the upstream budget authority;
                    // custom context labels do not silently rename those limits.
                    assert_eq!(
                        back_scope,
                        match profile {
                            "messages" | "messages_implicit" => "upstream:anthropic".into(),
                            "gemini" => "upstream:gemini".into(),
                            _ => format!("upstream:vendor-{profile}"),
                        }
                    );
                    assert!(
                        attempt.get("provider").is_none(),
                        "durable attempts retain identity/status, not diagnostic labels"
                    );

                    assert_eq!(attempt["attempt_id"], attempt_id);
                    assert_eq!(attempt["invalid"], false);
                    assert_eq!(attempt["dropped_observations"], 0);
                }
            }
        }
        let requests = origin.received_requests().await.unwrap();
        assert_eq!(requests.len(), 16);
        for request in requests {
            let body: Value = serde_json::from_slice(&request.body).unwrap();
            assert!(body.get("sandhi_cache").is_none());
            assert!(!request
                .headers
                .contains_key(sandhi_core::USAGE_CONVENTION_HEADER));
        }
    }
    assert_eq!(completed, 112);
    server.abort();
    let _ = server.await;
}

#[tokio::test]
async fn gateway_owns_cache_compilation_once_and_refuses_unsupported_ttl_before_admission() {
    let origin = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(|request: &wiremock::Request| {
            let body: Value = serde_json::from_slice(&request.body).unwrap();
            if body["stream"] == true {
                ResponseTemplate::new(200).set_body_raw(wire("messages"), "text/event-stream")
            } else {
                ResponseTemplate::new(200).set_body_json(buffered("messages"))
            }
        })
        .mount(&origin)
        .await;
    let handle = provider("messages", origin.uri());
    let mut binding = handle.backend_binding().unwrap().clone();
    binding.cache_policy=Some(serde_json::from_value(json!({"default_intent":{"mode":"auto_stable_prefix","lifetime":"5m"},"models":{"gpt-4o-mini":{"profile":"anthropic_breakpoints","lifetimes":["5m"],"max_breakpoints":4}}})).unwrap());
    let handle = handle.with_backend_binding(binding);
    let dir = TestDirectory::new();
    let database = dir.path().join("cache.db");
    let sink = Arc::new(InMemorySink::new());
    let (state, url, server) = gateway(handle, &database, sink.clone()).await;
    for dialect in [
        "chat_completions",
        "anthropic_messages",
        "responses",
        "gemini_generate_content",
    ] {
        let client = client(&url, dialect);
        for native in [false, true] {
            for streaming in [false, true] {
                let mut request = request(native);
                request.cache_intent = Some(
                    serde_json::from_value(json!({"mode":"auto_stable_prefix","lifetime":"5m"}))
                        .unwrap(),
                );
                if streaming {
                    let events: Vec<_> = client.stream(request).await.unwrap().collect().await;
                    for event in events {
                        event.unwrap();
                    }
                } else {
                    client.complete(request).await.unwrap();
                }
            }
        }
        let before = sink.events().len();
        let mut invalid = request(false);
        invalid.cache_intent = Some(
            serde_json::from_value(json!({"mode":"auto_stable_prefix","lifetime":"1h"})).unwrap(),
        );
        assert!(client.complete(invalid).await.is_err());
        assert_eq!(sink.events().len(), before);
    }
    tokio::time::timeout(Duration::from_secs(3), state.lifecycle.wait_idle())
        .await
        .unwrap();
    let requests = origin.received_requests().await.unwrap();
    assert_eq!(requests.len(), 16);
    for request in requests {
        let body: Value = serde_json::from_slice(&request.body).unwrap();
        assert!(body.get("sandhi_cache").is_none());
        assert_eq!(
            body["system"][0]["cache_control"],
            json!({"type":"ephemeral","ttl":"5m"})
        );
        assert_eq!(body.to_string().matches("cache_control").count(), 1);
    }
    let conn = rusqlite::Connection::open(&database).unwrap();
    let counts:(i64,i64)=conn.query_row("SELECT (SELECT count(*) FROM budget_execution_intent),(SELECT count(*) FROM budget_settlement_outbox)",[],|row|Ok((row.get(0)?,row.get(1)?))).unwrap();
    assert_eq!(counts, (16, 16));
    server.abort();
    let _ = server.await;
}

#[tokio::test]
async fn gateway_peer_invalid_metadata_refuses_before_decoding_without_another_inference() {
    for value in [
        None,
        Some("chatgpt_codex"),
        Some("v2-chat-standard"),
        Some("v1-messages-standard"),
        Some("v1-chat-standard, v1-chat-terminal"),
    ] {
        let origin = MockServer::start().await;
        let mut template = ResponseTemplate::new(200).set_body_json(buffered("chat"));
        if let Some(value) = value {
            template = template.insert_header(sandhi_core::USAGE_CONVENTION_HEADER, value);
        }
        Mock::given(method("POST"))
            .respond_with(template)
            .mount(&origin)
            .await;
        let client = client(&origin.uri(), "chat_completions");
        assert!(matches!(
            client.complete(request(false)).await,
            Err(sandhi_providers::ProviderError::OutputProjection(_))
        ));
        assert!(matches!(
            client.stream(request(false)).await,
            Err(sandhi_providers::ProviderError::OutputProjection(_))
        ));
        assert_eq!(origin.received_requests().await.unwrap().len(), 2);
    }
}

#[tokio::test]
async fn gateway_http_failure_keeps_terminal_typed_status_and_original_request_identity() {
    for dialect in [
        "chat_completions",
        "anthropic_messages",
        "responses",
        "gemini_generate_content",
    ] {
        for body in [
            json!({"error":{"code":"output_projection_error","message":"synthetic refusal","retryable":false,"http_status":409,"request_id":"body-id"}}).to_string(),
            "unstructured ambiguous failure".into(),
            format!("{{\"error\":{{\"code\":\"output_projection_error\",\"message\":\"{}\"}}}}","x".repeat(10000)),
            json!({"error":{"code":"bad\ncode","retryable":true}}).to_string(),
        ] {
            let origin=MockServer::start().await;
            Mock::given(method("POST")).respond_with(ResponseTemplate::new(502)
                .insert_header("x-should-retry","false").insert_header("x-request-id","origin-id").insert_header("x-sandhi-request-id","gateway-id")
                .set_body_raw(body.clone(),"application/json")).mount(&origin).await;
            let client=client(&origin.uri(),dialect);
            let error=client.complete(request(false)).await.unwrap_err().as_typed(None);
            assert!(!error.retryable);assert_eq!(error.http_status,Some(502));assert_eq!(error.request_id.as_deref(),Some("gateway-id"));
            assert_eq!(error.code,if body.contains("synthetic refusal") {"output_projection_error"}else{"gateway_error"});
            let error=match client.stream(request(false)).await {Ok(_)=>panic!("HTTP502 must fail setup"),Err(error)=>error.as_typed(None)};
            assert!(!error.retryable);assert_eq!(error.http_status,Some(502));assert_eq!(error.request_id.as_deref(),Some("gateway-id"));
            assert_eq!(origin.received_requests().await.unwrap().len(),2);
        }
    }
    // A direct-source HTTP502 retains its pre-existing category and retry advice.
    let origin = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(502)
                .insert_header("x-should-retry", "false")
                .set_body_json(
                    json!({"error":{"code":"output_projection_error","retryable":false}}),
                ),
        )
        .mount(&origin)
        .await;
    let error = provider("chat", origin.uri())
        .complete(request(false))
        .await
        .unwrap_err()
        .as_typed(None);
    assert_eq!(error.code, "upstream_error");
    assert!(error.retryable);
    assert_eq!(origin.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn gateway_sse_failure_keeps_delivery_terminal_and_source_rejection_separate() {
    for dialect in [
        "chat_completions",
        "anthropic_messages",
        "responses",
        "gemini_generate_content",
    ] {
        for native in [false, true] {
            let origin = MockServer::start().await;
            let convention = match dialect {
                "chat_completions" => "v1-chat-standard",
                "anthropic_messages" => "v1-messages-standard",
                "responses" => "v1-responses-standard",
                _ => "v1-gemini-standard",
            };
            Mock::given(method("POST")).respond_with(ResponseTemplate::new(200).insert_header(sandhi_core::USAGE_CONVENTION_HEADER,convention)
                .insert_header("x-sandhi-request-id","gateway-stream-id")
                .set_body_raw(frame(json!({"type":"error","error":{"code":"output_projection_error","message":"synthetic terminal refusal","retryable":false,"http_status":502,"request_id":"upstream-frame-id"}})),"text/event-stream")).mount(&origin).await;
            let client = client(&origin.uri(), dialect);
            let (context, receiver) =
                sandhi_providers::AttemptContext::channel("gateway-peer-error", 8).unwrap();
            let events: Vec<_> = client
                .stream_with_attempts(request(native), Default::default(), context)
                .await
                .unwrap()
                .collect()
                .await;
            assert!(!events.iter().any(|event| matches!(
                event,
                Ok(ChatStreamEventV1::Finish { .. }
                    | ChatStreamEventV1::Usage {
                        usage: sandhi_core::UsageV2 {
                            completeness: UsageCompleteness::Final,
                            ..
                        }
                    })
            )));
            let error = events
                .into_iter()
                .find_map(Result::err)
                .expect("terminal delivery refusal")
                .as_typed(None);
            assert_eq!(error.code, "output_projection_error");
            assert!(!error.retryable);
            assert_eq!(error.http_status, Some(502));
            assert_eq!(error.request_id.as_deref(), Some("gateway-stream-id"));
            let observations = receiver.drain();
            assert_eq!(observations.len(), 2);
            assert!(observations.iter().any(|observation| matches!(
                observation.phase,
                sandhi_providers::AttemptPhase::Terminal {
                    outcome: sandhi_providers::AttemptOutcome::ProviderRejected,
                    provider_status: Some(200),
                    ..
                }
            )));
            assert_eq!(origin.received_requests().await.unwrap().len(), 1);
        }
    }
}

#[tokio::test]
async fn real_gateway_unknown_source_usage_never_authorizes_peer_retry_or_zero_settlement() {
    let origin = MockServer::start().await;
    let source = frame(
        json!({"id":"r","object":"chat.completion.chunk","model":"gpt-4o-mini","choices":[{"index":0,"delta":{"content":"reply"},"finish_reason":"stop"}]}),
    ) + "data: [DONE]\n\n";
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(source, "text/event-stream"))
        .mount(&origin)
        .await;
    let dir = TestDirectory::new();
    let database = dir.path().join("unknown.db");
    let sink = Arc::new(InMemorySink::new());
    let (state, url, server) = gateway(provider("chat", origin.uri()), &database, sink).await;
    for dialect in [
        "chat_completions",
        "anthropic_messages",
        "responses",
        "gemini_generate_content",
    ] {
        for native in [false, true] {
            let client = client(&url, dialect);
            // Early body abort can close the connection before HTTP headers are
            // observable. Both setup and post-header failures remain terminal.
            let events: Vec<_> = match client.stream(request(native)).await {
                Ok(stream) => stream.collect().await,
                Err(error) => vec![Err(error)],
            };
            assert!(!events.iter().any(|event| matches!(
                event,
                Ok(ChatStreamEventV1::Finish { .. }
                    | ChatStreamEventV1::Usage {
                        usage: sandhi_core::UsageV2 {
                            completeness: UsageCompleteness::Final,
                            ..
                        }
                    })
            )));
            let error = events
                .into_iter()
                .find_map(Result::err)
                .expect("unknown usage must fail delivery")
                .as_typed(None);
            assert!(!error.retryable, "{dialect}/{native}: {error:?}");
            tokio::time::timeout(Duration::from_secs(3), state.lifecycle.wait_idle())
                .await
                .unwrap();
        }
    }
    assert_eq!(origin.received_requests().await.unwrap().len(), 8);
    let conn = rusqlite::Connection::open(&database).unwrap();
    let counts:(i64,i64,i64)=conn.query_row("SELECT (SELECT count(*) FROM budget_execution_intent),(SELECT count(*) FROM budget_settlement_outbox),(SELECT count(*) FROM budget_reservation WHERE settled=0)",[],|row|Ok((row.get(0)?,row.get(1)?,row.get(2)?))).unwrap();
    assert_eq!(counts, (8, 0, 16));
    server.abort();
    let _ = server.await;
}
