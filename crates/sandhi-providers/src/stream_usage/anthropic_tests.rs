use super::*;
use serde_json::json;

fn observer(profile: AnthropicUsageProfile) -> ProtocolStreamUsageObserver {
    ProtocolStreamUsageObserver::for_endpoint_with_usage_profiles(
        EndpointFamilyV1::AnthropicMessages,
        OpenAiStreamUsageProfile::Standard,
        profile,
    )
    .unwrap()
}

fn frame(event: Value) -> String {
    format!(
        "event: {}\ndata: {event}\n\n",
        event["type"].as_str().unwrap()
    )
}

fn start(usage: Value) -> String {
    frame(json!({"type":"message_start","message":{
        "id":"msg-synthetic","type":"message","role":"assistant","usage":usage
    }}))
}

fn delta(usage: Value, reason: Value) -> String {
    frame(json!({"type":"message_delta","delta":{"stop_reason":reason},"usage":usage}))
}

fn initial() -> Value {
    json!({"input_tokens":3,"output_tokens":1,"cache_read_input_tokens":8,
    "cache_creation_input_tokens":2,"cache_creation":{
        "ephemeral_5m_input_tokens":2,"ephemeral_1h_input_tokens":0
    }})
}

fn stop() -> String {
    frame(json!({"type":"message_stop"}))
}

#[test]
fn anthropic_complete_event_counts_at_every_byte_boundary() {
    for profile in [
        AnthropicUsageProfile::SeparateCacheWrites,
        AnthropicUsageProfile::ImplicitCache,
    ] {
        let mut counts = initial();
        if profile == AnthropicUsageProfile::ImplicitCache {
            counts
                .as_object_mut()
                .unwrap()
                .remove("cache_creation_input_tokens");
            counts.as_object_mut().unwrap().remove("cache_creation");
        }
        let wire = format!(
            "\u{feff}: keepalive\n\n{}{}{}{}{}{}{}",
            start(counts),
            frame(json!({"type":"ping"})),
            frame(
                json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}})
            ),
            frame(
                json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"雪"}})
            ),
            frame(json!({"type":"content_block_stop","index":0})),
            delta(
                json!({"output_tokens":4,"output_tokens_details":{"thinking_tokens":0}}),
                json!("end_turn")
            ),
            stop()
        );
        for newline in ["\n", "\r\n", "\r"] {
            let wire = wire.replace('\n', newline);
            for split in 0..=wire.len() {
                let mut observed = observer(profile);
                observed.push(&wire.as_bytes()[..split]).unwrap();
                observed.push(&wire.as_bytes()[split..]).unwrap();
                observed.finish().unwrap();
                assert!(observed.is_done());
                assert!(!observed.provider_failed());
                let usage = observed.canonical_usage().unwrap();
                assert_eq!(
                    (
                        usage.tokens_in,
                        usage.cache_read_tokens,
                        usage.cache_creation_tokens,
                        usage.tokens_out
                    ),
                    (
                        3,
                        8,
                        if profile == AnthropicUsageProfile::ImplicitCache {
                            0
                        } else {
                            2
                        },
                        4
                    )
                );
                assert_eq!(usage.reasoning_tokens, Some(0));
                assert_eq!(usage.reasoning_included, Some(true));
                assert_eq!(usage.completeness, UsageCompleteness::Final);
                assert_eq!(observed.usage(), Some((&usage).into()));
            }
        }
    }
}

#[test]
fn anthropic_initial_and_progress_totals_never_settle_without_terminal_usage() {
    for suffix in [
        String::new(),
        stop(),
        delta(json!({}), json!("end_turn")) + &stop(),
        delta(json!({"output_tokens":4}), Value::Null) + &stop(),
    ] {
        let mut observed = observer(Default::default());
        observed.push(start(initial()).as_bytes()).unwrap();
        assert_eq!(
            observed.canonical_usage().unwrap().completeness,
            UsageCompleteness::Partial
        );
        assert!(observed.usage().is_none());
        let _ = observed.push(suffix.as_bytes());
        assert!(observed.finish().is_err());
        assert!(observed.usage().is_none());
        assert_eq!(
            observed.canonical_usage().unwrap().tokens_out,
            if suffix.contains("output_tokens") {
                4
            } else {
                1
            }
        );
    }
}

#[test]
fn anthropic_absent_required_categories_are_never_invented_by_progress_merge() {
    for missing in [
        "input_tokens",
        "cache_read_input_tokens",
        "cache_creation_input_tokens",
    ] {
        let mut counts = initial();
        counts.as_object_mut().unwrap().remove(missing);
        let mut observed = observer(Default::default());
        observed.push(start(counts).as_bytes()).unwrap();
        observed
            .push(delta(json!({"output_tokens":4}), json!("max_tokens")).as_bytes())
            .unwrap();
        assert!(observed.push(stop().as_bytes()).is_err(), "{missing}");
        assert!(observed.usage().is_none());
        assert_eq!(
            observed.canonical_usage().unwrap().completeness,
            UsageCompleteness::Partial
        );
    }
    // A thinking subset raises the partial lower bound; it cannot become a
    // reported output parent, even if the stream later terminates.
    let mut observed = observer(Default::default());
    observed.push(start(initial()).as_bytes()).unwrap();
    observed
        .push(
            delta(
                json!({"output_tokens_details":{"thinking_tokens":3}}),
                Value::Null,
            )
            .as_bytes(),
        )
        .unwrap();
    assert_eq!(observed.canonical_usage().unwrap().tokens_out, 3);
    observed
        .push(delta(json!({"output_tokens":2}), json!("end_turn")).as_bytes())
        .unwrap();
    assert!(observed.push(stop().as_bytes()).is_err());
    assert!(observed.usage().is_none());
}

#[test]
fn anthropic_cumulative_updates_preserve_presence_and_do_not_sum_totals() {
    let mut observed = observer(Default::default());
    observed.push(start(initial()).as_bytes()).unwrap();
    observed
        .push(delta(json!({"output_tokens":2}), Value::Null).as_bytes())
        .unwrap();
    observed
        .push(
            delta(
                json!({"output_tokens":2,"output_tokens_details":{"thinking_tokens":0}}),
                Value::Null,
            )
            .as_bytes(),
        )
        .unwrap();
    observed
        .push(
            delta(
                json!({"output_tokens":4,"cache_creation_input_tokens":3}),
                json!("tool_use"),
            )
            .as_bytes(),
        )
        .unwrap();
    // Old TTL splits are lower bounds across frames, not an equality with a
    // newly reported aggregate. Same-frame contradictions are refused below.
    observed.push(stop().as_bytes()).unwrap();
    observed.finish().unwrap();
    let usage = observed.canonical_usage().unwrap();
    assert_eq!(usage.tokens_out, 4);
    assert_eq!(usage.cache_creation_tokens, 3);
    assert_eq!(usage.reasoning_tokens, Some(0));

    let mut zero = observer(Default::default());
    zero.push(
        start(json!({"input_tokens":0,"output_tokens":0,
        "cache_read_input_tokens":0,"cache_creation_input_tokens":0}))
        .as_bytes(),
    )
    .unwrap();
    zero.push(delta(json!({"output_tokens":0}), json!("end_turn")).as_bytes())
        .unwrap();
    zero.push(stop().as_bytes()).unwrap();
    zero.finish().unwrap();
    assert_eq!(
        zero.canonical_usage().unwrap().completeness,
        UsageCompleteness::Final
    );
    assert_eq!(zero.canonical_usage().unwrap().reasoning_tokens, None);
    assert_eq!(zero.usage().unwrap().tokens_in, 0);
}

#[test]
fn anthropic_regressions_malformed_counters_and_bad_envelopes_are_sticky() {
    let mut regressions = vec![
        delta(json!({"input_tokens":2}), Value::Null),
        delta(json!({"output_tokens":0}), Value::Null),
        delta(json!({"cache_read_input_tokens":7}), Value::Null),
        delta(json!({"cache_creation_input_tokens":1}), Value::Null),
        delta(
            json!({"cache_creation":{"ephemeral_5m_input_tokens":1}}),
            Value::Null,
        ),
        delta(
            json!({"cache_creation_input_tokens":4,"cache_creation":{"ephemeral_5m_input_tokens":2,"ephemeral_1h_input_tokens":0}}),
            Value::Null,
        ),
        start(initial()),
        frame(
            json!({"type":"message_delta","message_id":"other","delta":{},"usage":{"output_tokens":2}}),
        ),
        frame(json!({"type":"content_block_start","content_block":{"type":"fallback"}})),
        "event: ping\ndata: {\"type\":\"message_stop\"}\n\n".into(),
        "data: {\"type\":\"ping\",\"type\":\"message_stop\"}\n\n".into(),
        "data: [DONE]\n\n".into(),
        delta(json!({"output_tokens":4}), json!(3)),
    ];
    for field in [
        "input_tokens",
        "output_tokens",
        "cache_read_input_tokens",
        "cache_creation_input_tokens",
    ] {
        for bad in [
            json!(-1),
            json!(1.5),
            json!("2"),
            Value::Null,
            json!(50_000_000_001_u64),
        ] {
            regressions.push(delta(json!({field:bad}), Value::Null));
        }
    }
    for suffix in regressions {
        let mut observed = observer(Default::default());
        observed.push(start(initial()).as_bytes()).unwrap();
        let before = observed.canonical_usage().unwrap();
        assert!(observed.push(suffix.as_bytes()).is_err(), "{suffix}");
        assert_eq!(observed.canonical_usage(), Some(before));
        assert!(observed.usage().is_none());
        assert!(observed.finish().is_err());
        assert!(observed.push(stop().as_bytes()).is_err());
    }
}

#[test]
fn anthropic_truncated_and_post_terminal_events_invalidate_final_evidence() {
    let prefix = start(initial()) + &delta(json!({"output_tokens":4}), json!("end_turn"));
    for trim in [1, 2, 10] {
        let mut observed = observer(Default::default());
        observed.push(prefix.as_bytes()).unwrap();
        let end = stop();
        observed.push(&end.as_bytes()[..end.len() - trim]).unwrap();
        assert!(observed.finish().is_err());
        assert!(observed.usage().is_none());
        assert_eq!(
            observed.canonical_usage().unwrap().completeness,
            UsageCompleteness::Partial
        );
    }
    for suffix in [
        frame(json!({"type":"ping"})),
        stop(),
        "data: [DONE]\n\n".into(),
    ] {
        let mut observed = observer(Default::default());
        observed
            .push((prefix.clone() + &stop()).as_bytes())
            .unwrap();
        let final_count = observed.usage();
        assert!(final_count.is_some());
        assert_eq!(
            observed.push(suffix.as_bytes()),
            Err(StreamObservationError::DataAfterTerminator)
        );
        assert_eq!(
            observed.usage(),
            final_count,
            "legacy projection retains measurement facts"
        );
        assert_eq!(
            observed.canonical_usage().unwrap().completeness,
            UsageCompleteness::Partial
        );
    }
}

#[test]
fn anthropic_errors_and_abort_retain_only_observed_evidence() {
    let mut observed = observer(Default::default());
    observed.push(start(initial()).as_bytes()).unwrap();
    assert_eq!(
        observed.push(
            frame(json!({"type":"error","error":{"type":"overloaded_error","message":"private"}}))
                .as_bytes()
        ),
        Err(StreamObservationError::ProviderEvent)
    );
    assert!(observed.provider_failed());
    assert_eq!(observed.canonical_usage().unwrap().tokens_in, 3);
    assert_eq!(
        observed.canonical_usage().unwrap().completeness,
        UsageCompleteness::Partial
    );
    let full = start(initial()) + &delta(json!({"output_tokens":4}), json!("end_turn")) + &stop();
    for (suffix, expected) in [
        (": unfinished", UsageCompleteness::Final),
        ("data: unfinished", UsageCompleteness::Partial),
    ] {
        let mut observed = observer(Default::default());
        observed.push((full.clone() + suffix).as_bytes()).unwrap();
        observed.close_on_abort();
        assert_eq!(observed.canonical_usage().unwrap().completeness, expected);
    }
}

#[test]
fn anthropic_unknown_events_cannot_inject_usage_or_replace_identity() {
    let mut observed = observer(Default::default());
    observed
        .push(frame(json!({"type":"future_event","usage":{"input_tokens":1000}})).as_bytes())
        .unwrap();
    assert!(observed.canonical_usage().is_none());
    observed.push(start(initial()).as_bytes()).unwrap();
    observed
        .push(frame(json!({"type":"future_event","usage":{"output_tokens":1000}})).as_bytes())
        .unwrap();
    assert_eq!(observed.canonical_usage().unwrap().tokens_out, 1);
    observed
        .push(delta(json!({"output_tokens":4}), json!("end_turn")).as_bytes())
        .unwrap();
    assert!(observed
        .push(frame(json!({"type":"content_block_start"})).as_bytes())
        .is_err());
    for bad in [
        delta(json!({"output_tokens":4}), json!("end_turn")),
        stop(),
        frame(json!({"type":"content_block_delta"})),
    ] {
        assert!(observer(Default::default()).push(bad.as_bytes()).is_err());
    }
}

#[test]
fn anthropic_profiles_are_explicit_and_family_scoped() {
    assert!(
        ProtocolStreamUsageObserver::for_endpoint_with_usage_profiles(
            EndpointFamilyV1::OpenaiResponses,
            OpenAiStreamUsageProfile::Standard,
            AnthropicUsageProfile::ImplicitCache
        )
        .is_err()
    );
    assert!(
        ProtocolStreamUsageObserver::for_endpoint_with_usage_profiles(
            EndpointFamilyV1::AnthropicMessages,
            OpenAiStreamUsageProfile::TerminalChunk,
            AnthropicUsageProfile::ImplicitCache
        )
        .is_err()
    );
    for counts in [
        initial(),
        json!({"cache_creation":{"ephemeral_5m_input_tokens":1}}),
    ] {
        assert!(observer(AnthropicUsageProfile::ImplicitCache)
            .push(start(counts).as_bytes())
            .is_err());
    }
}

#[test]
fn anthropic_retained_subsets_cannot_regress_or_invent_final_parent_counts() {
    let mut observed = observer(Default::default());
    observed.push(start(initial()).as_bytes()).unwrap();
    observed
        .push(
            delta(
                json!({"cache_creation":{"ephemeral_5m_input_tokens":3}}),
                Value::Null,
            )
            .as_bytes(),
        )
        .unwrap();
    assert_eq!(observed.canonical_usage().unwrap().cache_creation_tokens, 3);
    observed
        .push(delta(json!({"output_tokens":4}), json!("end_turn")).as_bytes())
        .unwrap();
    assert!(
        observed.push(stop().as_bytes()).is_err(),
        "actual aggregate write remains two"
    );
    assert!(observed.usage().is_none());

    for (initial_usage, next) in [
        (
            json!({"output_tokens":4,"output_tokens_details":{"thinking_tokens":3}}),
            json!({"output_tokens_details":{"thinking_tokens":2}}),
        ),
        (
            json!({"cache_creation_input_tokens":2,"cache_creation":{"ephemeral_5m_input_tokens":1,"ephemeral_1h_input_tokens":1}}),
            json!({"cache_creation":{"ephemeral_1h_input_tokens":0}}),
        ),
    ] {
        let mut observed = observer(Default::default());
        observed.push(start(initial_usage).as_bytes()).unwrap();
        let before = observed.canonical_usage();
        assert!(observed.push(delta(next, Value::Null).as_bytes()).is_err());
        assert_eq!(observed.canonical_usage(), before);
    }
    for id in [
        json!(""),
        json!("invalid\n"),
        json!("x".repeat(257)),
        Value::Null,
        json!(3),
    ] {
        let mut observed = observer(Default::default());
        assert!(observed
            .push(
                frame(json!({"type":"message_start","message":{
                    "id":id,"type":"message","role":"assistant","usage":initial()
                }}))
                .as_bytes()
            )
            .is_err());
        assert!(observed.canonical_usage().is_none());
    }
}
