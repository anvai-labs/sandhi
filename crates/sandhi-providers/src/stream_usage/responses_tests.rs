use super::*;
use serde_json::json;

fn observer() -> ProtocolStreamUsageObserver {
    ProtocolStreamUsageObserver::for_endpoint(EndpointFamilyV1::OpenaiResponses).unwrap()
}

fn event(kind: &str, response: Value) -> String {
    format!(
        "event: {kind}\ndata: {}\n\n",
        json!({"type":kind,"response":response})
    )
}

fn terminal(status: &str) -> String {
    event(
        &format!("response.{status}"),
        json!({"id":"resp-test","status":status,
        "usage":{"input_tokens":10,"output_tokens":4,"total_tokens":14,
        "input_tokens_details":{"cached_tokens":6,"cache_write_tokens":2},
        "output_tokens_details":{"reasoning_tokens":0}}}),
    )
}

#[test]
fn responses_terminal_framing_and_disjoint_counts_at_every_split() {
    for status in ["completed", "incomplete", "failed"] {
        let prefix = event(
            "response.created",
            json!({"id":"resp-test","status":"in_progress","usage":null}),
        );
        let wire = format!(
            "\u{feff}: heartbeat\n\n{prefix}{}data: [DONE]\n\n",
            terminal(status)
        );
        for newline in ["\n", "\r\n", "\r"] {
            let wire = wire.replace('\n', newline);
            for split in 0..=wire.len() {
                let mut observed = observer();
                observed.push(&wire.as_bytes()[..split]).unwrap();
                observed.push(&wire.as_bytes()[split..]).unwrap();
                observed.finish().unwrap();
                let usage = observed.canonical_usage().unwrap();
                assert_eq!(
                    (
                        usage.tokens_in,
                        usage.cache_read_tokens,
                        usage.cache_creation_tokens,
                        usage.tokens_out
                    ),
                    (2, 6, 2, 4)
                );
                assert_eq!(usage.reasoning_tokens, Some(0));
                assert_eq!(usage.completeness, UsageCompleteness::Final);
                assert!(observed.is_done());
                assert_eq!(observed.provider_failed(), status == "failed");
            }
        }
    }
}

#[test]
fn responses_progress_merges_known_categories_but_never_becomes_final() {
    let mut observed = observer();
    observed
        .push(event("response.created", json!({"id":"resp-test","usage":null})).as_bytes())
        .unwrap();
    assert!(
        observed.canonical_usage().is_none(),
        "missing counters are unavailable, not measured zero"
    );
    for usage in [
        json!({"input_tokens":10,"total_tokens":10}),
        json!({"input_tokens_details":{"cached_tokens":6}}),
        json!({"output_tokens":4,"output_tokens_details":{"reasoning_tokens":0}}),
        json!({"input_tokens_details":{"cache_write_tokens":2}}),
    ] {
        observed
            .push(
                event(
                    "response.in_progress",
                    json!({"id":"resp-test","usage":usage}),
                )
                .as_bytes(),
            )
            .unwrap();
        assert!(observed.usage().is_none());
        assert_eq!(
            observed.canonical_usage().unwrap().completeness,
            UsageCompleteness::Partial
        );
    }
    let usage = observed.canonical_usage().unwrap();
    assert_eq!(
        (
            usage.tokens_in,
            usage.cache_read_tokens,
            usage.cache_creation_tokens,
            usage.tokens_out
        ),
        (2, 6, 2, 4)
    );
    assert_eq!(usage.reasoning_tokens, Some(0));
    assert_eq!(
        observed.finish(),
        Err(StreamObservationError::MissingTerminator)
    );
    assert_eq!(observed.canonical_usage().unwrap(), usage);

    let mut complete = observer();
    complete
        .push(
            event(
                "response.in_progress",
                json!({"id":"resp-test","usage":{"output_tokens":2}}),
            )
            .as_bytes(),
        )
        .unwrap();
    complete.push(terminal("completed").as_bytes()).unwrap();
    complete.finish().unwrap();
    assert_eq!(
        complete.canonical_usage().unwrap().completeness,
        UsageCompleteness::Final
    );

    let mut lower_bounds = observer();
    for usage in [
        json!({"input_tokens":1,"output_tokens":1}),
        json!({"input_tokens_details":{"cached_tokens":6},"output_tokens_details":{"reasoning_tokens":3}}),
    ] {
        lower_bounds
            .push(event("response.in_progress", json!({"usage":usage})).as_bytes())
            .unwrap();
    }
    let usage = lower_bounds.canonical_usage().unwrap();
    assert_eq!(
        (
            usage.tokens_in,
            usage.cache_read_tokens,
            usage.tokens_out,
            usage.reasoning_tokens
        ),
        (0, 6, 3, Some(3))
    );
}

#[test]
fn responses_reported_sequences_cannot_replay_or_regress() {
    for next in [json!(0), json!(1), json!(null), json!("2")] {
        let mut observed = observer();
        observed
            .push(b"data: {\"type\":\"response.output_text.delta\",\"sequence_number\":1}\n\n")
            .unwrap();
        let wire = format!(
            "data: {}\n\n",
            json!({"type":"response.output_text.delta","sequence_number":next})
        );
        assert!(observed.push(wire.as_bytes()).is_err());
        assert!(observed.canonical_usage().is_none());
    }
}

#[test]
fn responses_malformed_or_contradictory_events_preserve_partial_evidence() {
    for suffix in [
        "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"other\"}}\n\n".to_owned(),
        "data: {\"type\":\"response.output_text.delta\",\"response_id\":\"other\"}\n\n".to_owned(),
        "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp-test\",\"usage\":{\"input_tokens\":10,\"output_tokens\":1}}}\n\n".to_owned(),
        "event: response.failed\ndata: {\"type\":\"response.completed\"}\n\n".to_owned(),
        "data: {\"type\":\"response.completed\",\"type\":\"response.completed\"}\n\n".to_owned(),
        "data: [DONE]\n\n".to_owned(),
        "data: {\"type\":\"error\",\"error\":{\"message\":\"private content\"}}\n\n".to_owned(),
        event("response.in_progress",json!({"id":"resp-test","usage":{"output_tokens":1}})),
        event("response.in_progress",json!({"id":"resp-test","usage":{"output_tokens":null}})),
    ] {
        let mut observed = observer();
        observed.push(event("response.in_progress",json!({"id":"resp-test","usage":{"output_tokens":2}})).as_bytes()).unwrap();
        let before = observed.canonical_usage().unwrap();
        assert!(observed.push(suffix.as_bytes()).is_err(),"{suffix}");
        assert_eq!(observed.canonical_usage().unwrap(),before);
        assert!(observed.usage().is_none());
        assert!(observed.finish().is_err());
        assert!(observed.push(terminal("completed").as_bytes()).is_err(),"sticky error");
    }
}

#[test]
fn responses_terminal_presence_does_not_manufacture_usage_or_http_eof() {
    let mut no_usage = observer();
    assert!(no_usage
        .push(
            event(
                "response.failed",
                json!({"id":"resp-test","status":"failed","usage":null})
            )
            .as_bytes()
        )
        .is_err());
    assert!(no_usage.is_done());
    assert!(no_usage.provider_failed());
    assert!(no_usage.canonical_usage().is_none());

    for suffix in [
        "data: [DONE]\n\ndata: [DONE]\n\n",
        "data: {}\n\n",
        "data: {",
        "event: unknown\ndata: [DONE]\n\n",
    ] {
        let mut observed = observer();
        observed.push(terminal("completed").as_bytes()).unwrap();
        let _ = observed.push(suffix.as_bytes());
        assert!(observed.finish().is_err());
        assert_eq!(
            observed.canonical_usage().unwrap().completeness,
            UsageCompleteness::Partial
        );
        assert_eq!(observed.canonical_usage().unwrap().cache_read_tokens, 6);
    }
}

#[test]
fn responses_content_and_tools_are_not_usage_authority() {
    let mut observed = observer();
    for kind in [
        "response.output_text.delta",
        "response.function_call_arguments.done",
        "response.unknown_future_event",
    ] {
        observed.push(format!("data: {}\n\n",json!({"type":kind,"response_id":"resp-test","usage":{"input_tokens":999,"output_tokens":999}})).as_bytes()).unwrap();
        assert!(observed.canonical_usage().is_none());
    }
    observed.push(terminal("completed").as_bytes()).unwrap();
    observed.finish().unwrap();
    assert_eq!(observed.usage().unwrap().tokens_out, 4);
    assert!(
        ProtocolStreamUsageObserver::for_endpoint(EndpointFamilyV1::GeminiGenerateContent).is_err()
    );
}
