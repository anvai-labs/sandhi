//! TD-0031 client-credentials grant: integration tests over the axum app (no network).
//! Mirrors tests/operator.rs harness: in-memory stores, wiremock upstream, oneshot requests.

use std::collections::HashMap;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tower::ServiceExt;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use sandhi_core::{KeyStore, Sink};
use sandhi_proxy::client_credentials::{ClientCredentialEntry, ClientCredentialRegistry};
use sandhi_proxy::{build_app, ProxyLedger, ProxyState};
use sandhi_store::{SqliteStore, VaultStore, VirtualKeyStore};

const TOKEN: &str = "admin-secret";
const CLIENT_ID: &str = "victor-mac";
const CLIENT_SECRET: &str = "client-secret-abc";

fn sha256_hex(value: &str) -> String {
    let digest = Sha256::digest(value.as_bytes());
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

fn entry() -> ClientCredentialEntry {
    ClientCredentialEntry {
        client_id: CLIENT_ID.into(),
        secret_hash: sha256_hex(CLIENT_SECRET),
        subject_id: "victor-mac".into(),
        group_id: None,
        upstream_ref: "openai:default".into(),
        models: vec!["claude-x".into()],
        budget_scope: Some("victor:mac".into()),
        rate_limit_per_min: Some(30),
        max_ttl_seconds: 600,
        enabled: true,
    }
}

fn registry_file(entries: &[ClientCredentialEntry]) -> std::path::PathBuf {
    let path = std::env::temp_dir().join(format!(
        "sandhi-cc-test-{}-{:p}",
        std::process::id(),
        entries.as_ptr()
    ));
    std::fs::write(&path, serde_json::to_string(entries).unwrap()).unwrap();
    // Production contract: the registry file must be owner-only.
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    path
}

fn state_with_registry(registry: Option<ClientCredentialRegistry>) -> Arc<ProxyState> {
    let store = Arc::new(SqliteStore::in_memory().unwrap());
    let vault = Arc::new(VaultStore::in_memory().unwrap());
    let vkeys = Arc::new(VirtualKeyStore::in_memory().unwrap());
    let sink: Arc<dyn Sink> = store.clone();
    let mut state = ProxyState::new(
        KeyStore::new(),
        ProxyLedger::in_memory(),
        sink,
        HashMap::new(),
        Some(store),
    );
    state.vault = Some(vault);
    state.vkeys = Some(vkeys);
    state.admin_token = Some(TOKEN.into());
    state.client_credentials = registry.map(Arc::new);
    Arc::new(state)
}

fn state_without_registry() -> Arc<ProxyState> {
    state_with_registry(None)
}

async fn post_json(
    app: &axum::Router,
    uri: &str,
    token: Option<&str>,
    body: Value,
) -> (StatusCode, Value) {
    let mut builder = Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/json");
    if let Some(t) = token {
        builder = builder.header("authorization", format!("Bearer {t}"));
    }
    let response = app
        .clone()
        .oneshot(builder.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

async fn add_upstream(app: &axum::Router, base_url: &str) {
    let (status, _) = post_json(
        app,
        "/admin/keys",
        Some(TOKEN),
        json!({"provider": "openai", "label": "default", "base_url": base_url, "secret": "REAL-KEY"}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "upstream add must succeed");
}

async fn exchange(app: &axum::Router, body: Value) -> (StatusCode, Value) {
    post_json(app, "/auth/token", None, body).await
}

async fn mount_upstream(server: &MockServer) {
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(header("authorization", "Bearer REAL-KEY"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "choices": [{ "message": { "content": "hi" } }],
            "usage": { "prompt_tokens": 10, "completion_tokens": 2 }
        })))
        .mount(server)
        .await;
}

// ── feature gate ─────────────────────────────────────────────────────────────

#[tokio::test]
async fn endpoint_is_404_without_registry() {
    let app = build_app(state_without_registry());
    let (status, body) = exchange(&app, json!({"client_id": "x", "client_secret": "y"})).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"], "client-credentials grant not configured");
}

// ── happy path: exchange → minted vkey authorizes inference ─────────────────

#[tokio::test]
async fn exchange_mints_token_that_authorizes_inference() {
    let upstream = MockServer::start().await;
    mount_upstream(&upstream).await;
    let state = state_with_registry(Some(
        ClientCredentialRegistry::load(&registry_file(&[entry()])).unwrap(),
    ));
    let app = build_app(state.clone());
    add_upstream(&app, &upstream.uri()).await;

    let (status, body) = exchange(
        &app,
        json!({"client_id": CLIENT_ID, "client_secret": CLIENT_SECRET}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(body["token_type"], "bearer");
    let token = body["access_token"]
        .as_str()
        .expect("access_token")
        .to_string();
    assert!(token.starts_with("vk_"), "token is a normal vkey: {token}");

    // The minted token authorizes the model route end-to-end (allowlist holds).
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/chat/completions")
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(r#"{"model":"claude-x","messages":[]}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    // TTL honored from the registry ceiling (entry max_ttl_seconds = 600).
    assert_eq!(body["expires_in"], 600);
}

// ── anti-enumeration + limiter ───────────────────────────────────────────────

#[tokio::test]
async fn unknown_client_and_bad_secret_are_indistinguishable() {
    let app = build_app(state_with_registry(Some(
        ClientCredentialRegistry::load(&registry_file(&[entry()])).unwrap(),
    )));
    let (unknown_status, unknown_body) = exchange(
        &app,
        json!({"client_id": "no-such-client", "client_secret": "whatever"}),
    )
    .await;
    let (bad_status, bad_body) = exchange(
        &app,
        json!({"client_id": CLIENT_ID, "client_secret": "wrong"}),
    )
    .await;
    assert_eq!(unknown_status, StatusCode::UNAUTHORIZED);
    assert_eq!(bad_status, StatusCode::UNAUTHORIZED);
    assert_eq!(unknown_body, bad_body);
}

#[tokio::test]
async fn over_limit_throttles_silently_with_no_registration_oracle() {
    let app = build_app(state_with_registry(Some(
        ClientCredentialRegistry::load(&registry_file(&[entry()])).unwrap(),
    )));
    // 12 attempts per class: past the 10-attempt window the KNOWN id must
    // still return the exact same 401 as the UNKNOWN id — no 429 divergence,
    // so probing cannot reveal whether a client_id is registered.
    let mut known_bodies = Vec::new();
    let mut unknown_bodies = Vec::new();
    for i in 0..12 {
        let (ks, kb) = exchange(
            &app,
            json!({"client_id": CLIENT_ID, "client_secret": format!("wrong-{i}")}),
        )
        .await;
        let (us, ub) = exchange(
            &app,
            json!({"client_id": "no-such-client", "client_secret": format!("wrong-{i}")}),
        )
        .await;
        assert_eq!(ks, StatusCode::UNAUTHORIZED);
        assert_eq!(us, StatusCode::UNAUTHORIZED);
        known_bodies.push(kb);
        unknown_bodies.push(ub);
    }
    assert!(known_bodies.iter().all(|b| *b == known_bodies[0]));
    assert_eq!(known_bodies[0], unknown_bodies[0]);
}

#[tokio::test]
async fn zero_ttl_is_rejected() {
    let upstream = MockServer::start().await;
    mount_upstream(&upstream).await;
    let app = build_app(state_with_registry(Some(
        ClientCredentialRegistry::load(&registry_file(&[entry()])).unwrap(),
    )));
    add_upstream(&app, &upstream.uri()).await;
    let (status, body) = exchange(
        &app,
        json!({"client_id": CLIENT_ID, "client_secret": CLIENT_SECRET, "ttl_seconds": 0}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body["error"].as_str().unwrap().contains("at least 1"));
}

#[tokio::test]
async fn minted_token_stops_working_after_expiry() {
    let upstream = MockServer::start().await;
    mount_upstream(&upstream).await;
    let app = build_app(state_with_registry(Some(
        ClientCredentialRegistry::load(&registry_file(&[entry()])).unwrap(),
    )));
    add_upstream(&app, &upstream.uri()).await;
    let (status, body) = exchange(
        &app,
        json!({"client_id": CLIENT_ID, "client_secret": CLIENT_SECRET, "ttl_seconds": 1}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let token = body["access_token"].as_str().unwrap().to_string();
    tokio::time::sleep(std::time::Duration::from_millis(1300)).await;
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/chat/completions")
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(r#"{"model":"claude-x","messages":[]}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn disabled_client_cannot_mint() {
    let mut e = entry();
    e.enabled = false;
    let app = build_app(state_with_registry(Some(
        ClientCredentialRegistry::load(&registry_file(&[e])).unwrap(),
    )));
    let (status, _) = exchange(
        &app,
        json!({"client_id": CLIENT_ID, "client_secret": CLIENT_SECRET}),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

// ── bounds ───────────────────────────────────────────────────────────────────

#[tokio::test]
async fn ttl_is_clamped_to_registry_max() {
    let upstream = MockServer::start().await;
    mount_upstream(&upstream).await;
    let app = build_app(state_with_registry(Some(
        ClientCredentialRegistry::load(&registry_file(&[entry()])).unwrap(),
    )));
    add_upstream(&app, &upstream.uri()).await;
    let (status, body) = exchange(
        &app,
        json!({"client_id": CLIENT_ID, "client_secret": CLIENT_SECRET, "ttl_seconds": 100_000}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["expires_in"], 600);
}

#[tokio::test]
async fn unregistered_upstream_is_rejected() {
    let app = build_app(state_with_registry(Some(
        ClientCredentialRegistry::load(&registry_file(&[entry()])).unwrap(),
    )));
    let (status, body) = exchange(
        &app,
        json!({"client_id": CLIENT_ID, "client_secret": CLIENT_SECRET}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        body["error"].as_str().unwrap().contains("not configured"),
        "body: {body}"
    );
}

// ── registry loading is fail-closed ─────────────────────────────────────────

#[test]
fn registry_rejects_empty_and_blank_models() {
    let mut e = entry();
    e.models = Vec::new(); // would mint tokens valid for EVERY model
    assert!(ClientCredentialRegistry::load(&registry_file(&[e])).is_err());
    // Blank entries are stripped downstream by model_list(): same all-model hole.
    let mut blank = entry();
    blank.models = vec!["".into(), "  ".into()];
    assert!(ClientCredentialRegistry::load(&registry_file(&[blank])).is_err());
}

#[test]
fn registry_rejects_world_readable_file() {
    let path = registry_file(&[entry()]);
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert!(ClientCredentialRegistry::load(&path).is_err());
    std::fs::remove_file(&path).ok();
}

#[test]
fn registry_rejects_malformed_entries() {
    let mut bad = entry();
    bad.secret_hash = "tooshort".into();
    assert!(ClientCredentialRegistry::load(&registry_file(&[bad])).is_err());

    let mut bad = entry();
    bad.max_ttl_seconds = 0;
    assert!(ClientCredentialRegistry::load(&registry_file(&[bad])).is_err());

    assert!(ClientCredentialRegistry::load(&registry_file(&[entry(), entry()])).is_err());

    let mut bad = entry();
    bad.upstream_ref = String::new();
    assert!(ClientCredentialRegistry::load(&registry_file(&[bad])).is_err());
}

#[test]
fn registry_accepts_valid_entries() {
    let registry = ClientCredentialRegistry::load(&registry_file(&[entry()]));
    assert!(registry.is_ok());
}
