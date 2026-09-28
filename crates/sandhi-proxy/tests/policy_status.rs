use axum::{
    body::{to_bytes, Body},
    http::{Request, StatusCode},
};
use sandhi_core::{policy::Engine, InMemorySink, KeyStore};
use sandhi_proxy::{build_app, policy::PolicyGate, ProxyLedger, ProxyState};
use sandhi_store::policy::PolicyAuditStore;
use serde_json::{json, Value};
use std::{collections::HashMap, sync::Arc};
use tower::ServiceExt;

#[tokio::test]
async fn policy_status_is_protected_redacted_and_reports_durable_capacity() {
    let temp = tempfile::tempdir().unwrap();
    let audit = Arc::new(PolicyAuditStore::open(&temp.path().join("audit.db"), 10).unwrap());
    let engine = Engine::from_slice(
        &serde_json::to_vec(&json!({
            "schema_version":"1", "revision":17, "deadline_ms":200, "max_body_bytes":65536,
            "rules":[{"id":"secret-rule","effect":"audit", "when":{"subjects":["private-subject"]},
                      "evaluator":{"kind":"regex", "pattern":"private-pattern"}}]
        }))
        .unwrap(),
    )
    .unwrap();
    let gate = Arc::new(PolicyGate::new(engine, audit));
    gate.check(
        bytes::Bytes::from_static(br#"{"model":"test","messages":[]}"#),
        Default::default(),
        "private-key-id".into(),
        "openai".into(),
        "test".into(),
        true,
    )
    .await
    .unwrap();
    let mut state = ProxyState::new(
        KeyStore::new(),
        ProxyLedger::in_memory(),
        Arc::new(InMemorySink::new()),
        HashMap::new(),
        None,
    );
    state.admin_token = Some("test-admin".into());
    state.dashboard_public = true;
    state.policy = Some(gate);
    let app = build_app(Arc::new(state));
    let denied = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/admin/policy")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(denied.status(), StatusCode::UNAUTHORIZED);
    let response = app
        .oneshot(
            Request::builder()
                .uri("/admin/policy")
                .header("authorization", "Bearer test-admin")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), 65536).await.unwrap();
    let text = std::str::from_utf8(&body).unwrap();
    for secret in ["private-subject", "private-key-id", "private-pattern"] {
        assert!(!text.contains(secret));
    }
    let status: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(status["enabled"], true);
    assert_eq!(status["revision"], 17);
    assert_eq!(status["receipts"]["used"], 1);
    assert_eq!(status["receipts"]["capacity"], 10);
    assert_eq!(status["rules"][0]["effect"], "audit");
    assert_eq!(status["rules"][0]["evaluator"], "regex");
}

#[tokio::test]
async fn unconfigured_policy_is_explicit() {
    let mut state = ProxyState::new(
        KeyStore::new(),
        ProxyLedger::in_memory(),
        Arc::new(InMemorySink::new()),
        HashMap::new(),
        None,
    );
    state.admin_token = Some("test-admin".into());
    let response = build_app(Arc::new(state))
        .oneshot(
            Request::builder()
                .uri("/admin/policy")
                .header("authorization", "Bearer test-admin")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), 65536).await.unwrap()).unwrap();
    assert_eq!(body["enabled"], false);
}
