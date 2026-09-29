#![cfg(unix)]
use sandhi_core::policy::{Disposition, Engine, Identity, Registry};
use sandhi_proxy::policy_workers::register_workers;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{
    fs,
    time::{Duration, Instant},
};
const FIXTURE: &str = r#"import sys,json,os,time,hashlib
p=sys.argv[1]
b=open(p,'rb').read()
a=json.loads(b)
open(a['pid_path'],'w').write(str(os.getpid()))
if a['mode']=='startup_hang': time.sleep(10)
if a['mode']=='bad_ready': print('{}',flush=True); time.sleep(10)
print(json.dumps({'version':1,'kind':'ready','artifact_sha256':hashlib.sha256(b).hexdigest()}),flush=True)
for line in sys.stdin:
 r=json.loads(line)
 mode=a['mode']
 if mode=='hang': open(a['marker'],'w').close(); time.sleep(10)
 if mode=='crash': os._exit(2)
 if mode=='oversize': print('x'*5000,flush=True); continue
 if mode=='stale': r['id']='wrong'
 if mode=='nan': print('{"version":1,"id":"'+r['id']+'","score":NaN}',flush=True); continue
 if mode=='extra': print(json.dumps({'version':1,'id':r['id'],'score':0,'allow':True}),flush=True); continue
 if mode=='once':
  try:
   f=os.open(a['marker'],os.O_CREAT|os.O_EXCL|os.O_WRONLY,0o600); os.close(f); os._exit(2)
  except FileExistsError: pass
 score=1.0 if 'restricted' in r['text'] or 'restricted' in r['joined'] else 0.0
 if 'SANDHI_WORKER_TEST_SECRET' in os.environ: score=1.0
 print(json.dumps({'version':1,'id':r['id'],'score':score}),flush=True)
"#;
fn fixture(mode: &str) -> (tempfile::TempDir, serde_json::Value) {
    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("fixture.py");
    fs::write(&script, FIXTURE).unwrap();
    let artifact = dir.path().join("model.json");
    let bytes = serde_json::to_vec(
        &json!({"mode":mode,"marker":dir.path().join("once"),"pid_path":dir.path().join("pid")}),
    )
    .unwrap();
    fs::write(&artifact, &bytes).unwrap();
    let python = std::env::var("SANDHI_TEST_PYTHON").unwrap_or("/usr/bin/python3".into());
    let manifest = json!({"version":1,"workers":[{"name":"test.python.v1","python":python,
  "script":script,"script_sha256":format!("{:x}",Sha256::digest(FIXTURE.as_bytes())),
  "artifact":artifact,"artifact_sha256":format!("{:x}",Sha256::digest(&bytes)),
  "pool_size":1,"startup_timeout_ms":5000}]});
    (dir, manifest)
}
fn engine(manifest: &serde_json::Value) -> Engine {
    let mut registry = Registry::default();
    register_workers(&mut registry, &serde_json::to_vec(manifest).unwrap()).unwrap();
    Engine::from_slice_with_registry(
        &serde_json::to_vec(&json!({"schema_version":"1","revision":1,
 "deadline_ms":200,"max_body_bytes":65536,"rules":[{"id":"python","effect":"block",
 "evaluator":{"kind":"registered","name":"test.python.v1","configuration":{"at_least":0.5}}}]}))
        .unwrap(),
        &registry,
    )
    .unwrap()
}
fn run(engine: &Engine, text: &str) -> Disposition {
    let body =
        serde_json::to_vec(&json!({"model":"test","messages":[{"role":"user","content":text}]}))
            .unwrap();
    engine
        .evaluate(
            &body,
            &Identity::default(),
            "openai",
            "test",
            Instant::now() + Duration::from_millis(200),
        )
        .disposition
}
#[test]
fn python_worker_warm_findings_and_environment_are_bounded() {
    let (_dir, m) = fixture("normal");
    let e = engine(&m);
    assert_eq!(run(&e, "weather"), Disposition::Forward);
    assert_eq!(run(&e, "restricted"), Disposition::Block);
    assert_eq!(run(&e, "weather"), Disposition::Forward);
}
#[test]
fn python_worker_failures_never_become_clean_findings() {
    for mode in ["hang", "crash", "oversize", "stale", "nan", "extra"] {
        let (_dir, m) = fixture(mode);
        let e = engine(&m);
        let start = Instant::now();
        assert_eq!(run(&e, "weather"), Disposition::Unavailable, "{mode}");
        assert!(start.elapsed() < Duration::from_secs(2), "{mode}");
    }
}
#[test]
fn python_worker_replaces_crashed_process_before_reuse() {
    let (_dir, m) = fixture("once");
    let e = engine(&m);
    assert_eq!(run(&e, "weather"), Disposition::Unavailable);
    let end = Instant::now() + Duration::from_secs(5);
    loop {
        if run(&e, "weather") == Disposition::Forward {
            break;
        }
        assert!(Instant::now() < end);
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(run(&e, "restricted"), Disposition::Block);
}
#[test]
fn python_worker_startup_rejects_changed_artifacts_and_unknown_configuration() {
    let (_dir, m) = fixture("normal");
    for field in ["script_sha256", "artifact_sha256"] {
        let mut bad = m.clone();
        bad["workers"][0][field] = json!("0".repeat(64));
        assert!(
            register_workers(&mut Registry::default(), &serde_json::to_vec(&bad).unwrap()).is_err()
        );
    }
    let mut bad = m.clone();
    bad["workers"][0]["pool_size"] = json!(0);
    assert!(
        register_workers(&mut Registry::default(), &serde_json::to_vec(&bad).unwrap()).is_err()
    );
    let mut bad = m;
    bad["workers"][0]["url"] = json!("https://unapproved.invalid");
    assert!(
        register_workers(&mut Registry::default(), &serde_json::to_vec(&bad).unwrap()).is_err()
    );
}

#[test]
fn python_worker_rejects_saturation_without_queue_or_deadline_extension() {
    let (dir, m) = fixture("hang");
    let e = std::sync::Arc::new(engine(&m));
    let other = e.clone();
    let handle = std::thread::spawn(move || run(&other, "weather"));
    let marker = dir.path().join("once");
    let end = Instant::now() + Duration::from_secs(1);
    while !marker.exists() {
        assert!(Instant::now() < end);
        std::thread::sleep(Duration::from_millis(1));
    }
    let start = Instant::now();
    assert_eq!(run(&e, "weather"), Disposition::Unavailable);
    assert!(start.elapsed() < Duration::from_millis(100));
    assert_eq!(handle.join().unwrap(), Disposition::Unavailable);
}

#[tokio::test]
async fn python_worker_failure_and_findings_hold_zero_provider_calls() {
    use axum::{
        body::{to_bytes, Body},
        http::{Request, StatusCode},
    };
    use sandhi_core::{InMemorySink, KeyStore, VirtualKey};
    use sandhi_providers::ProviderRuntime;
    use sandhi_proxy::{build_app, policy::PolicyGate, ProxyLedger, ProxyState};
    use sandhi_store::policy::PolicyAuditStore;
    use std::{collections::HashMap, sync::Arc};
    use tower::ServiceExt;
    use wiremock::MockServer;
    for mode in [
        "normal", "hang", "crash", "oversize", "stale", "nan", "extra",
    ] {
        let (dir, m) = fixture(mode);
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
            engine(&m),
            Arc::new(PolicyAuditStore::open(&dir.path().join("audit.db"), 100).unwrap()),
        )));
        let app = build_app(Arc::new(state));
        let body=serde_json::to_vec(&json!({"model":"test","max_tokens":4,"messages":[{"role":"user","content":"restricted"}]})).unwrap();
        let request = Request::builder()
            .uri("/v1/chat/completions")
            .method("POST")
            .header("authorization", "Bearer client")
            .header("content-type", "application/json")
            .body(Body::from(body))
            .unwrap();
        let response = app.oneshot(request).await.unwrap();
        assert_eq!(
            response.status(),
            if mode == "normal" {
                StatusCode::FORBIDDEN
            } else {
                StatusCode::SERVICE_UNAVAILABLE
            },
            "{mode}"
        );
        let bytes = to_bytes(response.into_body(), 8192).await.unwrap();
        assert!(!String::from_utf8_lossy(&bytes).contains("restricted"));
        assert!(
            upstream.received_requests().await.unwrap().is_empty(),
            "{mode}"
        );
    }
}

#[test]
#[ignore = "requires SANDHI_TEST_PYTHON pointing to the pinned scikit-learn environment"]
fn python_worker_real_sklearn_model_acceptance() {
    let (_dir, mut manifest) = fixture("normal");
    let script = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../scripts/policy_text_worker.py")
        .canonicalize()
        .unwrap();
    let bytes = fs::read(&script).unwrap();
    manifest["workers"][0]["script"] = json!(script);
    manifest["workers"][0]["script_sha256"] = json!(format!("{:x}", Sha256::digest(&bytes)));
    let artifact = serde_json::to_vec(&json!({"version":1,"kind":"tfidf_reference",
    "references":["restricted customer records","private account credentials"],"max_features":256}))
    .unwrap();
    let path = manifest["workers"][0]["artifact"].as_str().unwrap();
    fs::write(path, &artifact).unwrap();
    manifest["workers"][0]["artifact_sha256"] = json!(format!("{:x}", Sha256::digest(&artifact)));
    manifest["workers"][0]["startup_timeout_ms"] = json!(30000);
    let e = engine(&manifest);
    assert_eq!(run(&e, "weather forecast"), Disposition::Forward);
    assert_eq!(run(&e, "restricted customer records"), Disposition::Block);
    assert_eq!(run(&e, "private account credentials"), Disposition::Block);
}

#[test]
fn python_worker_startup_timeout_and_shutdown_reap_owned_processes() {
    for mode in ["normal", "startup_hang", "bad_ready"] {
        let (dir, mut manifest) = fixture(mode);
        if mode == "startup_hang" {
            manifest["workers"][0]["startup_timeout_ms"] = json!(200);
        }
        let start = Instant::now();
        let mut registry = Registry::default();
        let result = register_workers(&mut registry, &serde_json::to_vec(&manifest).unwrap());
        assert_eq!(result.is_ok(), mode == "normal");
        drop(registry);
        assert!(start.elapsed() < Duration::from_secs(6));
        let pid = fs::read_to_string(dir.path().join("pid")).unwrap();
        let status = std::process::Command::new("/bin/kill")
            .args(["-0", pid.trim()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(!status.success(), "{mode}: worker survived shutdown");
    }
}
