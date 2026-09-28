use sandhi_core::policy::{Disposition, Engine, Identity, Registry};
use sandhi_proxy::policy_remote::register_remotes;
use serde_json::{json, Value};
use std::{
    fs,
    sync::Arc,
    time::{Duration, Instant},
};
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn manifest(uris: &[String]) -> (tempfile::TempDir, Value) {
    let dir = tempfile::tempdir().unwrap();
    let key = dir.path().join("service.key");
    fs::write(&key, "synthetic_evaluator_service_key_12345").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&key, fs::Permissions::from_mode(0o600)).unwrap();
    }
    let endpoints: Vec<_> = uris
        .iter()
        .map(|u| json!({"url":u,"auth_file":key}))
        .collect();
    (
        dir,
        json!({"version":1,"evaluators":[{"name":"test.remote.v1", "artifact_sha256":"a".repeat(64), "code_sha256":"b".repeat(64), "timeout_ms":150,"replicas":endpoints}]}),
    )
}
fn engine(m: &Value) -> Engine {
    let mut r = Registry::default();
    register_remotes(&mut r, &serde_json::to_vec(m).unwrap()).unwrap();
    Engine::from_slice_with_registry(&serde_json::to_vec(&json!({"schema_version":"1","revision":1,"deadline_ms":200,"max_body_bytes":65536,"rules":[{"id":"remote","effect":"block","evaluator":{"kind":"registered","name":"test.remote.v1","configuration":{"at_least":0.5}}}]})).unwrap(),&r).unwrap()
}
fn run(e: &Engine, text: &str) -> Disposition {
    e.evaluate(
        &serde_json::to_vec(&json!({"messages":[{"role":"user","content":text}]})).unwrap(),
        &Identity::default(),
        "openai",
        "test",
        Instant::now() + Duration::from_millis(200),
    )
    .disposition
}
fn reply(request: &wiremock::Request) -> ResponseTemplate {
    let v: Value = serde_json::from_slice(&request.body).unwrap();
    assert_eq!(v.as_object().unwrap().len(), 8);
    assert!(v["timeout_ms"].as_u64().unwrap() <= 150);
    ResponseTemplate::new(200).set_body_json(json!({"version":1,"id":v["id"],"evaluator":v["evaluator"],"artifact_sha256":v["artifact_sha256"],"code_sha256":v["code_sha256"],"score":if v["text"].as_str().unwrap().contains("restricted"){1.0}else{0.0}}))
}
async fn good(server: &MockServer) {
    Mock::given(method("POST"))
        .and(path("/v1/evaluators/test.remote.v1/evaluate"))
        .and(header(
            "authorization",
            "Bearer synthetic_evaluator_service_key_12345",
        ))
        .respond_with(reply)
        .mount(server)
        .await;
}
#[tokio::test]
async fn remote_routes_replicas_and_binds_score_provenance() {
    let a = MockServer::start().await;
    let b = MockServer::start().await;
    good(&a).await;
    good(&b).await;
    let (_d, m) = manifest(&[a.uri(), b.uri()]);
    let e = Arc::new(engine(&m));
    for text in ["weather", "restricted", "weather", "restricted"] {
        let e = e.clone();
        let t = text.to_owned();
        assert_eq!(
            tokio::task::spawn_blocking(move || run(&e, &t))
                .await
                .unwrap(),
            if text == "weather" {
                Disposition::Forward
            } else {
                Disposition::Block
            }
        );
    }
    assert_eq!(a.received_requests().await.unwrap().len(), 2);
    assert_eq!(b.received_requests().await.unwrap().len(), 2);
}
#[tokio::test]
async fn failed_replica_is_excluded_without_replay() {
    let a = MockServer::start().await;
    let b = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&a)
        .await;
    good(&b).await;
    let (_d, m) = manifest(&[a.uri(), b.uri()]);
    let e = Arc::new(engine(&m));
    let x = e.clone();
    assert_eq!(
        tokio::task::spawn_blocking(move || run(&x, "weather"))
            .await
            .unwrap(),
        Disposition::Unavailable
    );
    assert!(b.received_requests().await.unwrap().is_empty());
    for _ in 0..3 {
        let x = e.clone();
        assert_eq!(
            tokio::task::spawn_blocking(move || run(&x, "weather"))
                .await
                .unwrap(),
            Disposition::Forward
        );
    }
    assert_eq!(a.received_requests().await.unwrap().len(), 1);
    assert_eq!(b.received_requests().await.unwrap().len(), 3);
}
#[tokio::test]
async fn remote_rejects_redirects_bad_frames_provenance_and_deadline() {
    let target = MockServer::start().await;
    good(&target).await;
    let cases=vec![ResponseTemplate::new(302).insert_header("location",target.uri()),
        ResponseTemplate::new(200).set_body_string("x".repeat(4097)),
        ResponseTemplate::new(200).set_body_raw("{\"score\":NaN}","application/json"),
        ResponseTemplate::new(200).set_body_json(json!({"version":1,"id":"1","evaluator":"test.remote.v1","artifact_sha256":"a".repeat(64),"code_sha256":"b".repeat(64),"score":0,"allow":true})),
        ResponseTemplate::new(200).set_body_json(json!({"version":1,"id":"stale","evaluator":"test.remote.v1","artifact_sha256":"a".repeat(64),"code_sha256":"b".repeat(64),"score":0})),
        ResponseTemplate::new(200).set_body_json(json!({"version":1,"id":"1","evaluator":"test.remote.v1","artifact_sha256":"c".repeat(64),"code_sha256":"b".repeat(64),"score":0})),
        ResponseTemplate::new(200).set_body_json(json!({"version":1,"id":"1","evaluator":"test.remote.v1","artifact_sha256":"a".repeat(64),"code_sha256":"b".repeat(64),"score":2})),
        ResponseTemplate::new(200).set_delay(Duration::from_millis(500))];
    for response in cases {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(response)
            .mount(&server)
            .await;
        let (_d, m) = manifest(&[server.uri()]);
        let e = engine(&m);
        let start = Instant::now();
        assert_eq!(
            tokio::task::spawn_blocking(move || run(&e, "private synthetic"))
                .await
                .unwrap(),
            Disposition::Unavailable
        );
        assert!(start.elapsed() < Duration::from_millis(400));
    }
    assert!(target.received_requests().await.unwrap().is_empty());
}
#[test]
fn remote_configuration_rejects_unsafe_destinations_and_unbounded_resources() {
    for url in [
        "http://example.com",
        "http://localhost:9088",
        "https://user:pass@example.com",
        "https://example.com/path",
        "https://example.com/?token=x",
        "file:///tmp/model",
        "https://example.com/#fragment",
    ] {
        let (_d, m) = manifest(&[url.into()]);
        assert!(
            register_remotes(&mut Registry::default(), &serde_json::to_vec(&m).unwrap()).is_err(),
            "{url}"
        );
    }
    let (_d, mut m) = manifest(&["https://example.com".into()]);
    m["evaluators"][0]["replicas"] = json!((0..5)
        .map(|_| m["evaluators"][0]["replicas"][0].clone())
        .collect::<Vec<_>>());
    assert!(register_remotes(&mut Registry::default(), &serde_json::to_vec(&m).unwrap()).is_err());
}
#[tokio::test]
async fn saturation_rejects_immediately_and_expired_requests_never_egress() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_millis(500)))
        .mount(&server)
        .await;
    let (_d, m) = manifest(&[server.uri()]);
    let e = Arc::new(engine(&m));
    let x = e.clone();
    let first = tokio::task::spawn_blocking(move || run(&x, "weather"));
    let start = Instant::now();
    while server.received_requests().await.unwrap().is_empty() {
        assert!(start.elapsed() < Duration::from_secs(1));
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    let start = Instant::now();
    let x = e.clone();
    assert_eq!(
        tokio::task::spawn_blocking(move || run(&x, "weather"))
            .await
            .unwrap(),
        Disposition::Unavailable
    );
    assert!(start.elapsed() < Duration::from_millis(100));
    assert_eq!(first.await.unwrap(), Disposition::Unavailable);
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

#[test]
#[ignore = "requires generated template deployments and running Flask replicas"]
fn remote_real_flask_and_local_zipapp_share_policy_results() {
    let path = std::env::var("SANDHI_REMOTE_TEST_MANIFEST").unwrap();
    let m: Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
    // A missing trust root or client identity must fail before any clean finding.
    let mut untrusted = m.clone();
    untrusted["evaluators"][0]["replicas"] = json!([m["evaluators"][0]["replicas"][0].clone()]);
    untrusted["evaluators"][0]["replicas"][0]
        .as_object_mut()
        .unwrap()
        .remove("ca_file");
    assert_eq!(
        run(&engine(&untrusted), "weather"),
        Disposition::Unavailable
    );
    let mut no_identity = m.clone();
    no_identity["evaluators"][0]["replicas"] = json!([m["evaluators"][0]["replicas"][1].clone()]);
    no_identity["evaluators"][0]["replicas"][0]
        .as_object_mut()
        .unwrap()
        .remove("identity_file");
    assert_eq!(
        run(&engine(&no_identity), "weather"),
        Disposition::Unavailable
    );
    let e = engine(&m);
    for _ in 0..4 {
        assert_eq!(run(&e, "weather"), Disposition::Forward);
        assert_eq!(run(&e, "SYNTHETIC_RESTRICTED"), Disposition::Block);
    }
    fn benchmark(engine: &Engine, label: &str) -> Value {
        let mut times = Vec::new();
        for i in 0..100 {
            let text = if i % 2 == 0 {
                "weather"
            } else {
                "SYNTHETIC_RESTRICTED"
            };
            let start = Instant::now();
            assert_eq!(
                run(engine, text),
                if i % 2 == 0 {
                    Disposition::Forward
                } else {
                    Disposition::Block
                }
            );
            times.push(start.elapsed().as_micros() as u64);
        }
        times.sort_unstable();
        json!({"backend":label,"samples":100,"p50_us":times[49],"p95_us":times[94],"p99_us":times[98],"max_us":times[99]})
    }
    let remote_times = benchmark(&e, "remote_tls_two_replicas");
    let workers = fs::read(std::env::var("SANDHI_WORKER_TEST_MANIFEST").unwrap()).unwrap();
    let mut registry = Registry::default();
    sandhi_proxy::policy_workers::register_workers(&mut registry, &workers).unwrap();
    let e=Engine::from_slice_with_registry(&serde_json::to_vec(&json!({"schema_version":"1","revision":1,"deadline_ms":200,"max_body_bytes":65536,"rules":[{"id":"worker","effect":"block","evaluator":{"kind":"registered","name":"test.remote.v1","configuration":{"at_least":0.5}}}]})).unwrap(),&registry).unwrap();
    assert_eq!(run(&e, "weather"), Disposition::Forward);
    assert_eq!(run(&e, "SYNTHETIC_RESTRICTED"), Disposition::Block);
    let local_times = benchmark(&e, "local_python_zipapp");
    let native=Engine::from_slice(&serde_json::to_vec(&json!({"schema_version":"1","revision":1,"deadline_ms":200,"max_body_bytes":65536,"rules":[{"id":"native","effect":"block","evaluator":{"kind":"regex","pattern":"SYNTHETIC_RESTRICTED"}}]})).unwrap()).unwrap();
    let native_times = benchmark(&native, "native_regex");
    println!(
        "evaluator_comparison={}",
        json!([native_times, local_times, remote_times])
    );
}

#[tokio::test]
async fn remote_findings_and_failures_hold_zero_provider_calls() {
    use axum::{
        body::{to_bytes, Body},
        http::{Request, StatusCode},
    };
    use sandhi_core::{InMemorySink, KeyStore, VirtualKey};
    use sandhi_providers::ProviderRuntime;
    use sandhi_proxy::{build_app, policy::PolicyGate, ProxyLedger, ProxyState};
    use sandhi_store::policy::PolicyAuditStore;
    use std::collections::HashMap;
    use tower::ServiceExt;
    for failed in [false, true] {
        let remote = MockServer::start().await;
        if failed {
            Mock::given(method("POST"))
                .respond_with(ResponseTemplate::new(503))
                .mount(&remote)
                .await;
        } else {
            good(&remote).await;
        }
        let upstream = MockServer::start().await;
        let (dir, m) = manifest(&[remote.uri()]);
        let keys = KeyStore::new();
        keys.insert(VirtualKey {
            id: "client".into(),
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
            engine(&m),
            Arc::new(PolicyAuditStore::open(&dir.path().join("audit.db"), 100).unwrap()),
        )));
        let app = build_app(Arc::new(state));
        let body=serde_json::to_vec(&json!({"model":"test","max_tokens":4,"messages":[{"role":"user","content":"restricted"}]})).unwrap();
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/v1/chat/completions")
                    .method("POST")
                    .header("authorization", "Bearer client")
                    .header("content-type", "application/json")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            if failed {
                StatusCode::SERVICE_UNAVAILABLE
            } else {
                StatusCode::FORBIDDEN
            }
        );
        assert!(
            !String::from_utf8_lossy(&to_bytes(response.into_body(), 8192).await.unwrap())
                .contains("restricted")
        );
        assert!(upstream.received_requests().await.unwrap().is_empty());
        let requests = remote.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(
            requests[0].headers.get("authorization").unwrap(),
            "Bearer synthetic_evaluator_service_key_12345"
        );
    }
}

#[test]
fn remote_private_credentials_and_optional_tls_material_are_validated() {
    for mutation in [
        "public_key",
        "symlink",
        "short_key",
        "bad_ca",
        "bad_identity",
        "bad_hash",
        "zero_timeout",
        "extra_field",
    ] {
        let (d, mut m) = manifest(&["https://example.com".into()]);
        let key = d.path().join("service.key");
        match mutation {
            "public_key" => {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    fs::set_permissions(&key, fs::Permissions::from_mode(0o644)).unwrap();
                }
            }
            "symlink" => {
                #[cfg(unix)]
                {
                    let link = d.path().join("link");
                    std::os::unix::fs::symlink(&key, &link).unwrap();
                    m["evaluators"][0]["replicas"][0]["auth_file"] = json!(link);
                }
            }
            "short_key" => fs::write(&key, "short").unwrap(),
            "bad_ca" => {
                m["evaluators"][0]["replicas"][0]["ca_file"] = json!(key);
            }
            "bad_identity" => {
                m["evaluators"][0]["replicas"][0]["identity_file"] = json!(key);
            }
            "bad_hash" => {
                m["evaluators"][0]["artifact_sha256"] = json!("z".repeat(64));
            }
            "zero_timeout" => {
                m["evaluators"][0]["timeout_ms"] = json!(0);
            }
            _ => {
                m["evaluators"][0]["roles"] = json!(["admin"]);
            }
        }
        assert!(
            register_remotes(&mut Registry::default(), &serde_json::to_vec(&m).unwrap()).is_err(),
            "{mutation}"
        );
    }
}

#[tokio::test]
async fn local_denial_prevents_remote_text_egress_even_when_remote_rule_is_first() {
    let server = MockServer::start().await;
    good(&server).await;
    let (_dir, m) = manifest(&[server.uri()]);
    for effect in ["block", "quarantine"] {
        let mut registry = Registry::default();
        register_remotes(&mut registry, &serde_json::to_vec(&m).unwrap()).unwrap();
        let e=Engine::from_slice_with_registry(&serde_json::to_vec(&json!({"schema_version":"1","revision":1,"deadline_ms":200,"max_body_bytes":65536,"rules":[
            {"id":"remote","effect":"block","evaluator":{"kind":"registered","name":"test.remote.v1","configuration":{"at_least":0.5}}},
            {"id":"local","effect":effect,"evaluator":{"kind":"regex","pattern":"restricted"}}
        ]})).unwrap(),&registry).unwrap();
        assert_eq!(
            tokio::task::spawn_blocking(move || run(&e, "restricted"))
                .await
                .unwrap(),
            if effect == "block" {
                Disposition::Block
            } else {
                Disposition::Quarantine
            }
        );
    }
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn remote_denial_stops_later_remote_egress_and_unknown_group_identity_stops_all() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(reply)
        .mount(&server)
        .await;
    let (_dir, mut m) = manifest(&[server.uri()]);
    let mut second = m["evaluators"][0].clone();
    second["name"] = json!("test.second.v1");
    m["evaluators"].as_array_mut().unwrap().push(second);
    let mut registry = Registry::default();
    register_remotes(&mut registry, &serde_json::to_vec(&m).unwrap()).unwrap();
    let rules = json!([
        {"id":"first","effect":"block","evaluator":{"kind":"registered","name":"test.remote.v1","configuration":{"at_least":0.5}}},
        {"id":"second","effect":"audit","evaluator":{"kind":"registered","name":"test.second.v1","configuration":{"at_least":0.5}}}
    ]);
    let mut policy = json!({"schema_version":"1","revision":1,"deadline_ms":200,"max_body_bytes":65536,"rules":rules});
    let e =
        Engine::from_slice_with_registry(&serde_json::to_vec(&policy).unwrap(), &registry).unwrap();
    assert_eq!(
        tokio::task::spawn_blocking(move || run(&e, "restricted"))
            .await
            .unwrap(),
        Disposition::Block
    );
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
    server.reset().await;
    policy["rules"][0]["when"] = json!({"groups":["reviewers"]});
    let e =
        Engine::from_slice_with_registry(&serde_json::to_vec(&policy).unwrap(), &registry).unwrap();
    assert_eq!(
        tokio::task::spawn_blocking(move || run(&e, "weather"))
            .await
            .unwrap(),
        Disposition::Unavailable
    );
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn replica_recovers_after_cooldown_without_replaying_failed_text() {
    let a = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&a)
        .await;
    let (_d, m) = manifest(&[a.uri()]);
    let e = Arc::new(engine(&m));
    let x = e.clone();
    assert_eq!(
        tokio::task::spawn_blocking(move || run(&x, "first failed request"))
            .await
            .unwrap(),
        Disposition::Unavailable
    );
    a.reset().await;
    good(&a).await;
    let x = e.clone();
    assert_eq!(
        tokio::task::spawn_blocking(move || run(&x, "during cooldown"))
            .await
            .unwrap(),
        Disposition::Unavailable
    );
    assert!(a.received_requests().await.unwrap().is_empty());
    tokio::time::sleep(Duration::from_millis(1100)).await;
    let x = e.clone();
    assert_eq!(
        tokio::task::spawn_blocking(move || run(&x, "new healthy request"))
            .await
            .unwrap(),
        Disposition::Forward
    );
    let requests = a.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1);
    assert!(String::from_utf8_lossy(&requests[0].body).contains("new healthy request"));
    assert!(!String::from_utf8_lossy(&requests[0].body).contains("first failed request"));
}

#[tokio::test]
async fn concurrent_replicas_have_fixed_capacity_without_pending_work() {
    let a = MockServer::start().await;
    let b = MockServer::start().await;
    for s in [&a, &b] {
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_millis(500)))
            .mount(s)
            .await;
    }
    let (_d, m) = manifest(&[a.uri(), b.uri()]);
    let e = Arc::new(engine(&m));
    let x = e.clone();
    let first = tokio::task::spawn_blocking(move || run(&x, "first"));
    let x = e.clone();
    let second = tokio::task::spawn_blocking(move || run(&x, "second"));
    let start = Instant::now();
    while a.received_requests().await.unwrap().is_empty()
        || b.received_requests().await.unwrap().is_empty()
    {
        assert!(start.elapsed() < Duration::from_secs(1));
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    let start = Instant::now();
    let mut calls = Vec::new();
    for _ in 0..8 {
        let x = e.clone();
        calls.push(tokio::task::spawn_blocking(move || run(&x, "overflow")));
    }
    for call in calls {
        assert_eq!(call.await.unwrap(), Disposition::Unavailable);
    }
    assert!(start.elapsed() < Duration::from_millis(100));
    assert_eq!(first.await.unwrap(), Disposition::Unavailable);
    assert_eq!(second.await.unwrap(), Disposition::Unavailable);
    assert_eq!(a.received_requests().await.unwrap().len(), 1);
    assert_eq!(b.received_requests().await.unwrap().len(), 1);
}
