use super::*;
use serde_json::json;

fn terminal(output: u64) -> String {
    format!(
        "data: {}\n\n",
        json!({"object":"chat.completion.chunk","choices":[{"index":0,"delta":{"content":""},"finish_reason":"stop"}],
        "usage":{"prompt_tokens":10,"completion_tokens":output,"total_tokens":10+output,
            "prompt_tokens_details":{"cached_tokens":6,"cache_write_tokens":2},"completion_tokens_details":{"reasoning_tokens":0}}})
    )
}
fn observer() -> ProtocolStreamUsageObserver {
    ProtocolStreamUsageObserver::for_endpoint_with_openai_usage_profile(
        EndpointFamilyV1::OpenaiChatCompletions,
        OpenAiStreamUsageProfile::TerminalChunk,
    )
    .unwrap()
}
#[test]
fn explicit_terminal_chunk_profile_retains_usage_across_all_byte_boundaries() {
    let wire = format!("{}data: [DONE]\n\n", terminal(4));
    for split in 0..=wire.len() {
        let mut observed = observer();
        observed.push(&wire.as_bytes()[..split]).unwrap();
        observed.push(&wire.as_bytes()[split..]).unwrap();
        observed.finish().unwrap();
        assert!(observed.is_done());
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
    }
    let mut standard = ProtocolStreamUsageObserver::new();
    assert_eq!(
        standard.push(wire.as_bytes()),
        Err(StreamObservationError::InvalidUsage(
            StreamUsageError::NonFinalUsage
        ))
    );
    assert!(standard.canonical_usage().is_none());
    assert!(
        ProtocolStreamUsageObserver::for_endpoint_with_openai_usage_profile(
            EndpointFamilyV1::OpenaiResponses,
            OpenAiStreamUsageProfile::TerminalChunk
        )
        .is_err()
    );
}
#[test]
fn terminal_profile_preserves_strict_conflict_framing_and_completion_fences() {
    for profile in [
        OpenAiStreamUsageProfile::TerminalChunk,
        OpenAiStreamUsageProfile::ZaiFinalChoice,
        OpenAiStreamUsageProfile::DeepSeekFinalChoice,
    ] {
        for fault in ["conflict", "truncated", "post_terminal", "missing_terminal"] {
            let mut observed = ProtocolStreamUsageObserver::for_endpoint_with_openai_usage_profile(
                EndpointFamilyV1::OpenaiChatCompletions,
                profile,
            )
            .unwrap();
            observed.push(terminal(4).as_bytes()).unwrap();
            let result = match fault {
                "conflict" => observed.push(terminal(5).as_bytes()),
                "truncated" => {
                    observed.push(b"data: {").unwrap();
                    observed.finish()
                }
                "post_terminal" => observed.push(b"data: [DONE]\n\ndata: {}\n\n"),
                _ => observed.finish(),
            };
            assert!(result.is_err(), "{fault}");
            assert_eq!(
                observed.canonical_usage().unwrap().completeness,
                UsageCompleteness::Partial
            );
            assert_eq!(observed.canonical_usage().unwrap().tokens_out, 4);
        }
    }
}

#[test]
fn identical_totals_do_not_erase_later_explicit_reasoning_zero() {
    let mut event: Value =
        serde_json::from_str(terminal(4).trim().strip_prefix("data: ").unwrap()).unwrap();
    let final_zero = event.clone();
    event["usage"]
        .as_object_mut()
        .unwrap()
        .remove("completion_tokens_details");
    let mut observed = observer();
    for frame in [&event, &final_zero] {
        observed
            .push(format!("data: {frame}\n\n").as_bytes())
            .unwrap();
    }
    observed.push(b"data: [DONE]\n\n").unwrap();
    observed.finish().unwrap();
    assert_eq!(
        observed.canonical_usage().unwrap().reasoning_tokens,
        Some(0)
    );
}

#[test]
fn explicit_vendor_profiles_retain_counts_across_all_byte_boundaries() {
    for (profile, reason, omit_object) in [
        (OpenAiStreamUsageProfile::ZaiFinalChoice, "sensitive", true),
        (
            OpenAiStreamUsageProfile::DeepSeekFinalChoice,
            "aborted",
            false,
        ),
    ] {
        let mut event: Value =
            serde_json::from_str(terminal(4).trim().strip_prefix("data: ").unwrap()).unwrap();
        event["choices"][0]["finish_reason"] = json!(reason);
        if omit_object {
            event.as_object_mut().unwrap().remove("object");
        }
        let wire = format!("data: {event}\n\ndata: [DONE]\n\n");
        for split in 0..=wire.len() {
            let mut observed = ProtocolStreamUsageObserver::for_endpoint_with_openai_usage_profile(
                EndpointFamilyV1::OpenaiChatCompletions,
                profile,
            )
            .unwrap();
            observed.push(&wire.as_bytes()[..split]).unwrap();
            observed.push(&wire.as_bytes()[split..]).unwrap();
            observed.finish().unwrap();
            let usage = observed.canonical_usage().unwrap();
            assert_eq!(
                (
                    usage.tokens_in,
                    usage.tokens_out,
                    usage.cache_read_tokens,
                    usage.cache_creation_tokens
                ),
                (2, 4, 6, 2)
            );
            assert_eq!(usage.completeness, UsageCompleteness::Final);
        }
        let mut standard = ProtocolStreamUsageObserver::new();
        assert!(standard.push(wire.as_bytes()).is_err());
        assert!(standard.canonical_usage().is_none());
    }
}

#[test]
fn vendor_profiles_preserve_strict_counters_and_terminal_envelopes() {
    use sandhi_core::usage::qualify_openai_stream_usage_for_profile as qualify;
    for profile in [
        OpenAiStreamUsageProfile::ZaiFinalChoice,
        OpenAiStreamUsageProfile::DeepSeekFinalChoice,
    ] {
        let event: Value =
            serde_json::from_str(terminal(4).trim().strip_prefix("data: ").unwrap()).unwrap();
        assert!(qualify(&event, profile).unwrap().is_some());
        for fault in [
            "object",
            "null_object",
            "index",
            "delta",
            "reason",
            "choices",
            "negative",
            "missing_count",
            "inconsistent_cache",
            "total",
        ] {
            let mut bad = event.clone();
            match fault {
                "object" => bad["object"] = json!("chat.completion"),
                "null_object" => bad["object"] = Value::Null,
                "index" => bad["choices"][0]["index"] = json!(1),
                "delta" => bad["choices"][0]["delta"] = Value::Null,
                "reason" => bad["choices"][0]["finish_reason"] = json!("unknown"),
                "choices" => bad["choices"] = json!([]),
                "negative" => bad["usage"]["prompt_tokens"] = json!(-1),
                "missing_count" => {
                    bad["usage"]
                        .as_object_mut()
                        .unwrap()
                        .remove("completion_tokens");
                }
                "inconsistent_cache" => {
                    bad["usage"]["prompt_tokens_details"]["cached_tokens"] = json!(20)
                }
                _ => bad["usage"]["total_tokens"] = json!(20),
            }
            assert!(qualify(&bad, profile).is_err(), "{profile:?}/{fault}");
        }
        let mut zero = event.clone();
        zero["usage"] = json!({"prompt_tokens":0,"completion_tokens":0,"total_tokens":0});
        let counts = qualify(&zero, profile).unwrap().unwrap();
        assert_eq!((counts.tokens_in, counts.tokens_out), (0, 0));
        let mut absent = event.clone();
        absent.as_object_mut().unwrap().remove("usage");
        assert!(qualify(&absent, profile).unwrap().is_none());
        if profile == OpenAiStreamUsageProfile::DeepSeekFinalChoice {
            for (field, value) in [
                ("content", json!("text")),
                ("reasoning_content", json!("reason")),
                ("tool_calls", json!([{}])),
                ("content", json!(3)),
            ] {
                let mut bad = event.clone();
                bad["choices"][0]["delta"][field] = value;
                assert_eq!(qualify(&bad, profile), Err(StreamUsageError::NonFinalUsage));
            }
            for reason in ["aborted", "insufficient_system_resource", "content_filter"] {
                let mut valid = event.clone();
                valid["choices"][0]["finish_reason"] = json!(reason);
                assert!(qualify(&valid, profile).unwrap().is_some());
            }
        }
    }
}

#[test]
fn deepseek_terminal_delta_rejects_alias_content_unknowns_and_invalid_roles() {
    use sandhi_core::usage::qualify_openai_stream_usage_for_profile as qualify;
    let event: Value =
        serde_json::from_str(terminal(4).trim().strip_prefix("data: ").unwrap()).unwrap();
    let profile = OpenAiStreamUsageProfile::DeepSeekFinalChoice;
    for field in ["content", "reasoning_content", "reasoning", "refusal"] {
        for value in [json!("late"), json!([]), json!({}), json!(false), json!(1)] {
            let mut bad = event.clone();
            bad["choices"][0]["delta"][field] = value;
            assert_eq!(
                qualify(&bad, profile),
                Err(StreamUsageError::NonFinalUsage),
                "{field}"
            );
        }
        for value in [Value::Null, json!("")] {
            let mut good = event.clone();
            good["choices"][0]["delta"][field] = value;
            assert!(qualify(&good, profile).unwrap().is_some());
        }
    }
    for (field, values) in [
        (
            "tool_calls",
            vec![json!([{}]), json!(""), json!({}), json!(true)],
        ),
        ("role", vec![json!("user"), json!(""), json!(3), json!([])]),
        (
            "unknown",
            vec![Value::Null, json!(""), json!([]), json!({}), json!("late")],
        ),
    ] {
        for value in values {
            let mut bad = event.clone();
            bad["choices"][0]["delta"][field] = value;
            assert_eq!(
                qualify(&bad, profile),
                Err(StreamUsageError::NonFinalUsage),
                "{field}"
            );
        }
    }
    for value in [Value::Null, json!("assistant")] {
        let mut good = event.clone();
        good["choices"][0]["delta"]["role"] = value;
        assert!(qualify(&good, profile).unwrap().is_some());
    }
}

#[test]
fn vendor_final_choice_fences_all_later_data_before_done_and_keeps_first_totals() {
    for profile in [
        OpenAiStreamUsageProfile::ZaiFinalChoice,
        OpenAiStreamUsageProfile::DeepSeekFinalChoice,
    ] {
        for next in [
            "data: {\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"late\"},\"finish_reason\":null}]}\n\n".to_string(),
            terminal(4),
            terminal(4).replace("\"content\":\"\"", "\"reasoning\":\"late\""),
        ] {
            let mut observed = ProtocolStreamUsageObserver::for_endpoint_with_openai_usage_profile(
                EndpointFamilyV1::OpenaiChatCompletions,profile).unwrap();
            observed.push(terminal(4).as_bytes()).unwrap();
            assert!(observed.push(format!("{next}data: [DONE]\n\n").as_bytes()).is_err());
            let counts = observed.canonical_usage().unwrap();
            assert_eq!((counts.tokens_in,counts.tokens_out,counts.cache_read_tokens,counts.cache_creation_tokens),(2,4,6,2));
            assert_eq!(counts.completeness, UsageCompleteness::Partial);
            assert!(!observed.is_done());
            assert!(observed.finish().is_err());
        }
    }
}
