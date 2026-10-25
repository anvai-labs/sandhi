//! Subscription credentials stay gateway-side; clients use independent scoped keys.
use sandhi_providers::{ProviderFamily, ProviderRuntime};
use sandhi_proxy::build_provider_handle;
use sandhi_store::CredentialScheme;
use serde_json::json;

fn envelope(expires: u64) -> String {
    json!({"access_token":"synthetic-bearer", "account_id":"workspace-test", "expires_at":expires})
        .to_string()
}

#[test]
fn subscription_scheme_selects_codex_codec_and_disables_raw_forwarding() {
    let handle = build_provider_handle(
        &ProviderRuntime::new(),
        "openai",
        None,
        &envelope(4_000_000_000),
        CredentialScheme::Oauth,
    )
    .expect("valid leased subscription credential");
    assert_eq!(handle.family(), ProviderFamily::OpenAiResponses);
    assert!(handle.supports_streaming_deadlines());
    assert!(
        handle.raw_forwarder().is_none(),
        "raw forwarding bypasses subscription constraints"
    );
}

#[test]
fn subscription_rejects_missing_expiry_and_upstream_override() {
    let runtime = ProviderRuntime::new();
    for secret in [
        "raw-token".to_string(),
        json!({"access_token":"x","account_id":"a"}).to_string(),
        envelope(1),
    ] {
        assert!(
            build_provider_handle(&runtime, "openai", None, &secret, CredentialScheme::Oauth)
                .is_none()
        );
    }
    assert!(build_provider_handle(
        &runtime,
        "openai",
        Some("https://untrusted.test"),
        &envelope(4_000_000_000),
        CredentialScheme::Oauth
    )
    .is_none());
}

#[test]
fn call_headers_cannot_replace_subscription_account() {
    let mut base = axum::http::HeaderMap::new();
    base.insert("chatgpt-account-id", "owner".parse().unwrap());
    let mut call = axum::http::HeaderMap::new();
    call.insert("chatgpt-account-id", "attacker".parse().unwrap());
    call.insert("authorization", "Bearer attacker".parse().unwrap());
    let merged = sandhi_providers::merge_call_headers(&base, &call);
    assert_eq!(merged["chatgpt-account-id"], "owner");
    assert!(!merged.contains_key("authorization"));
}

#[tokio::test]
async fn expiry_fences_complete_and_stream_without_contacting_upstream() {
    let handle = build_provider_handle(
        &ProviderRuntime::new(),
        "openai",
        None,
        &envelope(4_000_000_000),
        CredentialScheme::Oauth,
    )
    .unwrap()
    .with_credential_expiry(1);
    let request: sandhi_core::ChatRequestV1 = serde_json::from_value(json!({
        "model":"test", "messages":[{"role":"user","content":[{"type":"text","text":"hello"}]}]
    }))
    .unwrap();
    assert!(matches!(
        handle.complete(request.clone()).await,
        Err(sandhi_providers::ProviderError::Auth)
    ));
    assert!(matches!(
        handle.stream(request).await,
        Err(sandhi_providers::ProviderError::Auth)
    ));
}

#[tokio::test]
async fn independent_clients_share_subscription_but_not_identity_or_authority() {
    use axum::{
        body::{to_bytes, Body},
        http::{Request, StatusCode},
    };
    use sandhi_core::{InMemorySink, KeyStore, VirtualKey};
    use sandhi_proxy::{build_app, ProxyLedger, ProxyState};
    use std::{collections::HashMap, sync::Arc};
    use tower::ServiceExt;
    use wiremock::{
        matchers::{header, method, path},
        Mock, MockServer, ResponseTemplate,
    };
    let upstream = MockServer::start().await;
    let sse=concat!(
        "data: {\"type\":\"response.output_text.delta\",\"delta\":\"hello\"}\n\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"output\":[],\"usage\":{\"input_tokens\":5,\"output_tokens\":2}}}\n\n"
    );
    Mock::given(method("POST"))
        .and(path("/responses"))
        .and(header("authorization", "Bearer upstream-subscription"))
        .and(header("chatgpt-account-id", "owner-account"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(sse),
        )
        .expect(4)
        .mount(&upstream)
        .await;
    let mut headers = axum::http::HeaderMap::new();
    headers.insert("chatgpt-account-id", "owner-account".parse().unwrap());
    // Loopback mock is injected through the runtime test seam. The admin constructor
    // rejects upstream overrides, covered above.
    let handle = ProviderRuntime::new()
        .chatgpt_responses(
            "openai",
            upstream.uri(),
            "upstream-subscription",
            headers,
            Some(0),
            None,
            None,
            None,
        )
        .with_credential_expiry(4_000_000_000);
    let keys = KeyStore::new();
    for client in ["a", "b"] {
        keys.insert(VirtualKey {
            id: format!("vk_{client}"),
            subject_id: Some(format!("victor:{client}")),
            group_id: Some("owner".into()),
            upstream_ref: "openai:subscription".into(),
            models: Some(vec!["allowed".into()]),
            ..Default::default()
        });
    }
    let sink = Arc::new(InMemorySink::new());
    let mut state = ProxyState::new(
        keys,
        ProxyLedger::in_memory(),
        sink.clone(),
        HashMap::from([("openai:subscription".into(), handle)]),
        None,
    );
    state.streaming_deadlines = sandhi_proxy::deadlines::streaming_from_json(
        r#"{"streaming_deadlines":{"ceiling_ms":10000,"default":{"setup_ms":2000,"idle_ms":2000,"body_ms":5000}}}"#,
    ).unwrap();
    let app = build_app(Arc::new(state));
    for (client, streaming) in [("a", false), ("b", false), ("a", true), ("b", true)] {
        let req=Request::builder().method("POST").uri("/v1/chat/completions")
            .header("authorization",format!("Bearer vk_{client}")).header("content-type","application/json")
            .header("chatgpt-account-id","attacker")
            .body(Body::from(json!({"model":"allowed","stream":streaming,"messages":[{"role":"system","content":"Be concise"},{"role":"user","content":"hello"}]}).to_string())).unwrap();
        let response = app.clone().oneshot(req).await.unwrap();
        let status = response.status();
        let body = to_bytes(response.into_body(), 100000).await.unwrap();
        assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
        assert!(String::from_utf8_lossy(&body).contains("hello"));
    }
    // Owned streams finalize accounting asynchronously; wire EOF is not its receipt.
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while sink.events().len() != 4 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let events = sink.events();
    assert_eq!(events.len(), 4);
    assert_eq!(events[0].subject_id.as_deref(), Some("victor:a"));
    assert_eq!(events[1].subject_id.as_deref(), Some("victor:b"));
    for e in events {
        assert_eq!(e.tokens_in, 5);
        assert_eq!(e.tokens_out, 2);
    }
    for (model, spoof) in [("forbidden", false), ("allowed", true)] {
        let mut req = Request::builder()
            .method("POST")
            .uri("/v1/chat/completions")
            .header("authorization", "Bearer vk_a")
            .header("content-type", "application/json");
        if spoof {
            req = req.header("x-sandhi-subject-id", "victor:b");
        }
        let res = app
            .clone()
            .oneshot(
                req.body(Body::from(json!({"model":model,"messages":[]}).to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::FORBIDDEN);
    }
    for request in upstream.received_requests().await.unwrap() {
        let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
        assert_eq!(body["store"], false);
        assert_eq!(body["stream"], true);
        assert!(body["instructions"]
            .as_str()
            .unwrap()
            .contains("Be concise"));
        assert!(!String::from_utf8_lossy(&request.body).contains("vk_"));
    }
}

#[tokio::test]
async fn subscription_stream_enforces_idle_and_body_limits_without_replay() {
    use axum::{
        body::{to_bytes, Body, Bytes},
        http::{Request, StatusCode},
        routing::post,
        Router,
    };
    use sandhi_core::{InMemorySink, KeyStore, VirtualKey};
    use sandhi_proxy::{build_app, ProxyLedger, ProxyState};
    use std::{
        collections::HashMap,
        sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        },
        time::Duration,
    };
    use tower::ServiceExt;

    for (idle_ms, body_ms) in [(30, 2000), (2000, 30)] {
        let calls = Arc::new(AtomicUsize::new(0));
        let observed_calls = calls.clone();
        let origin = Router::new().route("/responses", post(move || {
            observed_calls.fetch_add(1, Ordering::SeqCst);
            async {
                let bytes = async_stream::stream! {
                    yield Ok::<Bytes, std::io::Error>(Bytes::from_static(b"data: {\"type\":\"response.created\",\"response\":{\"id\":\"fake\",\"model\":\"fixture\"}}\n\n"));
                    futures_util::future::pending::<()>().await;
                };
                ([("content-type", "text/event-stream")], Body::from_stream(bytes))
            }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, origin).await.unwrap();
        });
        let handle = ProviderRuntime::new()
            .chatgpt_responses(
                "openai",
                format!("http://{address}"),
                "synthetic",
                Default::default(),
                Some(0),
                None,
                None,
                None,
            )
            .with_credential_expiry(4_000_000_000);
        assert!(handle.raw_forwarder().is_none());
        let keys = KeyStore::new();
        keys.insert(VirtualKey {
            id: "vk_fixture".into(),
            upstream_ref: "openai:codex".into(),
            ..Default::default()
        });
        let sink = Arc::new(InMemorySink::new());
        let mut state = ProxyState::new(
            keys,
            ProxyLedger::in_memory(),
            sink.clone(),
            HashMap::from([("openai:codex".into(), handle)]),
            None,
        );
        state.streaming_deadlines = sandhi_proxy::deadlines::streaming_from_json(&json!({
            "streaming_deadlines":{"ceiling_ms":10000,"default":{"setup_ms":2000,"idle_ms":idle_ms,"body_ms":body_ms}}
        }).to_string()).unwrap();
        let state = Arc::new(state);
        let request = Request::builder().method("POST").uri("/v1/chat/completions")
            .header("authorization", "Bearer vk_fixture").header("content-type", "application/json")
            .body(Body::from(json!({"model":"fixture","stream":true,"messages":[{"role":"system","content":"Be concise"},{"role":"user","content":"hello"}]}).to_string())).unwrap();
        let response = build_app(state.clone()).oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let result = tokio::time::timeout(
            Duration::from_secs(2),
            to_bytes(response.into_body(), 100000),
        )
        .await
        .expect("configured limit terminates body");
        assert!(
            result.is_err(),
            "failed streams must not appear successfully complete"
        );
        tokio::time::timeout(Duration::from_secs(2), async {
            while sink.events().len() != 1 || state.lifecycle.active_operations() != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1, "never replay a model POST");
        server.abort();
    }
}
