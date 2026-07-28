//! End-to-end proxy tests: a client hits the proxy with a **virtual key**; the proxy resolves
//! it, budget-checks, forwards to a **wiremock** upstream with the **real** key, streams the
//! response back, and emits a usage event. No live API keys.

use std::collections::HashMap;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt; // oneshot
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use sandhi_core::{InMemorySink, KeyStore, Policy, VirtualKey, Window};
use sandhi_providers::{
    ChatEventStream, ChatProvider, ProviderError, ProviderHandle, ProviderRuntime,
};
use sandhi_proxy::{build_app, ProxyLedger, ProxyState};

fn state_with(
    upstream_uri: String,
    sink: Arc<InMemorySink>,
    ledger: ProxyLedger,
) -> Arc<ProxyState> {
    let keys = KeyStore::new();
    keys.insert(VirtualKey {
        id: "vk_demo".into(),
        subject_id: Some("alice".into()),
        group_id: Some("platform".into()),
        upstream_ref: "up1".into(),
        ..Default::default()
    });
    let mut providers: HashMap<String, ProviderHandle> = HashMap::new();
    providers.insert(
        "up1".into(),
        ProviderRuntime::new().openai_compat(
            "openai",
            upstream_uri,
            "REAL-KEY",
            Default::default(),
            Some(0),
            None,
            None,
        ),
    );
    Arc::new(ProxyState::new(keys, ledger, sink, providers, None))
}

#[tokio::test]
async fn complete_attributes_meters_and_records_budget() {
    let upstream = MockServer::start().await;
    let resp = serde_json::json!({
        "choices": [{ "message": { "content": "hi" } }],
        "usage": { "prompt_tokens": 100, "completion_tokens": 20,
                   "prompt_tokens_details": { "cached_tokens": 60 } }
    });
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        // the proxy forwards the REAL upstream key, never the client's virtual key
        .and(header("authorization", "Bearer REAL-KEY"))
        .respond_with(ResponseTemplate::new(200).set_body_json(resp))
        .mount(&upstream)
        .await;

    let sink = Arc::new(InMemorySink::new());
    let state = state_with(upstream.uri(), sink.clone(), ProxyLedger::in_memory());
    let app = build_app(state.clone());

    let req = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("authorization", "Bearer vk_demo") // client presents the VIRTUAL key
        .header("content-type", "application/json")
        .header("x-sandhi-session", "conv_1")
        .body(Body::from(r#"{"model":"gpt-x","messages":[]}"#))
        .unwrap();

    let response = app.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let events = sink.events();
    assert_eq!(events.len(), 1);
    let ev = &events[0];
    assert_eq!(ev.subject_id.as_deref(), Some("alice"));
    assert_eq!(ev.group_id.as_deref(), Some("platform"));
    assert_eq!(ev.virtual_key_id.as_deref(), Some("vk_demo"));
    assert_eq!(ev.session_id.as_deref(), Some("conv_1"));
    assert_eq!(ev.provider, "openai");
    assert_eq!(ev.tokens_in, 40); // 100 - 60 cached
    assert_eq!(ev.cache_read_tokens, 60);
    // Display and enforcement now report the same number: 40 fresh in + 60 cache-read + 20 out.
    assert_eq!(ev.billable_tokens(), 120);
    assert_eq!(ev.usage_completeness, sandhi_core::UsageCompleteness::Final);
    assert_eq!(ev.outcome.as_deref(), Some("success"));

    // ADR-0005 D4: the ledger settles via `billable()`, which counts the cache split — and the
    // event helper above agrees with it exactly, so an operator reading usage sees what was
    // charged. (Unifying those two was the tracked follow-up this closes.)
    assert_eq!(state.ledger.lock().unwrap().spent("group:platform"), 120);
    assert_eq!(state.ledger.lock().unwrap().reserved("group:platform"), 0);
}

/// A proxy fronting a mocked **Anthropic** upstream that answers `/v1/messages` once.
async fn anthropic_state(upstream: &MockServer, sink: Arc<InMemorySink>) -> Arc<ProxyState> {
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(header("x-api-key", "REAL-KEY"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id":"msg_1","type":"message","role":"assistant","model":"claude-test",
            "content":[{"type":"text","text":"hello"}],"stop_reason":"end_turn",
            "usage":{"input_tokens":7,"output_tokens":3,"cache_read_input_tokens":2}
        })))
        .mount(upstream)
        .await;

    let keys = KeyStore::new();
    keys.insert(VirtualKey {
        id: "vk_demo".into(),
        subject_id: Some("alice".into()),
        group_id: Some("platform".into()),
        upstream_ref: "up1".into(),
        ..Default::default()
    });
    let mut providers = HashMap::new();
    providers.insert(
        "up1".into(),
        ProviderRuntime::new().anthropic(
            upstream.uri(),
            "REAL-KEY",
            sandhi_providers::AnthropicAuthScheme::ApiKey,
            Some(0),
            None,
            None,
        ),
    );
    Arc::new(ProxyState::new(
        keys,
        ProxyLedger::in_memory(),
        sink,
        providers,
        None,
    ))
}

const ANTHROPIC_BODY: &str =
    r#"{"model":"claude-test","max_tokens":32,"messages":[{"role":"user","content":"hi"}]}"#;

#[tokio::test]
async fn anthropic_ingress_uses_the_same_typed_runtime_and_meter() {
    let upstream = MockServer::start().await;
    let sink = Arc::new(InMemorySink::new());
    let state = anthropic_state(&upstream, sink.clone()).await;

    let response = build_app(state.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/messages")
                .header("authorization", "Bearer vk_demo")
                .body(Body::from(ANTHROPIC_BODY))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(value["type"], "message");
    assert_eq!(value["content"][0]["text"], "hello");
    assert_eq!(value["usage"]["cache_read_input_tokens"], 2);
    assert_eq!(sink.events().len(), 1);
    assert_eq!(sink.events()[0].provider, "anthropic");
    assert_eq!(sink.events()[0].tokens_in, 7);
    assert_eq!(sink.events()[0].cache_read_tokens, 2);
    assert_eq!(state.ledger.lock().unwrap().reserved("group:platform"), 0);
}

/// TD-0010 D1 — the regression this fixes: the stock Anthropic SDK authenticates with
/// `x-api-key` and nothing else, so `anthropic.Anthropic(base_url=…, api_key="vk_…")` used to
/// get a flat 401 from the proxy's single `Authorization: Bearer` credential path. Nothing in
/// the request below is Sandhi-specific — it is exactly what the vendor SDK puts on the wire.
#[tokio::test]
async fn anthropic_ingress_accepts_the_sdk_x_api_key_header() {
    let upstream = MockServer::start().await;
    let sink = Arc::new(InMemorySink::new());
    let state = anthropic_state(&upstream, sink.clone()).await;

    let response = build_app(state.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/messages")
                .header("x-api-key", "vk_demo")
                .header("anthropic-version", "2023-06-01")
                .header("content-type", "application/json")
                .body(Body::from(ANTHROPIC_BODY))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(value["content"][0]["text"], "hello");
    // The call was attributed and metered like any other — the credential form is presentation,
    // not policy.
    assert_eq!(sink.events().len(), 1);
    assert_eq!(sink.events()[0].virtual_key_id.as_deref(), Some("vk_demo"));
    assert_eq!(sink.events()[0].subject_id.as_deref(), Some("alice"));
}

/// `x-api-key` is Anthropic's scheme, not OpenAI's. Accepting it on `/v1/chat/completions` would
/// invent a cross-vendor auth form no client sends and no vendor documents.
#[tokio::test]
async fn openai_ingress_rejects_x_api_key() {
    let sink = Arc::new(InMemorySink::new());
    let state = state_with(
        "http://127.0.0.1:1".into(),
        sink.clone(),
        ProxyLedger::in_memory(),
    );

    let response = build_app(state)
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/chat/completions")
                .header("x-api-key", "vk_demo")
                .body(Body::from(r#"{"model":"gpt-x","messages":[]}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(sink.len(), 0);
}

/// Absent or malformed credentials still fail closed on every ingress path, with the same
/// error body shape as before.
#[tokio::test]
async fn missing_or_malformed_credential_is_401_on_every_ingress_path() {
    for (uri, body) in [
        ("/v1/chat/completions", r#"{"model":"gpt-x","messages":[]}"#),
        ("/v1/messages", ANTHROPIC_BODY),
        ("/v1/responses", r#"{"model":"gpt-x","input":[]}"#),
    ] {
        for credential in [None, Some(("authorization", "Basic vk_demo"))] {
            let sink = Arc::new(InMemorySink::new());
            let state = state_with(
                "http://127.0.0.1:1".into(),
                sink.clone(),
                ProxyLedger::in_memory(),
            );
            let mut request = Request::builder().method("POST").uri(uri);
            if let Some((name, value)) = credential {
                request = request.header(name, value);
            }
            let response = build_app(state)
                .oneshot(request.body(Body::from(body)).unwrap())
                .await
                .unwrap();

            assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{uri}");
            let payload = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap();
            let value: serde_json::Value = serde_json::from_slice(&payload).unwrap();
            // Dialect-shaped now (TD-0010 D2 auth slice): OpenAI/Responses nest under `error`,
            // Anthropic wraps in `{"type":"error",...}`, and the text names the scheme THAT
            // dialect's SDK sends rather than telling everyone to use bearer.
            let message = value["error"]["message"].as_str().unwrap_or_else(|| {
                panic!("{uri}: expected a structured error object, got {value}")
            });
            assert!(
                message.starts_with("missing virtual key"),
                "{uri}: {message}"
            );
            if uri == "/v1/messages" {
                assert_eq!(value["type"], "error", "{uri}");
                assert!(message.contains("x-api-key"), "{uri}: {message}");
            } else {
                assert!(
                    message.contains("Authorization: Bearer"),
                    "{uri}: {message}"
                );
            }
            assert_eq!(sink.len(), 0);
        }
    }
}

#[tokio::test]
async fn responses_ingress_same_family_forwards_transparently() {
    let upstream = MockServer::start().await;
    // Responses ingress → a Responses upstream is SAME-FAMILY, so the transparent plane forwards
    // the client's bytes verbatim and meters usage at the source (ADR-0004 D1 / TD-0006). The
    // client receives the upstream's own body (not a re-encoded one); the metering event still
    // carries the correct fresh-input split parsed at the source.
    Mock::given(method("POST"))
        .and(path("/responses"))
        .and(header("authorization", "Bearer REAL-KEY"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id":"resp_1","model":"gpt-test","status":"completed",
            "output":[
                {"type":"message","content":[{"type":"output_text","text":"hello"}]},
                {"type":"function_call","call_id":"call_1","name":"weather","arguments":"{\"city\":\"Austin\"}"}
            ],
            "usage":{"input_tokens":7,"output_tokens":3,
                     "input_tokens_details":{"cached_tokens":2}}
        })))
        .mount(&upstream)
        .await;

    let keys = KeyStore::new();
    keys.insert(VirtualKey {
        id: "vk_demo".into(),
        subject_id: Some("alice".into()),
        group_id: Some("platform".into()),
        upstream_ref: "up1".into(),
        ..Default::default()
    });
    let mut providers: HashMap<String, ProviderHandle> = HashMap::new();
    providers.insert(
        "up1".into(),
        ProviderRuntime::new().openai_responses(
            "openai",
            upstream.uri(),
            "REAL-KEY",
            Default::default(),
            Some(0),
            None,
            None,
        ),
    );
    let sink = Arc::new(InMemorySink::new());
    let state = Arc::new(ProxyState::new(
        keys,
        ProxyLedger::in_memory(),
        sink.clone(),
        providers,
        None,
    ));

    let response = build_app(state.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/responses")
                .header("authorization", "Bearer vk_demo")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"model":"gpt-test","instructions":"be precise","stream":false,"input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"weather?"}]}]}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
    // The upstream's body is forwarded verbatim (transparent) — raw upstream counts, not a
    // re-encoded fresh split.
    assert_eq!(value["status"], "completed");
    assert_eq!(value["output"][0]["type"], "message");
    assert_eq!(value["output"][0]["content"][0]["text"], "hello");
    assert_eq!(value["output"][1]["type"], "function_call");
    assert_eq!(value["usage"]["input_tokens"], 7); // verbatim upstream body

    // One usage event, attributed to the virtual key, routed through /v1/responses.
    let events = sink.events();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].provider, "openai");
    assert_eq!(events[0].route.as_deref(), Some("/v1/responses"));
    assert_eq!(events[0].tokens_in, 5);
    assert_eq!(events[0].cache_read_tokens, 2);
    assert_eq!(state.ledger.lock().unwrap().reserved("group:platform"), 0);
}

#[tokio::test]
async fn unknown_virtual_key_is_401() {
    let sink = Arc::new(InMemorySink::new());
    let state = state_with(
        "http://127.0.0.1:1".into(),
        sink.clone(),
        ProxyLedger::in_memory(),
    );
    let app = build_app(state);

    let req = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("authorization", "Bearer vk_nope")
        .body(Body::from(r#"{"model":"m"}"#))
        .unwrap();

    let response = app.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(sink.len(), 0);
}

#[tokio::test]
async fn exhausted_budget_is_429_before_calling_upstream() {
    let sink = Arc::new(InMemorySink::new());
    let mut ledger = ProxyLedger::in_memory();
    // A tiny hard cap: the conservative ceiling of any real request (input estimate + the default
    // output ceiling) can't fit, so admission is refused before the upstream is ever called
    // (ADR-0005 D1 — the ceiling is the gate, not a lower-bound estimate).
    ledger.set_budget("group:platform", Some(10), Window::Total, Policy::Block);

    // An upstream with no mounts — reaching it would 404; asserting 429 proves we never do.
    let upstream = MockServer::start().await;
    let state = state_with(upstream.uri(), sink.clone(), ledger);
    let app = build_app(state);

    let req = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("authorization", "Bearer vk_demo")
        .body(Body::from(r#"{"model":"m","messages":[]}"#))
        .unwrap();

    let response = app.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(sink.len(), 0);
}

#[tokio::test]
async fn streaming_passes_through_and_emits_usage() {
    let upstream = MockServer::start().await;
    let sse = concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\"he\"}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"content\":\"llo\"}}]}\n\n",
        "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":5,\"prompt_tokens_details\":{\"cached_tokens\":4}}}\n\n",
        "data: [DONE]\n\n"
    );
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(sse),
        )
        .mount(&upstream)
        .await;

    let sink = Arc::new(InMemorySink::new());
    let state = state_with(upstream.uri(), sink.clone(), ProxyLedger::in_memory());
    let app = build_app(state.clone());

    let req = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("authorization", "Bearer vk_demo")
        .body(Body::from(
            r#"{"model":"gpt-x","messages":[],"stream":true}"#,
        ))
        .unwrap();

    let response = app.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let text = String::from_utf8(body.to_vec()).unwrap();
    assert!(text.contains("he") && text.contains("llo") && text.contains("[DONE]"));

    // Usage emitted after the stream completed; budget recorded (6 fresh in + 5 out).
    let events = sink.events();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].tokens_out, 5);
    assert_eq!(events[0].cache_read_tokens, 4);
    // 6 fresh in + 4 cache-read + 5 out = 15 — identical to what the ledger settled below.
    assert_eq!(events[0].billable_tokens(), 15);
    // ADR-0005 D4: ledger settles via billable() incl. the cache split — 6 in + 4 cache-read + 5 out = 15.
    assert_eq!(state.ledger.lock().unwrap().spent("group:platform"), 15);
}

#[tokio::test]
async fn dashboard_reports_aggregates_from_the_store() {
    use sandhi_core::{Backend, Sink, UsageEvent};
    use sandhi_store::SqliteStore;

    let store = Arc::new(SqliteStore::in_memory().unwrap());
    let ev = |subject: &str, tin: u64, tout: u64| {
        UsageEvent::new("r", "t", "openai", "m", Backend::External)
            .with_attribution(Some("vk".into()), Some(subject.into()), Some("team".into()))
            .with_tokens(tin, tout)
    };
    store.emit(&ev("alice", 100, 20));
    store.emit(&ev("bob", 50, 10));

    let state = Arc::new(ProxyState::new(
        KeyStore::new(),
        ProxyLedger::in_memory(),
        store.clone(),
        HashMap::new(),
        Some(store.clone()),
    ));
    let app = build_app(state);

    // JSON API reflects the persisted events.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/dashboard/api/usage")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["total"]["calls"], 2);
    assert_eq!(json["total"]["tokens_in"], 150);
    assert_eq!(json["by_subject"][0]["key"], "alice"); // busiest first (120 > 60)

    // The HTML page serves.
    let html = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/dashboard")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(html.status(), StatusCode::OK);
}

/// A stub upstream that always times out — pins the `Timeout` → 504 mapping and that no
/// usage event is emitted for a call with no measured usage.
struct AlwaysTimeout;

#[async_trait::async_trait]
impl ChatProvider for AlwaysTimeout {
    fn slug(&self) -> &str {
        "timeout"
    }
    async fn complete(
        &self,
        _req: sandhi_core::ChatRequestV1,
    ) -> Result<sandhi_core::ChatResponseV1, ProviderError> {
        Err(ProviderError::Timeout(std::time::Duration::from_millis(50)))
    }
    async fn stream(
        &self,
        _req: sandhi_core::ChatRequestV1,
    ) -> Result<ChatEventStream, ProviderError> {
        Err(ProviderError::Timeout(std::time::Duration::from_millis(50)))
    }
}

#[tokio::test]
async fn upstream_timeout_maps_to_504() {
    let sink = Arc::new(InMemorySink::new());
    let keys = KeyStore::new();
    keys.insert(VirtualKey {
        id: "vk_demo".into(),
        subject_id: Some("alice".into()),
        group_id: Some("platform".into()),
        upstream_ref: "up1".into(),
        ..Default::default()
    });
    let mut providers: HashMap<String, ProviderHandle> = HashMap::new();
    providers.insert("up1".into(), ProviderHandle::new(Arc::new(AlwaysTimeout)));
    let state = Arc::new(ProxyState::new(
        keys,
        ProxyLedger::in_memory(),
        sink.clone(),
        providers,
        None,
    ));
    let app = build_app(state);

    let req = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("authorization", "Bearer vk_demo")
        .header("content-type", "application/json")
        .body(Body::from(r#"{"model":"gpt-x","messages":[]}"#))
        .unwrap();

    let response = app.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::GATEWAY_TIMEOUT);
    assert_eq!(
        sink.events().len(),
        1,
        "failed calls retain an outcome observation"
    );
    assert_eq!(sink.events()[0].billable_tokens(), 0);
    assert_eq!(
        sink.events()[0].usage_completeness,
        sandhi_core::UsageCompleteness::Unavailable
    );
    assert_eq!(sink.events()[0].outcome.as_deref(), Some("error"));
}

#[tokio::test]
async fn client_disconnect_mid_stream_still_meters() {
    let upstream = MockServer::start().await;
    let sse = "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n\
data: {\"usage\":{\"prompt_tokens\":100,\"completion_tokens\":20}}\n\n\
data: [DONE]\n\n";
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(sse)
                .insert_header("content-type", "text/event-stream"),
        )
        .mount(&upstream)
        .await;

    let sink = Arc::new(InMemorySink::new());
    let state = state_with(upstream.uri(), sink.clone(), ProxyLedger::in_memory());
    let app = build_app(state);

    let req = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("authorization", "Bearer vk_demo")
        .header("content-type", "application/json")
        .body(Body::from(
            r#"{"model":"gpt-x","messages":[],"stream":true}"#,
        ))
        .unwrap();

    let response = app.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    // Read ONE body frame, then drop the body — a client disconnect mid-stream.
    let mut body_stream = response.into_body().into_data_stream();
    use futures_util::StreamExt;
    let first = body_stream.next().await;
    assert!(first.is_some(), "expected at least one forwarded frame");
    drop(body_stream);

    // Metering must survive the disconnect: exactly one event, with whatever usage was seen.
    assert_eq!(
        sink.events().len(),
        1,
        "client disconnect must not lose the usage event"
    );
}

#[tokio::test]
async fn ceiling_reservation_rejects_unbounded_but_admits_bounded_output() {
    // ADR-0005 D1: the reservation is a CEILING (input estimate + effective output max), not a
    // `+1` lower bound. On a tight budget an unbounded request (no max → the conservative default
    // ceiling) is refused before dispatch, while the same budget admits a request that bounds its
    // own output — proving the ceiling tracks the effective max, not a blanket reject.
    let upstream = MockServer::start().await;
    let resp = serde_json::json!({
        "id":"c","object":"chat.completion","model":"gpt-x",
        "choices":[{"index":0,"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}],
        "usage":{"prompt_tokens":1,"completion_tokens":1}
    });
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(resp))
        .mount(&upstream)
        .await;

    let mut ledger = ProxyLedger::in_memory();
    ledger.set_budget("group:platform", Some(100), Window::Total, Policy::Block);
    let sink = Arc::new(InMemorySink::new());
    let state = state_with(upstream.uri(), sink.clone(), ledger);
    let app = build_app(state);

    // Unbounded output → ceiling (default) far exceeds the 100-token cap → 429, upstream untouched.
    let unbounded = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("authorization", "Bearer vk_demo")
        .header("content-type", "application/json")
        .body(Body::from(r#"{"model":"gpt-x","messages":[]}"#))
        .unwrap();
    let r1 = app.clone().oneshot(unbounded).await.unwrap();
    assert_eq!(
        r1.status(),
        StatusCode::TOO_MANY_REQUESTS,
        "an unbounded output must not fit a tight cap"
    );
    assert_eq!(
        sink.events().len(),
        0,
        "a rejected request never dispatches or meters"
    );

    // Same budget, but the client bounds output to 50 → ceiling fits → admitted.
    let bounded = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("authorization", "Bearer vk_demo")
        .header("content-type", "application/json")
        .body(Body::from(
            r#"{"model":"gpt-x","messages":[],"max_tokens":50}"#,
        ))
        .unwrap();
    let r2 = app.oneshot(bounded).await.unwrap();
    assert_eq!(
        r2.status(),
        StatusCode::OK,
        "a request that bounds its own output fits the cap and is admitted"
    );
    assert_eq!(sink.events().len(), 1);
}

#[tokio::test]
async fn neutral_identity_headers_flow_onto_the_usage_event() {
    // ADR-0005 D7: idempotency + agent cost-tree + trace linkage ride as neutral metadata.
    let upstream = MockServer::start().await;
    let resp = serde_json::json!({
        "id":"c","object":"chat.completion","model":"gpt-x",
        "choices":[{"index":0,"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}],
        "usage":{"prompt_tokens":5,"completion_tokens":3}
    });
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(resp))
        .mount(&upstream)
        .await;

    let sink = Arc::new(InMemorySink::new());
    let state = state_with(upstream.uri(), sink.clone(), ProxyLedger::in_memory());
    let app = build_app(state);

    let req = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("authorization", "Bearer vk_demo")
        .header("content-type", "application/json")
        .header("idempotency-key", "idem-123")
        .header("traceparent", "00-abcabc-def-01")
        .header("x-sandhi-run-id", "run-9")
        .body(Body::from(r#"{"model":"gpt-x","messages":[]}"#))
        .unwrap();
    let response = app.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let events = sink.events();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].idempotency_key.as_deref(), Some("idem-123"));
    assert_eq!(events[0].trace_context.as_deref(), Some("00-abcabc-def-01"));
    assert_eq!(events[0].run_id.as_deref(), Some("run-9"));
}

#[tokio::test]
async fn cross_family_ingress_routes_through_the_typed_translation_plane() {
    // OpenAI ingress → an Anthropic upstream is CROSS-family, so the transparent plane does not
    // apply: the proxy decodes to ChatRequestV1, the typed Anthropic provider re-encodes and POSTs
    // /v1/messages, and the neutral response is re-encoded back into the OpenAI egress shape. This
    // keeps the typed translation path covered now that same-family ingress goes transparent.
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(header("x-api-key", "REAL-KEY"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id":"msg_1","type":"message","role":"assistant","model":"claude-test",
            "content":[{"type":"text","text":"translated"}],"stop_reason":"end_turn",
            "usage":{"input_tokens":9,"output_tokens":4}
        })))
        .mount(&upstream)
        .await;

    let keys = KeyStore::new();
    keys.insert(VirtualKey {
        id: "vk_demo".into(),
        subject_id: Some("alice".into()),
        group_id: Some("platform".into()),
        upstream_ref: "up1".into(),
        ..Default::default()
    });
    let mut providers: HashMap<String, ProviderHandle> = HashMap::new();
    providers.insert(
        "up1".into(),
        ProviderRuntime::new().anthropic(
            upstream.uri(),
            "REAL-KEY",
            sandhi_providers::AnthropicAuthScheme::ApiKey,
            Some(0),
            None,
            None,
        ),
    );
    let sink = Arc::new(InMemorySink::new());
    let state = Arc::new(ProxyState::new(
        keys,
        ProxyLedger::in_memory(),
        sink.clone(),
        providers,
        None,
    ));

    // The client speaks OpenAI Chat Completions; the resolved upstream is Anthropic.
    let response = build_app(state.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/chat/completions")
                .header("authorization", "Bearer vk_demo")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"model":"claude-test","max_tokens":16,"messages":[{"role":"user","content":"hi"}]}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
    // Egress is TRANSLATED into the OpenAI shape — proof the typed plane ran, not passthrough.
    assert_eq!(value["choices"][0]["message"]["content"], "translated");
    // Metering still lands: one event, attributed, with the source counts.
    let events = sink.events();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].provider, "anthropic");
    assert_eq!(events[0].tokens_in, 9);
    assert_eq!(events[0].tokens_out, 4);
}

/// TD-0010 D3: discovery lists exactly what the key may call, so the allowlist is discoverable
/// instead of surfacing as a 403 at call time. The SDK-conformance suite covers the unfiltered
/// case; this covers the filtered one, which is the part the decision is actually about.
#[tokio::test]
async fn discovery_lists_only_the_models_the_key_permits() {
    let upstream = MockServer::start().await;
    let sink = Arc::new(InMemorySink::default());
    let keys = KeyStore::new();
    keys.insert(VirtualKey {
        id: "vk_scoped".into(),
        subject_id: Some("alice".into()),
        group_id: Some("platform".into()),
        upstream_ref: "up1".into(),
        models: Some(vec!["gpt-only-this".into()]),
        ..Default::default()
    });
    keys.insert(VirtualKey {
        id: "vk_open".into(),
        subject_id: Some("bob".into()),
        group_id: Some("platform".into()),
        upstream_ref: "up1".into(),
        ..Default::default()
    });
    let mut providers: HashMap<String, ProviderHandle> = HashMap::new();
    providers.insert(
        "up1".into(),
        ProviderRuntime::new().openai_compat(
            "openai",
            upstream.uri(),
            "REAL-KEY",
            Default::default(),
            None,
            None,
            None,
        ),
    );
    let state = Arc::new(ProxyState::new(
        keys,
        ProxyLedger::in_memory(),
        sink,
        providers,
        None,
    ));

    let listing = |token: &'static str| {
        let app = build_app(Arc::clone(&state));
        async move {
            let response = app
                .oneshot(
                    Request::builder()
                        .method("GET")
                        .uri("/v1/models")
                        .header("authorization", format!("Bearer {token}"))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let payload = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap();
            serde_json::from_slice::<serde_json::Value>(&payload).unwrap()
        }
    };

    // A scoped key sees exactly its allowlist — one entry, the one it may call.
    let scoped = listing("vk_scoped").await;
    let ids: Vec<&str> = scoped["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec!["gpt-only-this"]);
    assert_eq!(scoped["object"], "list");

    // An unscoped key is NOT narrowed: absent allowlist means the upstream's own catalog, and
    // it must not pick up the other key's single entry.
    let open = listing("vk_open").await;
    let open_ids: Vec<&str> = open["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_str().unwrap())
        .collect();
    assert_ne!(
        open_ids, ids,
        "an unscoped key inherited a scoped key's narrowing"
    );
    assert!(
        !open_ids.contains(&"gpt-only-this"),
        "the allowlist-only entry leaked into an unscoped key's listing: {open_ids:?}"
    );

    // Discovery is authenticated: it reveals which models a credential may use.
    let app = build_app(Arc::clone(&state));
    let anon = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/v1/models")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(anon.status(), StatusCode::UNAUTHORIZED);
}

/// TD-0010 D2: a failure on a VENDOR path must be parseable by that vendor's SDK. These are the
/// paths a client actually hits, so a bare `{"error":"<string>"}` there defeats the client's own
/// error handling — the operator API (`/admin/*`, `/dashboard/api/*`) keeps its flat shape on
/// purpose, since no vendor SDK reads it.
#[tokio::test]
async fn upstream_failures_are_rendered_in_the_callers_dialect() {
    // A key bound to an upstream that was never registered: the 502 path every dialect shares.
    let keys = KeyStore::new();
    for id in ["vk_openai", "vk_anthropic", "vk_gemini"] {
        keys.insert(VirtualKey {
            id: id.into(),
            subject_id: Some("alice".into()),
            group_id: Some("platform".into()),
            upstream_ref: "nonexistent".into(),
            ..Default::default()
        });
    }
    let state = Arc::new(ProxyState::new(
        keys,
        ProxyLedger::in_memory(),
        Arc::new(InMemorySink::default()),
        HashMap::new(),
        None,
    ));

    for (uri, header, token, body) in [
        (
            "/v1/chat/completions",
            "authorization",
            "Bearer vk_openai",
            r#"{"model":"m","messages":[]}"#,
        ),
        ("/v1/messages", "x-api-key", "vk_anthropic", ANTHROPIC_BODY),
        (
            "/v1beta/models/m:generateContent",
            "x-goog-api-key",
            "vk_gemini",
            r#"{"contents":[{"role":"user","parts":[{"text":"hi"}]}]}"#,
        ),
    ] {
        let app = build_app(Arc::clone(&state));
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(uri)
                    .header("content-type", "application/json")
                    .header(header, token)
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY, "{uri}");
        let payload = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let value: serde_json::Value = serde_json::from_slice(&payload).unwrap();

        // Structured in every dialect — never a bare string.
        assert!(
            value["error"].is_object(),
            "{uri}: expected a structured error, got {value}"
        );
        match uri {
            "/v1/messages" => assert_eq!(value["type"], "error", "{uri}"),
            // Google's shape: numeric code + canonical status name.
            "/v1beta/models/m:generateContent" => {
                assert_eq!(value["error"]["code"], 502, "{uri}");
                assert_eq!(value["error"]["status"], "INTERNAL", "{uri}");
            }
            _ => {}
        }
    }
}

/// TD-0011 P2: `/metrics` serves the registry, gated exactly like the dashboard, and never
/// carries an unbounded label. The unit tests in `metrics.rs` cover rendering; this covers the
/// wiring — that a real request lands in the registry and that the endpoint's gate is the
/// dashboard's, not a second policy.
#[tokio::test]
async fn metrics_endpoint_reflects_traffic_and_is_gated() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id":"c1","object":"chat.completion","created":1,"model":"gpt-test",
            "choices":[{"index":0,"message":{"role":"assistant","content":"hi"},"finish_reason":"stop"}],
            "usage":{"prompt_tokens":100,"completion_tokens":20,"total_tokens":120,
                     "prompt_tokens_details":{"cached_tokens":60}}
        })))
        .mount(&upstream)
        .await;

    let sink = Arc::new(InMemorySink::default());
    let state = state_with(upstream.uri(), Arc::clone(&sink), ProxyLedger::in_memory());

    // Drive one call so the registry has something in it.
    let app = build_app(Arc::clone(&state));
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/chat/completions")
                .header("content-type", "application/json")
                .header("authorization", "Bearer vk_demo")
                .body(Body::from(
                    r#"{"model":"gpt-test","messages":[{"role":"user","content":"hi"}]}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    // No admin token configured on this state, so the gate is open (same rule as the dashboard).
    let app = build_app(Arc::clone(&state));
    let metrics = app
        .oneshot(
            Request::builder()
                .uri("/metrics")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(metrics.status(), StatusCode::OK);
    let body = axum::body::to_bytes(metrics.into_body(), usize::MAX)
        .await
        .unwrap();
    let text = String::from_utf8(body.to_vec()).unwrap();

    // The call is counted, with the transparent plane (OpenAI ingress -> OpenAI upstream).
    assert!(text.contains("sandhi_requests_total{"), "{text}");
    assert!(text.contains("plane=\"transparent\""), "{text}");
    assert!(text.contains("dialect=\"openai\""), "{text}");
    // Tokens are recorded per kind, and `billable` is the settled 40 fresh + 60 cache-read + 20 out.
    assert!(text.contains("kind=\"cache_read\"} 60"), "{text}");
    assert!(text.contains("kind=\"billable\"} 120"), "{text}");

    // TD-0011 D2: nothing unbounded, ever.
    for forbidden in [
        "subject_id",
        "session_id",
        "virtual_key_id",
        "vk_demo",
        "alice",
    ] {
        assert!(
            !text.contains(forbidden),
            "'{forbidden}' leaked into /metrics:\n{text}"
        );
    }

    // With an admin token configured the endpoint requires it — the dashboard's gate (D5).
    let mut gated = ProxyState::new(
        KeyStore::new(),
        ProxyLedger::in_memory(),
        Arc::new(InMemorySink::default()),
        HashMap::new(),
        None,
    );
    gated.admin_token = Some("admin-secret".into());
    let gated = Arc::new(gated);

    let app = build_app(Arc::clone(&gated));
    let anon = app
        .oneshot(
            Request::builder()
                .uri("/metrics")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(anon.status(), StatusCode::UNAUTHORIZED);

    let app = build_app(Arc::clone(&gated));
    let authed = app
        .oneshot(
            Request::builder()
                .uri("/metrics")
                .header("authorization", "Bearer admin-secret")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(authed.status(), StatusCode::OK);
}

/// TD-0012 P1: the limit an operator sets is actually enforced, and a refused call costs nothing.
///
/// The unit tests in `ratelimit.rs` cover the bucket arithmetic; this covers the wiring — that the
/// stored `rate_limit_per_min` reaches the request path at all (it did not, for three releases),
/// that the refusal is dialect-shaped with `Retry-After`, and that a throttled request consumes no
/// lease and emits no usage event.
#[tokio::test]
async fn rate_limited_requests_are_refused_without_consuming_budget() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id":"c1","object":"chat.completion","created":1,"model":"gpt-test",
            "choices":[{"index":0,"message":{"role":"assistant","content":"hi"},"finish_reason":"stop"}],
            "usage":{"prompt_tokens":10,"completion_tokens":5,"total_tokens":15}
        })))
        .mount(&upstream)
        .await;

    let keys = KeyStore::new();
    keys.insert(VirtualKey {
        id: "vk_slow".into(),
        subject_id: Some("alice".into()),
        group_id: Some("platform".into()),
        upstream_ref: "up1".into(),
        // Two requests per minute: small enough to exhaust deterministically in a test.
        rate_limit_per_min: Some(2),
        ..Default::default()
    });
    let mut providers: HashMap<String, ProviderHandle> = HashMap::new();
    providers.insert(
        "up1".into(),
        ProviderRuntime::new().openai_compat(
            "openai",
            upstream.uri(),
            "REAL-KEY",
            Default::default(),
            None,
            None,
            None,
        ),
    );
    let sink = Arc::new(InMemorySink::default());
    // Coerce to the trait object the state holds, keeping the concrete handle for assertions.
    let sink_for_state: Arc<dyn sandhi_core::Sink> = sink.clone();
    let state = Arc::new(ProxyState::new(
        keys,
        ProxyLedger::in_memory(),
        sink_for_state,
        providers,
        None,
    ));

    let call = |state: Arc<ProxyState>| async move {
        build_app(state)
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/chat/completions")
                    .header("content-type", "application/json")
                    .header("authorization", "Bearer vk_slow")
                    .body(Body::from(
                        r#"{"model":"gpt-test","messages":[{"role":"user","content":"hi"}]}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap()
    };

    // The configured burst is admitted.
    for i in 0..2 {
        let response = call(Arc::clone(&state)).await;
        assert_eq!(response.status(), StatusCode::OK, "burst request {i}");
    }

    // The next one is refused — this is the assertion that would have failed for three releases.
    let limited = call(Arc::clone(&state)).await;
    assert_eq!(limited.status(), StatusCode::TOO_MANY_REQUESTS);

    // D4: Retry-After must be present, or a well-behaved SDK retries immediately.
    let retry_after = limited
        .headers()
        .get("retry-after")
        .expect("a 429 without Retry-After turns a throttle into a hot loop")
        .to_str()
        .unwrap()
        .to_string();
    assert!(
        retry_after.parse::<u64>().is_ok_and(|s| s >= 1),
        "Retry-After must be whole seconds, never 0: {retry_after}"
    );

    // The refusal is in the caller's dialect (TD-0010 D2), not a bare string.
    let payload = axum::body::to_bytes(limited.into_body(), usize::MAX)
        .await
        .unwrap();
    let value: serde_json::Value = serde_json::from_slice(&payload).unwrap();
    assert!(
        value["error"].is_object(),
        "expected a structured error: {value}"
    );

    // D5: the throttled call never reached a provider, so it must leave no trace in accounting.
    assert_eq!(
        sink.events().len(),
        2,
        "a rate-limited request must not emit a usage event"
    );
    assert_eq!(
        state.ledger.lock().unwrap().reserved("group:platform"),
        0,
        "a rate-limited request must not hold a lease"
    );

    // And it is visible to an operator (TD-0012 D6) with bounded labels only.
    let metrics = build_app(Arc::clone(&state))
        .oneshot(
            Request::builder()
                .uri("/metrics")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body = axum::body::to_bytes(metrics.into_body(), usize::MAX)
        .await
        .unwrap();
    let text = String::from_utf8(body.to_vec()).unwrap();
    assert!(text.contains("sandhi_rate_limited_total{"), "{text}");
    assert!(text.contains("outcome=\"rate_limited\""), "{text}");
    assert!(
        !text.contains("vk_slow"),
        "the key must not become a label:\n{text}"
    );
}

/// A key with no configured limit is never throttled — an absent limit must not become a block.
#[tokio::test]
async fn a_key_without_a_limit_is_never_rate_limited() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id":"c1","object":"chat.completion","created":1,"model":"gpt-test",
            "choices":[{"index":0,"message":{"role":"assistant","content":"hi"},"finish_reason":"stop"}],
            "usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}
        })))
        .mount(&upstream)
        .await;
    let sink = Arc::new(InMemorySink::default());
    let state = state_with(upstream.uri(), Arc::clone(&sink), ProxyLedger::in_memory());

    for i in 0..25 {
        let response = build_app(Arc::clone(&state))
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/chat/completions")
                    .header("content-type", "application/json")
                    .header("authorization", "Bearer vk_demo")
                    .body(Body::from(
                        r#"{"model":"gpt-test","messages":[{"role":"user","content":"hi"}]}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK, "request {i}");
    }
}

// ---------------------------------------------------------------------------------------------
// TD-0013 — streaming usage fidelity.
//
// The pre-existing disconnect test (`client_disconnect_mid_stream_still_meters`) cannot fail for
// the right reason: wiremock writes the whole SSE body in one go with a `content-length`, so the
// first frame the client reads usually already carries the terminal usage and the estimated-partial
// branch is never reached. These tests need an upstream that emits frames one at a time and then
// *holds the connection open*, so a disconnect genuinely lands mid-stream.
// ---------------------------------------------------------------------------------------------

/// A deliberately **paced** SSE upstream: writes each frame separately, flushes it, and then holds
/// the connection open without ever sending a terminal usage frame.
///
/// Blocking std sockets on a background thread rather than a tokio listener — this needs no extra
/// tokio features and no shutdown choreography, and the thread dies with the test process.
fn paced_sse_upstream(frames: Vec<String>) -> String {
    use std::io::{Read, Write};

    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local addr");
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut sock) = stream else { continue };
            let frames = frames.clone();
            std::thread::spawn(move || {
                // Drain the request head. Its contents do not matter here; the credential
                // substitution it would prove is covered by the wiremock tests above.
                let mut scratch = [0_u8; 8192];
                let _ = sock.read(&mut scratch);
                let _ = sock.write_all(
                    b"HTTP/1.1 200 OK\r\n\
                      content-type: text/event-stream\r\n\
                      transfer-encoding: chunked\r\n\r\n",
                );
                for frame in &frames {
                    let _ = sock.write_all(format!("{:x}\r\n{frame}\r\n", frame.len()).as_bytes());
                    let _ = sock.flush();
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                // Never send the terminating chunk: this stream is abandoned by the *client*.
                std::thread::sleep(std::time::Duration::from_secs(30));
            });
        }
    });
    format!("http://{addr}")
}

/// `message_start` as Anthropic really sends it — input and the full cache split arrive before a
/// single content byte. Mirrors `sandhi-providers/tests/fixtures/anthropic/stream_cache_split.sse`.
const PACED_FRAMES_INPUT: u64 = 1024;
const PACED_FRAMES_CACHE_CREATION: u64 = 2048;
const PACED_FRAMES_CACHE_READ: u64 = 4096;

fn paced_anthropic_frames() -> Vec<String> {
    let message_start = serde_json::json!({
        "type": "message_start",
        "message": {
            "id": "msg_1",
            "model": "claude-test",
            "usage": {
                "input_tokens": PACED_FRAMES_INPUT,
                "cache_creation_input_tokens": PACED_FRAMES_CACHE_CREATION,
                "cache_read_input_tokens": PACED_FRAMES_CACHE_READ,
                // Anthropic seeds output at 1 here; the real count arrives on `message_delta`,
                // which this stream is abandoned before ever reaching.
                "output_tokens": 1
            }
        }
    });
    let text_delta = serde_json::json!({
        "type": "content_block_delta",
        "index": 0,
        "delta": {"type": "text_delta", "text": "hi"}
    });
    vec![
        format!("event: message_start\ndata: {message_start}\n\n"),
        format!("event: content_block_delta\ndata: {text_delta}\n\n"),
    ]
}

fn paced_anthropic_state(upstream_uri: String, sink: Arc<InMemorySink>) -> Arc<ProxyState> {
    let keys = KeyStore::new();
    keys.insert(VirtualKey {
        id: "vk_demo".into(),
        subject_id: Some("alice".into()),
        group_id: Some("platform".into()),
        upstream_ref: "up1".into(),
        ..Default::default()
    });
    let mut providers = HashMap::new();
    providers.insert(
        "up1".into(),
        ProviderRuntime::new().anthropic(
            upstream_uri,
            "REAL-KEY",
            sandhi_providers::AnthropicAuthScheme::ApiKey,
            Some(0),
            None,
            None,
        ),
    );
    Arc::new(ProxyState::new(
        keys,
        ProxyLedger::in_memory(),
        sink,
        providers,
        None,
    ))
}

/// **The test TD-0013 exists to pass.** A client disconnects right after `message_start`, which
/// announced 1024 input + 2048 cache-creation + 4096 cache-read. Those 7168 tokens are real,
/// provider-reported, and known before any content streams — and the byte-only fallback recorded
/// every one of them as zero, settling the `bytes/4` of two characters of text instead.
///
/// Reverting the `usage_running` plumbing makes this assert ~1 against an expected 7168.
#[tokio::test]
async fn a_disconnect_after_message_start_settles_the_reported_cache_split() {
    use futures_util::StreamExt;

    let uri = paced_sse_upstream(paced_anthropic_frames());
    let sink = Arc::new(InMemorySink::new());
    let state = paced_anthropic_state(uri, sink.clone());
    let app = build_app(state.clone());

    let req = Request::builder()
        .method("POST")
        .uri("/v1/messages")
        .header("x-api-key", "vk_demo")
        .body(Body::from(
            r#"{"model":"claude-test","max_tokens":32,"stream":true,"messages":[{"role":"user","content":"hi"}]}"#,
        ))
        .unwrap();

    let response = app.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    // Read until the frame carrying `message_start` has reached us. The proxy observes usage
    // before it forwards the bytes, so once we hold this frame the accounting has already seen it.
    let mut body = response.into_body().into_data_stream();
    let mut seen = String::new();
    for _ in 0..4 {
        match body.next().await {
            Some(Ok(bytes)) => seen.push_str(&String::from_utf8_lossy(&bytes)),
            _ => break,
        }
        if seen.contains("message_start") {
            break;
        }
    }
    assert!(
        seen.contains("message_start"),
        "the paced upstream should have delivered message_start; got: {seen}"
    );

    // The disconnect. Dropping the body drops the stream generator, which drops
    // `RequestAccounting`, which finalizes.
    drop(body);

    let expected = PACED_FRAMES_INPUT + PACED_FRAMES_CACHE_CREATION + PACED_FRAMES_CACHE_READ;
    let spent = state.ledger.lock().unwrap().spent("group:platform");
    assert!(
        spent >= expected,
        "a disconnect after message_start must settle the reported {expected} tokens, not a byte \
         guess — settled {spent}"
    );

    // And the event must still say it is an interrupted call, not a completed one — and must
    // admit that its output number came from the byte fallback (TD-0013 D5).
    let events = sink.events();
    assert_eq!(events.len(), 1, "the disconnect must still emit one event");
    assert_eq!(
        events[0].usage_completeness,
        sandhi_core::UsageCompleteness::Partial
    );
    assert_eq!(
        events[0].usage_basis,
        sandhi_core::UsageBasis::Estimated,
        "input and cache are real here, but output was estimated — the call must not present as \
         a clean measurement"
    );
    assert_eq!(events[0].tokens_in, PACED_FRAMES_INPUT);
    assert_eq!(events[0].cache_read_tokens, PACED_FRAMES_CACHE_READ);
    assert_eq!(events[0].cache_creation_tokens, PACED_FRAMES_CACHE_CREATION);
}

/// The second integrity hole TD-0013 closes, at the layer where it costs money.
///
/// When the sniffer never matches — an upstream that ignores `stream_options.include_usage`, a
/// proxy in front of it that strips the frame, or a usage frame past the sniff budget — the
/// terminal item used to carry an all-zero *finalized* usage. That overwrote the accrued running
/// estimate, so a fully-delivered response settled `0`: a metering hole that needs no disconnect
/// and no bad intent to trigger.
#[tokio::test]
async fn a_stream_whose_usage_is_never_reported_still_settles_what_it_accrued() {
    let upstream = MockServer::start().await;
    let sse = concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\"a fully delivered response\"}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"content\":\" with no usage frame at all\"}}]}\n\n",
        "data: [DONE]\n\n"
    );
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(sse),
        )
        .mount(&upstream)
        .await;

    let sink = Arc::new(InMemorySink::new());
    let state = state_with(upstream.uri(), sink.clone(), ProxyLedger::in_memory());
    let app = build_app(state.clone());

    let req = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("authorization", "Bearer vk_demo")
        .body(Body::from(
            r#"{"model":"gpt-x","messages":[],"stream":true}"#,
        ))
        .unwrap();

    let response = app.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    assert!(
        String::from_utf8_lossy(&body).contains("[DONE]"),
        "forwarded"
    );

    let spent = state.ledger.lock().unwrap().spent("group:platform");
    assert!(
        spent > 0,
        "a delivered response whose usage was never reported must still settle the accrued \
         estimate — settling 0 is a metering hole that needs no disconnect to exploit"
    );

    // And it must be labelled an estimate, not passed off as a measured call.
    let events = sink.events();
    assert_eq!(events.len(), 1);
    assert_eq!(
        events[0].usage_completeness,
        sandhi_core::UsageCompleteness::Partial,
        "an unreported stream is not a finalized measurement"
    );
    assert_eq!(
        events[0].usage_basis,
        sandhi_core::UsageBasis::Estimated,
        "nothing here was measured — the event must say so"
    );

    // The operator-facing signal: how much settled spend was guessed (TD-0013 P3).
    let metrics = state.metrics.render();
    assert!(
        metrics.contains("sandhi_estimated_tokens_total{"),
        "an operator must be able to measure estimated spend, not just find it per-event"
    );
    for forbidden in [
        "subject_id",
        "session_id",
        "virtual_key_id",
        "vk_demo",
        "alice",
    ] {
        assert!(
            !metrics.contains(forbidden),
            "{forbidden} must never become a metric label (TD-0011 D2)"
        );
    }
}
