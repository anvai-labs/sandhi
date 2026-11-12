//! Synthetic cache wire controls, both planes, neutral accounting and namespace
//! fences. These tests make no claim about live provider cache hits.
use axum::{
    body::{to_bytes, Body},
    http::{Request, StatusCode},
};
use sandhi_core::{
    BackendCachePolicyV1, CacheModeV1, ChatRequestV1, EndpointFamilyV1 as Family, InMemorySink,
    KeyStore, Policy, VirtualKey, Window,
};
use sandhi_providers::{BackendBinding, ProviderHandle, ProviderRuntime};
use sandhi_proxy::{
    backend::BackendEndpoint, build_app, build_provider_handle_for_endpoint, ProxyLedger,
    ProxyState,
};
use sandhi_store::CredentialScheme;
use serde_json::{json, Value};
use std::{collections::HashMap, sync::Arc};
use tower::ServiceExt;
use wiremock::{matchers::method, Mock, MockServer, ResponseTemplate};

const FAMILIES: [Family; 4] = [
    Family::OpenaiChatCompletions,
    Family::AnthropicMessages,
    Family::OpenaiResponses,
    Family::GeminiGenerateContent,
];

#[tokio::test]
async fn unreported_secret_change_changes_scoped_keys_and_refuses_old_plan() {
    let upstream = MockServer::start().await;
    let original = provider(
        Family::OpenaiChatCompletions,
        &upstream,
        Some(policy(Family::OpenaiChatCompletions, "auto")),
    );
    let replacement = build_provider_handle_for_endpoint(
        &ProviderRuntime::new(),
        "fixture",
        Some(&upstream.uri()),
        "replacement-private-credential",
        CredentialScheme::Bearer,
        None,
        BackendEndpoint::standard(Family::OpenaiChatCompletions),
    )
    .unwrap()
    .with_backend_binding(original.backend_binding().unwrap().clone());
    let mut request: sandhi_core::ChatRequestV1 = serde_json::from_value(json!({
        "model":"fixture","messages":[{"role":"user","content":"hello"}],
        "metadata":{"virtual_key_id":"vk_fixture","session_id":"same-session"}
    }))
    .unwrap();
    let before = original.compile_cache_plan(&request).unwrap().unwrap();
    let after = replacement.compile_cache_plan(&request).unwrap().unwrap();
    assert_ne!(before.scoped_key, after.scoped_key);
    assert_ne!(before.binding_sha256, after.binding_sha256);
    request.compiled_cache = Some(before);
    assert!(replacement.compile_cache_plan(&request).is_err());
    assert!(upstream.received_requests().await.unwrap().is_empty());
}

fn policy(family: Family, mode: &str) -> BackendCachePolicyV1 {
    let (profile, lifetimes, limit, namespace) = match family {
        Family::AnthropicMessages => (
            "anthropic_breakpoints",
            json!(["5m", "1h"]),
            json!(4),
            "provider_default",
        ),
        Family::OpenaiResponses => (
            "openai_explicit",
            json!(["30m", "12h"]),
            json!(4),
            "scoped_key",
        ),
        Family::OpenaiChatCompletions => (
            "openai_legacy",
            json!(["in_memory", "24h"]),
            Value::Null,
            "scoped_key",
        ),
        Family::GeminiGenerateContent => {
            ("implicit_only", json!([]), Value::Null, "provider_default")
        }
        _ => unreachable!(),
    };
    serde_json::from_value(json!({"default_intent":{"mode":mode},"namespace":namespace,
        "models":{"fixture":{"profile":profile,"lifetimes":lifetimes,"max_breakpoints":limit}}}))
    .unwrap()
}

fn provider(
    family: Family,
    upstream: &MockServer,
    policy: Option<BackendCachePolicyV1>,
) -> ProviderHandle {
    build_provider_handle_for_endpoint(
        &ProviderRuntime::new(),
        "fixture",
        Some(&upstream.uri()),
        "fixture-secret",
        CredentialScheme::ApiKey,
        None,
        BackendEndpoint::standard(family),
    )
    .unwrap()
    .with_backend_binding(BackendBinding {
        id: "primary".into(),
        revision: 1,
        provider: "fixture".into(),
        credential_ref: "fixture:default".into(),
        credential_generation: 1,
        endpoint_family: family,
        endpoint_profile: "standard".into(),
        models: vec![serde_json::from_value(json!({"id":"fixture"})).unwrap()],
        cache_policy: policy,
    })
}

fn state(provider: ProviderHandle) -> (Arc<ProxyState>, Arc<InMemorySink>, String) {
    let keys = KeyStore::new();
    for id in ["vk_fixture", "vk_second"] {
        keys.insert(VirtualKey {
            id: id.into(),
            subject_id: Some(id.into()),
            upstream_ref: "fixed".into(),
            ..Default::default()
        });
    }
    // Diagnostic identity is not enforcement authority.
    let upstream_scope = provider.upstream_budget_scope().to_owned();
    let mut ledger = ProxyLedger::in_memory();
    for scope in ["vk:vk_fixture", "vk:vk_second", &upstream_scope] {
        ledger
            .set_budget(scope, Some(1_000_000), Window::Total, Policy::Block)
            .unwrap();
    }
    let sink = Arc::new(InMemorySink::new());
    let mut state = ProxyState::new(
        keys,
        ledger,
        sink.clone(),
        HashMap::from([("fixed".into(), provider)]),
        None,
    );
    state.cache_read_weight_bp = 1000;
    (Arc::new(state), sink, upstream_scope)
}

fn input(family: Family) -> Value {
    match family {
        Family::OpenaiChatCompletions => {
            json!({"model":"fixture","max_tokens":32,"messages":[{"role":"system","content":"stable policy"},{"role":"user","content":"question"}]})
        }
        Family::AnthropicMessages => {
            json!({"model":"fixture","max_tokens":32,"system":"stable policy","messages":[{"role":"user","content":"question"}]})
        }
        Family::OpenaiResponses => {
            json!({"model":"fixture","max_output_tokens":32,"input":[{"role":"developer","content":"stable policy"},{"role":"user","content":"question"}]})
        }
        Family::GeminiGenerateContent => {
            json!({"systemInstruction":{"parts":[{"text":"stable policy"}]},"contents":[{"role":"user","parts":[{"text":"question"}]}],"generationConfig":{"maxOutputTokens":32}})
        }
        _ => unreachable!(),
    }
}
async fn call(
    state: &Arc<ProxyState>,
    family: Family,
    mut body: Value,
    stream: bool,
    key: &str,
) -> (StatusCode, Vec<u8>) {
    let route = match family {
        Family::OpenaiChatCompletions => "/v1/chat/completions".to_owned(),
        Family::AnthropicMessages => "/v1/messages".to_owned(),
        Family::OpenaiResponses => "/v1/responses".to_owned(),
        Family::GeminiGenerateContent => format!(
            "/v1beta/models/fixture:{}",
            if stream {
                "streamGenerateContent?alt=sse"
            } else {
                "generateContent"
            }
        ),
        _ => unreachable!(),
    };
    if family != Family::GeminiGenerateContent {
        body["stream"] = json!(stream);
    }
    let response = build_app(state.clone())
        .oneshot(
            Request::post(route)
                .header("authorization", format!("Bearer {key}"))
                .header("content-type", "application/json")
                .header("x-sandhi-session", "client-session-hint")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    (
        response.status(),
        to_bytes(response.into_body(), 100_000)
            .await
            .unwrap()
            .to_vec(),
    )
}

fn reply(family: Family, stream: bool) -> ResponseTemplate {
    let usage = match family {
        Family::OpenaiChatCompletions => {
            json!({"prompt_tokens":100,"completion_tokens":20,"prompt_tokens_details":{"cached_tokens":60,"cache_write_tokens":30}})
        }
        Family::OpenaiResponses => {
            json!({"input_tokens":100,"output_tokens":20,"input_tokens_details":{"cached_tokens":60,"cache_write_tokens":30}})
        }
        Family::AnthropicMessages => {
            json!({"input_tokens":10,"output_tokens":20,"cache_read_input_tokens":60,"cache_creation_input_tokens":30})
        }
        Family::GeminiGenerateContent => {
            json!({"promptTokenCount":100,"candidatesTokenCount":20,"cachedContentTokenCount":60,"totalTokenCount":120})
        }
        _ => unreachable!(),
    };
    let completed = match family {
        Family::OpenaiChatCompletions => {
            json!({"model":"fixture","choices":[{"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}],"usage":usage})
        }
        Family::OpenaiResponses => {
            json!({"id":"resp_fixture","model":"fixture","status":"completed","output":[{"type":"message","id":"msg_fixture","role":"assistant","content":[{"type":"output_text","text":"ok"}]}],"usage":usage})
        }
        Family::AnthropicMessages => {
            json!({"id":"msg_fixture","type":"message","role":"assistant","model":"fixture","content":[{"type":"text","text":"ok"}],"stop_reason":"end_turn","usage":usage})
        }
        Family::GeminiGenerateContent => {
            json!({"candidates":[{"content":{"role":"model","parts":[{"text":"ok"}]},"finishReason":"STOP"}],"usageMetadata":usage})
        }
        _ => unreachable!(),
    };
    if !stream {
        return ResponseTemplate::new(200).set_body_json(completed);
    }
    let frames = match family {
        Family::OpenaiChatCompletions => vec![
            json!({"object":"chat.completion.chunk","id":"chat_fixture","model":"fixture","choices":[{"index":0,"delta":{"content":"ok"},"finish_reason":"stop"}]}),
            json!({"object":"chat.completion.chunk","choices":[],"usage":usage}),
        ],
        Family::OpenaiResponses => vec![
            json!({"type":"response.created","response":{"id":"resp_fixture","model":"fixture","status":"in_progress","output":[]}}),
            json!({"type":"response.completed","response":completed}),
        ],
        Family::AnthropicMessages => vec![
            json!({"type":"message_start","message":{"type":"message","role":"assistant","content":[],"id":"msg_fixture","model":"fixture","usage":usage}}),
            json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"ok"}}),
            json!({"type":"content_block_stop","index":0}),
            json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":20}}),
            json!({"type":"message_stop"}),
        ],
        Family::GeminiGenerateContent => vec![completed],
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

#[tokio::test]
async fn all_ingress_backend_pairs_compile_auto_on_both_delivery_modes_and_settle_once() {
    for source in FAMILIES {
        for target in FAMILIES {
            for stream in [false, true] {
                let upstream = MockServer::start().await;
                Mock::given(method("POST"))
                    .respond_with(reply(target, stream))
                    .expect(1)
                    .mount(&upstream)
                    .await;
                let (state, sink, upstream_scope) =
                    state(provider(target, &upstream, Some(policy(target, "auto"))));
                let mut body = input(source);
                body["sandhi_cache"] = json!({"mode":"auto"});
                let (status, response) = call(&state, source, body, stream, "vk_fixture").await;
                assert_eq!(
                    status,
                    StatusCode::OK,
                    "{source:?}/{target:?}/{stream}: {}",
                    String::from_utf8_lossy(&response)
                );
                let requests = upstream.received_requests().await.unwrap();
                let sent: Value = serde_json::from_slice(&requests[0].body).unwrap();
                for key in ["sandhi_cache", "cache_intent", "compiled_cache"] {
                    assert!(sent.get(key).is_none(), "{sent}");
                }
                match target {
                    Family::AnthropicMessages => assert_eq!(
                        sent.pointer("/messages/0/content/0/cache_control/type"),
                        Some(&json!("ephemeral"))
                    ),
                    Family::OpenaiResponses => {
                        assert_eq!(sent["prompt_cache_options"]["mode"], "implicit")
                    }
                    Family::OpenaiChatCompletions => assert!(sent["prompt_cache_key"]
                        .as_str()
                        .unwrap()
                        .starts_with("sandhi:")),
                    Family::GeminiGenerateContent => assert!(sent.get("cachedContent").is_none()),
                    _ => unreachable!(),
                }
                if matches!(
                    target,
                    Family::OpenaiChatCompletions | Family::OpenaiResponses
                ) {
                    let key = sent["prompt_cache_key"].as_str().unwrap();
                    assert!(
                        !key.contains("vk_fixture")
                            && !key.contains("client-session-hint")
                            && !key.contains("default")
                    );
                }
                let events = sink.events();
                assert_eq!(events.len(), 1);
                assert_eq!(events[0].cache_read_tokens, 60);
                assert_eq!(
                    events[0].tokens_in,
                    if target == Family::GeminiGenerateContent {
                        40
                    } else {
                        10
                    }
                );
                assert_eq!(
                    events[0].cache_creation_tokens,
                    if target == Family::GeminiGenerateContent {
                        0
                    } else {
                        30
                    }
                );
                assert_eq!(events[0].tokens_out, 20);
                for scope in ["vk:vk_fixture", &upstream_scope] {
                    let ledger = state.ledger.lock().unwrap();
                    assert_eq!(
                        ledger.spent(scope),
                        66,
                        "{source:?}/{target:?}/{stream}/{scope}"
                    );
                    assert_eq!(ledger.reserved(scope), 0);
                }
            }
        }
    }
}

#[tokio::test]
async fn anthropic_terminal_input_cache_updates_reach_all_ingress_and_both_budget_legs() {
    for source in FAMILIES {
        for late in [false, true] {
            let upstream = MockServer::start().await;
            let input_usage = json!({"input_tokens":10,"cache_creation_input_tokens":30,"cache_read_input_tokens":60});
            let frames = [
                json!({"type":"message_start","message":{"type":"message","role":"assistant","content":[],"id":"msg_fixture","model":"fixture",
                    "usage":if late { json!({"input_tokens":0,"output_tokens":0}) } else { input_usage.clone() }}}),
                json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
                json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"ok"}}),
                json!({"type":"content_block_stop","index":0}),
                json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},
                    "usage":if late { json!({"input_tokens":10,"cache_creation_input_tokens":30,"cache_read_input_tokens":60,"output_tokens":20}) }
                            else { json!({"output_tokens":20}) }}),
                json!({"type":"message_stop"}),
            ];
            let sse = frames
                .iter()
                .map(|frame| format!("data: {frame}\n\n"))
                .collect::<String>();
            Mock::given(method("POST"))
                .respond_with(
                    ResponseTemplate::new(200)
                        .insert_header("content-type", "text/event-stream")
                        .set_body_string(sse),
                )
                .expect(1)
                .mount(&upstream)
                .await;
            let (state, sink, upstream_scope) = state(provider(
                Family::AnthropicMessages,
                &upstream,
                Some(policy(Family::AnthropicMessages, "auto")),
            ));
            let (status, _) = call(&state, source, input(source), true, "vk_fixture").await;
            assert_eq!(status, StatusCode::OK);
            let events = sink.events();
            assert_eq!(events.len(), 1);
            assert_eq!(
                (
                    events[0].tokens_in,
                    events[0].cache_read_tokens,
                    events[0].cache_creation_tokens,
                    events[0].tokens_out
                ),
                (10, 60, 30, 20),
                "{source:?}, late={late}"
            );
            for scope in ["vk:vk_fixture", &upstream_scope] {
                let ledger = state.ledger.lock().unwrap();
                assert_eq!(ledger.spent(scope), 66, "{source:?}/{late}/{scope}");
                assert_eq!(ledger.reserved(scope), 0);
            }
        }
    }
}

#[tokio::test]
async fn explicit_stable_policy_ignores_unrelated_extras_and_keeps_legacy_opt_out() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(reply(Family::AnthropicMessages, false))
        .mount(&upstream)
        .await;
    for (mode, extra, expected) in [
        (None, json!({}), false),
        (Some("auto_stable_prefix"), json!({"top_k":5}), true),
        (Some("auto_stable_prefix"), json!({}), true),
        (Some("suppress_gateway"), json!({"top_k":5}), false),
        (Some("preserve_native"), json!({}), false),
    ] {
        let selected = provider(
            Family::AnthropicMessages,
            &upstream,
            mode.map(|mode| policy(Family::AnthropicMessages, mode)),
        );
        let (state, _, _) = state(selected);
        let mut body = input(Family::OpenaiChatCompletions);
        body["sandhi_extensions"] = json!({"anthropic":extra});
        let (status, response) = call(
            &state,
            Family::OpenaiChatCompletions,
            body,
            false,
            "vk_fixture",
        )
        .await;
        assert_eq!(
            status,
            StatusCode::OK,
            "{}",
            String::from_utf8_lossy(&response)
        );
        let requests = upstream.received_requests().await.unwrap();
        let sent: Value = serde_json::from_slice(&requests.last().unwrap().body).unwrap();
        assert_eq!(
            sent.pointer("/system/0/cache_control/type").is_some(),
            expected,
            "{mode:?}: {sent}"
        );
    }
}

#[tokio::test]
async fn unsupported_or_malformed_cache_intent_never_reserves_or_dispatches() {
    for family in FAMILIES {
        for stream in [false, true] {
            for directive in [
                json!({"mode":"invented"}),
                json!({"mode":"auto_stable_prefix"}),
                json!({"mode":"suppress_gateway","lifetime":"1h"}),
                json!({"mode":"auto","backend":"foreign"}),
            ] {
                let upstream = MockServer::start().await;
                let (state, sink, upstream_scope) =
                    state(provider(Family::OpenaiChatCompletions, &upstream, None));
                let mut body = input(family);
                body["sandhi_cache"] = directive;
                assert_eq!(
                    call(&state, family, body, stream, "vk_fixture").await.0,
                    StatusCode::BAD_REQUEST
                );
                assert!(upstream.received_requests().await.unwrap().is_empty());
                assert!(sink.events().is_empty());
                for scope in ["vk:vk_fixture", &upstream_scope] {
                    let ledger = state.ledger.lock().unwrap();
                    assert_eq!(ledger.spent(scope), 0);
                    assert_eq!(ledger.reserved(scope), 0);
                }
            }
        }
    }
}

#[tokio::test]
async fn scoped_keys_are_stable_but_change_for_tenant_generation_and_backend_revision() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(reply(Family::OpenaiChatCompletions, false))
        .mount(&upstream)
        .await;
    let original = provider(
        Family::OpenaiChatCompletions,
        &upstream,
        Some(policy(Family::OpenaiChatCompletions, "auto")),
    );
    let (state, _, _) = state(original.clone());
    let mut body = input(Family::OpenaiChatCompletions);
    body["prompt_cache_key"] = json!("shared-client-key");
    let mut observed = Vec::new();
    for (key, generation, revision) in [
        ("vk_fixture", 1, 1),
        ("vk_fixture", 1, 1),
        ("vk_second", 1, 1),
        ("vk_fixture", 2, 1),
        ("vk_fixture", 2, 2),
    ] {
        let mut binding = original.backend_binding().unwrap().clone();
        binding.credential_generation = generation;
        binding.revision = revision;
        state.providers.lock().unwrap().insert(
            "fixed".into(),
            original.clone().with_backend_binding(binding),
        );
        assert_eq!(
            call(
                &state,
                Family::OpenaiChatCompletions,
                body.clone(),
                false,
                key
            )
            .await
            .0,
            StatusCode::OK
        );
        let requests = upstream.received_requests().await.unwrap();
        let sent: Value = serde_json::from_slice(&requests.last().unwrap().body).unwrap();
        observed.push(sent["prompt_cache_key"].as_str().unwrap().to_owned());
    }
    assert_eq!(observed[0], observed[1]);
    for i in [2, 3, 4] {
        assert_ne!(observed[i], observed[0]);
    }
    assert_ne!(observed[3], observed[4]);
    assert!(observed.iter().all(|key| key != "shared-client-key"));
}

#[tokio::test]
async fn compiled_plan_cannot_be_imported_from_json_or_retargeted_after_admission() {
    let upstream = MockServer::start().await;
    let original = provider(
        Family::OpenaiChatCompletions,
        &upstream,
        Some(policy(Family::OpenaiChatCompletions, "auto")),
    );
    let mut request:ChatRequestV1=serde_json::from_value(json!({"model":"fixture","messages":[{"role":"user","content":"question"}],
        "metadata":{"virtual_key_id":"vk_fixture"},"compiled_cache":{"mutations":[{"model":"foreign"}]}})).unwrap();
    assert!(request.compiled_cache.is_none());
    request.compiled_cache = original.compile_cache_plan(&request).unwrap();
    let mut binding = original.backend_binding().unwrap().clone();
    binding.credential_generation += 1;
    let replacement = original.clone().with_backend_binding(binding);
    let mut body = json!({"model":"fixture","messages":[{"role":"user","content":"question"}]});
    assert!(replacement.apply_cache_plan(&request, &mut body).is_err());
    request.max_output_tokens = Some(7);
    original.apply_cache_plan(&request, &mut body).unwrap();
    request.metadata.virtual_key_id = Some("vk_foreign".into());
    assert!(original.apply_cache_plan(&request, &mut body).is_err());
    request.cache_intent = Some(sandhi_core::CacheIntentV1 {
        mode: CacheModeV1::SuppressGateway,
        lifetime: None,
    });
    assert!(original.compile_cache_plan(&request).is_err());
}

#[tokio::test]
async fn codex_transport_refuses_cache_synthesis_even_with_forged_host_capabilities() {
    let upstream = MockServer::start().await;
    let declared = provider(
        Family::OpenaiResponses,
        &upstream,
        Some(policy(Family::OpenaiResponses, "auto")),
    );
    let codex = ProviderRuntime::new()
        .chatgpt_responses(
            "fixture",
            upstream.uri(),
            "fixture-access",
            Default::default(),
            Some(0),
            None,
            None,
            None,
        )
        .with_backend_binding(declared.backend_binding().unwrap().clone());
    let request:ChatRequestV1=serde_json::from_value(json!({"model":"fixture","messages":[{"role":"system","content":"policy"},{"role":"user","content":"hello"}],"metadata":{"virtual_key_id":"fixture"}})).unwrap();
    assert!(
        matches!(codex.compile_cache_plan(&request),Err(sandhi_providers::ProviderError::InvalidRequest(message)) if message.contains("transport profile"))
    );
    assert!(upstream.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn declared_native_only_policy_without_default_cannot_inherit_legacy_synthesis() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(reply(Family::AnthropicMessages, false))
        .expect(2)
        .mount(&upstream)
        .await;
    let mut selected = policy(Family::AnthropicMessages, "auto");
    selected.default_intent = None;
    selected.allow_request_override = false;
    selected.models.get_mut("fixture").unwrap().profile =
        sandhi_core::CacheCapabilityProfileV1::NativeOnly;
    selected
        .models
        .get_mut("fixture")
        .unwrap()
        .lifetimes
        .clear();
    selected.models.get_mut("fixture").unwrap().max_breakpoints = None;
    let (state, _, _) = state(provider(
        Family::AnthropicMessages,
        &upstream,
        Some(selected),
    ));
    for redundant in [false, true] {
        let mut body = input(Family::OpenaiChatCompletions);
        if redundant {
            body["sandhi_cache"] = json!({"mode":"preserve_native"});
        }
        let (status, response) = call(
            &state,
            Family::OpenaiChatCompletions,
            body,
            false,
            "vk_fixture",
        )
        .await;
        assert_eq!(
            status,
            StatusCode::OK,
            "{}",
            String::from_utf8_lossy(&response)
        );
        let requests = upstream.received_requests().await.unwrap();
        let sent: Value = serde_json::from_slice(&requests.last().unwrap().body).unwrap();
        assert!(
            sent.pointer("/system/0/cache_control").is_none(),
            "native-only policy inherited legacy synthesis: {sent}"
        );
    }
}

#[tokio::test]
async fn stable_lifetime_placement_survives_raw_and_forced_typed_execution() {
    for family in [Family::AnthropicMessages, Family::OpenaiResponses] {
        for stream in [false, true] {
            for force_typed in [false, true] {
                let upstream = MockServer::start().await;
                Mock::given(method("POST"))
                    .respond_with(reply(family, stream))
                    .expect(1)
                    .mount(&upstream)
                    .await;
                let mut selected = policy(family, "auto_stable_prefix");
                selected.default_intent.as_mut().unwrap().lifetime =
                    Some(if family == Family::AnthropicMessages {
                        sandhi_core::CacheLifetimeV1::OneHour
                    } else {
                        sandhi_core::CacheLifetimeV1::TwelveHours
                    });
                let (state, _, _) = state(provider(family, &upstream, Some(selected)));
                let mut body = input(family);
                if force_typed {
                    body.as_object_mut()
                        .unwrap()
                        .remove(if family == Family::AnthropicMessages {
                            "max_tokens"
                        } else {
                            "max_output_tokens"
                        });
                }
                let (status, response) = call(&state, family, body, stream, "vk_fixture").await;
                assert_eq!(
                    status,
                    StatusCode::OK,
                    "{family:?}/{stream}/{force_typed}: {}",
                    String::from_utf8_lossy(&response)
                );
                let sent: Value =
                    serde_json::from_slice(&upstream.received_requests().await.unwrap()[0].body)
                        .unwrap();
                if family == Family::AnthropicMessages {
                    assert_eq!(sent["system"][0]["text"], "stable policy");
                    assert_eq!(
                        sent["system"][0]["cache_control"],
                        json!({"type":"ephemeral","ttl":"1h"})
                    );
                    assert!(sent
                        .pointer("/messages/0/content/0/cache_control")
                        .is_none());
                } else {
                    assert_eq!(sent["input"][0]["content"][0]["text"], "stable policy");
                    assert_eq!(
                        sent["input"][0]["content"][0]["prompt_cache_breakpoint"],
                        json!({"mode":"explicit"})
                    );
                    assert_eq!(
                        sent["prompt_cache_options"],
                        json!({"mode":"explicit","ttl":"12h"})
                    );
                    assert!(sent
                        .pointer("/input/1/content/0/prompt_cache_breakpoint")
                        .is_none());
                }
            }
        }
    }
}

#[tokio::test]
async fn native_markers_win_and_conflicting_or_over_limit_lifetimes_refuse_before_admission() {
    for stream in [false, true] {
        let upstream = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(reply(Family::AnthropicMessages, stream))
            .expect(1)
            .mount(&upstream)
            .await;
        let (state, sink, upstream_scope) = state(provider(
            Family::AnthropicMessages,
            &upstream,
            Some(policy(
                Family::AnthropicMessages,
                "auto_conversation_prefix",
            )),
        ));
        let mut original = input(Family::AnthropicMessages);
        original["system"] = json!([{"type":"text","text":"stable policy","cache_control":{"type":"ephemeral","ttl":"1h"}}]);
        assert_eq!(
            call(
                &state,
                Family::AnthropicMessages,
                original.clone(),
                stream,
                "vk_fixture"
            )
            .await
            .0,
            StatusCode::OK
        );
        let sent: Value =
            serde_json::from_slice(&upstream.received_requests().await.unwrap()[0].body).unwrap();
        assert_eq!(sent["system"], original["system"]);
        assert_eq!(sent["messages"], original["messages"]);
        original["sandhi_cache"] = json!({"mode":"auto","lifetime":"5m"});
        assert_eq!(
            call(
                &state,
                Family::AnthropicMessages,
                original.clone(),
                stream,
                "vk_fixture"
            )
            .await
            .0,
            StatusCode::BAD_REQUEST
        );
        original.as_object_mut().unwrap().remove("sandhi_cache");
        original["system"] = json!([{"type":"text","text":"first","cache_control":{"type":"ephemeral","ttl":"5m"}},
            {"type":"text","text":"second","cache_control":{"type":"ephemeral","ttl":"1h"}}]);
        assert_eq!(
            call(
                &state,
                Family::AnthropicMessages,
                original,
                stream,
                "vk_fixture"
            )
            .await
            .0,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(sink.events().len(), 1);
        for scope in ["vk:vk_fixture", &upstream_scope] {
            let ledger = state.ledger.lock().unwrap();
            assert_eq!(ledger.spent(scope), 66);
            assert_eq!(ledger.reserved(scope), 0);
        }
    }
    for mode in ["implicit", "explicit"] {
        let upstream = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(reply(Family::OpenaiResponses, false))
            .mount(&upstream)
            .await;
        let (state, _, _) = state(provider(
            Family::OpenaiResponses,
            &upstream,
            Some(policy(Family::OpenaiResponses, "auto")),
        ));
        let mut body = input(Family::OpenaiResponses);
        body["prompt_cache_options"] = json!({"mode":mode});
        body["input"][0]["content"]=json!((0..4).map(|i|json!({"type":"input_text","text":format!("prefix-{i}"),"prompt_cache_breakpoint":{"mode":"explicit"}})).collect::<Vec<_>>());
        assert_eq!(
            call(&state, Family::OpenaiResponses, body, false, "vk_fixture")
                .await
                .0,
            if mode == "implicit" {
                StatusCode::BAD_REQUEST
            } else {
                StatusCode::OK
            }
        );
    }
}

#[tokio::test]
async fn native_cache_controls_cannot_silently_cross_families() {
    for source in FAMILIES {
        for stream in [false, true] {
            let upstream = MockServer::start().await;
            let target = if source == Family::AnthropicMessages {
                Family::OpenaiChatCompletions
            } else {
                Family::AnthropicMessages
            };
            let (state, sink, _) = state(provider(target, &upstream, Some(policy(target, "auto"))));
            let mut body = input(source);
            match source {
                Family::OpenaiChatCompletions => {
                    body["prompt_cache_key"] = json!("native-namespace")
                }
                Family::OpenaiResponses => {
                    body["prompt_cache_options"] = json!({"mode":"explicit","ttl":"12h"})
                }
                Family::AnthropicMessages => body["cache_control"] = json!({"type":"ephemeral"}),
                Family::GeminiGenerateContent => {
                    body["cachedContent"] = json!("cachedContents/native-resource")
                }
                _ => unreachable!(),
            }
            assert_eq!(
                call(&state, source, body, stream, "vk_fixture").await.0,
                StatusCode::BAD_REQUEST
            );
            assert!(upstream.received_requests().await.unwrap().is_empty());
            assert!(sink.events().is_empty());
            assert_eq!(state.ledger.lock().unwrap().reserved("vk:vk_fixture"), 0);
        }
    }
}
