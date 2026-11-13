//! Logical aliases must dispatch only to operator-owned target/model pairs.
mod support;
use axum::{
    body::{to_bytes, Body},
    http::{Request, StatusCode},
};
use sandhi_core::{EndpointFamilyV1 as Family, InMemorySink, KeyStore, Policy, VirtualKey, Window};
use sandhi_providers::{BackendBinding, ProviderHandle, ProviderRuntime};
use sandhi_proxy::{
    backend::BackendEndpoint, build_app, build_provider_handle_for_endpoint, ProxyLedger,
    ProxyState,
};
use sandhi_store::{
    routes::{RouteDefinition, RouteStore},
    CredentialScheme,
};
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
async fn tracked_logical_dispatch_persists_selection_and_cache_policy_with_its_intent() {
    for (source, stream) in [
        (Family::OpenaiChatCompletions, false),
        (Family::AnthropicMessages, false),
        (Family::OpenaiChatCompletions, true),
    ] {
        let origin = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(reply(Family::OpenaiChatCompletions, stream))
            .expect(1)
            .mount(&origin)
            .await;
        let mut handle = provider(&origin, "primary", Family::OpenaiChatCompletions, 1, 1);
        let mut binding = handle.backend_binding().unwrap().clone();
        binding.cache_policy=Some(serde_json::from_value(json!({"models":{"physical":{"profile":"implicit_only"}},"default_intent":{"mode":"auto"}})).unwrap());
        handle = handle.with_backend_binding(binding);
        let (mut state, sink) = state(vec![("primary", handle)], definition(&["primary"]));
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.db");
        let mut ledger = ProxyLedger::durable(path.to_str().unwrap(), 1).unwrap();
        ledger
            .set_budget("vk:vk_one", Some(100_000), Window::Total, Policy::Block)
            .unwrap();
        let unique = Arc::get_mut(&mut state).unwrap();
        unique.ledger = std::sync::Mutex::new(ledger);
        unique.cache_read_weight_bp = 1000;
        if stream {
            unique.streaming_deadlines=Some(serde_json::from_value(json!({"ceiling_ms":10000,"default":{"setup_ms":1000,"idle_ms":1000,"body_ms":2000}})).unwrap());
        }
        sandhi_proxy::settlement::buffered::enable(
            &state,
            sandhi_proxy::settlement::buffered::Config::new(
                4,
                std::time::Duration::from_secs(2),
                std::time::Duration::from_secs(5),
            )
            .unwrap(),
        )
        .unwrap();
        let (route, payload) = body(source, stream);
        assert_eq!(
            call(&state, &route, payload, "vk_one", "tracked").await.0,
            StatusCode::OK
        );
        let mut ledger = sandhi_store::SqliteLedger::open(path.to_str().unwrap()).unwrap();
        let entries = ledger
            .recovery_page_durable("vk:vk_one", None, 10)
            .unwrap()
            .entries;
        assert_eq!(entries.len(), 1);
        let context = ledger
            .execution_context_durable("vk:vk_one", &entries[0].execution_id)
            .unwrap()
            .expect("dispatch selection must be durable");
        assert_eq!(context.upstream_ref, "route/coding");
        assert_eq!(context.route.unwrap().revision, 1);
        assert_eq!(context.requested_model, "agent");
        assert_eq!(context.actual_model, "physical");
        assert_eq!(context.backend.unwrap().id, "primary");
        assert_eq!(
            context.cache.unwrap().intent.mode,
            sandhi_core::CacheModeV1::Auto
        );
        assert_eq!(
            context.accounting_policy.cache_read_weight.basis_points(),
            1000
        );
        assert_eq!(context.ingress, source);
        assert_eq!(
            context.delivery,
            if stream {
                sandhi_store::ledger::evidence::context::Delivery::Streaming
            } else {
                sandhi_store::ledger::evidence::context::Delivery::Buffered
            }
        );
        assert_eq!(sink.events().len(), 1);
        assert!(
            matches!(&entries[0].state,sandhi_store::ledger::evidence::RecoveryState::Settled(receipt) if receipt.charged_tokens==66)
        );
    }
}

#[tokio::test]
async fn ambiguous_json_is_refused_before_fixed_or_logical_dispatch() {
    let origin = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(reply(Family::OpenaiChatCompletions, false))
        .mount(&origin)
        .await;
    let (state, sink) = state(
        vec![(
            "primary",
            provider(&origin, "primary", Family::OpenaiChatCompletions, 1, 1),
        )],
        definition(&["primary"]),
    );
    state.keys.insert(VirtualKey {
        id: "vk_fixed".into(),
        upstream_ref: "backend/primary".into(),
        models: Some(vec!["physical".into()]),
        ..Default::default()
    });
    for key in ["vk_one", "vk_fixed"] {
        for (path, payload) in [
            (
                "/v1/chat/completions",
                r#"{"model":"unauthorized","mo\u0064el":"physical","model":"agent","messages":[{"role":"user","content":"hello"}]}"#,
            ),
            (
                "/v1/chat/completions",
                r#"{"model":"physical","messages":[{"role":"user","content":"first","content":"second"}]}"#,
            ),
            (
                "/v1/responses",
                r#"{"model":"unauthorized","model":"physical","input":"hello"}"#,
            ),
            (
                "/v1/messages",
                r#"{"model":"unauthorized","model":"physical","max_tokens":64,"messages":[{"role":"user","content":"hello"}]}"#,
            ),
            (
                "/v1beta/models/physical:generateContent",
                r#"{"contents":[{"role":"user","parts":[{"text":"first","text":"second"}]}]}"#,
            ),
        ] {
            let response = build_app(state.clone())
                .oneshot(
                    Request::post(path)
                        .header("authorization", format!("Bearer {key}"))
                        .header("content-type", "application/json")
                        .body(Body::from(payload))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{key} {path}");
        }
    }
    assert!(origin.received_requests().await.unwrap().is_empty());
    assert!(sink.events().is_empty());
    let ledger = state.ledger.lock().unwrap();
    assert_eq!(ledger.reserved("vk:vk_one"), 0);
    assert_eq!(ledger.spent("vk:vk_one"), 0);
}
fn provider(
    server: &MockServer,
    id: &str,
    family: Family,
    revision: u64,
    generation: u64,
) -> ProviderHandle {
    build_provider_handle_for_endpoint(
        &ProviderRuntime::new(),
        "fixture",
        Some(&server.uri()),
        "origin-secret",
        CredentialScheme::Bearer,
        None,
        BackendEndpoint::standard(family),
    )
    .unwrap()
    .with_backend_binding(BackendBinding {
        id: id.into(),
        revision,
        provider: "fixture".into(),
        credential_ref: "fixture:default".into(),
        credential_generation: generation,
        endpoint_family: family,
        endpoint_profile: "standard".into(),
        models: vec![serde_json::from_value(json!({"id":"physical"})).unwrap()],
        cache_policy: None,
    })
}
fn definition(targets: &[&str]) -> RouteDefinition {
    serde_json::from_value(
        json!({"id":"coding","models":[{"id":"agent","targets":targets.iter()
        .map(|backend|json!({"backend":backend,"model":"physical"})).collect::<Vec<_>>()}]}),
    )
    .unwrap()
}
fn state(
    handles: Vec<(&str, ProviderHandle)>,
    definition: RouteDefinition,
) -> (Arc<ProxyState>, Arc<InMemorySink>) {
    let keys = KeyStore::new();
    for id in ["vk_one", "vk_two"] {
        keys.insert(VirtualKey {
            id: id.into(),
            upstream_ref: "route/coding".into(),
            models: Some(vec!["agent".into()]),
            ..Default::default()
        });
    }
    let mut ledger = support::Ledger::new();
    for scope in ["vk:vk_one", "vk:vk_two", "upstream:fixture"] {
        ledger
            .set_budget(scope, Some(100_000), Window::Total, Policy::Block)
            .unwrap();
    }
    for (_, handle) in &handles {
        ledger
            .set_budget(
                handle.upstream_budget_scope(),
                Some(100_000),
                Window::Total,
                Policy::Block,
            )
            .unwrap();
    }
    let sink = Arc::new(InMemorySink::new());
    let mut state = support::state(
        keys,
        ledger,
        sink.clone(),
        handles
            .into_iter()
            .map(|(id, handle)| (format!("backend/{id}"), handle))
            .collect::<HashMap<_, _>>(),
        None,
    );
    let routes = RouteStore::in_memory().unwrap();
    routes.put(&definition, None).unwrap();
    state.routes = Some(Arc::new(routes));
    state.admin_token = Some("admin".into());
    (Arc::new(state), sink)
}
fn body(source: Family, stream: bool) -> (String, Value) {
    match source {
        Family::OpenaiChatCompletions => (
            "/v1/chat/completions".into(),
            json!({"model":"agent","max_tokens":64,"stream":stream,"messages":[{"role":"user","content":"hello"}]}),
        ),
        Family::AnthropicMessages => (
            "/v1/messages".into(),
            json!({"model":"agent","max_tokens":64,"stream":stream,"messages":[{"role":"user","content":"hello"}]}),
        ),
        Family::OpenaiResponses => (
            "/v1/responses".into(),
            json!({"model":"agent","max_output_tokens":64,"stream":stream,"input":"hello"}),
        ),
        Family::GeminiGenerateContent => (
            format!(
                "/v1beta/models/agent:{}",
                if stream {
                    "streamGenerateContent?alt=sse"
                } else {
                    "generateContent"
                }
            ),
            json!({"contents":[{"role":"user","parts":[{"text":"hello"}]}],"generationConfig":{"maxOutputTokens":64}}),
        ),
        _ => unreachable!(),
    }
}
async fn call(
    state: &Arc<ProxyState>,
    route: &str,
    body: Value,
    key: &str,
    session: &str,
) -> (StatusCode, Vec<u8>) {
    let response = build_app(state.clone())
        .oneshot(
            Request::post(route)
                .header("authorization", format!("Bearer {key}"))
                .header("x-sandhi-session", session)
                .header("content-type", "application/json")
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
fn reply(target: Family, stream: bool) -> ResponseTemplate {
    let usage = json!({"input_tokens":10,"output_tokens":20,"cache_read_input_tokens":60,"cache_creation_input_tokens":30});
    let completed = match target {
        Family::OpenaiChatCompletions => {
            json!({"id":"chat","model":"physical","choices":[{"index":0,"message":{"role":"assistant","content":"reply"},"finish_reason":"stop"}],"usage":{"prompt_tokens":100,"completion_tokens":20,"prompt_tokens_details":{"cached_tokens":60,"cache_write_tokens":30}}})
        }
        Family::AnthropicMessages => {
            json!({"id":"msg","type":"message","role":"assistant","model":"physical","content":[{"type":"text","text":"reply"}],"stop_reason":"end_turn","usage":usage})
        }
        Family::OpenaiResponses => {
            json!({"id":"resp","model":"physical","status":"completed","output":[{"type":"message","id":"msg_output","role":"assistant","content":[{"type":"output_text","text":"reply"}]}],"usage":{"input_tokens":100,"output_tokens":20,"input_tokens_details":{"cached_tokens":60,"cache_write_tokens":30}}})
        }
        Family::GeminiGenerateContent => {
            json!({"candidates":[{"content":{"role":"model","parts":[{"text":"reply"}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":100,"candidatesTokenCount":20,"cachedContentTokenCount":60,"totalTokenCount":120}})
        }
        _ => unreachable!(),
    };
    if !stream {
        return ResponseTemplate::new(200).set_body_json(completed);
    }
    let frames = match target {
        Family::OpenaiChatCompletions => vec![
            json!({"object":"chat.completion.chunk","model":"physical","choices":[{"index":0,"delta":{"content":"reply"},"finish_reason":"stop"}]}),
            json!({"object":"chat.completion.chunk","choices":[],"usage":completed["usage"]}),
        ],
        Family::AnthropicMessages => vec![
            json!({"type":"message_start","message":{"type":"message","role":"assistant","content":[],"id":"msg","model":"physical","usage":usage}}),
            json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":"reply"}}),
            json!({"type":"content_block_stop","index":0}),
            json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":20}}),
            json!({"type":"message_stop"}),
        ],
        Family::OpenaiResponses => vec![
            json!({"type":"response.created","response":{"id":"resp","model":"physical","status":"in_progress","output":[]}}),
            json!({"type":"response.completed","response":completed}),
        ],
        Family::GeminiGenerateContent => vec![completed],
        _ => unreachable!(),
    };
    let mut sse = frames
        .iter()
        .map(|frame| format!("data: {frame}\n\n"))
        .collect::<String>();
    if target == Family::OpenaiChatCompletions {
        sse.push_str("data: [DONE]\n\n");
    }
    ResponseTemplate::new(200)
        .insert_header("content-type", "text/event-stream")
        .set_body_string(sse)
}

#[tokio::test]
async fn aliases_rewrite_real_models_on_all_ingress_backend_pairs_and_modes() {
    for source in FAMILIES {
        for target in FAMILIES {
            for stream in [false, true] {
                let origin = MockServer::start().await;
                Mock::given(method("POST"))
                    .respond_with(reply(target, stream))
                    .expect(1)
                    .mount(&origin)
                    .await;
                let (state, sink) = state(
                    vec![("primary", provider(&origin, "primary", target, 1, 1))],
                    definition(&["primary"]),
                );
                let (route, body) = body(source, stream);
                let (status, response) = call(&state, &route, body, "vk_one", "session-one").await;
                assert_eq!(
                    status,
                    StatusCode::OK,
                    "{source:?}/{target:?}/{stream}: {}",
                    String::from_utf8_lossy(&response)
                );
                assert!(String::from_utf8_lossy(&response).contains("reply"));
                let requests = origin.received_requests().await.unwrap();
                let sent: Value = serde_json::from_slice(&requests[0].body).unwrap();
                if target == Family::GeminiGenerateContent {
                    assert!(requests[0].url.path().contains("models/physical:"));
                } else {
                    assert_eq!(sent["model"], "physical");
                }
                assert_eq!(requests[0].headers["authorization"], "Bearer origin-secret");
                let events = sink.events();
                assert_eq!(events.len(), 1);
                assert_eq!(events[0].model, "physical");
                assert_eq!(events[0].cache_read_tokens, 60);
                let upstream_scope = state.providers.lock().unwrap()["backend/primary"]
                    .upstream_budget_scope()
                    .to_owned();
                for scope in ["vk:vk_one", upstream_scope.as_str()] {
                    let ledger = state.ledger.lock().unwrap();
                    assert_eq!(ledger.spent(scope), 120);
                    assert_eq!(ledger.reserved(scope), 0);
                }
            }
        }
    }
}

#[tokio::test]
async fn pinned_backend_loss_or_generation_change_refuses_instead_of_switching() {
    let origin = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(reply(Family::OpenaiChatCompletions, false))
        .mount(&origin)
        .await;
    for generation in [false, true] {
        let (state, sink) = state(
            vec![
                (
                    "primary",
                    provider(&origin, "primary", Family::OpenaiChatCompletions, 1, 1),
                ),
                (
                    "secondary",
                    provider(&origin, "secondary", Family::OpenaiChatCompletions, 1, 1),
                ),
            ],
            definition(&["primary", "secondary"]),
        );
        let (route, body) = body(Family::OpenaiChatCompletions, false);
        assert_eq!(
            call(&state, &route, body.clone(), "vk_one", "session-one")
                .await
                .0,
            StatusCode::OK
        );
        if generation {
            state.providers.lock().unwrap().insert(
                "backend/primary".into(),
                provider(&origin, "primary", Family::OpenaiChatCompletions, 1, 2),
            );
        } else {
            state.providers.lock().unwrap().remove("backend/primary");
        }
        let sent = origin.received_requests().await.unwrap().len();
        assert_eq!(
            call(&state, &route, body.clone(), "vk_one", "session-one")
                .await
                .0,
            StatusCode::CONFLICT
        );
        assert_eq!(origin.received_requests().await.unwrap().len(), sent);
        assert_eq!(sink.events().len(), 1);
        assert_eq!(
            call(&state, &route, body.clone(), "vk_one", "new-session")
                .await
                .0,
            StatusCode::OK
        );
        // The same caller hint under a different key has independent authority/affinity.
        assert_eq!(
            call(&state, &route, body, "vk_two", "session-one").await.0,
            StatusCode::OK
        );
    }
}

#[tokio::test]
async fn denied_alias_and_unproven_state_never_dispatch_or_reserve_and_discovery_is_scoped() {
    let origin = MockServer::start().await;
    let (state, sink) = state(
        vec![(
            "primary",
            provider(&origin, "primary", Family::OpenaiResponses, 1, 1),
        )],
        definition(&["primary"]),
    );
    let (route, mut payload) = body(Family::OpenaiResponses, false);
    payload["model"] = json!("physical");
    assert_eq!(
        call(&state, &route, payload, "vk_one", "one").await.0,
        StatusCode::FORBIDDEN
    );
    // Counting a request must not establish a generation session pin.
    let (_, body) = body(Family::AnthropicMessages, false);
    assert_eq!(
        call(&state, "/v1/messages/count_tokens", body, "vk_one", "one")
            .await
            .0,
        StatusCode::OK
    );
    let body = json!({"model":"agent","input":"continue","previous_response_id":"resp_foreign","max_output_tokens":64});
    assert_eq!(
        call(&state, &route, body, "vk_one", "one").await.0,
        StatusCode::CONFLICT
    );
    let response = build_app(state.clone())
        .oneshot(
            Request::get("/v1/models")
                .header("authorization", "Bearer vk_one")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let models: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), 10_000).await.unwrap()).unwrap();
    assert_eq!(models["data"][0]["id"], "agent");
    assert_eq!(models["data"].as_array().unwrap().len(), 1);
    assert!(origin.received_requests().await.unwrap().is_empty());
    assert!(sink.events().is_empty());
    let ledger = state.ledger.lock().unwrap();
    assert_eq!(ledger.spent("vk:vk_one"), 0);
    assert_eq!(ledger.reserved("vk:vk_one"), 0);
}

#[tokio::test]
async fn file_references_require_an_established_stateful_route_before_pinning() {
    for family in [Family::OpenaiChatCompletions, Family::OpenaiResponses] {
        for affinity in [
            sandhi_store::routes::Affinity::Session,
            sandhi_store::routes::Affinity::None,
        ] {
            let origin = MockServer::start().await;
            Mock::given(method("POST"))
                .respond_with(reply(family, false))
                .mount(&origin)
                .await;
            let mut definition = definition(&["primary"]);
            definition.affinity = affinity;
            let (state, sink) = state(
                vec![("primary", provider(&origin, "primary", family, 1, 1))],
                definition,
            );
            let (route, body) = if family == Family::OpenaiChatCompletions {
                (
                    "/v1/chat/completions",
                    json!({"model":"agent","max_tokens":64,"messages":[{"role":"user","content":[{"type":"file","file":{"file_id":"file_foreign"}}]}]}),
                )
            } else {
                (
                    "/v1/responses",
                    json!({"model":"agent","max_output_tokens":64,"input":[{"type":"function_call_output","call_id":"call_1","output":[{"type":"input_file","file_id":"file_foreign"}]}]}),
                )
            };
            let (status, _) = call(&state, route, body, "vk_one", "unbound-file").await;
            assert_eq!(status, StatusCode::CONFLICT, "{family:?}/{affinity:?}");
            assert!(origin.received_requests().await.unwrap().is_empty());
            assert!(sink.events().is_empty());
            let ledger = state.ledger.lock().unwrap();
            assert_eq!(ledger.reserved("vk:vk_one"), 0);
            assert_eq!(ledger.spent("vk:vk_one"), 0);
        }
    }
}

#[tokio::test]
async fn gemini_discovery_only_lists_path_safe_logical_aliases() {
    let origin = MockServer::start().await;
    let mut desired = definition(&["primary"]);
    let mut incompatible = desired.models[0].clone();
    incompatible.id = "org/agent".into();
    desired.models.push(incompatible);
    let (state, _) = state(
        vec![(
            "primary",
            provider(&origin, "primary", Family::OpenaiChatCompletions, 1, 1),
        )],
        desired,
    );
    state.keys.insert(VirtualKey {
        id: "vk_catalog".into(),
        upstream_ref: "route/coding".into(),
        ..Default::default()
    });
    for (path, field, expected) in [
        ("/v1/models", "data", vec!["agent", "org/agent"]),
        ("/v1beta/models", "models", vec!["models/agent"]),
    ] {
        let response = build_app(state.clone())
            .oneshot(
                Request::get(path)
                    .header("authorization", "Bearer vk_catalog")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let catalog: Value =
            serde_json::from_slice(&to_bytes(response.into_body(), 10_000).await.unwrap()).unwrap();
        let names = catalog[field]
            .as_array()
            .unwrap()
            .iter()
            .map(|model| {
                model[if field == "data" { "id" } else { "name" }]
                    .as_str()
                    .unwrap()
            })
            .collect::<Vec<_>>();
        assert_eq!(names, expected);
    }
    assert!(origin.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn target_encoder_preflight_selects_compatible_candidates_and_never_moves_a_pin() {
    for stream in [false, true] {
        let responses = MockServer::start().await;
        let chat = MockServer::start().await;
        for (origin, family) in [
            (&responses, Family::OpenaiResponses),
            (&chat, Family::OpenaiChatCompletions),
        ] {
            Mock::given(method("POST"))
                .respond_with(reply(family, stream))
                .mount(origin)
                .await;
        }
        let (state, sink) = state(
            vec![
                (
                    "responses",
                    provider(&responses, "responses", Family::OpenaiResponses, 1, 1),
                ),
                (
                    "chat",
                    provider(&chat, "chat", Family::OpenaiChatCompletions, 1, 1),
                ),
            ],
            definition(&["responses", "chat"]),
        );
        let (path, mut payload) = body(Family::OpenaiChatCompletions, stream);
        // stop is a portable Chat option that the Responses encoder explicitly refuses.
        payload["stop"] = json!(["END"]);
        let (status, _) = call(&state, &path, payload.clone(), "vk_one", "needs-stop").await;
        assert_eq!(status, StatusCode::OK);
        assert!(responses.received_requests().await.unwrap().is_empty());
        assert_eq!(chat.received_requests().await.unwrap().len(), 1);
        assert_eq!(sink.events().len(), 1);
        // A different first turn can choose Responses. Later adding stop must refuse,
        // even though Chat is otherwise eligible: affinity cannot silently move.
        let (_, plain) = body(Family::OpenaiChatCompletions, stream);
        assert_eq!(
            call(&state, &path, plain, "vk_one", "pinned-responses")
                .await
                .0,
            StatusCode::OK
        );
        assert_eq!(
            call(&state, &path, payload, "vk_one", "pinned-responses")
                .await
                .0,
            StatusCode::CONFLICT
        );
        assert_eq!(responses.received_requests().await.unwrap().len(), 1);
        assert_eq!(chat.received_requests().await.unwrap().len(), 1);
        assert_eq!(sink.events().len(), 2);
        let ledger = state.ledger.lock().unwrap();
        assert_eq!(ledger.reserved("vk:vk_one"), 0);
        assert_eq!(ledger.spent("vk:vk_one"), 240);
    }
}

#[tokio::test]
async fn rendezvous_selection_is_order_independent_and_distributes_stateless_inputs() {
    let primary = MockServer::start().await;
    let secondary = MockServer::start().await;
    for origin in [&primary, &secondary] {
        Mock::given(method("POST"))
            .respond_with(reply(Family::OpenaiChatCompletions, false))
            .mount(origin)
            .await;
    }
    let mut desired = definition(&["primary", "secondary"]);
    desired.selection = sandhi_store::routes::Selection::Rendezvous;
    desired.affinity = sandhi_store::routes::Affinity::None;
    let (state, _) = state(
        vec![
            (
                "primary",
                provider(&primary, "primary", Family::OpenaiChatCompletions, 1, 1),
            ),
            (
                "secondary",
                provider(&secondary, "secondary", Family::OpenaiChatCompletions, 1, 1),
            ),
        ],
        desired.clone(),
    );
    let mut decisions = Vec::new();
    for pass in 0..2 {
        if pass == 1 {
            desired.models[0].targets.reverse();
            state
                .routes
                .as_ref()
                .unwrap()
                .put(&desired, Some(1))
                .unwrap();
        }
        let mut observed = Vec::new();
        for i in 0..16 {
            let before = primary.received_requests().await.unwrap().len();
            let (path, mut payload) = body(Family::OpenaiChatCompletions, false);
            payload["messages"][0]["content"] = json!(format!("stateless-input-{i}"));
            assert_eq!(
                call(&state, &path, payload, "vk_one", "same-hint").await.0,
                StatusCode::OK
            );
            observed.push(primary.received_requests().await.unwrap().len() > before);
        }
        if pass == 0 {
            decisions = observed;
        } else {
            assert_eq!(decisions, observed);
        }
    }
    assert!(decisions.contains(&true) && decisions.contains(&false));
}

#[tokio::test]
async fn resolved_target_policy_cannot_be_bypassed_with_a_logical_alias() {
    let origin = MockServer::start().await;
    let (mut state, sink) = state(
        vec![(
            "primary",
            provider(&origin, "primary", Family::OpenaiChatCompletions, 1, 1),
        )],
        definition(&["primary"]),
    );
    let temp = tempfile::tempdir().unwrap();
    let config = serde_json::to_vec(&json!({"schema_version":"1","revision":1,"deadline_ms":500,
        "max_body_bytes":65536,"rules":[{"id":"deny-physical","effect":"block",
        "when":{"upstreams":["backend/primary"],"models":["physical"]},
        "evaluator":{"kind":"threshold","metric":"body_bytes","above":0}}]}))
    .unwrap();
    Arc::get_mut(&mut state).unwrap().policy =
        Some(Arc::new(sandhi_proxy::policy::PolicyGate::new(
            sandhi_core::policy::Engine::from_slice(&config).unwrap(),
            Arc::new(
                sandhi_store::policy::PolicyAuditStore::open(&temp.path().join("audit.db"), 100)
                    .unwrap(),
            ),
        )));
    let (path, payload) = body(Family::OpenaiChatCompletions, false);
    assert_eq!(
        call(&state, &path, payload, "vk_one", "one").await.0,
        StatusCode::FORBIDDEN
    );
    assert!(origin.received_requests().await.unwrap().is_empty());
    assert!(sink.events().is_empty());
    assert_eq!(state.ledger.lock().unwrap().reserved("vk:vk_one"), 0);
}

#[tokio::test]
async fn logical_routes_cannot_shadow_existing_host_owned_names() {
    let origin = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(reply(Family::OpenaiChatCompletions, false))
        .expect(1)
        .mount(&origin)
        .await;
    let (state, _) = state(
        vec![(
            "primary",
            provider(&origin, "primary", Family::OpenaiChatCompletions, 1, 1),
        )],
        definition(&["primary"]),
    );
    let host = ProviderRuntime::new().openai_compat(
        "host-owned",
        origin.uri(),
        "legacy-origin-secret",
        Default::default(),
        None,
        None,
        None,
        None,
    );
    state
        .providers
        .lock()
        .unwrap()
        .insert("route/coding".into(), host);
    let (path, payload) = body(Family::OpenaiChatCompletions, false);
    assert_eq!(
        call(&state, &path, payload, "vk_one", "one").await.0,
        StatusCode::OK
    );
    let requests = origin.received_requests().await.unwrap();
    let sent: Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(sent["model"], "agent");
    assert_eq!(
        requests[0].headers["authorization"],
        "Bearer legacy-origin-secret"
    );
    let (status, _) = call(
        &state,
        "/admin/routes",
        json!({"definition":definition(&["primary"]),"expected_revision":1}),
        "admin",
        "",
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        state
            .routes
            .as_ref()
            .unwrap()
            .get("coding")
            .unwrap()
            .unwrap()
            .revision,
        1
    );
}
