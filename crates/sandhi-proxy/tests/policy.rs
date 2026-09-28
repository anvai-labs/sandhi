use axum::{
    body::{to_bytes, Body},
    http::{Request, StatusCode},
};
use sandhi_core::{policy::Engine, InMemorySink, KeyStore, VirtualKey};
use sandhi_providers::ProviderRuntime;
use sandhi_proxy::{build_app, policy::PolicyGate, ProxyLedger, ProxyState};
use sandhi_store::policy::PolicyAuditStore;
use serde_json::{json, Value};
use std::{collections::HashMap, sync::Arc};
use tower::ServiceExt;
use wiremock::{matchers::method, Mock, MockServer, ResponseTemplate};

fn policy() -> Vec<u8> {
    serde_json::to_vec(&json!({"schema_version":"1","revision":7,"deadline_ms":500,
        "max_body_bytes":65536,"rules":[
        {"id":"block-secret","effect":"block","evaluator":{"kind":"regex","pattern":"SECRET-[0-9]+"}},
        {"id":"review","effect":"quarantine","evaluator":{"kind":"lexical_similarity","reference":"private customer records","at_least":0.5}},
        {"id":"audit","effect":"audit","when":{"subjects":["alice"]},"evaluator":{"kind":"threshold","metric":"body_bytes","above":0}}
    ]})).unwrap()
}
fn req(body: &[u8]) -> Request<Body> {
    Request::builder()
        .uri("/v1/chat/completions")
        .method("POST")
        .header("authorization", "Bearer client")
        .header("content-type", "application/json")
        .body(Body::from(body.to_vec()))
        .unwrap()
}
fn body(text: &str) -> Vec<u8> {
    serde_json::to_vec(
        &json!({"model":"test","messages":[{"role":"user","content":text}],"max_tokens":10}),
    )
    .unwrap()
}
async fn setup(audit: Arc<PolicyAuditStore>) -> (axum::Router, MockServer) {
    let upstream = MockServer::start().await;
    Mock::given(method("POST")).respond_with(ResponseTemplate::new(200).set_body_json(json!({
        "id":"synthetic","object":"chat.completion","model":"test","created":1,
        "choices":[{"index":0,"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}],
        "usage":{"prompt_tokens":2,"completion_tokens":1,"total_tokens":3}
    }))).mount(&upstream).await;
    let keys = KeyStore::new();
    keys.insert(VirtualKey {
        id: "client".into(),
        subject_id: Some("alice".into()),
        upstream_ref: "openai".into(),
        ..Default::default()
    });
    let provider = ProviderRuntime::new().openai_compat(
        "openai",
        upstream.uri(),
        "synthetic",
        Default::default(),
        None,
        None,
        None,
    );
    let mut state = ProxyState::new(
        keys,
        ProxyLedger::in_memory(),
        Arc::new(InMemorySink::new()),
        HashMap::from([("openai".into(), provider)]),
        None,
    );
    state.policy = Some(Arc::new(PolicyGate::new(
        Engine::from_slice(&policy()).unwrap(),
        audit,
    )));
    (build_app(Arc::new(state)), upstream)
}

#[tokio::test]
async fn policy_denials_hold_zero_provider_calls_and_allowed_body_is_byte_exact() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("audit.db");
    let (app, upstream) = setup(Arc::new(PolicyAuditStore::open(&path, 100).unwrap())).await;
    for (text, code) in [
        ("SECRET-123", "policy_blocked"),
        ("private customer records", "policy_quarantined"),
    ] {
        let r = app.clone().oneshot(req(&body(text))).await.unwrap();
        assert_eq!(r.status(), StatusCode::FORBIDDEN);
        assert!(r.headers().contains_key("x-sandhi-policy-receipt"));
        let bytes = to_bytes(r.into_body(), 8192).await.unwrap();
        assert!(String::from_utf8_lossy(&bytes).contains(code));
        assert!(!String::from_utf8_lossy(&bytes).contains(text));
    }
    assert!(upstream.received_requests().await.unwrap().is_empty());
    let raw=b"{ \"model\":\"test\", \"messages\":[{\"role\":\"user\",\"content\":\"weather\"}], \"max_tokens\":10 }";
    let r = app.clone().oneshot(req(raw)).await.unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    let _ = to_bytes(r.into_body(), 8192).await.unwrap();
    assert_eq!(upstream.received_requests().await.unwrap()[0].body, raw);
    let db = rusqlite::Connection::open(path).unwrap();
    let records: Vec<String> = db
        .prepare("SELECT decision_json FROM policy_receipts")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(records.len(), 3);
    assert!(!records.join("").contains("SECRET-123"));
    assert!(records.iter().any(
        |r| serde_json::from_str::<Value>(r).unwrap()["matched_rules"]
            .as_array()
            .unwrap()
            .contains(&json!("audit"))
    ));
}

#[tokio::test]
async fn policy_required_audit_capacity_and_unsupported_input_fail_closed() {
    let temp = tempfile::tempdir().unwrap();
    let (app, upstream) = setup(Arc::new(
        PolicyAuditStore::open(&temp.path().join("audit.db"), 1).unwrap(),
    ))
    .await;
    let bad=br#"{"model":"test","messages":[{"role":"user","content":[{"type":"image_url","image_url":{"url":"https://example/a"}}]}]}"#;
    let r = app.clone().oneshot(req(bad)).await.unwrap();
    assert_eq!(r.status(), StatusCode::SERVICE_UNAVAILABLE);
    // The failed-inspection receipt filled the durable audit capacity.
    let r = app.clone().oneshot(req(&body("weather"))).await.unwrap();
    assert_eq!(r.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert!(upstream.received_requests().await.unwrap().is_empty());
}
