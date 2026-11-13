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
    for fault in ["conflict", "truncated", "post_terminal", "missing_terminal"] {
        let mut observed = observer();
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
fn request_totals_fence_late_progress_and_only_allow_empty_identical_repetitions() {
    for profile in [
        OpenAiStreamUsageProfile::Standard,
        OpenAiStreamUsageProfile::TerminalChunk,
    ] {
        let mut first: Value =
            serde_json::from_str(terminal(4).trim().strip_prefix("data: ").unwrap()).unwrap();
        if profile == OpenAiStreamUsageProfile::Standard {
            first["choices"] = json!([]);
        }
        let frame = format!("data: {first}\n\n");
        for fault in [
            "progress",
            "content",
            "reasoning",
            "reasoning_content",
            "refusal",
            "tool_calls",
            "unknown",
            "role",
            "missing_done",
            "identical",
        ] {
            let mut observed = ProtocolStreamUsageObserver::for_endpoint_with_openai_usage_profile(
                EndpointFamilyV1::OpenaiChatCompletions,
                profile,
            )
            .unwrap();
            observed.push(frame.as_bytes()).unwrap();
            let mut later: Value =
                serde_json::from_str(terminal(4).trim().strip_prefix("data: ").unwrap()).unwrap();
            let result = match fault {
                "missing_done" => observed.finish(),
                "identical" => {
                    observed.push(frame.as_bytes()).unwrap();
                    observed.push(b"data: [DONE]\n\n").unwrap();
                    observed.finish()
                }
                _ => {
                    if fault == "progress" {
                        later.as_object_mut().unwrap().remove("usage");
                        later["choices"][0]["finish_reason"] = Value::Null;
                        later["choices"][0]["delta"]["content"] = json!("late");
                    } else {
                        later["choices"][0]["delta"][fault] = match fault {
                            "tool_calls" => json!([{}]),
                            "unknown" => Value::Null,
                            "role" => json!("user"),
                            _ => json!("late"),
                        };
                    }
                    observed.push(format!("data: {later}\n\ndata: [DONE]\n\n").as_bytes())
                }
            };
            assert_eq!(result.is_ok(), fault == "identical", "{profile:?}/{fault}");
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
            assert_eq!(
                usage.completeness,
                if fault == "identical" {
                    UsageCompleteness::Final
                } else {
                    UsageCompleteness::Partial
                }
            );
        }
    }
}

#[test]
fn first_content_bearing_generic_terminal_can_be_followed_by_an_empty_total_repeat() {
    let first = terminal(4).replace("\"content\":\"\"", "\"content\":\"terminal reply\"");
    let mut observed = observer();
    observed.push(first.as_bytes()).unwrap();
    observed.push(terminal(4).as_bytes()).unwrap();
    observed.push(b"data: [DONE]\n\n").unwrap();
    observed.finish().unwrap();
    assert_eq!(
        observed.canonical_usage().unwrap().completeness,
        UsageCompleteness::Final
    );
    assert_eq!(observed.canonical_usage().unwrap().tokens_out, 4);
}
