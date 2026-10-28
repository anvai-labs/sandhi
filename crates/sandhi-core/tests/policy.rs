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
    // Bodies between the old 256 KiB engine cap and the new 4 MiB ceiling
    // must now evaluate (forward), not fail closed — with a document whose
    // max_body_bytes covers them.
    let big = Engine::from_slice(
        &serde_json::to_vec(&json!({
            "schema_version":"1","revision":1,"deadline_ms":200,
            "max_body_bytes": sandhi_core::policy::MAX_BODY_BYTES_CEILING,
            "rules":[rule("test",json!({"kind":"regex","pattern":"nomatch"}),"audit")]
        }))
        .unwrap(),
    )
    .unwrap();
    let mut payload = br#"{"messages":[],"model":"test"}"#.to_vec();
    let old_gap = 300 * 1024;
    while payload.len() < old_gap {
        payload.push(b' ');
    }
    assert_eq!(
        run(&big, &payload, &identity()).disposition,
        Disposition::Forward
    );
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
        (
            "max_body_bytes",
            json!(sandhi_core::policy::MAX_BODY_BYTES_CEILING + 1),
        ),
        ("rules", json!([])),
        ("unknown", json!(true)),
    ] {
        let mut document = valid.clone();
        document[field] = value;
        assert!(Engine::from_slice(&serde_json::to_vec(&document).unwrap()).is_err());
    }
    // The raised ceiling itself (4 MiB) is accepted — agentic sessions
    // exceed 256 KiB mid-conversation and were previously bricked (#380).
    let mut at_ceiling = valid.clone();
    at_ceiling["max_body_bytes"] = json!(sandhi_core::policy::MAX_BODY_BYTES_CEILING);
    assert!(Engine::from_slice(&serde_json::to_vec(&at_ceiling).unwrap()).is_ok());
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

// --- Anthropic `/v1/messages` input format ----------------------------------

fn anthropic_body(messages: Value) -> Vec<u8> {
    serde_json::to_vec(&json!({
        "model":"claude-test",
        "max_tokens":64,
        "messages":messages,
    }))
    .unwrap()
}

fn run_messages(engine: &Engine, bytes: &[u8], who: &Identity) -> Decision {
    engine.evaluate_with_format(
        bytes,
        who,
        "anthropic",
        "claude-test",
        InputFormat::Messages,
        Instant::now() + Duration::from_secs(1),
    )
}

#[test]
fn anthropic_messages_benign_body_forwards() {
    let e = engine(vec![rule(
        "noop",
        json!({"kind":"regex","pattern":"NEVER-MATCHES"}),
        "audit",
    )]);
    for body in [
        anthropic_body(json!([{"role":"user","content":"hello"}])),
        anthropic_body(json!([{"role":"user","content":[
            {"type":"text","text":"part one "},
            {"type":"text","text":"part two"},
        ]}])),
        serde_json::to_vec(&json!({
            "model":"claude-test","max_tokens":64,
            "system":"You are terse.",
            "messages":[{"role":"user","content":"hi"}],
            "temperature":0.2,"top_k":40,"stop_sequences":["\n\n"],
            "thinking":{"type":"enabled","budget_tokens":1024},
            "metadata":{"user_id":"u-1"},
        }))
        .unwrap(),
    ] {
        let d = run_messages(&e, &body, &identity());
        assert_eq!(d.disposition, Disposition::Forward, "body: {body:?}");
    }
}

#[test]
fn anthropic_messages_block_rule_matches_every_text_surface() {
    let e = engine(vec![rule(
        "secret",
        json!({"kind":"regex","pattern":"SECRET-[0-9]+"}),
        "block",
    )]);
    for body in [
        // system prompt (string and block-array forms)
        serde_json::to_vec(&json!({"model":"m","max_tokens":1,"system":"SECRET-1",
            "messages":[{"role":"user","content":"ok"}]}))
        .unwrap(),
        serde_json::to_vec(&json!({"model":"m","max_tokens":1,
            "system":[{"type":"text","text":"SECRET-2"}],
            "messages":[{"role":"user","content":"ok"}]}))
        .unwrap(),
        // message content: string, text block, split across blocks
        anthropic_body(json!([{"role":"user","content":"SECRET-3"}])),
        anthropic_body(json!([{"role":"user","content":[
            {"type":"text","text":"SEC"},{"type":"text","text":"RET-4"}]}])),
        // thinking text
        anthropic_body(json!([{"role":"assistant","content":[
            {"type":"thinking","thinking":"SECRET-5","signature":"sig"}]}])),
        // tool_use input
        anthropic_body(json!([{"role":"assistant","content":[
            {"type":"tool_use","id":"t1","name":"read","input":{"path":"SECRET-6"}}]}])),
        // tool_result payload (string and nested text block)
        anthropic_body(json!([{"role":"user","content":[
            {"type":"tool_result","tool_use_id":"t1","content":"SECRET-7"}]}])),
        anthropic_body(json!([{"role":"user","content":[
            {"type":"tool_result","tool_use_id":"t1","is_error":true,
             "content":[{"type":"text","text":"SECRET-8"}]}]}])),
        // image url source
        anthropic_body(json!([{"role":"user","content":[
            {"type":"image","source":{"type":"url","url":"https://x/SECRET-9.png"}}]}])),
    ] {
        let d = run_messages(&e, &body, &identity());
        assert_eq!(d.disposition, Disposition::Block, "body: {body:?}");
        assert_eq!(d.matched_rules, vec!["secret"]);
        assert!(!serde_json::to_string(&d).unwrap().contains("SECRET"));
    }
}

#[test]
fn anthropic_messages_unknown_shapes_fail_closed() {
    let e = engine(vec![rule(
        "noop",
        json!({"kind":"regex","pattern":"NEVER-MATCHES"}),
        "audit",
    )]);
    for body in [
        // unknown top-level field
        serde_json::to_vec(&json!({"model":"m","max_tokens":1,
            "messages":[{"role":"user","content":"ok"}],"surprise":1}))
        .unwrap(),
        // unknown message key
        serde_json::to_vec(&json!({"model":"m","max_tokens":1,
            "messages":[{"role":"user","content":"ok","name":"x"}]}))
        .unwrap(),
        // unknown block type
        anthropic_body(json!([{"role":"user","content":[{"type":"video","text":"x"}]}])),
        // malformed block (extra key)
        anthropic_body(json!([{"role":"user","content":[
            {"type":"text","text":"x","extra":1}]}])),
        // messages missing entirely
        serde_json::to_vec(&json!({"model":"m","max_tokens":1})).unwrap(),
        // malformed system blocks
        serde_json::to_vec(
            &json!({"model":"m","max_tokens":1,"system":[{"type":"text"}],
            "messages":[{"role":"user","content":"ok"}]}),
        )
        .unwrap(),
        // malformed image source
        anthropic_body(json!([{"role":"user","content":[
            {"type":"image","source":{"type":"carrier-pigeon"}}]}])),
    ] {
        let d = run_messages(&e, &body, &identity());
        assert_eq!(d.disposition, Disposition::Unavailable, "body: {body:?}");
        assert_eq!(d.reason, "incomplete_evaluation");
    }
}

#[test]
fn anthropic_messages_redacted_thinking_and_base64_source_are_structurally_checked() {
    let e = engine(vec![rule(
        "noop",
        json!({"kind":"regex","pattern":"NEVER-MATCHES"}),
        "audit",
    )]);
    // redacted_thinking data is collected; base64 source requires the media_type/data shape.
    let ok = serde_json::to_vec(&json!({
        "model":"m","max_tokens":1,
        "messages":[{"role":"assistant","content":[
            {"type":"redacted_thinking","data":"opaque"}]}]}))
    .unwrap();
    assert_eq!(
        run_messages(&e, &ok, &identity()).disposition,
        Disposition::Forward
    );
    let ok2 = anthropic_body(json!([{"role":"user","content":[
        {"type":"document","source":{"type":"base64","media_type":"application/pdf","data":"AAAA"}}]}]));
    assert_eq!(
        run_messages(&e, &ok2, &identity()).disposition,
        Disposition::Forward
    );
    // base64 without data is malformed
    let bad = anthropic_body(json!([{"role":"user","content":[
        {"type":"image","source":{"type":"base64","media_type":"image/png"}}]}]));
    assert_eq!(
        run_messages(&e, &bad, &identity()).disposition,
        Disposition::Unavailable
    );
}

/// Review round 1 (independent): cardinality-only checks on tool_result and
/// base64 sources let junk keys carry uninspected text past every rule. These
/// pin the identity-checked fix — each of the exploit shapes now fails closed.
#[test]
fn anthropic_messages_junk_keys_fail_closed() {
    let e = engine(vec![rule(
        "secret",
        json!({"kind":"regex","pattern":"SECRET-[0-9]+"}),
        "block",
    )]);
    for body in [
        // junk key on tool_result (was: Forward — the exploit)
        anthropic_body(json!([{"role":"user","content":[
            {"type":"tool_result","tool_use_id":"t1","x":"SECRET-42"}]}])),
        anthropic_body(json!([{"role":"user","content":[
            {"type":"tool_result","tool_use_id":"t1","content":"ok","x":"SECRET-42"}]}])),
        // junk key on a base64 source (was: Forward)
        anthropic_body(json!([{"role":"user","content":[
            {"type":"image","source":{"type":"base64","data":"QUFB","x":"SECRET-42"}}]}])),
        // plaintext smuggled in the data field (not base64) fails closed
        anthropic_body(json!([{"role":"user","content":[
            {"type":"image","source":{"type":"base64","media_type":"image/png","data":"SECRET-42"}}]}])),
        // junk key on a text block / tool_use / url source
        anthropic_body(json!([{"role":"user","content":[
            {"type":"text","text":"ok","x":"SECRET-42"}]}])),
        anthropic_body(json!([{"role":"assistant","content":[
            {"type":"tool_use","id":"t","name":"n","input":{},"x":"SECRET-42"}]}])),
        anthropic_body(json!([{"role":"user","content":[
            {"type":"image","source":{"type":"url","url":"https://x","x":"SECRET-42"}}]}])),
        // junk key inside a system block
        serde_json::to_vec(&json!({"model":"m","max_tokens":1,
            "system":[{"type":"text","text":"ok","x":"SECRET-42"}],
            "messages":[{"role":"user","content":"ok"}]}))
        .unwrap(),
    ] {
        let d = run_messages(&e, &body, &identity());
        assert_eq!(d.disposition, Disposition::Unavailable, "body: {body:?}");
    }
}

/// Review round 1 (independent): real Claude SDK shapes — `cache_control`
/// (prompt caching, pervasive in agent traffic), `citations` on text blocks,
/// and documented top-level params — must forward, with their string content
/// swept, not rejected.
#[test]
fn anthropic_messages_real_sdk_shapes_forward() {
    let e = engine(vec![rule(
        "noop",
        json!({"kind":"regex","pattern":"NEVER"}),
        "audit",
    )]);
    for body in [
        // cache_control on a message text block
        anthropic_body(json!([{"role":"user","content":[
            {"type":"text","text":"cached context","cache_control":{"type":"ephemeral"}}]}])),
        // cache_control on system blocks
        serde_json::to_vec(&json!({"model":"m","max_tokens":1,
            "system":[{"type":"text","text":"sys","cache_control":{"type":"ephemeral"}}],
            "messages":[{"role":"user","content":"ok"}]}))
        .unwrap(),
        // citations on an assistant text block
        anthropic_body(json!([{"role":"assistant","content":[
            {"type":"text","text":"answer","citations":[{"cited_text":"src","document_index":0}]}]}])),
        // cache_control on tool_result
        anthropic_body(json!([{"role":"user","content":[
            {"type":"tool_result","tool_use_id":"t1","content":"ok",
             "cache_control":{"type":"ephemeral"}}]}])),
        // documented top-level params
        serde_json::to_vec(&json!({"model":"m","max_tokens":1,
            "service_tier":"standard",
            "betas":["context-1m-2025-08-07"],
            "messages":[{"role":"user","content":"ok"}]}))
        .unwrap(),
    ] {
        let d = run_messages(&e, &body, &identity());
        assert_eq!(d.disposition, Disposition::Forward, "body: {body:?}");
    }
}

/// The extras (cache_control / citations / top-level params) are SWEPT, not
/// merely tolerated: a blocked pattern inside them is a block.
#[test]
fn anthropic_messages_extras_are_swept_not_skipped() {
    let e = engine(vec![rule(
        "secret",
        json!({"kind":"regex","pattern":"SECRET-[0-9]+"}),
        "block",
    )]);
    for body in [
        anthropic_body(json!([{"role":"user","content":[
            {"type":"text","text":"ok","cache_control":{"type":"SECRET-1"}}]}])),
        anthropic_body(json!([{"role":"assistant","content":[
            {"type":"text","text":"ok","citations":[{"cited_text":"SECRET-2"}]}]}])),
        serde_json::to_vec(&json!({"model":"m","max_tokens":1,"betas":["SECRET-3"],
            "messages":[{"role":"user","content":"ok"}]}))
        .unwrap(),
    ] {
        let d = run_messages(&e, &body, &identity());
        assert_eq!(d.disposition, Disposition::Block, "body: {body:?}");
    }
}

/// Review round 1: system was collected twice (strict branch + tail extras
/// loop), doubling classifier input. The strict regex that only matches the
/// doubled concatenation must now fail to match at all.
#[test]
fn anthropic_messages_system_collected_once() {
    let e = engine(vec![rule(
        "sysdouble",
        json!({"kind":"regex","pattern":"SYS hello SYS"}),
        "block",
    )]);
    let body = serde_json::to_vec(&json!({
        "model":"m","max_tokens":1,"system":"SYS",
        "messages":[{"role":"user","content":"hello"}]}))
    .unwrap();
    // parts order: "SYS", "hello" — the tail no longer re-collects system.
    let d = run_messages(&e, &body, &identity());
    assert_eq!(d.disposition, Disposition::Forward);
}
