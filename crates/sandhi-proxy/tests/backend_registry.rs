//! Real admin -> stored backend -> scoped key -> inference -> rotation/restart/revocation.
use std::{collections::HashMap, path::Path, sync::Arc};

use axum::{
    body::{to_bytes, Body},
    http::{Request, StatusCode},
};
use sandhi_core::{InMemorySink, KeyStore};
use sandhi_providers::ProviderRuntime;
use sandhi_proxy::{
    backend::rehydrate_backends, build_app, rehydrate_live_keys, ProxyLedger, ProxyState,
};
use sandhi_store::{BackendStore, VaultStore, VirtualKeyStore};
use serde_json::{json, Value};
use tower::ServiceExt;
use wiremock::{
    matchers::{header, method, path},
    Mock, MockServer, ResponseTemplate,
};

const ADMIN: &str = "synthetic-registry-admin";

#[tokio::test]
async fn backend_registry_cannot_replace_host_owned_handles() {
    let directory = tempfile::tempdir().unwrap();
    let state = state(
        &directory.path().join("backends.db"),
        Arc::new(VaultStore::in_memory().unwrap()),
    );
    credential(&state, "http://127.0.0.1:9", "synthetic-key").await;
    register(&state, definition("http://127.0.0.1:9")).await;
    let host = state
        .providers
        .lock()
        .unwrap()
        .get("openai:default")
        .unwrap()
        .clone();
    state
        .providers
        .lock()
        .unwrap()
        .insert("backend/primary".into(), host.clone());
    let (_, inventory) = call(&state, "GET", "/admin/backends", ADMIN, None).await;
    assert_eq!(inventory["backends"][0]["ready"], false);
    let (status, _) = call(
        &state,
        "POST",
        "/admin/backends",
        ADMIN,
        Some(json!({"definition":definition("http://127.0.0.1:9")})),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    credential(&state, "http://127.0.0.1:9", "rotated-key").await;
    assert!(state.providers.lock().unwrap()["backend/primary"]
        .backend_binding()
        .is_none());
    let mut providers = HashMap::from([("backend/primary".into(), host)]);
    rehydrate_backends(
        state.vault.as_ref().unwrap(),
        state.backends.as_ref().unwrap(),
        &ProviderRuntime::new(),
        &mut providers,
    )
    .unwrap();
    assert!(providers["backend/primary"].backend_binding().is_none());
    assert_eq!(
        call(
            &state,
            "DELETE",
            "/admin/backends/primary?revision=1",
            ADMIN,
            None
        )
        .await
        .0,
        StatusCode::OK
    );
    assert!(state.providers.lock().unwrap()["backend/primary"]
        .backend_binding()
        .is_none());
}

fn state(db: &Path, vault: Arc<VaultStore>) -> Arc<ProxyState> {
    let backends = Arc::new(BackendStore::open(db.to_str().unwrap()).unwrap());
    let vkeys = Arc::new(VirtualKeyStore::open(db.to_str().unwrap()).unwrap());
    let keys = KeyStore::new();
    rehydrate_live_keys(&keys, &vkeys);
    let mut providers = HashMap::new();
    rehydrate_backends(&vault, &backends, &ProviderRuntime::new(), &mut providers).unwrap();
    let mut state = ProxyState::new(
        keys,
        ProxyLedger::in_memory(),
        Arc::new(InMemorySink::new()),
        providers,
        None,
    );
    state.vault = Some(vault);
    state.backends = Some(backends);
    state.vkeys = Some(vkeys);
    state.admin_token = Some(ADMIN.into());
    Arc::new(state)
}

async fn call(
    state: &Arc<ProxyState>,
    method: &str,
    route: &str,
    token: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let response = build_app(state.clone())
        .oneshot(
            Request::builder()
                .method(method)
                .uri(route)
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(
                    body.map(|body| body.to_string()).unwrap_or_default(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let body = to_bytes(response.into_body(), 100_000).await.unwrap();
    (status, serde_json::from_slice(&body).unwrap_or(Value::Null))
}

fn definition(base: &str) -> Value {
    json!({"id":"primary","provider":"custom-openai","credential_ref":"openai:default",
        "endpoint":{"endpoint_family":"openai_responses"},"base_url":base,
        "models":[{"id":"fixture","capabilities":{"tools":true}}]})
}

fn inference(model: &str) -> Value {
    json!({"model":model,"max_tokens":64,"messages":[
        {"role":"system","content":"Be precise"},{"role":"user","content":"hello"}]})
}

async fn credential(state: &Arc<ProxyState>, base: &str, secret: &str) {
    let (status, body) = call(
        state,
        "POST",
        "/admin/keys",
        ADMIN,
        Some(json!({
            "provider":"openai","base_url":base,"secret":secret
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
}

async fn register(state: &Arc<ProxyState>, def: Value) -> Value {
    let (status, body) = call(
        state,
        "POST",
        "/admin/backends",
        ADMIN,
        Some(json!({"definition":def})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    body
}

async fn key(state: &Arc<ProxyState>, upstream: &str) -> String {
    let (status, body) = call(
        state,
        "POST",
        "/admin/keys/share",
        ADMIN,
        Some(json!({"upstream":upstream,"models":["fixture","undeclared"]})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    body["virtual_key"].as_str().unwrap().into()
}

#[tokio::test]
async fn registry_binding_survives_rotation_restart_and_credential_revocation() {
    let upstream = MockServer::start().await;
    let response = json!({"id":"resp_fixture","model":"fixture","status":"completed",
        "output":[{"type":"message","content":[{"type":"output_text","text":"reply"}]}],
        "usage":{"input_tokens":5,"output_tokens":2,"input_tokens_details":{"cached_tokens":2,"cache_write_tokens":1}}});
    for (secret, expected) in [("first-key", 1), ("rotated-key", 2)] {
        Mock::given(method("POST"))
            .and(path("/responses"))
            .and(header("authorization", format!("Bearer {secret}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(response.clone()))
            .expect(expected)
            .mount(&upstream)
            .await;
    }
    let directory = tempfile::tempdir().unwrap();
    let db = directory.path().join("backends.db");
    let vault = Arc::new(VaultStore::in_memory().unwrap());
    let state = state(&db, vault.clone());
    credential(&state, &upstream.uri(), "first-key").await;
    let registered = register(&state, definition(&upstream.uri())).await;
    assert_eq!(registered["revision"], 1);
    assert_eq!(registered["upstream_ref"], "backend/primary");
    assert_eq!(
        vault.list().unwrap().len(),
        1,
        "backend registration creates no credential"
    );
    let token = key(&state, "backend/primary").await;
    let (status, body) = call(
        &state,
        "POST",
        "/v1/chat/completions",
        &token,
        Some(inference("fixture")),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["choices"][0]["message"]["content"], "reply");
    let (status, models) = call(&state, "GET", "/v1/models", &token, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(models["data"].as_array().unwrap().len(), 1);
    assert_eq!(models["data"][0]["id"], "fixture");
    assert_eq!(
        call(
            &state,
            "POST",
            "/v1/chat/completions",
            &token,
            Some(inference("undeclared"))
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    credential(&state, &upstream.uri(), "rotated-key").await;
    assert_eq!(
        state.providers.lock().unwrap()["backend/primary"]
            .backend_binding()
            .unwrap()
            .credential_generation,
        2
    );
    assert_eq!(
        call(
            &state,
            "POST",
            "/v1/chat/completions",
            &token,
            Some(inference("fixture"))
        )
        .await
        .0,
        StatusCode::OK
    );
    drop(state);
    let restarted = self::state(&db, vault.clone());
    assert_eq!(
        restarted.providers.lock().unwrap()["backend/primary"]
            .backend_binding()
            .unwrap()
            .credential_generation,
        2
    );
    assert_eq!(
        call(
            &restarted,
            "POST",
            "/v1/chat/completions",
            &token,
            Some(inference("fixture"))
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        call(
            &restarted,
            "DELETE",
            "/admin/keys/openai/default",
            ADMIN,
            None
        )
        .await
        .0,
        StatusCode::OK
    );
    assert!(!restarted
        .providers
        .lock()
        .unwrap()
        .contains_key("backend/primary"));
    assert_eq!(
        call(
            &restarted,
            "POST",
            "/v1/chat/completions",
            &token,
            Some(inference("fixture"))
        )
        .await
        .0,
        StatusCode::BAD_GATEWAY
    );
    drop(restarted);
    let restarted = self::state(&db, vault);
    assert!(!restarted
        .providers
        .lock()
        .unwrap()
        .contains_key("backend/primary"));
    assert_eq!(upstream.received_requests().await.unwrap().len(), 3);
}

#[tokio::test]
async fn invalid_or_stale_backend_changes_do_not_retarget_existing_keys() {
    let directory = tempfile::tempdir().unwrap();
    let state = state(
        &directory.path().join("backends.db"),
        Arc::new(VaultStore::in_memory().unwrap()),
    );
    credential(&state, "http://127.0.0.1:9", "synthetic-key").await;
    register(&state, definition("http://127.0.0.1:9")).await;
    for base in [
        "https://other.invalid",
        "http://user:password@127.0.0.1:9",
        "http://127.0.0.1:9?credential=x",
    ] {
        let mut def = definition(base);
        def["user_agent"] = json!("changed");
        let (status, _) = call(
            &state,
            "POST",
            "/admin/backends",
            ADMIN,
            Some(json!({"definition":def,"expected_revision":1})),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }
    let mut changed = definition("http://127.0.0.1:9");
    changed["user_agent"] = json!("changed");
    assert_eq!(
        call(
            &state,
            "POST",
            "/admin/backends",
            ADMIN,
            Some(json!({"definition":changed,"expected_revision":0}))
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    assert_eq!(
        state
            .backends
            .as_ref()
            .unwrap()
            .get("primary")
            .unwrap()
            .unwrap()
            .revision,
        1
    );
    assert_eq!(
        call(
            &state,
            "DELETE",
            "/admin/backends/primary?revision=0",
            ADMIN,
            None
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    assert!(state
        .providers
        .lock()
        .unwrap()
        .contains_key("backend/primary"));
    assert_eq!(
        call(
            &state,
            "DELETE",
            "/admin/backends/primary?revision=1",
            ADMIN,
            None
        )
        .await
        .0,
        StatusCode::OK
    );
    assert!(!state
        .providers
        .lock()
        .unwrap()
        .contains_key("backend/primary"));
    assert_eq!(
        state.vault.as_ref().unwrap().list().unwrap()[0].status,
        "active"
    );
}

#[tokio::test]
async fn backend_admin_access_is_gated_and_definition_never_accepts_secrets() {
    let directory = tempfile::tempdir().unwrap();
    let state = state(
        &directory.path().join("backends.db"),
        Arc::new(VaultStore::in_memory().unwrap()),
    );
    assert_eq!(
        call(&state, "GET", "/admin/backends", "invalid", None)
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    let mut def = definition("http://127.0.0.1:9");
    def["secret"] = json!("must-not-store");
    assert!(call(
        &state,
        "POST",
        "/admin/backends",
        ADMIN,
        Some(json!({"definition":def}))
    )
    .await
    .0
    .is_client_error());
    assert!(state.backends.as_ref().unwrap().list().unwrap().is_empty());
    assert!(state.vault.as_ref().unwrap().list().unwrap().is_empty());
}

#[tokio::test]
async fn declarative_backends_apply_before_key_mint_and_omission_never_revokes() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            json!({"model":"fixture","status":"completed",
            "output":[{"type":"message","content":[{"type":"output_text","text":"configured"}]}],
            "usage":{"input_tokens":3,"output_tokens":1}}),
        ))
        .expect(1)
        .mount(&upstream)
        .await;
    let directory = tempfile::tempdir().unwrap();
    let config_path = directory.path().join("config.json");
    std::fs::write(
        &config_path,
        json!({
            "providers":[{"provider":"openai","base_url":upstream.uri()}],
            "backends":[definition(&upstream.uri())],
            "vkeys":[{"upstream":"backend/primary","subject":"configured","models":["fixture"]}]
        })
        .to_string(),
    )
    .unwrap();
    let mut state = Arc::try_unwrap(state(
        &directory.path().join("backends.db"),
        Arc::new(VaultStore::in_memory().unwrap()),
    ))
    .ok()
    .unwrap();
    state.config_path = Some(config_path.clone());
    let state = Arc::new(state);
    let (status, preview) = call(&state, "GET", "/admin/config", ADMIN, None).await;
    assert_eq!(status, StatusCode::OK, "{preview}");
    assert_eq!(preview["backends"][0]["action"], "create");
    let (status, applied) = call(&state, "POST", "/admin/config/apply", ADMIN, None).await;
    assert_eq!(status, StatusCode::OK, "{applied}");
    assert_eq!(applied["backends"]["applied"][0]["id"], "primary");
    let token = applied["vkeys"]["minted"][0]["virtual_key"]
        .as_str()
        .unwrap();
    assert_eq!(
        call(
            &state,
            "POST",
            "/v1/chat/completions",
            token,
            Some(inference("fixture"))
        )
        .await
        .0,
        StatusCode::OK
    );
    let (status, repeated) = call(&state, "POST", "/admin/config/apply", ADMIN, None).await;
    assert_eq!(status, StatusCode::OK, "{repeated}");
    assert_eq!(repeated["vkeys"]["skipped"].as_array().unwrap().len(), 1);
    assert_eq!(
        state
            .backends
            .as_ref()
            .unwrap()
            .get("primary")
            .unwrap()
            .unwrap()
            .revision,
        1
    );
    std::fs::write(config_path, "{}").unwrap();
    assert_eq!(
        call(&state, "POST", "/admin/config/apply", ADMIN, None)
            .await
            .0,
        StatusCode::OK
    );
    assert!(state
        .providers
        .lock()
        .unwrap()
        .contains_key("backend/primary"));
}

#[tokio::test]
async fn invalid_backend_config_is_rejected_before_any_credential_write() {
    let directory = tempfile::tempdir().unwrap();
    let config_path = directory.path().join("config.json");
    let mut def = definition("http://127.0.0.1:9");
    def["endpoint"]["endpoint_profile"] = json!("chatgpt_codex");
    std::fs::write(
        &config_path,
        json!({"providers":[{"provider":"openai","base_url":"http://127.0.0.1:9"}],
        "backends":[def]})
        .to_string(),
    )
    .unwrap();
    let mut state = Arc::try_unwrap(state(
        &directory.path().join("backends.db"),
        Arc::new(VaultStore::in_memory().unwrap()),
    ))
    .ok()
    .unwrap();
    state.config_path = Some(config_path);
    let state = Arc::new(state);
    assert_eq!(
        call(&state, "POST", "/admin/config/apply", ADMIN, None)
            .await
            .0,
        StatusCode::BAD_REQUEST
    );
    assert!(state.vault.as_ref().unwrap().list().unwrap().is_empty());
    assert!(state.backends.as_ref().unwrap().list().unwrap().is_empty());
}
