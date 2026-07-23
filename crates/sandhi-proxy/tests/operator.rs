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
use sandhi_proxy::{build_app, rehydrate_alerts, ProxyState};
use sandhi_store::{AlertStore, SqliteStore, VaultStore, VirtualKeyStore};

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
    // P2: wire the alert store + a live registry (rehydrated; the tokio test runtime backs the
    // webhook transport so best-effort webhook rules can be exercised end-to-end).
    let alert_store = Arc::new(AlertStore::in_memory().unwrap());
    let registry = rehydrate_alerts(&alert_store);
    state.alert_store = Some(alert_store);
    state.alerts = Some(Arc::new(std::sync::Mutex::new(registry)));
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

// --- P2: budget windows, warn policy, alerts --------------------------------

/// Mount a wiremock upstream that returns the given token usage.
async fn mount_usage_upstream(server: &MockServer, prompt: u64, completion: u64) {
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "choices": [{ "message": { "content": "hi" } }],
            "usage": { "prompt_tokens": prompt, "completion_tokens": completion }
        })))
        .mount(server)
        .await;
}

/// Set up an upstream + a minted key (budget scope `group:platform`) + a budget on that scope.
async fn scoped_setup(
    _state: &Arc<ProxyState>,
    app: &axum::Router,
    upstream_uri: &str,
    limit_tokens: u64,
    window: &str,
    policy: &str,
    alert: Option<u8>,
) -> String {
    add_upstream(app, "openai", upstream_uri, "REAL-KEY").await;
    let (secret, _id) = mint_key(app, "openai:default", None).await;
    let mut body = json!({
        "scope": "group:platform",
        "limit_tokens": limit_tokens,
        "window": window,
        "policy": policy,
    });
    if let Some(pct) = alert {
        body["alert_thresholds"] = json!([pct]);
    }
    let r = app
        .clone()
        .oneshot(req("POST", "/admin/budget", Some(TOKEN), Some(body)))
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK, "budget set must succeed");
    secret
}

#[tokio::test]
async fn warn_policy_allows_over_limit_and_block_rejects() {
    // Warn: a request whose projected spend exceeds the cap is still forwarded.
    let upstream = MockServer::start().await;
    mount_usage_upstream(&upstream, 3, 2).await; // 5 tokens
    let state = admin_state();
    let app = build_app(state.clone());
    let secret = scoped_setup(&state, &app, &upstream.uri(), 10, "total", "warn", None).await;

    // Pre-record near the cap so the reservation tips over (10 + reservation > 10).
    state.ledger.lock().unwrap().record("group:platform", 10);
    let (status, _body) = v1_request(&app, &secret).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "warn policy must allow an over-cap request"
    );
    // The measured usage was recorded despite the over-cap reservation.
    assert!(state.ledger.lock().unwrap().spent("group:platform") > 10);

    // Block: same setup, same pre-exhaust → 429.
    let upstream2 = MockServer::start().await;
    let state2 = admin_state();
    let app2 = build_app(state2.clone());
    let secret2 = scoped_setup(&state2, &app2, &upstream2.uri(), 10, "total", "block", None).await;
    state2.ledger.lock().unwrap().record("group:platform", 10);
    let (status2, _) = v1_request(&app2, &secret2).await;
    assert_eq!(status2, StatusCode::TOO_MANY_REQUESTS);
}

#[tokio::test]
async fn budget_set_with_window_policy_and_alert_thresholds_creates_rules() {
    let state = admin_state();
    let app = build_app(state.clone());

    let r = app
        .clone()
        .oneshot(req(
            "POST",
            "/admin/budget",
            Some(TOKEN),
            Some(json!({
                "scope": "group:platform",
                "limit_tokens": 1000,
                "window": "daily",
                "policy": "warn",
                "alert_thresholds": [80, 100],
            })),
        ))
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    let v = body_json(r).await;
    assert_eq!(v["window"], "daily");
    assert_eq!(v["policy"], "warn");
    let created = v["alerts_created"].as_array().unwrap();
    assert_eq!(created.len(), 2);

    // The ledger carries the window + policy.
    assert_eq!(
        state.ledger.lock().unwrap().window_of("group:platform"),
        sandhi_core::Window::Daily
    );
    assert_eq!(
        state.ledger.lock().unwrap().policy_of("group:platform"),
        sandhi_core::Policy::Warn
    );

    // Rules appear in /admin/alerts.
    let r = app
        .oneshot(req("GET", "/admin/alerts", Some(TOKEN), None))
        .await
        .unwrap();
    let list = body_json(r).await;
    assert_eq!(list["alerts"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn alert_fires_when_threshold_crossed_and_marks_last_fired_at() {
    let upstream = MockServer::start().await;
    mount_usage_upstream(&upstream, 50, 35).await; // 85 tokens
    let state = admin_state();
    let app = build_app(state.clone());
    // limit 100, warn policy (so the call is admitted), alert at 80%.
    let secret = scoped_setup(
        &state,
        &app,
        &upstream.uri(),
        100,
        "total",
        "warn",
        Some(80),
    )
    .await;

    let (status, _) = v1_request(&app, &secret).await;
    assert_eq!(status, StatusCode::OK);
    // 85 spent >= 80% of 100 → the rule fired.
    let r = app
        .clone()
        .oneshot(req("GET", "/admin/alerts", Some(TOKEN), None))
        .await
        .unwrap();
    let list = body_json(r).await;
    let rule = &list["alerts"][0];
    assert_eq!(rule["threshold_pct"], 80);
    assert!(
        rule["last_fired_at"].as_str().is_some(),
        "alert must have fired (last_fired_at set)"
    );
}

#[tokio::test]
async fn webhook_alert_failure_does_not_break_the_request() {
    // A webhook pointed at a non-listening endpoint: the POST fails, but best-effort delivery
    // must never break the request.
    let upstream = MockServer::start().await;
    mount_usage_upstream(&upstream, 50, 35).await; // 85 tokens
    let state = admin_state();
    let app = build_app(state.clone());
    let secret = scoped_setup(&state, &app, &upstream.uri(), 100, "total", "warn", None).await;

    // Create a webhook-channel rule pointing at an unreachable URL.
    let r = app
        .clone()
        .oneshot(req(
            "POST",
            "/admin/alerts",
            Some(TOKEN),
            Some(json!({
                "scope": "group:platform",
                "threshold_pct": 50,
                "webhook_url": "http://127.0.0.1:1/nonexistent-endpoint",
            })),
        ))
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::CREATED);

    // The request crosses the 50% threshold → webhook fires (and fails) → request still 200.
    let (status, _) = v1_request(&app, &secret).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "a webhook failure must never break the request"
    );
}

#[tokio::test]
async fn alerts_list_create_ack_delete_endpoints() {
    let app = build_app(admin_state());

    // Create.
    let r = app
        .clone()
        .oneshot(req(
            "POST",
            "/admin/alerts",
            Some(TOKEN),
            Some(json!({ "scope": "group:x", "threshold_pct": 90, "channel": "log" })),
        ))
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::CREATED);
    let created = body_json(r).await;
    let id = created["id"].as_str().unwrap().to_string();
    assert!(id.starts_with("alert_"));

    // List (filtered).
    let r = app
        .clone()
        .oneshot(req("GET", "/admin/alerts?scope=group:x", Some(TOKEN), None))
        .await
        .unwrap();
    let list = body_json(r).await;
    assert_eq!(list["alerts"].as_array().unwrap().len(), 1);

    // Ack.
    let r = app
        .clone()
        .oneshot(req(
            "POST",
            &format!("/admin/alerts/{id}/ack"),
            Some(TOKEN),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(body_json(r).await["acked"], true);

    // Delete.
    let r = app
        .oneshot(req(
            "DELETE",
            &format!("/admin/alerts/{id}"),
            Some(TOKEN),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(body_json(r).await["deleted"], true);
}

// Silence unused import when an upstream body is unused.
#[allow(dead_code)]
fn _touch(_: Bytes) {}
