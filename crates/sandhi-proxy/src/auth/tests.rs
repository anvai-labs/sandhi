use super::*;
use serde_json::json;

fn config() -> Config {
    serde_json::from_value(json!({
        "issuer": "https://sso.example.test/oauth2/openid/sandhi",
        "client_id": "sandhi", "redirect_url": "https://gateway.example.test/auth/callback",
        "subjects": {"viewer-id": {"role":"viewer"}, "operator-id": {"role":"operator"},
            "admin-id": {"role":"admin"}, "agent-id": {"grants": {"local": {
                "upstream": "inferflux:local", "models": ["qwen3-coder-30b"], "group": "team-a"
            }}}}
    }))
    .unwrap()
}

#[test]
fn configuration_rejects_ambiguous_or_insecure_authority() {
    let c = config();
    assert!(c.validate().is_ok());
    for (field, value) in [
        ("issuer", "http://sso.example.test"),
        ("issuer", "https://user:password@sso.example.test"),
        ("redirect_url", "http://localhost/auth/callback"),
        ("redirect_url", "https://gateway.example.test/elsewhere"),
        ("client_id", ""),
    ] {
        let mut data = serde_json::to_value(&c).unwrap();
        data[field] = json!(value);
        assert!(
            serde_json::from_value::<Config>(data)
                .unwrap()
                .validate()
                .is_err(),
            "{field}={value}"
        );
    }
    let mut data = serde_json::to_value(&c).unwrap();
    data["subjects"]["agent-id"]["grants"]["local"]["models"] = json!([]);
    assert!(serde_json::from_value::<Config>(data)
        .unwrap()
        .validate()
        .is_err());
    let mut data = serde_json::to_value(&c).unwrap();
    data["allow_insecure"] = json!(true);
    assert!(serde_json::from_value::<Config>(data).is_err());
}

#[test]
fn roles_are_explicit_and_inference_does_not_imply_operator_access() {
    let c = config();
    for (subject, expected) in [
        ("viewer-id", vec![true, false, false]),
        ("operator-id", vec![true, true, false]),
        ("admin-id", vec![true, true, true]),
        ("agent-id", vec![false, false, false]),
        ("unknown", vec![false, false, false]),
    ] {
        let actual: Vec<_> = [Permission::Read, Permission::Operate, Permission::Admin]
            .into_iter()
            .map(|p| c.permits(subject, p))
            .collect();
        assert_eq!(actual, expected, "{subject}");
    }
    assert!(c.grant("viewer-id", None).is_none());
    let first = c.grant("agent-id", None).unwrap();
    assert_eq!(first, c.grant("agent-id", None).unwrap());
    assert!(first.permits_attribution(Some("agent-id"), Some("team-a")));
    assert!(!first.permits_attribution(Some("admin-id"), Some("team-a")));
    assert!(!first.permits_model("glm-5.3"));
}
fn offline_oidc() -> Oidc {
    Oidc {
        config: config(),
        http: reqwest::Client::new(),
        secret: None,
        introspection_url: "https://sso.example.test/introspect".into(),
        sessions: Mutex::new(Sessions {
            pending: HashMap::new(),
            active: HashMap::new(),
        }),
        permits: tokio::sync::Semaphore::new(16),
    }
}
fn app_state(oidc: Oidc) -> Arc<ProxyState> {
    let store = Arc::new(sandhi_store::SqliteStore::in_memory().unwrap());
    let mut state = ProxyState::new(
        sandhi_core::KeyStore::new(),
        crate::ProxyLedger::in_memory(),
        store.clone(),
        HashMap::new(),
        Some(store),
    );
    state.oidc = Some(Arc::new(oidc));
    state.dashboard_public = true; // Stale compatibility setting must not bypass SSO.
    Arc::new(state)
}
#[tokio::test]
async fn oidc_overrides_legacy_public_dashboard_and_missing_admin_token() {
    use tower::ServiceExt;
    let app = crate::build_app(app_state(offline_oidc()));
    for path in [
        "/dashboard/api/usage",
        "/dashboard/api/keys",
        "/dashboard/api/budgets",
        "/dashboard/api/alerts",
        "/metrics",
        "/admin/usage",
    ] {
        let response = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .uri(path)
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{path}");
    }
}
#[tokio::test]
async fn dashboard_roles_and_csrf_are_enforced_at_real_handlers() {
    use tower::ServiceExt;
    for (subject, expected_write) in [
        ("viewer-id", StatusCode::FORBIDDEN),
        ("operator-id", StatusCode::OK),
        ("admin-id", StatusCode::OK),
    ] {
        let oidc = offline_oidc();
        oidc.sessions.lock().unwrap().active.insert(
            digest("session"),
            Session {
                subject: subject.into(),
                csrf: "proof".into(),
                expires: unix_now() + 60,
            },
        );
        let app = crate::build_app(app_state(oidc));
        for (method, path, body, csrf, expected) in [
            ("GET", "/dashboard/api/usage", "", false, StatusCode::OK),
            (
                "POST",
                "/admin/budget",
                r#"{"scope":"user:test","limit_tokens":100}"#,
                false,
                StatusCode::FORBIDDEN,
            ),
            (
                "POST",
                "/admin/budget",
                r#"{"scope":"user:test","limit_tokens":100}"#,
                true,
                expected_write,
            ),
        ] {
            let mut request = axum::http::Request::builder()
                .method(method)
                .uri(path)
                .header("cookie", format!("{SESSION_COOKIE}=session"))
                .header("content-type", "application/json");
            if csrf {
                request = request
                    .header("origin", "https://gateway.example.test")
                    .header("x-sandhi-csrf", "proof");
            }
            let response = app
                .clone()
                .oneshot(request.body(axum::body::Body::from(body)).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), expected, "{subject} {path} csrf={csrf}");
        }
    }
}

// A real HTTPS mock authority with signed ID tokens; no verifier override or token fixtures
// accepted without cryptographic validation. Reuses the existing test-only TLS identity.
struct Authority {
    config: Config,
    state: Arc<Mutex<IdpState>>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Authority {
    fn drop(&mut self) {
        self.task.abort();
    }
}
struct IdpState {
    issuer: String,
    jwks: Value,
    tokens: HashMap<String, Value>,
    codes: HashMap<String, Value>,
    verifier: Option<String>,
}
impl Authority {
    async fn start() -> Self {
        use openidconnect::{core::CoreRsaPrivateSigningKey, PrivateSigningKey};
        use rsa::{pkcs1::EncodeRsaPrivateKey, pkcs8::DecodePrivateKey};
        let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/tls");
        let key = rsa::RsaPrivateKey::from_pkcs8_pem(
            &std::fs::read_to_string(fixture.join("localhost-key.pem")).unwrap(),
        )
        .unwrap();
        let signing = CoreRsaPrivateSigningKey::from_pem(
            &key.to_pkcs1_pem(rsa::pkcs1::LineEnding::LF).unwrap(),
            None,
        )
        .unwrap();
        let jwks =
            serde_json::to_value(CoreJsonWebKeySet::new(vec![signing.as_verification_key()]))
                .unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut config = config();
        config.issuer = format!(
            "https://localhost:{}/oidc",
            listener.local_addr().unwrap().port()
        );
        config.ca_file = Some(fixture.join("localhost-cert.pem"));
        let state = Arc::new(Mutex::new(IdpState {
            issuer: config.issuer.clone(),
            jwks,
            tokens: HashMap::new(),
            codes: HashMap::new(),
            verifier: None,
        }));
        let app = axum::Router::new()
            .route(
                "/oidc/.well-known/openid-configuration",
                axum::routing::get(idp_metadata),
            )
            .route("/jwks", axum::routing::get(idp_jwks))
            .route("/token", axum::routing::post(idp_token))
            .route("/introspect", axum::routing::post(idp_introspect))
            .with_state(state.clone());
        let tls = crate::TlsConfig::from_pem_files(
            fixture.join("localhost-cert.pem"),
            fixture.join("localhost-key.pem"),
        )
        .unwrap();
        let task = tokio::spawn(async move {
            let mut connections = tokio::task::JoinSet::new();
            loop {
                let (socket, _) = listener.accept().await.unwrap();
                let accept = tls.acceptor.clone();
                let app = app.clone();
                connections.spawn(async move {
                    if let Ok(stream) = accept.accept(socket).await {
                        let _ = hyper::server::conn::http1::Builder::new()
                            .serve_connection(
                                hyper_util::rt::TokioIo::new(stream),
                                hyper_util::service::TowerToHyperService::new(app),
                            )
                            .await;
                    }
                });
            }
        });
        Self {
            config,
            state,
            task,
        }
    }
    fn issue_es256(&self, code: &str, nonce: &str) -> String {
        use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
        use p256::ecdsa::{signature::Signer, Signature, SigningKey};
        let key = SigningKey::random(&mut p256::elliptic_curve::rand_core::OsRng);
        let point = key.verifying_key().to_encoded_point(false);
        let jwk = json!({"kty":"EC","crv":"P-256","alg":"ES256","use":"sig",
            "x":URL_SAFE_NO_PAD.encode(point.x().unwrap()),"y":URL_SAFE_NO_PAD.encode(point.y().unwrap())});
        self.state.lock().unwrap().jwks = json!({"keys":[jwk]});
        let payload = json!({"iss":self.config.issuer,"aud":["sandhi"],"sub":"admin-id","exp":unix_now()+300,"iat":unix_now(),"nonce":nonce});
        let signing_input = format!(
            "{}.{}",
            URL_SAFE_NO_PAD.encode(br#"{"alg":"ES256"}"#),
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(&payload).unwrap())
        );
        let signature: Signature = key.sign(signing_input.as_bytes());
        let token = format!(
            "{signing_input}.{}",
            URL_SAFE_NO_PAD.encode(signature.to_bytes())
        );
        self.state.lock().unwrap().codes.insert(code.into(),json!({"access_token":"access","token_type":"Bearer","id_token":token,"expires_in":300}));
        token
    }
    fn issue(&self, code: &str, nonce: &str, overrides: Value) -> String {
        use openidconnect::{
            core::{
                CoreIdToken, CoreIdTokenClaims, CoreJwsSigningAlgorithm, CoreRsaPrivateSigningKey,
            },
            AccessToken,
        };
        use rsa::{pkcs1::EncodeRsaPrivateKey, pkcs8::DecodePrivateKey};
        let key = rsa::RsaPrivateKey::from_pkcs8_pem(include_str!(
            "../../tests/fixtures/tls/localhost-key.pem"
        ))
        .unwrap();
        let signing = CoreRsaPrivateSigningKey::from_pem(
            &key.to_pkcs1_pem(rsa::pkcs1::LineEnding::LF).unwrap(),
            None,
        )
        .unwrap();
        let mut claims = json!({"iss":self.config.issuer,"aud":["sandhi"],"sub":"admin-id","exp":unix_now()+300,"iat":unix_now(),"nonce":nonce});
        for (k, v) in overrides.as_object().unwrap() {
            claims[k] = v.clone();
        }
        let claims: CoreIdTokenClaims = serde_json::from_value(claims).unwrap();
        let mut token = CoreIdToken::new(
            claims,
            &signing,
            CoreJwsSigningAlgorithm::RsaSsaPkcs1V15Sha256,
            Some(&AccessToken::new(
                if overrides.get("at_hash").is_some() {
                    "different-access"
                } else {
                    "access"
                }
                .into(),
            )),
            None,
        )
        .unwrap()
        .to_string();
        if overrides.get("bad_signature").is_some() {
            let i = token.rfind('.').unwrap() + 1;
            token.replace_range(i..i + 1, if &token[i..i + 1] == "A" { "B" } else { "A" });
        }
        self.state.lock().unwrap().codes.insert(code.into(),json!({"access_token":"access","token_type":"Bearer","id_token":token,"expires_in":300}));
        token
    }
}
async fn idp_metadata(State(state): State<Arc<Mutex<IdpState>>>) -> Json<Value> {
    let state = state.lock().unwrap();
    let root = state.issuer.trim_end_matches("/oidc");
    Json(
        json!({"issuer":state.issuer,"authorization_endpoint":format!("{root}/authorize"),"token_endpoint":format!("{root}/token"),
        "jwks_uri":format!("{root}/jwks"),"introspection_endpoint":format!("{root}/introspect"),"response_types_supported":["code"],
        "subject_types_supported":["public"],"id_token_signing_alg_values_supported":["RS256","ES256"],"code_challenge_methods_supported":["S256"]}),
    )
}
async fn idp_jwks(State(state): State<Arc<Mutex<IdpState>>>) -> Json<Value> {
    Json(state.lock().unwrap().jwks.clone())
}
async fn idp_token(
    State(state): State<Arc<Mutex<IdpState>>>,
    axum::Form(data): axum::Form<HashMap<String, String>>,
) -> Response {
    let mut state = state.lock().unwrap();
    state.verifier = data.get("code_verifier").cloned();
    match data.get("code").and_then(|code| state.codes.remove(code)) {
        Some(v) => Json(v).into_response(),
        None => (
            StatusCode::BAD_REQUEST,
            Json(json!({"error":"invalid_grant"})),
        )
            .into_response(),
    }
}
async fn idp_introspect(
    State(state): State<Arc<Mutex<IdpState>>>,
    axum::Form(data): axum::Form<HashMap<String, String>>,
) -> Json<Value> {
    Json(
        data.get("token")
            .and_then(|token| state.lock().unwrap().tokens.get(token).cloned())
            .unwrap_or_else(|| json!({"active":false})),
    )
}
async fn start_login(app: &axum::Router) -> (String, HashMap<String, String>) {
    use tower::ServiceExt;
    let response = app
        .clone()
        .oneshot(
            axum::http::Request::builder()
                .uri("/auth/login")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(response.headers()["cache-control"], "no-store");
    assert_eq!(response.headers()["referrer-policy"], "no-referrer");
    let cookie = response.headers()["set-cookie"].to_str().unwrap();
    for required in ["__Host-", "Secure", "HttpOnly", "SameSite=Lax", "Path=/"] {
        assert!(cookie.contains(required));
    }
    let params = reqwest::Url::parse(response.headers()["location"].to_str().unwrap())
        .unwrap()
        .query_pairs()
        .into_owned()
        .collect();
    (cookie.split(';').next().unwrap().into(), params)
}
async fn finish_login(app: &axum::Router, cookie: &str, state: &str, code: &str) -> Response {
    use tower::ServiceExt;
    app.clone()
        .oneshot(
            axum::http::Request::builder()
                .uri(format!("/auth/callback?state={state}&code={code}"))
                .header("cookie", cookie)
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap()
}
#[tokio::test]
async fn signed_oidc_login_pkce_csrf_logout_and_one_use_callback() {
    for algorithm in ["RS256", "ES256"] {
        verify_signed_login(algorithm).await;
    }
}
async fn verify_signed_login(algorithm: &str) {
    use tower::ServiceExt;
    let authority = Authority::start().await;
    let app = crate::build_app(app_state(
        Oidc::new(authority.config.clone()).await.unwrap(),
    ));
    let (cookie, query) = start_login(&app).await;
    assert_eq!(query["code_challenge_method"], "S256");
    let id_token = if algorithm == "ES256" {
        authority.issue_es256("good", &query["nonce"])
    } else {
        authority.issue("good", &query["nonce"], json!({}))
    };
    let response = finish_login(&app, &cookie, &query["state"], "good").await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(response.headers()["location"], "/dashboard");
    let actual_verifier = authority.state.lock().unwrap().verifier.clone().unwrap();
    assert_eq!(
        PkceCodeChallenge::from_code_verifier_sha256(&PkceCodeVerifier::new(actual_verifier))
            .as_str(),
        query["code_challenge"]
    );
    let session_cookie = response
        .headers()
        .get_all("set-cookie")
        .iter()
        .map(|v| v.to_str().unwrap())
        .find(|v| v.starts_with(SESSION_COOKIE))
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_owned();
    assert_eq!(
        finish_login(&app, &cookie, &query["state"], "good")
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );
    let request = || axum::http::Request::builder().header("cookie", &session_cookie);
    let response = app
        .clone()
        .oneshot(
            request()
                .uri("/auth/session")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let bytes = axum::body::to_bytes(response.into_body(), 4096)
        .await
        .unwrap();
    assert!(!String::from_utf8_lossy(&bytes).contains(&id_token));
    let session: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(session["role"], "admin");
    for origin in [None, Some("https://evil.example.test")] {
        let mut req = request()
            .method("POST")
            .uri("/auth/logout")
            .header("x-sandhi-csrf", session["csrf"].as_str().unwrap());
        if let Some(origin) = origin {
            req = req.header("origin", origin);
        }
        assert_eq!(
            app.clone()
                .oneshot(req.body(axum::body::Body::empty()).unwrap())
                .await
                .unwrap()
                .status(),
            StatusCode::FORBIDDEN
        );
    }
    let logout = request()
        .method("POST")
        .uri("/auth/logout")
        .header("origin", "https://gateway.example.test")
        .header("x-sandhi-csrf", session["csrf"].as_str().unwrap())
        .body(axum::body::Body::empty())
        .unwrap();
    assert_eq!(
        app.clone().oneshot(logout).await.unwrap().status(),
        StatusCode::OK
    );
    assert_eq!(
        app.oneshot(
            request()
                .uri("/dashboard/api/usage")
                .body(axum::body::Body::empty())
                .unwrap()
        )
        .await
        .unwrap()
        .status(),
        StatusCode::UNAUTHORIZED
    );
}
#[tokio::test]
async fn signed_identity_and_browser_binding_fail_closed() {
    let authority = Authority::start().await;
    let oidc = Oidc::new(authority.config.clone()).await.unwrap();
    let app = crate::build_app(app_state(oidc));
    for overrides in [
        json!({"iss":"https://evil.example.test"}),
        json!({"aud":["other"]}),
        json!({"exp":unix_now()-1}),
        json!({"nonce":"wrong"}),
        json!({"at_hash":"wrong"}),
        json!({"bad_signature":true}),
        json!({"sub":"unassigned"}),
    ] {
        let (cookie, query) = start_login(&app).await;
        authority.issue("bad", &query["nonce"], overrides.clone());
        let expected = if overrides.get("sub").is_some() {
            StatusCode::FORBIDDEN
        } else {
            StatusCode::UNAUTHORIZED
        };
        assert_eq!(
            finish_login(&app, &cookie, &query["state"], "bad")
                .await
                .status(),
            expected,
            "{overrides}"
        );
    }
    let (cookie, query) = start_login(&app).await;
    authority.issue("good", &query["nonce"], json!({}));
    assert_eq!(
        finish_login(&app, "", &query["state"], "good")
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        finish_login(&app, &cookie, "wrong", "good").await.status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        finish_login(&app, &cookie, &query["state"], "good")
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );
}
#[tokio::test]
async fn introspection_requires_access_token_identity_and_explicit_grant() {
    let authority = Authority::start().await;
    let oidc = Oidc::new(authority.config.clone()).await.unwrap();
    let good = json!({"active":true,"iss":authority.config.issuer,"aud":"sandhi","sub":"agent-id","exp":unix_now()+60,"token_type":"Bearer"});
    authority
        .state
        .lock()
        .unwrap()
        .tokens
        .insert("access".into(), good.clone());
    let grant = oidc.inference("access", &HeaderMap::new()).await.unwrap();
    assert_eq!(grant.subject_id.as_deref(), Some("agent-id"));
    assert_eq!(grant.upstream_ref, "inferflux:local");
    for (field, value) in [
        ("active", json!(false)),
        ("iss", json!("https://evil.example")),
        ("aud", json!("other")),
        ("exp", json!(unix_now())),
        ("exp", json!("bad")),
        ("nbf", json!(unix_now() + 30)),
        ("sub", json!("unknown")),
        ("token_type", json!("id_token")),
    ] {
        let mut invalid = good.clone();
        invalid[field] = value;
        authority
            .state
            .lock()
            .unwrap()
            .tokens
            .insert("invalid".into(), invalid);
        assert!(
            oidc.inference("invalid", &HeaderMap::new()).await.is_err(),
            "{field}"
        );
    }
    let id = authority.issue("code", "nonce", json!({}));
    assert_eq!(
        oidc.inference(&id, &HeaderMap::new())
            .await
            .unwrap_err()
            .status(),
        StatusCode::UNAUTHORIZED
    );
    let permit = oidc.permits.acquire_many(16).await.unwrap();
    assert_eq!(
        oidc.inference("access", &HeaderMap::new())
            .await
            .unwrap_err()
            .status(),
        StatusCode::SERVICE_UNAVAILABLE
    );
    drop(permit);
    authority.task.abort();
    assert_eq!(
        oidc.inference("access", &HeaderMap::new())
            .await
            .unwrap_err()
            .status(),
        StatusCode::SERVICE_UNAVAILABLE
    );
}

#[tokio::test]
async fn multi_provider_grants_use_existing_accounting_and_correlation_path() {
    use tower::ServiceExt;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};
    let authority = Authority::start().await;
    authority.state.lock().unwrap().tokens.insert("agent-access".into(),json!({"active":true,"iss":authority.config.issuer,"aud":"sandhi","sub":"agent-id","exp":unix_now()+300,"token_type":"Bearer"}));
    let mut config = authority.config.clone();
    config.subjects.get_mut("agent-id").unwrap().grants.insert(
        "cloud".into(),
        Grant {
            upstream: "zai:cloud".into(),
            models: vec!["glm-5.3".into()],
            group: Some("team-a".into()),
            budget_scope: None,
            rate_limit_per_min: None,
        },
    );
    let upstream = MockServer::start().await;
    Mock::given(method("POST")).and(path("/chat/completions")).and(header("authorization","Bearer provider-only"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id":"chatcmpl-test","object":"chat.completion","choices":[{"index":0,"message":{"role":"assistant","content":"done"},"finish_reason":"stop"}],"usage":{"prompt_tokens":7,"completion_tokens":3,"total_tokens":10}}))).expect(2).mount(&upstream).await;
    let sink = Arc::new(sandhi_core::InMemorySink::new());
    let mut providers = HashMap::new();
    let runtime = sandhi_providers::ProviderRuntime::new();
    for (reference, provider) in [("inferflux:local", "inferflux"), ("zai:cloud", "zai")] {
        providers.insert(
            reference.into(),
            runtime.openai_compat(
                provider,
                upstream.uri(),
                "provider-only",
                HeaderMap::new(),
                Some(0),
                None,
                None,
            ),
        );
    }
    let mut state = ProxyState::new(
        sandhi_core::KeyStore::new(),
        crate::ProxyLedger::in_memory(),
        sink.clone(),
        providers,
        None,
    );
    state.oidc = Some(Arc::new(Oidc::new(config).await.unwrap()));
    let state = Arc::new(state);
    let app = crate::build_app(state.clone());
    let request = |grant: Option<&str>, model: &str, spoof: bool| {
        let mut req = axum::http::Request::builder()
            .method("POST")
            .uri("/v1/chat/completions")
            .header("authorization", "Bearer agent-access")
            .header("content-type", "application/json")
            .header("x-sandhi-run-id", "run-oidc");
        if let Some(grant) = grant {
            req = req
                .header("x-sandhi-grant", grant)
                .header("x-sandhi-session", format!("member-{grant}"))
                .header("x-sandhi-step-id", grant);
        }
        if spoof {
            req = req.header("x-sandhi-subject-id", "admin-id");
        }
        req.body(axum::body::Body::from(
            json!({"model":model,"messages":[{"role":"user","content":"ordinary member task"}]})
                .to_string(),
        ))
        .unwrap()
    };
    for (grant, model, spoof, expected) in [
        (None, "qwen3-coder-30b", false, StatusCode::FORBIDDEN),
        (Some("local"), "glm-5.3", false, StatusCode::FORBIDDEN),
        (
            Some("local"),
            "qwen3-coder-30b",
            true,
            StatusCode::FORBIDDEN,
        ),
        (Some("local"), "qwen3-coder-30b", false, StatusCode::OK),
        (Some("cloud"), "glm-5.3", false, StatusCode::OK),
    ] {
        let response = app
            .clone()
            .oneshot(request(grant, model, spoof))
            .await
            .unwrap();
        assert_eq!(response.status(), expected);
        axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
    }
    let mut ambiguous = request(Some("local"), "qwen3-coder-30b", false);
    ambiguous
        .headers_mut()
        .append("authorization", HeaderValue::from_static("Bearer other"));
    assert_eq!(
        app.oneshot(ambiguous).await.unwrap().status(),
        StatusCode::UNAUTHORIZED
    );
    let events = sink.events();
    assert_eq!(events.len(), 2);
    assert_eq!(state.ledger.lock().unwrap().spent("group:team-a"), 20);
    for (event, grant) in events.iter().zip(["local", "cloud"]) {
        assert_eq!(event.subject_id.as_deref(), Some("agent-id"));
        assert_eq!(event.group_id.as_deref(), Some("team-a"));
        assert_eq!(event.run_id.as_deref(), Some("run-oidc"));
        assert_eq!(event.step_id.as_deref(), Some(grant));
        assert_eq!(
            event.session_id.as_deref(),
            Some(format!("member-{grant}").as_str())
        );
    }
    let sent = upstream.received_requests().await.unwrap();
    assert_eq!(
        sent[0].headers["x-inferflux-client-request-id"],
        events[0].request_id
    );
    assert_eq!(sent[0].headers["x-inferflux-session-id"], "member-local");
}
