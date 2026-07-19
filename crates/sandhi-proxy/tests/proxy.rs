//! End-to-end proxy tests: a client hits the proxy with a **virtual key**; the proxy resolves
//! it, budget-checks, forwards to a **wiremock** upstream with the **real** key, streams the
//! response back, and emits a usage event. No live API keys.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt; // oneshot
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use sandhi_core::{Budget, BudgetLedger, InMemorySink, KeyStore, VirtualKey};
use sandhi_providers::{OpenAiCompat, Provider};
use sandhi_proxy::{build_app, ProxyState};

fn state_with(
    upstream_uri: String,
    sink: Arc<InMemorySink>,
    ledger: BudgetLedger,
) -> Arc<ProxyState> {
    let mut keys = KeyStore::new();
    keys.insert(VirtualKey {
        id: "vk_demo".into(),
        subject_id: Some("alice".into()),
        group_id: Some("platform".into()),
        upstream_ref: "up1".into(),
    });
    let mut providers: HashMap<String, Arc<dyn Provider>> = HashMap::new();
    providers.insert(
        "up1".into(),
        Arc::new(OpenAiCompat::new("openai", upstream_uri, "REAL-KEY")),
    );
    Arc::new(ProxyState {
        keys,
        ledger: Mutex::new(ledger),
        sink,
        providers,
    })
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
    let state = state_with(upstream.uri(), sink.clone(), BudgetLedger::new());
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
    assert_eq!(ev.billable_tokens(), 60);

    assert_eq!(state.ledger.lock().unwrap().spent("group:platform"), 60);
}

#[tokio::test]
async fn unknown_virtual_key_is_401() {
    let sink = Arc::new(InMemorySink::new());
    let state = state_with(
        "http://127.0.0.1:1".into(),
        sink.clone(),
        BudgetLedger::new(),
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
    let mut ledger = BudgetLedger::new();
    ledger.set_limit("group:platform", Budget::tokens(10));
    ledger.record("group:platform", 10); // already at the cap

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
    let state = state_with(upstream.uri(), sink.clone(), BudgetLedger::new());
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
    assert_eq!(events[0].billable_tokens(), 11);
    assert_eq!(state.ledger.lock().unwrap().spent("group:platform"), 11);
}
