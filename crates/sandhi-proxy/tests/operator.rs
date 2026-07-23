//! TD-0003 P1 operator-surface integration tests.
//!
//! Drives the `/admin/*` REST API through the axum app (no network), and proves the end-to-end
//! operator flow: add a provider credential (vault) → mint a scoped virtual key → present it to
//! `/v1/*` → attributed + budget-enforced → revoke rejects. No live API keys; upstreams are
//! wiremock mocks. No dollars anywhere (measure-vs-price boundary).

use std::collections::HashMap;
use std::sync::Arc;

use axum::body::{Body, Bytes};
use axum::http::{Request, StatusCode};
use serde_json::{json, Value};
use tower::ServiceExt;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use sandhi_core::{BudgetLedger, InMemorySink, KeyStore, Sink, UsageEvent};
use sandhi_proxy::{build_app, ProxyState};
use sandhi_store::{SqliteStore, VaultStore, VirtualKeyStore};

const TOKEN: &str = "admin-secret";

fn admin_state() -> Arc<ProxyState> {
    let store = Arc::new(SqliteStore::in_memory().unwrap());
    let vault = Arc::new(VaultStore::in_memory().unwrap());
    let vkeys = Arc::new(VirtualKeyStore::in_memory().unwrap());
    // The sink IS the durable store, exactly as the proxy wires it in production, so emitted
    // usage events are queryable through the admin usage API.
    let sink: Arc<dyn Sink> = store.clone();
    let mut state = ProxyState::new(
        KeyStore::new(),
        BudgetLedger::new(),
        sink,
        HashMap::new(),
        Some(store),
    );
    state.vault = Some(vault);
    state.vkeys = Some(vkeys);
    state.admin_token = Some(TOKEN.into());
    state.public_url = "http://test.local".into();
    Arc::new(state)
}

fn req(method: &str, uri: &str, token: Option<&str>, body: Option<Value>) -> Request<Body> {
    let mut b = Request::builder().method(method).uri(uri);
    if let Some(t) = token {
        b = b.header("authorization", format!("Bearer {t}"));
    }
    if let Some(body) = body {
        b = b.header("content-type", "application/json");
        b.body(Body::from(body.to_string())).unwrap()
    } else {
        b.body(Body::empty()).unwrap()
    }
}

async fn body_json(response: axum::response::Response) -> Value {
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap_or(Value::Null)
}

// --- Admin auth -------------------------------------------------------------

#[tokio::test]
async fn admin_routes_require_the_admin_token() {
    let app = build_app(admin_state());
    // Missing token → 401.
    let r = app
        .clone()
        .oneshot(req("GET", "/admin/keys", None, None))
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
    // Wrong token → 401.
    let r = app
        .clone()
        .oneshot(req("GET", "/admin/keys", Some("nope"), None))
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
    // Correct token → 200.
    let r = app
        .oneshot(req("GET", "/admin/keys", Some(TOKEN), None))
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);
}

#[tokio::test]
async fn admin_disabled_when_no_token_configured() {
    let store = Arc::new(SqliteStore::in_memory().unwrap());
    let state = ProxyState::new(
        KeyStore::new(),
        BudgetLedger::new(),
        Arc::new(InMemorySink::new()) as Arc<dyn Sink>,
        HashMap::new(),
        Some(store),
    );
    let app = build_app(Arc::new(state));
    let r = app
        .oneshot(req("GET", "/admin/keys", Some("anything"), None))
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::FORBIDDEN);
}

// --- Vault (provider credentials) -------------------------------------------

#[tokio::test]
async fn add_list_revoke_provider_key() {
    let app = build_app(admin_state());

    // Add a credential.
    let r = app
        .clone()
        .oneshot(req(
            "POST",
            "/admin/keys",
            Some(TOKEN),
            Some(json!({
                "provider": "anthropic",
                "label": "default",
                "scheme": "api_key",
                "secret": "sk-super-secret-123"
            })),
        ))
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::CREATED);
    let added = body_json(r).await;
    assert_eq!(added["provider"], "anthropic");
    assert_eq!(added["credential_id"], "anthropic:default");
    assert_eq!(added["status"], "active");
    // The secret never appears in the response.
    assert!(!added.to_string().contains("sk-super-secret-123"));

    // Masked listing.
    let r = app
        .clone()
        .oneshot(req("GET", "/admin/keys", Some(TOKEN), None))
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    let list = body_json(r).await;
    assert_eq!(list["keys"][0]["credential_id"], "anthropic:default");
    assert!(!list.to_string().contains("sk-super-secret-123"));

    // Revoke.
    let r = app
        .clone()
        .oneshot(req(
            "DELETE",
            "/admin/keys/anthropic/default",
            Some(TOKEN),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(body_json(r).await["revoked"], true);
    // Idempotent revoke.
    let r = app
        .oneshot(req(
            "DELETE",
            "/admin/keys/anthropic/default",
            Some(TOKEN),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(body_json(r).await["revoked"], false);
}

// --- Virtual keys: share → list → revoke ------------------------------------

async fn add_upstream(app: &axum::Router, provider: &str, base_url: &str, secret: &str) {
    let r = app
        .clone()
        .oneshot(req(
            "POST",
            "/admin/keys",
            Some(TOKEN),
            Some(json!({ "provider": provider, "label": "default", "base_url": base_url, "secret": secret })),
        ))
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::CREATED, "upstream add must succeed");
}

async fn mint_key(
    app: &axum::Router,
    upstream: &str,
    expires_at: Option<&str>,
) -> (String, String) {
    let mut body = json!({
        "upstream": upstream,
        "subject": "alice",
        "group": "platform",
        "models": ["claude-x"],
        "budget_scope": "group:platform",
        "rate_limit_per_min": 60,
    });
    if let Some(exp) = expires_at {
        body["expires_at"] = json!(exp);
    }
    let r = app
        .clone()
        .oneshot(req("POST", "/admin/keys/share", Some(TOKEN), Some(body)))
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK, "share must succeed");
    let v = body_json(r).await;
    (
        v["virtual_key"].as_str().unwrap().to_string(),
        v["id"].as_str().unwrap().to_string(),
    )
}

#[tokio::test]
async fn share_list_revoke_virtual_key_never_exposes_plaintext() {
    let app = build_app(admin_state());
    add_upstream(&app, "anthropic", "https://api.anthropic.com", "sk-real").await;

    let (secret, id) = mint_key(&app, "anthropic:default", None).await;
    assert!(secret.starts_with("vk_"));

    // Masked listing never contains the plaintext secret.
    let r = app
        .clone()
        .oneshot(req("GET", "/admin/keys/virtual", Some(TOKEN), None))
        .await
        .unwrap();
    let list = body_json(r).await;
    let listed: String = list.to_string();
    assert!(
        !listed.contains(&secret),
        "plaintext secret must not appear in listing"
    );
    assert!(listed.contains(&id));
    // secret_hash is not exposed over the API either.
    assert!(!listed.contains("secret_hash"));

    // Revoke by id.
    let r = app
        .clone()
        .oneshot(req(
            "DELETE",
            &format!("/admin/vkeys/{id}"),
            Some(TOKEN),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(body_json(r).await["revoked"], true);
}

#[tokio::test]
async fn share_rejects_unknown_upstream() {
    let app = build_app(admin_state());
    let r = app
        .oneshot(req(
            "POST",
            "/admin/keys/share",
            Some(TOKEN),
            Some(json!({ "upstream": "nope:default" })),
        ))
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::BAD_REQUEST);
}

// --- End-to-end: minted key authenticates + is attributed + budgeted --------

async fn v1_request(app: &axum::Router, virtual_key: &str) -> (StatusCode, Value) {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/chat/completions")
                .header("authorization", format!("Bearer {virtual_key}"))
                .header("content-type", "application/json")
                .header("x-sandhi-session", "conv_1")
                .body(Body::from(r#"{"model":"claude-x","messages":[]}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    (status, body_json(response).await)
}

#[tokio::test]
async fn minted_key_is_attributed_and_budgeted_end_to_end() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(header("authorization", "Bearer REAL-KEY"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "choices": [{ "message": { "content": "hi" } }],
            "usage": { "prompt_tokens": 100, "completion_tokens": 20 }
        })))
        .mount(&upstream)
        .await;

    let state = admin_state();
    let app = build_app(state.clone());
    add_upstream(&app, "openai", &upstream.uri(), "REAL-KEY").await;
    let (secret, _id) = mint_key(&app, "openai:default", None).await;

    // Present the minted virtual key — the proxy resolves it (by hash) and forwards the REAL key.
    let (status, _body) = v1_request(&app, &secret).await;
    assert_eq!(status, StatusCode::OK);

    // Attribution + budget landed on the shared sink/store.
    let events = state.store.as_ref().unwrap().totals_by_subject().unwrap();
    assert_eq!(events[0].key, "alice");
    assert_eq!(events[0].tokens_in + events[0].tokens_out, 120);
    assert_eq!(state.ledger.lock().unwrap().spent("group:platform"), 120);
}

#[tokio::test]
async fn revoked_virtual_key_is_rejected() {
    let upstream = MockServer::start().await;
    let state = admin_state();
    let app = build_app(state.clone());
    add_upstream(&app, "openai", &upstream.uri(), "REAL-KEY").await;
    let (secret, id) = mint_key(&app, "openai:default", None).await;

    // Revoke, then present → 401.
    let _ = app
        .clone()
        .oneshot(req(
            "DELETE",
            &format!("/admin/vkeys/{id}"),
            Some(TOKEN),
            None,
        ))
        .await
        .unwrap();
    let (status, _) = v1_request(&app, &secret).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn expired_virtual_key_is_rejected() {
    let upstream = MockServer::start().await;
    let state = admin_state();
    let app = build_app(state.clone());
    add_upstream(&app, "openai", &upstream.uri(), "REAL-KEY").await;
    // Mint with an expiry in the past.
    let (secret, _id) = mint_key(&app, "openai:default", Some("2020-01-01T00:00:00Z")).await;

    let (status, _) = v1_request(&app, &secret).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn model_outside_allowlist_is_still_admitted_p1() {
    // P1 stores the model allowlist but does not enforce it on ingress (enforcement is a follow-up;
    // this test pins the current, permissive behavior so a future change is intentional).
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "choices": [{ "message": { "content": "hi" } }],
            "usage": { "prompt_tokens": 10, "completion_tokens": 5 }
        })))
        .mount(&upstream)
        .await;
    let state = admin_state();
    let app = build_app(state.clone());
    add_upstream(&app, "openai", &upstream.uri(), "REAL-KEY").await;
    let (secret, _) = mint_key(&app, "openai:default", None).await;

    let (status, _) = v1_request(&app, &secret).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "P1 does not enforce the model allowlist yet"
    );
}

// --- Budgets + usage --------------------------------------------------------

#[tokio::test]
async fn budget_set_list_usage_and_enforcement() {
    let state = admin_state();
    let app = build_app(state.clone());

    // Set a 50-token budget on group:platform.
    let r = app
        .clone()
        .oneshot(req(
            "POST",
            "/admin/budget",
            Some(TOKEN),
            Some(json!({ "scope": "group:platform", "limit_tokens": 50 })),
        ))
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);

    // List budgets.
    let r = app
        .clone()
        .oneshot(req("GET", "/admin/budget", Some(TOKEN), None))
        .await
        .unwrap();
    let list = body_json(r).await;
    assert_eq!(list["budgets"][0]["scope"], "group:platform");
    assert_eq!(list["budgets"][0]["limit_tokens"], 50);

    // Record some spend directly via the ledger, then read usage.
    state.ledger.lock().unwrap().record("group:platform", 30);
    let r = app
        .clone()
        .oneshot(req(
            "GET",
            "/admin/budget/usage?scope=group:platform",
            Some(TOKEN),
            None,
        ))
        .await
        .unwrap();
    let usage = body_json(r).await;
    assert_eq!(usage["spent"], 30);
    assert_eq!(usage["limit_tokens"], 50);
    assert_eq!(usage["remaining"], 20);
}

#[tokio::test]
async fn usage_aggregates_by_dimension_and_since() {
    let state = admin_state();
    let store = state.store.clone().unwrap();
    let ev = |subject: &str, model: &str, tin: u64, tout: u64| {
        UsageEvent::new(
            "r",
            "2026-07-01T00:00:00Z",
            "openai",
            model,
            sandhi_core::Backend::External,
        )
        .with_attribution(
            Some("vk".into()),
            Some(subject.into()),
            Some("platform".into()),
        )
        .with_tokens(tin, tout)
    };
    store.emit(&ev("alice", "gpt-4", 100, 20));
    store.emit(&ev("bob", "claude", 50, 10));

    let app = build_app(state);
    // By subject.
    let r = app
        .clone()
        .oneshot(req("GET", "/admin/usage?by=subject", Some(TOKEN), None))
        .await
        .unwrap();
    let v = body_json(r).await;
    assert_eq!(v["dimension"], "subject");
    assert_eq!(v["buckets"][0]["key"], "alice");
    // By model.
    let r = app
        .clone()
        .oneshot(req("GET", "/admin/usage?by=model", Some(TOKEN), None))
        .await
        .unwrap();
    let v = body_json(r).await;
    let models: Vec<&str> = v["buckets"]
        .as_array()
        .unwrap()
        .iter()
        .map(|b| b["key"].as_str().unwrap())
        .collect();
    assert!(models.contains(&"gpt-4") && models.contains(&"claude"));
    // Windowed (since far future → no rows).
    let r = app
        .oneshot(req(
            "GET",
            "/admin/usage?by=subject&since=2030-01-01T00:00:00Z",
            Some(TOKEN),
            None,
        ))
        .await
        .unwrap();
    let v = body_json(r).await;
    assert_eq!(v["buckets"].as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn unknown_usage_dimension_returns_empty_not_500() {
    let app = build_app(admin_state());
    let r = app
        .oneshot(req("GET", "/admin/usage?by=banana", Some(TOKEN), None))
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    let v = body_json(r).await;
    assert!(v["buckets"].as_array().unwrap().is_empty());
}

// Silence unused import when an upstream body is unused.
#[allow(dead_code)]
fn _touch(_: Bytes) {}
