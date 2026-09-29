#![cfg(feature = "policy-onnx")]
use sandhi_core::policy::{Disposition, Engine, Identity, Inspection, Registry};
use serde_json::{json, Value};
use std::{
    path::PathBuf,
    time::{Duration, Instant},
};

#[test]
#[ignore = "requires pinned SANDHI_SENSITIVE_TEST_MANIFEST runtime deployment"]
fn bundled_model_parity_unicode_and_default_policy_actions() {
    let path = PathBuf::from(std::env::var("SANDHI_SENSITIVE_TEST_MANIFEST").unwrap());
    let manifest = std::fs::read(&path).unwrap();
    let backend = sandhi_proxy::policy_onnx::load_backend(&manifest).unwrap();
    let golden: Vec<Value> =
        serde_json::from_slice(&std::fs::read(path.with_file_name("golden.json")).unwrap())
            .unwrap();
    for case in golden {
        let input = Inspection {
            text: case["text"].as_str().unwrap().into(),
            joined: case["joined"].as_str().unwrap().into(),
            body_bytes: 0,
            max_output_tokens: None,
        };
        let score = backend
            .score(&input, Instant::now() + Duration::from_secs(1))
            .unwrap();
        assert!(
            (score - case["score"].as_f64().unwrap()).abs() < 1e-6,
            "Rust/Python preprocessing or inference drift"
        );
    }
    let mut registry = Registry::default();
    sandhi_proxy::policy_onnx::register_models(&mut registry, &manifest).unwrap();
    let engine = Engine::from_slice_with_registry(
        &std::fs::read(path.with_file_name("policy.json")).unwrap(),
        &registry,
    )
    .unwrap();
    let synthetic_token = format!("ghp_{}", "a".repeat(36));
    for (text, disposition, audit) in [
        ("-----BEGIN PRIVATE KEY-----", Disposition::Block, false),
        (synthetic_token.as_str(), Disposition::Block, true),
        (
            "Dana Park social security number: 987-65-4321",
            Disposition::Forward,
            true,
        ),
        (
            "Write a sorting algorithm in Rust",
            Disposition::Forward,
            false,
        ),
        ("你好，今天的天气如何？", Disposition::Forward, false),
    ] {
        let body = serde_json::to_vec(
            &json!({"model":"test", "messages":[{"role":"user","content":text}]}),
        )
        .unwrap();
        let decision = engine.evaluate(
            &body,
            &Identity::default(),
            "openai",
            "test",
            Instant::now() + Duration::from_secs(1),
        );
        assert_eq!(decision.reason, "evaluated", "fixture: {text}");
        assert_eq!(decision.disposition, disposition, "fixture: {text}");
        assert_eq!(
            decision
                .matched_rules
                .iter()
                .any(|r| r == "audit-sensitive-text"),
            audit,
            "fixture: {text}; decision: {decision:?}"
        );
    }
}
