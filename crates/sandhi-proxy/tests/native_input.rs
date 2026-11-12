//! Native ordered history must survive forced typed execution; incompatible state
//! must fail before reservation or inference. All origins and state are synthetic.

#[tokio::test]
async fn codex_native_terminal_state_survives_stream_buffering_and_next_turn() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(reply(Family::OpenaiResponses, true))
        .expect(3)
        .mount(&upstream)
        .await;
    let (state, sink) = state(Family::OpenaiResponses, &upstream);
    let mut headers = axum::http::HeaderMap::new();
    headers.insert("chatgpt-account-id", "fixture-account".parse().unwrap());
    let provider = ProviderRuntime::new().chatgpt_responses(
        "fixture",
        upstream.uri(),
        "fixture-access",
        headers,
        Some(0),
        None,
        None,
        None,
    );
    state
        .providers
        .lock()
        .unwrap()
        .insert("fixed".into(), provider);
    let (status, buffered) = call(
        &state,
        Family::OpenaiResponses,
        input(Family::OpenaiResponses),
        false,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&buffered)
    );
    let response: Value = serde_json::from_slice(&buffered).unwrap();
    assert_eq!(response["output"][0]["encrypted_content"], "output-opaque");
    let (status, streamed) = call(
        &state,
        Family::OpenaiResponses,
        input(Family::OpenaiResponses),
        true,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let wire = String::from_utf8(streamed).unwrap();
    assert!(wire.contains("output-opaque"));
    assert_eq!(
        wire.matches("\"type\":\"response.completed\"").count(),
        1,
        "{wire}"
    );
    let mut history = response["output"].as_array().unwrap().clone();
    history.push(json!({"role":"user","content":"continue"}));
    let (status, _) = call(
        &state,
        Family::OpenaiResponses,
        json!({"model":"fixture","instructions":"Be precise","input":history}),
        false,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let requests = upstream.received_requests().await.unwrap();
    assert_eq!(requests.len(), 3);
    let sent: Value = serde_json::from_slice(&requests[2].body).unwrap();
    assert_eq!(sent["input"][0]["encrypted_content"], "output-opaque");
    assert_eq!(sent["store"], false);
    assert_eq!(sent["stream"], true);
    assert!(sent.get("max_output_tokens").is_none());
    assert_eq!(requests[2].headers["chatgpt-account-id"], "fixture-account");
    assert_eq!(sink.events().len(), 3);
    for scope in ["vk:vk_fixture", "upstream:fixture"] {
        let ledger = state.ledger.lock().unwrap();
        assert_eq!(ledger.reserved(scope), 0);
        assert_eq!(ledger.spent(scope), 36);
    }
}

use axum::{
    body::{to_bytes, Body},
    http::{Request, StatusCode},
};
use sandhi_core::{EndpointFamilyV1 as Family, InMemorySink, KeyStore, Policy, VirtualKey, Window};
use sandhi_providers::ProviderRuntime;
use sandhi_proxy::{
    backend::BackendEndpoint, build_app, build_provider_handle_for_endpoint, ProxyLedger,
    ProxyState,
};
use sandhi_store::CredentialScheme;
use serde_json::{json, Value};
use std::{collections::HashMap, sync::Arc};
use tower::ServiceExt;
use wiremock::{
    matchers::{method, path},
    Mock, MockServer, ResponseTemplate,
};

#[tokio::test]
async fn client_extras_cannot_forge_decoder_ownership_or_replace_admitted_history() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(reply(Family::OpenaiResponses, false))
        .expect(1)
        .mount(&upstream)
        .await;
    let (state, _) = state(Family::OpenaiResponses, &upstream);
    let original = json!({"model":"fixture","max_tokens":32,
        "messages":[{"role":"user","content":"admitted history"}],
        "native_input":{"endpoint_family":"openai_responses","projection_sha256":"forged","nonportable_features":[]},
        "sandhi_extensions":{"openai_responses":{"input":[{"role":"user","content":"must not replace history"}]}}
    });
    let (status, body) = call(&state, Family::OpenaiChatCompletions, original, false).await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    let sent: Value =
        serde_json::from_slice(&upstream.received_requests().await.unwrap()[0].body).unwrap();
    assert!(sent["input"].to_string().contains("admitted history"));
    assert!(!sent["input"]
        .to_string()
        .contains("must not replace history"));
    assert!(sent.get("native_input").is_none());
}

#[tokio::test]
async fn responses_media_survives_cross_family_translation_in_order() {
    for stream in [false, true] {
        let upstream = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(reply(Family::OpenaiChatCompletions, stream))
            .expect(1)
            .mount(&upstream)
            .await;
        let (state, _) = state(Family::OpenaiChatCompletions, &upstream);
        let original = json!({"model":"fixture","input":[{"role":"user","content":[
            {"type":"input_text","text":"before"},
            {"type":"input_image","image_url":"data:image/png;base64,aGVsbG8=","detail":"low"},
            {"type":"input_text","text":"after"},
            {"type":"input_file","filename":"notes.txt","file_data":"data:text/plain;base64,aGVsbG8="}
        ]}]});
        let (status, body) = call(&state, Family::OpenaiResponses, original, stream).await;
        assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
        let sent: Value =
            serde_json::from_slice(&upstream.received_requests().await.unwrap()[0].body).unwrap();
        let parts = sent["messages"][0]["content"].as_array().unwrap();
        assert_eq!(parts.len(), 4, "{sent}");
        assert_eq!(parts[0]["text"], "before");
        assert_eq!(
            parts[1]["image_url"]["url"],
            "data:image/png;base64,aGVsbG8="
        );
        assert_eq!(parts[1]["image_url"]["detail"], "low");
        assert_eq!(parts[2]["text"], "after");
        assert_eq!(parts[3]["file"]["filename"], "notes.txt");
        assert_eq!(
            parts[3]["file"]["file_data"],
            "data:text/plain;base64,aGVsbG8="
        );
    }
}

#[tokio::test]
async fn chat_file_ids_refuse_other_protocols_and_constrained_codex_before_admission() {
    for stream in [false, true] {
        for target in [
            Family::OpenaiResponses,
            Family::AnthropicMessages,
            Family::GeminiGenerateContent,
        ] {
            for position in ["user", "tool"] {
                let upstream = MockServer::start().await;
                Mock::given(method("POST"))
                    .respond_with(reply(target, stream))
                    .mount(&upstream)
                    .await;
                let (state, sink) = state(target, &upstream);
                let messages = if position == "user" {
                    json!([{"role":"user","content":[{"type":"file","file":{"file_id":"file_foreign"}}]}])
                } else {
                    json!([{"role":"assistant","tool_calls":[{"id":"call_1","type":"function","function":{"name":"lookup","arguments":"{}"}}]},
                        {"role":"tool","tool_call_id":"call_1","content":[{"type":"input_file","file_id":"file_foreign"}]}])
                };
                let body = json!({"model":"fixture","messages":messages});
                let (status, _) = call(&state, Family::OpenaiChatCompletions, body, stream).await;
                assert_eq!(
                    status,
                    StatusCode::BAD_REQUEST,
                    "{target:?}/{position}/{stream}"
                );
                assert!(upstream.received_requests().await.unwrap().is_empty());
                assert!(sink.events().is_empty());
                let ledger = state.ledger.lock().unwrap();
                assert_eq!(ledger.reserved("vk:vk_fixture"), 0);
                assert_eq!(ledger.spent("vk:vk_fixture"), 0);
            }
        }
        for source in [Family::OpenaiChatCompletions, Family::OpenaiResponses] {
            let upstream = MockServer::start().await;
            let (state, sink) = state(Family::OpenaiResponses, &upstream);
            state.providers.lock().unwrap().insert(
                "fixed".into(),
                ProviderRuntime::new().chatgpt_responses(
                    "fixture",
                    upstream.uri(),
                    "synthetic-access",
                    Default::default(),
                    Some(0),
                    None,
                    None,
                    None,
                ),
            );
            let body = if source == Family::OpenaiChatCompletions {
                json!({"model":"fixture","messages":[{"role":"developer","content":"Code"},{"role":"user","content":[{"type":"file","file":{"file_id":"file_foreign"}}]}]})
            } else {
                json!({"model":"fixture","instructions":"Code","input":[{"type":"function_call_output","call_id":"call_1","output":[{"type":"input_file","file_id":"file_foreign"}]}]})
            };
            let (status, _) = call(&state, source, body, stream).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "Codex/{source:?}/{stream}");
            assert!(upstream.received_requests().await.unwrap().is_empty());
            assert!(sink.events().is_empty());
            assert_eq!(state.ledger.lock().unwrap().reserved("vk:vk_fixture"), 0);
        }
    }
}

#[tokio::test]
async fn responses_nontext_tool_outputs_preserve_native_shape_or_refuse_translation() {
    for output in [
        json!([{"type":"input_image","image_url":"data:image/png;base64,aGVsbG8="}]),
        json!([{"type":"input_file","file_id":"file_foreign"}]),
        json!([{"type":"input_text","text":"first"},{"type":"input_text","text":"second"}]),
        json!([{"type":"future_media","payload":"opaque"}]),
        json!({"unprojected":{"value":42}}),
    ] {
        for stream in [false, true] {
            for target in [
                Family::OpenaiChatCompletions,
                Family::OpenaiResponses,
                Family::AnthropicMessages,
                Family::GeminiGenerateContent,
            ] {
                let upstream = MockServer::start().await;
                Mock::given(method("POST"))
                    .respond_with(reply(target, stream))
                    .mount(&upstream)
                    .await;
                let (state, sink) = state(target, &upstream);
                let original = json!({"model":"fixture","input":[
                    {"type":"function_call","call_id":"call_1","name":"lookup","arguments":"{}"},
                    {"type":"function_call_output","call_id":"call_1","output":output},
                    {"role":"user","content":"continue"}]});
                let (status, body) =
                    call(&state, Family::OpenaiResponses, original.clone(), stream).await;
                if target == Family::OpenaiResponses {
                    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
                    let calls = upstream.received_requests().await.unwrap();
                    assert_eq!(calls.len(), 1);
                    let sent: Value = calls[0].body_json().unwrap();
                    assert_eq!(sent["input"], original["input"]);
                } else {
                    assert_eq!(
                        status,
                        StatusCode::BAD_REQUEST,
                        "{target:?}/{stream}/{output}"
                    );
                    assert!(upstream.received_requests().await.unwrap().is_empty());
                    assert!(sink.events().is_empty());
                    let ledger = state.ledger.lock().unwrap();
                    assert_eq!(ledger.reserved("vk:vk_fixture"), 0);
                    assert_eq!(ledger.spent("vk:vk_fixture"), 0);
                }
            }
        }
    }
}

fn input(family: Family) -> Value {
    match family {
        Family::OpenaiChatCompletions => json!({
            "model":"fixture","messages":[
                {"role":"assistant","content":"answer","reasoning_content":"native reasoning"},
                {"role":"user","content":"continue"}]}),
        Family::OpenaiResponses => json!({
            "model":"fixture","instructions":"Be precise","input":[
                {"type":"reasoning","id":"rs_fixture","encrypted_content":"opaque"},
                {"type":"future_context","opaque":"preserve in order"},
                {"type":"function_call","call_id":"call_1","name":"lookup","arguments":"{}"},
                {"type":"function_call_output","call_id":"call_1","output":"found"},
                {"role":"user","content":"continue"}],"tools":[{"type":"web_search_preview"}]}),
        Family::AnthropicMessages => json!({
            "model":"fixture","system":[
                {"type":"text","text":"first","cache_control":{"type":"ephemeral","ttl":"1h"}},
                {"type":"text","text":"second","cache_control":{"type":"ephemeral"}}],
            "messages":[
                {"role":"assistant","content":[
                    {"type":"thinking","thinking":"native reasoning","signature":"signed"},
                    {"type":"tool_use","id":"call_1","name":"lookup","input":{}},
                    {"type":"text","text":"after tool"}]},
                {"role":"user","content":[
                    {"type":"tool_result","tool_use_id":"call_1","content":"found","cache_control":{"type":"ephemeral"}},
                    {"type":"text","text":"continue"}]}]}),
        Family::GeminiGenerateContent => json!({
            "systemInstruction":{"parts":[{"text":"Be precise"}]},
            "contents":[
                {"role":"model","parts":[
                    {"functionCall":{"id":"call_1","name":"lookup","args":{}},"thoughtSignature":"signed"}]},
                {"role":"user","parts":[{"text":"before"},
                    {"functionResponse":{"id":"call_1","name":"lookup","response":{"result":{"values":[1,2]}}}},
                    {"text":"continue"}]}]}),
        _ => unreachable!(),
    }
}

fn route(family: Family, stream: bool) -> String {
    match family {
        Family::OpenaiChatCompletions => "/v1/chat/completions".into(),
        Family::OpenaiResponses => "/v1/responses".into(),
        Family::AnthropicMessages => "/v1/messages".into(),
        Family::GeminiGenerateContent => format!(
            "/v1beta/models/fixture:{}",
            if stream {
                "streamGenerateContent?alt=sse"
            } else {
                "generateContent"
            }
        ),
        _ => unreachable!(),
    }
}

fn upstream_path(family: Family, stream: bool) -> &'static str {
    match family {
        Family::OpenaiChatCompletions => "/chat/completions",
        Family::OpenaiResponses => "/responses",
        Family::AnthropicMessages => "/v1/messages",
        Family::GeminiGenerateContent if stream => "/models/fixture:streamGenerateContent",
        Family::GeminiGenerateContent => "/models/fixture:generateContent",
        _ => unreachable!(),
    }
}

fn reply(family: Family, stream: bool) -> ResponseTemplate {
    let usage = json!({"input_tokens":10,"output_tokens":2});
    let completed = json!({"id":"resp_fixture","model":"fixture","status":"completed",
        "output":[{"type":"reasoning","encrypted_content":"output-opaque"},
            {"type":"message","role":"assistant","content":[{"type":"output_text","text":"ok"}]}],"usage":usage});
    let value = match family {
        Family::OpenaiChatCompletions => json!({"id":"chat_fixture","model":"fixture",
            "choices":[{"message":{"role":"assistant","content":"ok","reasoning_content":"native-out"},"finish_reason":"stop"}],
            "usage":{"prompt_tokens":10,"completion_tokens":2}}),
        Family::OpenaiResponses => completed.clone(),
        Family::AnthropicMessages => {
            json!({"id":"msg_fixture","type":"message","role":"assistant","model":"fixture",
            "content":[{"type":"thinking","thinking":"native-out","signature":"output-signed"},
                {"type":"text","text":"ok"}],"stop_reason":"end_turn","usage":usage})
        }
        Family::GeminiGenerateContent => {
            json!({"candidates":[{"content":{"role":"model","parts":[{"text":"ok","thoughtSignature":"output-signed"}]},"finishReason":"STOP"}],
            "usageMetadata":{"promptTokenCount":10,"candidatesTokenCount":2,"totalTokenCount":12}})
        }
        _ => unreachable!(),
    };
    if !stream {
        return ResponseTemplate::new(200).set_body_json(value);
    }
    let frames = match family {
        Family::OpenaiChatCompletions => vec![
            json!({"choices":[{"delta":{"content":"ok","provider_state":"output-opaque"},"finish_reason":"stop"}],
            "usage":{"prompt_tokens":10,"completion_tokens":2}}),
        ],
        Family::OpenaiResponses => vec![
            json!({"type":"response.output_text.delta","delta":"ok"}),
            json!({"type":"response.completed","response":completed}),
        ],
        Family::AnthropicMessages => vec![
            json!({"type":"message_start","message":{"id":"msg_fixture","model":"fixture","usage":{"input_tokens":10,"output_tokens":0}}}),
            json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"ok"}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"output-signed"}}),
            json!({"type":"content_block_stop","index":0}),
            json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":2}}),
            json!({"type":"message_stop"}),
        ],
        Family::GeminiGenerateContent => vec![value],
        _ => unreachable!(),
    };
    let mut sse = frames
        .iter()
        .map(|frame| format!("data: {frame}\n\n"))
        .collect::<String>();
    if family == Family::OpenaiChatCompletions {
        sse.push_str("data: [DONE]\n\n");
    }
    ResponseTemplate::new(200)
        .insert_header("content-type", "text/event-stream")
        .set_body_string(sse)
}

fn state(family: Family, upstream: &MockServer) -> (Arc<ProxyState>, Arc<InMemorySink>) {
    let provider = build_provider_handle_for_endpoint(
        &ProviderRuntime::new(),
        "fixture",
        Some(&upstream.uri()),
        "synthetic",
        CredentialScheme::ApiKey,
        None,
        BackendEndpoint::standard(family),
    )
    .unwrap();
    let keys = KeyStore::new();
    keys.insert(VirtualKey {
        id: "vk_fixture".into(),
        upstream_ref: "fixed".into(),
        ..Default::default()
    });
    let mut ledger = ProxyLedger::in_memory();
    ledger
        .set_budget("vk:vk_fixture", Some(100_000), Window::Total, Policy::Block)
        .unwrap();
    let sink = Arc::new(InMemorySink::new());
    (
        Arc::new(ProxyState::new(
            keys,
            ledger,
            sink.clone(),
            HashMap::from([("fixed".into(), provider)]),
            None,
        )),
        sink,
    )
}

async fn call(
    state: &Arc<ProxyState>,
    family: Family,
    mut body: Value,
    stream: bool,
) -> (StatusCode, Vec<u8>) {
    if family != Family::GeminiGenerateContent {
        body["stream"] = json!(stream);
    }
    let response = build_app(state.clone())
        .oneshot(
            Request::post(route(family, stream))
                .header("authorization", "Bearer vk_fixture")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    (
        status,
        to_bytes(response.into_body(), 100_000)
            .await
            .unwrap()
            .to_vec(),
    )
}

#[tokio::test]
async fn native_order_and_cache_boundaries_survive_output_bound_injection() {
    for family in [
        Family::OpenaiChatCompletions,
        Family::OpenaiResponses,
        Family::AnthropicMessages,
        Family::GeminiGenerateContent,
    ] {
        for stream in [false, true] {
            let upstream = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path(upstream_path(family, stream)))
                .respond_with(reply(family, stream))
                .expect(1)
                .mount(&upstream)
                .await;
            let (state, _) = state(family, &upstream);
            let original = input(family);
            let (status, body) = call(&state, family, original.clone(), stream).await;
            assert_eq!(
                status,
                StatusCode::OK,
                "{family:?}/{stream}: {}",
                String::from_utf8_lossy(&body)
            );
            if !stream {
                let returned: Value = serde_json::from_slice(&body).unwrap();
                let state = match family {
                    Family::OpenaiChatCompletions => {
                        &returned["choices"][0]["message"]["reasoning_content"]
                    }
                    Family::OpenaiResponses => &returned["output"][0]["encrypted_content"],
                    Family::AnthropicMessages => &returned["content"][0]["signature"],
                    Family::GeminiGenerateContent => {
                        &returned["candidates"][0]["content"]["parts"][0]["thoughtSignature"]
                    }
                    _ => unreachable!(),
                };
                assert!(state.as_str().is_some(), "{family:?}: {returned}");
            } else {
                let wire = String::from_utf8_lossy(&body);
                let state = if matches!(
                    family,
                    Family::OpenaiChatCompletions | Family::OpenaiResponses
                ) {
                    "output-opaque"
                } else {
                    "output-signed"
                };
                assert!(wire.contains(state), "{family:?}: {wire}");
                if family == Family::OpenaiChatCompletions {
                    assert_eq!(wire.matches("[DONE]").count(), 1, "{wire}");
                }
            }
            let sent: Value =
                serde_json::from_slice(&upstream.received_requests().await.unwrap()[0].body)
                    .unwrap();
            let fields: &[&str] = match family {
                Family::OpenaiChatCompletions => &["messages"],
                Family::OpenaiResponses => &["input", "instructions", "tools"],
                Family::AnthropicMessages => &["system", "messages"],
                Family::GeminiGenerateContent => &["systemInstruction", "contents"],
                _ => unreachable!(),
            };
            for field in fields {
                assert_eq!(
                    sent[*field], original[*field],
                    "{family:?}/{stream}/{field}"
                );
            }
            let bound = match family {
                Family::OpenaiResponses => sent.get("max_output_tokens"),
                Family::GeminiGenerateContent => sent.pointer("/generationConfig/maxOutputTokens"),
                _ => sent.get("max_tokens"),
            };
            assert!(
                bound.and_then(Value::as_u64).is_some_and(|bound| bound > 0),
                "{sent}"
            );
            assert!(
                sent.get("native_input").is_none(),
                "internal provenance must not reach the origin"
            );
        }
    }
}

#[tokio::test]
async fn nonportable_native_state_refuses_before_reservation_and_dispatch() {
    for source in [
        Family::OpenaiChatCompletions,
        Family::OpenaiResponses,
        Family::AnthropicMessages,
        Family::GeminiGenerateContent,
    ] {
        for stream in [false, true] {
            let upstream = MockServer::start().await;
            let target = if source == Family::OpenaiChatCompletions {
                Family::AnthropicMessages
            } else {
                Family::OpenaiChatCompletions
            };
            let (state, sink) = state(target, &upstream);
            let (status, _) = call(&state, source, input(source), stream).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{source:?}/{stream}");
            assert!(upstream.received_requests().await.unwrap().is_empty());
            assert!(sink.events().is_empty());
            let ledger = state.ledger.lock().unwrap();
            assert_eq!(ledger.reserved("vk:vk_fixture"), 0);
            assert_eq!(ledger.spent("vk:vk_fixture"), 0);
        }
    }
}

#[tokio::test]
async fn mixed_gemini_results_preserve_adjacent_text_and_arbitrary_response_objects() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(reply(Family::OpenaiChatCompletions, false))
        .expect(1)
        .mount(&upstream)
        .await;
    let (state, _) = state(Family::OpenaiChatCompletions, &upstream);
    let mut original = input(Family::GeminiGenerateContent);
    original["contents"][0]["parts"][0]
        .as_object_mut()
        .unwrap()
        .remove("thoughtSignature");
    let (status, body) = call(&state, Family::GeminiGenerateContent, original, false).await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    let sent: Value =
        serde_json::from_slice(&upstream.received_requests().await.unwrap()[0].body).unwrap();
    let history = sent["messages"].as_array().unwrap();
    assert_eq!(history.len(), 5, "{sent}");
    assert_eq!(history[2]["content"], "before");
    assert_eq!(history[3]["role"], "tool");
    let result: Value = serde_json::from_str(history[3]["content"].as_str().unwrap()).unwrap();
    assert_eq!(result, json!({"result":{"values":[1,2]}}));
    assert_eq!(history[4]["content"], "continue");
}
