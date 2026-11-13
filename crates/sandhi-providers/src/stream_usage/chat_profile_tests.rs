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
