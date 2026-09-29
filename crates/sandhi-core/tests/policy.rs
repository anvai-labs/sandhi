use sandhi_core::policy::*;
use serde_json::{json, Value};
use std::time::{Duration, Instant};

fn identity() -> Identity {
    Identity {
        issuer: Some("https://id.example/issuer".into()),
        subject: Some("alice".into()),
        groups: vec!["agents".into()],
        roles: vec!["viewer".into()],
        directory_known: true,
    }
}
fn body(text: &str) -> Vec<u8> {
    serde_json::to_vec(
        &json!({"model":"test","messages":[{"role":"user","content":text}],"max_tokens":10}),
    )
    .unwrap()
}
fn rule(id: &str, evaluator: Value, effect: &str) -> Value {
    json!({"id":id,"evaluator":evaluator,"effect":effect})
}
fn engine(rules: Vec<Value>) -> Engine {
    Engine::from_slice(
        &serde_json::to_vec(&json!({"schema_version":"1","revision":1,
        "deadline_ms":200,"max_body_bytes":65536,"rules":rules}))
        .unwrap(),
    )
    .unwrap()
}
fn run(engine: &Engine, bytes: &[u8], who: &Identity) -> Decision {
    engine.evaluate(
        bytes,
        who,
        "openai",
        "test",
        Instant::now() + Duration::from_secs(1),
    )
}

#[test]
fn policy_regex_inspects_prefill_tools_and_split_text_and_never_returns_content() {
    let e = engine(vec![rule(
        "secret",
        json!({"kind":"regex","pattern":"SECRET-[0-9]+"}),
        "block",
    )]);
    assert_eq!(
        run(&e, &body("ordinary"), &identity()).disposition,
        Disposition::Forward
    );
    for value in [
        json!({"model":"test","messages":[{"role":"assistant","content":"SECRET-123"}]}),
        json!({"model":"test","messages":[{"role":"user","content":[{"type":"text","text":"SEC"},{"type":"text","text":"RET-123"}]}]}),
        json!({"model":"test","messages":[{"role":"tool","content":"SECRET-123","tool_call_id":"a"}]}),
        json!({"model":"test","messages":[{"role":"user","content":"ok"}],"tools":[{"type":"function","function":{"name":"search","description":"SECRET-123","parameters":{}}}]}),
    ] {
        let d = run(&e, &serde_json::to_vec(&value).unwrap(), &identity());
        assert_eq!(d.disposition, Disposition::Block);
        assert_eq!(d.matched_rules, vec!["secret"]);
        assert!(!serde_json::to_string(&d).unwrap().contains("SECRET-123"));
    }
}

#[test]
fn policy_composes_all_rules_and_binds_verified_identity() {
    let mut audit = rule("audit", json!({"kind":"regex","pattern":"hello"}), "audit");
    audit["when"] =
        json!({"issuer":"https://id.example/issuer","groups":["agents"],"roles":["viewer"]});
    let block = rule(
        "block",
        json!({"kind":"threshold","metric":"body_bytes","above":1}),
        "block",
    );
    for rules in [
        vec![audit.clone(), block.clone()],
        vec![block, audit.clone()],
    ] {
        assert_eq!(
            run(&engine(rules), &body("hello"), &identity()).disposition,
            Disposition::Block
        );
    }
    let e = engine(vec![audit]);
    assert_eq!(
        run(&e, &body("hello"), &identity()).matched_rules,
        vec!["audit"]
    );
    let mut who = identity();
    who.groups.clear();
    assert!(run(&e, &body("hello"), &who).matched_rules.is_empty());
    who.directory_known = false;
    assert_eq!(
        run(&e, &body("hello"), &who).disposition,
        Disposition::Unavailable
    );
    who.issuer = Some("other".into());
    assert_eq!(
        run(&e, &body("hello"), &who).disposition,
        Disposition::Forward
    );
}

#[test]
fn policy_threshold_boundary_and_lexical_similarity() {
    let e = engine(vec![rule(
        "tokens",
        json!({"kind":"threshold","metric":"max_output_tokens","above":10}),
        "block",
    )]);
    assert_eq!(
        run(&e, &body("ok"), &identity()).disposition,
        Disposition::Forward
    );
    let bytes = serde_json::to_vec(&json!({"model":"test","messages":[],"max_tokens":11})).unwrap();
    assert_eq!(run(&e, &bytes, &identity()).disposition, Disposition::Block);
    let bytes = serde_json::to_vec(&json!({"model":"test","messages":[]})).unwrap();
    assert_eq!(
        run(&e, &bytes, &identity()).disposition,
        Disposition::Unavailable
    );
    let e = engine(vec![rule(
        "reference",
        json!({"kind":"lexical_similarity","reference":"private customer records","at_least":0.5}),
        "quarantine",
    )]);
    assert_eq!(
        run(&e, &body("CUSTOMER private records"), &identity()).disposition,
        Disposition::Quarantine
    );
    assert_eq!(
        run(&e, &body("weather outside"), &identity()).disposition,
        Disposition::Forward
    );
}

#[test]
fn policy_rejects_ambiguous_unsupported_and_unbounded_inputs() {
    let e = engine(vec![rule(
        "secret",
        json!({"kind":"regex","pattern":"secret"}),
        "block",
    )]);
    for bytes in [
        br#"{"messages":[],"messages":[],"model":"test"}"#.to_vec(),
        br#"{"messages":[{"role":"user","content":[{"type":"image_url","image_url":{"url":"https://example/a"}}]}],"model":"test"}"#.to_vec(),
        br#"{"messages":[],"model":"test","unknown":"text"}"#.to_vec(),
        vec![b' ';65537],
    ] { assert_eq!(run(&e,&bytes,&identity()).disposition,Disposition::Unavailable); }
    let d = e.evaluate(
        &body("hello"),
        &identity(),
        "openai",
        "test",
        Instant::now() - Duration::from_millis(1),
    );
    assert_eq!(d.disposition, Disposition::Unavailable);
    for evaluator in [
        json!({"kind":"remote","url":"https://example"}),
        json!({"kind":"regex","pattern":"("}),
        json!({"kind":"lexical_similarity","reference":"x","at_least":1.1}),
    ] {
        let document = json!({"schema_version":"1","revision":1,"deadline_ms":200,"max_body_bytes":65536,
            "rules":[rule("bad",evaluator,"block")]});
        assert!(Engine::from_slice(&serde_json::to_vec(&document).unwrap()).is_err());
    }
}

#[test]
fn policy_extensions_are_registered_trusted_factories_and_fail_closed() {
    struct Failing;
    impl Evaluator for Failing {
        fn evaluate(&self, _: &Inspection, _: Instant) -> Result<bool, EvaluationError> {
            Err(EvaluationError)
        }
    }
    let bytes=serde_json::to_vec(&json!({"schema_version":"1","revision":1,"deadline_ms":200,
        "max_body_bytes":65536,"rules":[rule("extension",json!({"kind":"registered","name":"test.failure.v1","configuration":{}}),"block")]})).unwrap();
    assert!(Engine::from_slice(&bytes).is_err());
    let mut registry = Registry::default();
    registry
        .register("test.failure.v1", |config| {
            if config != &json!({}) {
                return Err(EvaluationError);
            }
            Ok(Box::new(Failing))
        })
        .unwrap();
    assert!(registry
        .register("test.failure.v1", |_| Ok(Box::new(Failing)))
        .is_err());
    let engine = Engine::from_slice_with_registry(&bytes, &registry).unwrap();
    assert_eq!(
        run(&engine, &body("ok"), &identity()).disposition,
        Disposition::Unavailable
    );
}

#[test]
fn policy_decodes_tool_arguments_and_rejects_duplicate_keys_in_them() {
    let e = engine(vec![rule(
        "secret",
        json!({"kind":"regex","pattern":"SECRET-123"}),
        "block",
    )]);
    for arguments in [
        r#"{"value":"\u0053ECRET-123"}"#,
        r#"{"value":"SECRET-123","value":"ok"}"#,
    ] {
        let bytes=serde_json::to_vec(&json!({"model":"test","messages":[{"role":"assistant","content":null,
            "tool_calls":[{"id":"call","type":"function","function":{"name":"demo","arguments":arguments}}]}]})).unwrap();
        assert_ne!(
            run(&e, &bytes, &identity()).disposition,
            Disposition::Forward
        );
    }
}

#[test]
fn policy_rejects_invalid_documents_instead_of_loading_empty_policy() {
    let valid = json!({"schema_version":"1","revision":1,"deadline_ms":200,"max_body_bytes":65536,
        "rules":[rule("test",json!({"kind":"regex","pattern":"x"}),"audit")]});
    for (field, value) in [
        ("schema_version", json!("2")),
        ("revision", json!(0)),
        ("deadline_ms", json!(0)),
        ("deadline_ms", json!(2001)),
        ("max_body_bytes", json!(262145)),
        ("rules", json!([])),
        ("unknown", json!(true)),
    ] {
        let mut document = valid.clone();
        document[field] = value;
        assert!(Engine::from_slice(&serde_json::to_vec(&document).unwrap()).is_err());
    }
    let mut duplicate = valid.clone();
    duplicate["rules"]
        .as_array_mut()
        .unwrap()
        .push(valid["rules"][0].clone());
    assert!(Engine::from_slice(&serde_json::to_vec(&duplicate).unwrap()).is_err());
}

#[test]
fn solution_policy_fixture_is_executable_and_does_not_blanket_block_confidential() {
    let e =
        Engine::from_slice(include_bytes!("../../../examples/message-hub-policy.json")).unwrap();
    assert_eq!(
        run(&e, &body("a confidential status update"), &identity()).disposition,
        Disposition::Forward
    );
    assert_eq!(
        run(&e, &body("SANDHI_TEST_SECRET_123"), &identity()).disposition,
        Disposition::Block
    );
    assert_eq!(
        run(&e, &body("restricted customer records"), &identity()).disposition,
        Disposition::Quarantine
    );
}

#[test]
fn policy_score_contract_is_identical_for_embedded_and_external_adapters() {
    struct Embedded(f64);
    impl ScoreBackend for Embedded {
        fn score(&self, _: &Inspection, _: Instant) -> Result<f64, EvaluationError> {
            Ok(self.0)
        }
    }
    let input = Inspection {
        text: "text".into(),
        joined: "text".into(),
        body_bytes: 4,
        max_output_tokens: None,
    };
    for (score, matched) in [(0.49, false), (0.5, true), (1.0, true)] {
        let evaluator = ThresholdedScore::new(std::sync::Arc::new(Embedded(score)), 0.5).unwrap();
        assert_eq!(
            evaluator
                .evaluate(&input, Instant::now() + Duration::from_secs(1))
                .unwrap(),
            matched
        );
    }
    for score in [f64::NAN, f64::INFINITY, -0.1, 1.1] {
        let evaluator = ThresholdedScore::new(std::sync::Arc::new(Embedded(score)), 0.5).unwrap();
        assert!(evaluator
            .evaluate(&input, Instant::now() + Duration::from_secs(1))
            .is_err());
    }
    assert!(ThresholdedScore::new(std::sync::Arc::new(Embedded(0.)), f64::NAN).is_err());
    let evaluator = ThresholdedScore::new(std::sync::Arc::new(Embedded(0.)), 0.5).unwrap();
    assert!(evaluator
        .evaluate(&input, Instant::now() - Duration::from_secs(1))
        .is_err());
}
