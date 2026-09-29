//! The non-chat routes must cross the same authorization, audit and budget gates.
use axum::{
    body::{to_bytes, Body},
    http::{Request, StatusCode},
};
use sandhi_core::{policy::Engine, InMemorySink, KeyStore, Policy, VirtualKey, Window};
use sandhi_providers::ProviderRuntime;
use sandhi_proxy::{build_app, policy::PolicyGate, ProxyLedger, ProxyState};
use sandhi_store::policy::PolicyAuditStore;
use serde_json::{json, Value};
use std::{collections::HashMap, sync::Arc};
use tower::ServiceExt;
use wiremock::{
    matchers::{header, method, path},
    Mock, MockServer, ResponseTemplate,
};

async fn setup(
    limit: u64,
    receipts: u64,
    rate: Option<u32>,
    foreign: bool,
) -> (
    Arc<ProxyState>,
    Arc<InMemorySink>,
    MockServer,
    tempfile::TempDir,
) {
    let upstream = MockServer::start().await;
    let provider = if foreign {
        ProviderRuntime::new().anthropic(
            upstream.uri(),
            "upstream-secret",
            sandhi_providers::AnthropicAuthScheme::ApiKey,
            Default::default(),
            Some(0),
            None,
            None,
        )
    } else {
        ProviderRuntime::new().openai_compat(
            "inferflux",
            upstream.uri(),
            "upstream-secret",
            Default::default(),
            Some(0),
            None,
            None,
        )
    };
    let keys = KeyStore::new();
    keys.insert(VirtualKey {
        id: "client".into(),
        subject_id: Some("alice".into()),
        group_id: Some("team".into()),
        upstream_ref: "local".into(),
        models: Some(vec!["m".into()]),
        budget_scope: Some("test".into()),
        rate_limit_per_min: rate,
        ..Default::default()
    });
    let sink = Arc::new(InMemorySink::new());
    let mut ledger = ProxyLedger::in_memory();
    ledger
        .set_budget("test", Some(limit), Window::Total, Policy::Block)
        .unwrap();
    let mut state = ProxyState::new(
        keys,
        ledger,
        sink.clone(),
        HashMap::from([("local".into(), provider)]),
        None,
    );
    let dir = tempfile::tempdir().unwrap();
    let policy=serde_json::to_vec(&json!({"schema_version":"1","revision":1,"deadline_ms":500,"max_body_bytes":65536,"rules":[
        {"id":"secret","effect":"block","evaluator":{"kind":"regex","pattern":"SECRET-[0-9]+"}},
        {"id":"review","effect":"quarantine","evaluator":{"kind":"regex","pattern":"QUARANTINE"}},
        {"id":"audit","effect":"audit","when":{"subjects":["alice"]},"evaluator":{"kind":"threshold","metric":"body_bytes","above":0}}
    ]})).unwrap();
    state.policy = Some(Arc::new(PolicyGate::new(
        Engine::from_slice(&policy).unwrap(),
        Arc::new(PolicyAuditStore::open(&dir.path().join("audit.db"), receipts).unwrap()),
    )));
    (Arc::new(state), sink, upstream, dir)
}
fn req(route: &str, body: &[u8], token: bool, spoof: bool) -> Request<Body> {
    let mut r = Request::builder()
        .method("POST")
        .uri(route)
        .header("content-type", "application/json");
    if token {
        r = r.header("authorization", "Bearer client");
    }
    if spoof {
        r = r.header("x-sandhi-subject-id", "mallory");
    }
    r.body(Body::from(body.to_vec())).unwrap()
}
fn payload(route: &str, text: &str) -> Vec<u8> {
    serde_json::to_vec(&if route.ends_with("embeddings") {
        json!({"model":"m","input":text})
    } else {
        json!({"model":"m","prompt":text,"max_tokens":8})
    })
    .unwrap()
}
async fn mount(upstream: &MockServer, route: &str) {
    let result = if route.ends_with("embeddings") {
        json!({"object":"list","data":[{"object":"embedding","embedding":[0.1,0.2],"index":0}],"model":"m","usage":{"prompt_tokens":9,"total_tokens":9}})
    } else {
        json!({"id":"c","object":"text_completion","choices":[{"index":0,"text":"ok","finish_reason":"stop"}],"model":"m","usage":{"prompt_tokens":9,"completion_tokens":2,"total_tokens":11}})
    };
    Mock::given(method("POST"))
        .and(path(route.trim_start_matches("/v1")))
        .and(header("authorization", "Bearer upstream-secret"))
        .respond_with(ResponseTemplate::new(200).set_body_json(result))
        .mount(upstream)
        .await;
}
#[tokio::test]
async fn bytes_routes_usage_and_identity_are_preserved_for_both_endpoints() {
    for route in ["/v1/embeddings", "/v1/completions"] {
        let (state, sink, upstream, _dir) = setup(500, 100, None, false).await;
        mount(&upstream, route).await;
        let raw = if route.ends_with("embeddings") {
            br#"{ "model":"m", "input":["hello", "second"], "encoding_format":"float" }"#.to_vec()
        } else {
            br#"{ "model":"m", "prompt":["hello", "second"], "max_tokens":8 }"#.to_vec()
        };
        let response = build_app(state.clone())
            .oneshot(req(route, &raw, true, false))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK, "{route}");
        assert!(response.headers().contains_key("x-sandhi-policy-receipt"));
        let body: Value =
            serde_json::from_slice(&to_bytes(response.into_body(), 8192).await.unwrap()).unwrap();
        assert!(body
            .get(if route.ends_with("embeddings") {
                "data"
            } else {
                "choices"
            })
            .is_some());
        assert_eq!(upstream.received_requests().await.unwrap()[0].body, raw);
        let events = sink.events();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].subject_id.as_deref(), Some("alice"));
        assert_eq!(events[0].group_id.as_deref(), Some("team"));
        assert_eq!(events[0].route.as_deref(), Some(route));
        assert_eq!(events[0].tokens_in, 9);
        assert_eq!(
            events[0].tokens_out,
            if route.ends_with("embeddings") { 0 } else { 2 }
        );
        assert_eq!(
            state.ledger.lock().unwrap().spent("test"),
            if route.ends_with("embeddings") { 9 } else { 11 }
        );
    }
}
#[tokio::test]
async fn authentication_scope_and_content_denials_never_reach_upstream() {
    for route in ["/v1/embeddings", "/v1/completions"] {
        let (state, sink, upstream, _dir) = setup(10000, 100, None, false).await;
        let app = build_app(state.clone());
        let benign = payload(route, "hello");
        for (raw, token, spoof, status) in [
            (benign.clone(), false, false, StatusCode::UNAUTHORIZED),
            (benign.clone(), true, true, StatusCode::FORBIDDEN),
            (
                String::from_utf8(benign.clone())
                    .unwrap()
                    .replace("\"m\"", "\"other\"")
                    .into_bytes(),
                true,
                false,
                StatusCode::FORBIDDEN,
            ),
            (
                payload(route, "SECRET-123"),
                true,
                false,
                StatusCode::FORBIDDEN,
            ),
            (
                payload(route, "QUARANTINE"),
                true,
                false,
                StatusCode::FORBIDDEN,
            ),
        ] {
            let r = app
                .clone()
                .oneshot(req(route, &raw, token, spoof))
                .await
                .unwrap();
            assert_eq!(r.status(), status, "{route}");
        }
        assert!(upstream.received_requests().await.unwrap().is_empty());
        assert!(sink.events().is_empty());
        assert_eq!(state.ledger.lock().unwrap().spent("test"), 0);
    }
}
#[tokio::test]
async fn text_in_every_batch_suffix_and_user_is_inspected() {
    let (state, _, upstream, _dir) = setup(10000, 100, None, false).await;
    let app = build_app(state);
    for (route, body) in [
        (
            "/v1/embeddings",
            json!({"model":"m","input":["safe","SECRET-1"]}),
        ),
        (
            "/v1/embeddings",
            json!({"model":"m","input":"safe","user":"SECRET-1"}),
        ),
        (
            "/v1/completions",
            json!({"model":"m","prompt":["safe","SECRET-1"],"max_tokens":8}),
        ),
        (
            "/v1/completions",
            json!({"model":"m","prompt":"safe","suffix":"SECRET-1","max_tokens":8}),
        ),
        (
            "/v1/completions",
            json!({"model":"m","prompt":"safe","stop":["SECRET-1"],"max_tokens":8}),
        ),
    ] {
        let r = app
            .clone()
            .oneshot(req(route, &serde_json::to_vec(&body).unwrap(), true, false))
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::FORBIDDEN);
    }
    assert!(upstream.received_requests().await.unwrap().is_empty());
}
#[tokio::test]
async fn unsupported_and_ambiguous_inputs_are_rejected_before_egress() {
    let (state, _, upstream, _dir) = setup(10000, 100, None, false).await;
    let app = build_app(state);
    for (route, body) in [
        ("/v1/embeddings", r#"{"model":"m","input":[1,2]}"#),
        ("/v1/embeddings", r#"{"model":"m","input":[]}"#),
        (
            "/v1/embeddings",
            r#"{"model":"m","input":"safe","input":"SECRET-1"}"#,
        ),
        (
            "/v1/embeddings",
            r#"{"model":"m","input":"safe","stream":true}"#,
        ),
        (
            "/v1/embeddings",
            r#"{"model":"m","input":"safe","extra":"SECRET-1"}"#,
        ),
        (
            "/v1/embeddings",
            r#"{"model":"m","input":"safe","dimensions":0}"#,
        ),
        (
            "/v1/embeddings",
            r#"{"model":"m","input":"safe","encoding_format":"unknown"}"#,
        ),
        (
            "/v1/completions",
            r#"{"model":"m","prompt":[[1,2]],"max_tokens":1}"#,
        ),
        (
            "/v1/completions",
            r#"{"model":"m","prompt":"safe","max_tokens":-1}"#,
        ),
        (
            "/v1/completions",
            r#"{"model":"m","prompt":"safe","max_tokens":1,"n":0}"#,
        ),
        (
            "/v1/completions",
            r#"{"model":"m","prompt":"safe","max_tokens":1,"n":2,"best_of":1}"#,
        ),
        (
            "/v1/completions",
            r#"{"model":"m","prompt":"safe","max_tokens":1,"stream":"false"}"#,
        ),
        (
            "/v1/completions",
            r#"{"model":"m","prompt":"safe","max_tokens":18446744073709551615}"#,
        ),
        (
            "/v1/completions",
            r#"{"model":"m","prompt":"safe","max_tokens":1,"extension":{"private":"SECRET-1"}}"#,
        ),
    ] {
        let r = app
            .clone()
            .oneshot(req(route, body.as_bytes(), true, false))
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::BAD_REQUEST, "{body}");
    }
    assert!(upstream.received_requests().await.unwrap().is_empty());
}
#[tokio::test]
async fn budgets_include_completion_batches_and_alternatives_but_no_embedding_output() {
    let (state, _, upstream, _dir) = setup(500, 100, None, false).await;
    let app = build_app(state);
    mount(&upstream, "/v1/embeddings").await;
    let r = app
        .clone()
        .oneshot(req(
            "/v1/embeddings",
            &payload("/v1/embeddings", "hello"),
            true,
            false,
        ))
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    let raw = br#"{"model":"m","prompt":["a","b"],"max_tokens":100,"n":2,"best_of":3}"#;
    let r = app
        .oneshot(req("/v1/completions", raw, true, false))
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(upstream.received_requests().await.unwrap().len(), 1);
    let (state, _, upstream, _dir) = setup(0, 100, None, false).await;
    let r = build_app(state)
        .oneshot(req(
            "/v1/embeddings",
            &payload("/v1/embeddings", "hello"),
            true,
            false,
        ))
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::TOO_MANY_REQUESTS);
    assert!(upstream.received_requests().await.unwrap().is_empty());
}
#[tokio::test]
async fn completions_without_a_bound_receive_explicit_default_without_chat_translation() {
    let (state, _, upstream, _dir) = setup(500, 100, None, false).await;
    mount(&upstream, "/v1/completions").await;
    let r = build_app(state)
        .oneshot(req(
            "/v1/completions",
            br#"{"model":"m","prompt":"hello","suffix":"end","echo":true}"#,
            true,
            false,
        ))
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    let got: Value = upstream.received_requests().await.unwrap()[0]
        .body_json()
        .unwrap();
    assert_eq!(got["max_tokens"], 16);
    assert_eq!(got["prompt"], "hello");
    assert_eq!(got["suffix"], "end");
    assert_eq!(got["echo"], true);
    assert!(got.get("messages").is_none());
}
#[tokio::test]
async fn incompatible_upstreams_and_full_audit_store_fail_closed() {
    for route in ["/v1/embeddings", "/v1/completions"] {
        let (state, _, upstream, _dir) = setup(500, 100, None, true).await;
        let r = build_app(state)
            .oneshot(req(route, &payload(route, "hello"), true, false))
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::BAD_REQUEST);
        assert!(upstream.received_requests().await.unwrap().is_empty());
        let (state, _, upstream, _dir) = setup(500, 1, None, false).await;
        mount(&upstream, route).await;
        let app = build_app(state);
        let r = app
            .clone()
            .oneshot(req(route, &payload(route, "hello"), true, false))
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::OK);
        let r = app
            .oneshot(req(route, &payload(route, "hello"), true, false))
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(upstream.received_requests().await.unwrap().len(), 1);
    }
}
#[tokio::test]
async fn rate_limit_precedes_second_dispatch() {
    let (state, _, upstream, _dir) = setup(500, 100, Some(1), false).await;
    mount(&upstream, "/v1/embeddings").await;
    let app = build_app(state);
    let raw = payload("/v1/embeddings", "hello");
    let r = app
        .clone()
        .oneshot(req("/v1/embeddings", &raw, true, false))
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    let r = app
        .oneshot(req("/v1/embeddings", &raw, true, false))
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(upstream.received_requests().await.unwrap().len(), 1);
}
#[tokio::test]
async fn completion_sse_is_preserved_and_terminal_usage_is_settled() {
    let (state, sink, upstream, _dir) = setup(500, 100, None, false).await;
    let wire="data: {\"choices\":[{\"text\":\"ok\",\"index\":0}]}\n\ndata: {\"choices\":[],\"usage\":{\"prompt_tokens\":9,\"completion_tokens\":2,\"total_tokens\":11}}\n\ndata: [DONE]\n\n";
    Mock::given(method("POST"))
        .and(path("/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(wire, "text/event-stream"))
        .mount(&upstream)
        .await;
    let r=build_app(state.clone()).oneshot(req("/v1/completions",br#"{"model":"m","prompt":"hello","max_tokens":8,"stream":true,"stream_options":{"include_usage":true}}"#,true,false)).await.unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(to_bytes(r.into_body(), 8192).await.unwrap(), wire);
    assert_eq!(sink.events()[0].tokens_out, 2);
    assert_eq!(state.ledger.lock().unwrap().spent("test"), 11);
}

#[tokio::test]
async fn malformed_optional_controls_cannot_escape_validation() {
    let (state, _, upstream, _dir) = setup(10000, 100, None, false).await;
    let app = build_app(state);
    for (key, value) in [
        ("temperature", json!("hot")),
        ("temperature", json!(3)),
        ("top_p", json!(-1)),
        ("presence_penalty", json!(4)),
        ("frequency_penalty", json!(false)),
        ("user", json!({"secret":"SECRET-1"})),
        ("suffix", json!(7)),
        ("echo", json!(1)),
        ("logprobs", json!(101)),
        ("seed", json!(0.2)),
        ("stop", json!(["a", 2])),
        ("stream_options", json!(false)),
        ("stream_options", json!({"include_usage":true})),
        ("logit_bias", json!([])),
        ("logit_bias", json!({"arbitrary":1})),
        ("logit_bias", json!({"1":101})),
        ("best_of", json!(129)),
    ] {
        let mut body = json!({"model":"m","prompt":"hello","max_tokens":8});
        body[key] = value;
        let response = app
            .clone()
            .oneshot(req(
                "/v1/completions",
                &serde_json::to_vec(&body).unwrap(),
                true,
                false,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{key}");
    }
    for value in [json!({"include_usage":"yes"}), json!({"unknown":true})] {
        let body = json!({"model":"m","prompt":"hello","max_tokens":8,"stream":true,"stream_options":value});
        let response = app
            .clone()
            .oneshot(req(
                "/v1/completions",
                &serde_json::to_vec(&body).unwrap(),
                true,
                false,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }
    assert!(upstream.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn valid_controls_and_base64_vectors_remain_byte_exact() {
    let (state, _, upstream, _dir) = setup(10000, 100, None, false).await;
    let app = build_app(state);
    mount(&upstream, "/v1/completions").await;
    let completion = json!({"model":"m","prompt":["a","b"],"max_tokens":3,"n":2,"best_of":3,
        "temperature":0.5,"top_p":0.8,"presence_penalty":0.1,"frequency_penalty":-0.1,
        "logprobs":3,"echo":false,"stop":["END"],"logit_bias":{"12":1},"seed":-1,"user":"client-label"});
    let raw = serde_json::to_vec(&completion).unwrap();
    let response = app
        .clone()
        .oneshot(req("/v1/completions", &raw, true, false))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(upstream.received_requests().await.unwrap()[0].body, raw);
    let encoded = json!({"data":[{"embedding":"zcxMPs3MTD4=","index":0}],"usage":{"prompt_tokens":2,"total_tokens":2}});
    Mock::given(method("POST"))
        .and(path("/embeddings"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&encoded))
        .mount(&upstream)
        .await;
    let raw = br#"{"model":"m","input":"hello","dimensions":2,"encoding_format":"base64"}"#;
    let response = app
        .oneshot(req("/v1/embeddings", raw, true, false))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), 8192).await.unwrap()).unwrap();
    assert_eq!(body, encoded);
    assert_eq!(upstream.received_requests().await.unwrap()[1].body, raw);
}

#[tokio::test]
async fn shared_completion_suffix_is_reserved_for_each_prompt() {
    let (state, _, upstream, _dir) = setup(2000, 100, None, false).await;
    let body = json!({"model":"m","prompt":vec!["";128],"suffix":"x".repeat(4096),"max_tokens":0});
    let response = build_app(state)
        .oneshot(req(
            "/v1/completions",
            &serde_json::to_vec(&body).unwrap(),
            true,
            false,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    assert!(upstream.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn text_shape_validation_is_required_even_without_policy() {
    let (mut state, _, upstream, _dir) = setup(500, 100, None, false).await;
    Arc::get_mut(&mut state).unwrap().policy = None;
    let app = build_app(state);
    for (route, raw) in [
        (
            "/v1/embeddings",
            br#"{"model":"m","input":[1,2]}"#.as_slice(),
        ),
        (
            "/v1/completions",
            br#"{"model":"m","prompt":"first","prompt":"second"}"#.as_slice(),
        ),
        (
            "/v1/completions",
            br#"{"model":"m","prompt":"hello","extra":"opaque"}"#.as_slice(),
        ),
    ] {
        let response = app
            .clone()
            .oneshot(req(route, raw, true, false))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }
    assert!(upstream.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn completion_stream_without_usage_is_explicitly_estimated_even_when_cancelled() {
    use futures_util::StreamExt;
    for cancel in [false, true] {
        let (state, sink, upstream, _dir) = setup(10000, 100, None, false).await;
        let wire="data: {\"choices\":[{\"text\":\"some generated text\",\"index\":0}]}\n\ndata: [DONE]\n\n";
        Mock::given(method("POST"))
            .and(path("/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(wire, "text/event-stream"))
            .mount(&upstream)
            .await;
        let response = build_app(state.clone())
            .oneshot(req(
                "/v1/completions",
                br#"{"model":"m","prompt":"hello","max_tokens":32,"stream":true}"#,
                true,
                false,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        if cancel {
            let mut chunks = response.into_body().into_data_stream();
            assert!(chunks.next().await.unwrap().is_ok());
            drop(chunks);
        } else {
            let _ = to_bytes(response.into_body(), 8192).await.unwrap();
        }
        let events = sink.events();
        assert_eq!(events.len(), 1);
        assert_eq!(
            events[0].usage_completeness,
            sandhi_core::UsageCompleteness::Partial
        );
        assert_eq!(events[0].usage_basis, sandhi_core::UsageBasis::Estimated);
        assert!(state.ledger.lock().unwrap().spent("test") > 0);
    }
}
