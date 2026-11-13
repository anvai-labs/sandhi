//! Real SDK inline-image inputs through the qualified in-process gateway.
use sandhi_core::{InMemorySink, KeyStore, VirtualKey};
use sandhi_providers::{ProviderHandle, ProviderOptions, ProviderRuntime};
use sandhi_proxy::{build_app, ProxyLedger, ProxyState};
use serde_json::{json, Value};
use std::{collections::HashMap, sync::Arc, time::Duration};
use wiremock::{matchers::method, Mock, MockServer, ResponseTemplate};

struct TestDirectory(Option<tempfile::TempDir>);
impl TestDirectory {
    fn new() -> Self {
        Self(Some(
            if let Some(root) = std::env::var_os("SANDHI_MEDIA_EVIDENCE_DIR") {
                tempfile::tempdir_in(root).unwrap()
            } else {
                tempfile::tempdir().unwrap()
            },
        ))
    }
    fn path(&self) -> &std::path::Path {
        self.0.as_ref().unwrap().path()
    }
}
impl Drop for TestDirectory {
    fn drop(&mut self) {
        if std::thread::panicking() || std::env::var_os("SANDHI_MEDIA_EVIDENCE_DIR").is_some() {
            if let Some(dir) = self.0.take() {
                eprintln!("retained synthetic media fixture: {}", dir.keep().display());
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
    if matches!(profile, "cohere" | "ollama") {
        11
    } else {
        12 + written(profile) + if profile == "gemini" { 2 } else { 0 }
    }
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
             json!({"type":"message_stop"})].into_iter().map(|value| format!("event: {}\ndata: {value}\n\n", value["type"].as_str().unwrap())).collect()
        }
        "gemini" => {
            frame(
                json!({"responseId":"gemini_fixture","modelVersion":"gpt-4o-mini","candidates":[{"index":0,"content":{"role":"model","parts":[{"text":"reply"}]}}],"usageMetadata":{"promptTokenCount":7,"cachedContentTokenCount":6,"candidatesTokenCount":1,"thoughtsTokenCount":1,"toolUsePromptTokenCount":1,"totalTokenCount":10}}),
            ) + &frame(
                json!({"candidates":[{"index":0,"finishReason":"STOP"}],"usageMetadata":buffered(profile)["usageMetadata"]}),
            )
        }
        "cohere" => [json!({"type":"message-start","id":"cohere-image","delta":{"message":{"role":"assistant"}}}),
            json!({"type":"content-start","index":0,"delta":{"message":{"content":{"type":"text","text":""}}}}),
            json!({"type":"content-delta","index":0,"delta":{"message":{"content":{"text":"reply"}}}}),
            json!({"type":"content-end","index":0}),json!({"type":"message-end","delta":{"finish_reason":"COMPLETE","usage":buffered("cohere")["usage"]}})].into_iter().map(frame).collect(),
        "ollama" => format!("{}\n",buffered("ollama")),
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
        "cohere" => ("cohere_chat", "standard"),
        "ollama" => ("ollama_chat", "standard"),
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
        "cohere" => {
            json!({"id":"cohere-image","message":{"role":"assistant","content":[{"type":"text","text":"reply"}]},"finish_reason":"COMPLETE","usage":{"tokens":{"input_tokens":7,"output_tokens":4},"billed_units":{"input_tokens":2,"output_tokens":1}}})
        }
        "ollama" => {
            json!({"model":"gpt-4o-mini","message":{"role":"assistant","content":"reply"},"done":true,"done_reason":"stop","prompt_eval_count":7,"prompt_eval_cached_count":3,"eval_count":4})
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

const IMAGE: &str =
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+jYXkAAAAASUVORK5CYII=";
fn image_bytes(value: &Value) {
    use base64::Engine;
    let encoded = value.as_str().unwrap();
    let actual = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(encoded))
        .unwrap();
    assert_eq!(
        actual,
        base64::engine::general_purpose::STANDARD
            .decode(IMAGE)
            .unwrap()
    );
}
fn image_data_url(value: &Value) {
    let value = value.as_str().unwrap();
    let (mime, encoded) = value.split_once(";base64,").unwrap();
    assert_eq!(mime, "data:image/png");
    image_bytes(&json!(encoded));
}
fn image_on_source(profile: &str, body: &Value) {
    let parts = match profile {
        "responses" | "codex" => body.pointer("/input/0/content").unwrap(),
        "gemini" => body.pointer("/contents/0/parts").unwrap(),
        "ollama" => {
            image_bytes(body.pointer("/messages/0/images/0").unwrap());
            assert_eq!(
                body.pointer("/messages/0/content"),
                Some(&json!("Describe this single synthetic pixel."))
            );
            return;
        }
        _ => body.pointer("/messages/0/content").unwrap(),
    }
    .as_array()
    .unwrap();
    assert_eq!(parts.len(), 2, "{profile}: {parts:?}");
    match profile {
        "responses" | "codex" => {
            assert_eq!(parts[0]["type"], "input_image");
            image_data_url(&parts[0]["image_url"]);
        }
        "messages" => {
            assert_eq!(parts[0]["type"], "image");
            assert_eq!(parts[0]["source"]["type"], "base64");
            assert_eq!(parts[0]["source"]["media_type"], "image/png");
            image_bytes(&parts[0]["source"]["data"]);
        }
        "gemini" => {
            let inline = &parts[0]["inlineData"];
            assert_eq!(
                inline.get("mimeType").or_else(|| inline.get("mime_type")),
                Some(&json!("image/png"))
            );
            image_bytes(&inline["data"]);
        }
        _ => {
            assert_eq!(parts[0]["type"], "image_url");
            image_data_url(&parts[0]["image_url"]["url"]);
        }
    }
    assert_eq!(parts[1]["text"], "Describe this single synthetic pixel.");
}

async fn inline_image_for_source(profile: &'static str) {
    let python =
        std::env::var("SANDHI_MEDIA_SDK_PYTHON").expect("set explicit SDK Python interpreter");
    let helper = std::env::var_os("SANDHI_MEDIA_SDK_HELPER")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../tests/sdk-conformance/media_sdk_client.py")
        });
    let origin = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(move |request: &wiremock::Request| {
            let body: Value = serde_json::from_slice(&request.body).unwrap();
            let stream = body["stream"] == true
                || request.url.path().contains("streamGenerateContent")
                || profile == "codex";
            let response =
                ResponseTemplate::new(200).insert_header("x-request-id", "inline-image-origin");
            if stream {
                response.set_body_raw(
                    wire(profile),
                    if profile == "ollama" {
                        "application/x-ndjson"
                    } else {
                        "text/event-stream"
                    },
                )
            } else {
                response.set_body_json(buffered(profile))
            }
        })
        .mount(&origin)
        .await;
    let dir = TestDirectory::new();
    println!("media-source {profile} evidence={}", dir.path().display());
    let database = dir.path().join("gateway.db");
    let sink = Arc::new(InMemorySink::new());
    let (state, url, server) =
        gateway(provider(profile, origin.uri()), &database, sink.clone()).await;
    for dialect in [
        "chat_completions",
        "anthropic_messages",
        "responses",
        "gemini_generate_content",
    ] {
        for streaming in [false, true] {
            let stdout = dir.path().join(format!("{dialect}-{streaming}.stdout"));
            let stderr = dir.path().join(format!("{dialect}-{streaming}.stderr"));
            let mut child = std::process::Command::new(&python)
                .arg(&helper)
                .arg(&url)
                .arg(dialect)
                .arg(streaming.to_string())
                .arg(profile)
                .env_clear()
                .env("PATH", "/usr/bin:/bin")
                .env("LANG", "C.UTF-8")
                .env("PYTHONNOUSERSITE", "1")
                .stdout(std::fs::File::create(&stdout).unwrap())
                .stderr(std::fs::File::create(&stderr).unwrap())
                .spawn()
                .unwrap();
            let deadline = std::time::Instant::now() + Duration::from_secs(20);
            loop {
                if let Some(status) = child.try_wait().unwrap() {
                    assert!(
                        status.success(),
                        "{profile}/{dialect}/{streaming}: {}",
                        std::fs::read_to_string(&stderr).unwrap()
                    );
                    break;
                }
                if std::time::Instant::now() >= deadline {
                    child.kill().unwrap();
                    child.wait().unwrap();
                    panic!("SDK child deadline: {profile}/{dialect}/{streaming}");
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            let output: Value = serde_json::from_slice(&std::fs::read(&stdout).unwrap()).unwrap();
            assert_eq!(output["text"], "reply");
            tokio::time::timeout(Duration::from_secs(3), state.lifecycle.wait_idle())
                .await
                .unwrap();
            let received = origin.received_requests().await.unwrap();
            let body: Value = serde_json::from_slice(&received.last().unwrap().body).unwrap();
            image_on_source(profile, &body);
            let event = sink.events().last().unwrap().clone();
            let connection = rusqlite::Connection::open(&database).unwrap();
            let (context,usage,attempt,charged):(String,String,String,i64)=connection.query_row("SELECT c.context_json,o.usage_json,a.evidence_json,s.charged_tokens FROM budget_request_correlation r JOIN budget_execution_context c USING(execution_id) JOIN budget_terminal_observation o USING(execution_id) JOIN budget_physical_attempt a USING(execution_id) JOIN budget_execution_intent i USING(execution_id) JOIN budget_settlement_outbox s ON s.reservation_id=i.reservation_id WHERE r.request_id=?1",[&event.request_id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).unwrap();
            let context: Value = serde_json::from_str(&context).unwrap();
            let usage: Value = serde_json::from_str(&usage).unwrap();
            let attempt: Value = serde_json::from_str(&attempt).unwrap();
            assert_eq!(context["backend"]["id"], format!("matrix-source-{profile}"));
            assert_eq!(usage["completeness"], "final");
            let expected = match profile {
                "cohere" => (7, 0, 0, 4),
                "ollama" => (4, 3, 0, 4),
                "gemini" => (2, 6, 0, 4),
                _ => (2, 6, 2, 4),
            };
            assert_eq!(
                (
                    usage["tokens_in"].as_u64().unwrap(),
                    usage["cache_read_tokens"].as_u64().unwrap(),
                    usage["cache_creation_tokens"].as_u64().unwrap(),
                    usage["tokens_out"].as_u64().unwrap()
                ),
                expected
            );

            assert_eq!(attempt["attempt_ordinal"], 1);
            assert_eq!(charged, charge(profile) as i64);
            assert_eq!(
                attempt["terminal"]["provider_request_id"],
                "inline-image-origin"
            );
            assert_eq!(attempt["invalid"], false);
            println!("media-cell {profile}/{dialect}/{streaming}: {}", output);
        }
    }
    assert_eq!(origin.received_requests().await.unwrap().len(), 8);
    let connection = rusqlite::Connection::open(&database).unwrap();
    let (count, settled): (i64, i64) = connection
        .query_row(
            "SELECT count(*),sum(settled) FROM budget_reservation",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!((count, settled), (16, 16));
    state.lifecycle.start_draining();
    server.abort();
    let _ = server.await;
}

macro_rules! media_sources {
    ($($name:ident => $profile:literal),* $(,)?) => {$(
        #[ignore = "requires vendor SDKs; exercised by SDK conformance"]
        #[tokio::test(flavor="multi_thread",worker_threads=2)]
        async fn $name(){inline_image_for_source($profile).await;}
    )*};
}
media_sources! {
    real_sdk_inline_images_chat => "chat",
    real_sdk_inline_images_messages => "messages",
    real_sdk_inline_images_responses => "responses",
    real_sdk_inline_images_gemini => "gemini",
    real_sdk_inline_images_codex => "codex",
    real_sdk_inline_images_cohere => "cohere",
    real_sdk_inline_images_ollama => "ollama",
}
