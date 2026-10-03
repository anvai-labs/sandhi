#![cfg(feature = "policy-onnx")]
use sandhi_core::policy::{Disposition, Engine, Identity, Registry};
use sandhi_proxy::policy_onnx::register_models;
use serde_json::{json, Value};
use std::{
    path::PathBuf,
    time::{Duration, Instant},
};
fn manifest() -> Vec<u8> {
    std::fs::read(std::env::var("SANDHI_ONNX_TEST_MANIFEST").expect("test deployment manifest"))
        .unwrap()
}
fn engine(m: &[u8]) -> Engine {
    let mut registry = Registry::default();
    register_models(&mut registry, m).unwrap();
    Engine::from_slice_with_registry(
        &serde_json::to_vec(&json!({"schema_version":"1","revision":1,
 "deadline_ms":200,"max_body_bytes":65536,"rules":[{"id":"onnx","effect":"quarantine",
 "evaluator":{"kind":"registered","name":"onnx.tfidf.v1","configuration":{"at_least":0.6}}}]}))
        .unwrap(),
        &registry,
    )
    .unwrap()
}
fn run(e: &Engine, text: &str) -> Disposition {
    let body =
        serde_json::to_vec(&json!({"model":"test","messages":[{"role":"user","content":text}]}))
            .unwrap();
    e.evaluate(
        &body,
        &Identity::default(),
        "openai",
        "test",
        Instant::now() + Duration::from_millis(200),
    )
    .disposition
}
#[test]
#[ignore = "requires pinned runtime and generated SANDHI_ONNX_TEST_MANIFEST"]
fn embedded_onnx_parity_and_unsupported_preprocessing_fail_closed() {
    let m = manifest();
    let e = engine(&m);
    assert_eq!(
        run(&e, "restricted customer records"),
        Disposition::Quarantine
    );
    assert_eq!(run(&e, "weather forecast"), Disposition::Forward);
    assert_eq!(run(&e, "privaté credentials"), Disposition::Unavailable);
    // Exported Python golden scores and the actual Rust preprocessing/runtime.
    let path = PathBuf::from(std::env::var("SANDHI_ONNX_TEST_MANIFEST").unwrap());
    let golden: Vec<Value> =
        serde_json::from_slice(&std::fs::read(path.with_file_name("golden.json")).unwrap())
            .unwrap();
    let backend = sandhi_proxy::policy_onnx::load_backend(&m).unwrap();
    use sandhi_core::policy::Inspection;
    for item in golden {
        let text = item["text"].as_str().unwrap().to_string();
        let joined = item["joined"].as_str().unwrap().to_string();
        let input = Inspection {
            body_bytes: text.len(),
            text,
            joined,
            max_output_tokens: None,
        };
        let actual = backend
            .score(&input, Instant::now() + Duration::from_secs(1))
            .unwrap();
        assert!((actual - item["score"].as_f64().unwrap()).abs() < 1e-6);
    }
}
#[test]
#[ignore = "requires pinned runtime and generated SANDHI_ONNX_TEST_MANIFEST"]
fn embedded_onnx_rejects_tampered_model_and_deadlines() {
    let m = manifest();
    for field in ["model_sha256", "preprocessing_sha256"] {
        let mut bad: Value = serde_json::from_slice(&m).unwrap();
        bad["models"][0][field] = json!("0".repeat(64));
        assert!(
            register_models(&mut Registry::default(), &serde_json::to_vec(&bad).unwrap()).is_err()
        );
    }
    let e = engine(&m);
    let body = br#"{"model":"test","messages":[{"role":"user","content":"weather"}]}"#;
    assert_eq!(
        e.evaluate(body, &Identity::default(), "openai", "test", Instant::now())
            .disposition,
        Disposition::Unavailable
    );
}

#[tokio::test]
#[ignore = "requires pinned runtime, generated model and slow.onnx test fixture"]
async fn embedded_onnx_inflight_timeout_and_router_denial_never_dispatch() {
    use axum::{
        body::{to_bytes, Body},
        http::{Request, StatusCode},
    };
    use sandhi_core::{InMemorySink, KeyStore, VirtualKey};
    use sandhi_providers::ProviderRuntime;
    use sandhi_proxy::{build_app, policy::PolicyGate, ProxyLedger, ProxyState};
    use sandhi_store::policy::PolicyAuditStore;
    use sha2::{Digest, Sha256};
    use std::{collections::HashMap, sync::Arc};
    use tower::ServiceExt;
    use wiremock::MockServer;
    let original: Value = serde_json::from_slice(&manifest()).unwrap();
    for mode in ["normal", "slow"] {
        let mut m = original.clone();
        if mode == "slow" {
            let path = PathBuf::from(std::env::var("SANDHI_ONNX_TEST_MANIFEST").unwrap())
                .with_file_name("slow.onnx");
            let bytes = std::fs::read(&path).unwrap();
            m["models"][0]["model"] = json!(path);
            m["models"][0]["model_sha256"] = json!(format!("{:x}", Sha256::digest(bytes)));
        }
        let temp = tempfile::tempdir().unwrap();
        let upstream = MockServer::start().await;
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
            engine(&serde_json::to_vec(&m).unwrap()),
            Arc::new(PolicyAuditStore::open(&temp.path().join("audit.db"), 100).unwrap()),
        )));
        let app = build_app(Arc::new(state));
        for text in ["restricted customer records", "privaté credentials"] {
            let body = serde_json::to_vec(
                &json!({"model":"test","max_tokens":4,"messages":[{"role":"user","content":text}]}),
            )
            .unwrap();
            let request = Request::builder()
                .uri("/v1/chat/completions")
                .method("POST")
                .header("authorization", "Bearer client")
                .header("content-type", "application/json")
                .body(Body::from(body))
                .unwrap();
            let start = Instant::now();
            let response = app.clone().oneshot(request).await.unwrap();
            assert_eq!(
                response.status(),
                if mode == "normal" && text.is_ascii() {
                    StatusCode::FORBIDDEN
                } else {
                    StatusCode::SERVICE_UNAVAILABLE
                }
            );
            assert!(start.elapsed() < Duration::from_secs(2));
            let bytes = to_bytes(response.into_body(), 8192).await.unwrap();
            assert!(!String::from_utf8_lossy(&bytes).contains(text));
            assert!(upstream.received_requests().await.unwrap().is_empty());
        }
        if mode == "slow" {
            // This synchronous call returns only when native inference has actually ended.
            let backend =
                sandhi_proxy::policy_onnx::load_backend(&serde_json::to_vec(&m).unwrap()).unwrap();
            let mut input = sandhi_core::policy::Inspection {
                text: "restricted".into(),
                joined: "".into(),
                body_bytes: 10,
                max_output_tokens: None,
            };
            let start = Instant::now();
            assert!(backend
                .score(&input, start + Duration::from_millis(20))
                .is_err());
            assert!(start.elapsed() < Duration::from_secs(2));
            // Cancellation does not poison later inference or leave the model slot occupied.
            input.text = "weather".into();
            assert_eq!(
                backend
                    .score(&input, Instant::now() + Duration::from_secs(1))
                    .unwrap(),
                0.
            );
        }
    }
}

#[test]
#[ignore = "requires ONNX test manifest, SANDHI_ONNX_TEST_PYTHON and SANDHI_ONNX_TEST_ARTIFACT"]
fn embedded_onnx_and_python_worker_comparison() {
    use sha2::{Digest, Sha256};
    let source = PathBuf::from(std::env::var("SANDHI_ONNX_TEST_ARTIFACT").unwrap());
    let worker = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../scripts/policy_text_worker.py")
        .canonicalize()
        .unwrap();
    let deployment = json!({"version":1,"workers":[{"name":"onnx.tfidf.v1",
 "python":std::env::var("SANDHI_ONNX_TEST_PYTHON").unwrap(),
 "script":worker,"script_sha256":format!("{:x}",Sha256::digest(std::fs::read(&worker).unwrap())),
 "artifact":source,"artifact_sha256":format!("{:x}",Sha256::digest(std::fs::read(&source).unwrap())),
 "pool_size":1,"startup_timeout_ms":10000}]});
    let mut registry = Registry::default();
    sandhi_proxy::policy_workers::register_workers(
        &mut registry,
        &serde_json::to_vec(&deployment).unwrap(),
    )
    .unwrap();
    let policy = serde_json::to_vec(&json!({"schema_version":"1","revision":1,
 "deadline_ms":200,"max_body_bytes":65536,"rules":[{"id":"model","effect":"quarantine",
 "evaluator":{"kind":"registered","name":"onnx.tfidf.v1","configuration":{"at_least":0.6}}}]}))
    .unwrap();
    let python = Engine::from_slice_with_registry(&policy, &registry).unwrap();
    let native = engine(&manifest());
    let mut results = Vec::new();
    for (name, e) in [("embedded_onnx", &native), ("python_worker", &python)] {
        let mut elapsed = Vec::new();
        for i in 0..105 {
            let text = if i % 2 == 0 {
                "restricted customer records"
            } else {
                "weather forecast"
            };
            let start = Instant::now();
            assert_eq!(
                run(e, text),
                if i % 2 == 0 {
                    Disposition::Quarantine
                } else {
                    Disposition::Forward
                }
            );
            if i >= 5 {
                elapsed.push(start.elapsed().as_micros() as u64)
            }
        }
        elapsed.sort();
        results.push(json!({"backend":name,"samples":elapsed.len(),
   "p50_us":elapsed[49],"p95_us":elapsed[94],"p99_us":elapsed[98],"max_us":elapsed[99]}));
    }
    println!(
        "EVALUATOR_BENCHMARK={}",
        serde_json::to_string(&results).unwrap()
    );
}
