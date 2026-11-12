use super::*;
use serde_json::json;

fn observer() -> ProtocolStreamUsageObserver {
    ProtocolStreamUsageObserver::for_endpoint(EndpointFamilyV1::GeminiGenerateContent).unwrap()
}

fn frame(value: Value) -> String {
    format!("data: {value}\n\n")
}

fn counts() -> Value {
    json!({"promptTokenCount":11,"cachedContentTokenCount":8,"candidatesTokenCount":4,
        "thoughtsTokenCount":2,"toolUsePromptTokenCount":1,"totalTokenCount":18})
}

fn terminal(usage: Value) -> String {
    frame(
        json!({"responseId":"gemini-synthetic","modelVersion":"synthetic-1",
        "candidates":[{"index":0,"content":{"parts":[{"text":"雪"}]},"finishReason":"STOP"}],
        "usageMetadata":usage}),
    )
}

#[test]
fn gemini_complete_frames_are_invariant_at_every_byte_boundary_and_need_eof() {
    let wire = format!(
        "\u{feff}: ping\n\nid: untrusted-id\nevent: message\n{}{}: tail\n\n",
        frame(
            json!({"responseId":"gemini-synthetic","modelVersion":"synthetic-1",
        "candidates":[{"content":{"parts":[{"text":"hello"}]}}],
        "usageMetadata":{"promptTokenCount":11,"cachedContentTokenCount":8}})
        ),
        terminal(counts())
    );
    for newline in ["\n", "\r\n", "\r"] {
        let wire = wire.replace('\n', newline);
        for split in 0..=wire.len() {
            let mut observed = observer();
            observed.push(&wire.as_bytes()[..split]).unwrap();
            observed.push(&wire.as_bytes()[split..]).unwrap();
            assert!(!observed.is_done());
            assert!(observed.usage().is_none());
            assert_eq!(
                observed.canonical_usage().unwrap().completeness,
                UsageCompleteness::Partial
            );
            observed.finish().unwrap();
            observed.finish().unwrap();
            assert!(observed.is_done());
            let usage = observed.canonical_usage().unwrap();
            assert_eq!(
                (usage.tokens_in, usage.cache_read_tokens, usage.tokens_out),
                (4, 8, 4)
            );
            assert_eq!(usage.reasoning_tokens, Some(2));
            assert_eq!(usage.reasoning_included, Some(false));
            assert_eq!(usage.completeness, UsageCompleteness::Final);
            assert_eq!(observed.usage(), Some((&usage).into()));
            assert_eq!(observed.push(b"x"), Err(StreamObservationError::Closed));
        }
    }
}

#[test]
fn gemini_usage_trailer_is_cumulative_and_can_follow_candidate_finish() {
    let mut observed = observer();
    observed
        .push(frame(json!({"candidates":[{"finishReason":"MAX_TOKENS"}]})).as_bytes())
        .unwrap();
    observed
        .push(frame(json!({"usageMetadata":counts()})).as_bytes())
        .unwrap();
    observed
        .push(frame(json!({"usageMetadata":counts()})).as_bytes())
        .unwrap();
    observed.finish().unwrap();
    assert_eq!(observed.usage().unwrap().tokens_out, 4);

    // A new candidate invalidates the earlier terminal measurement. Its own
    // finish without another complete usage frame must not reuse older totals.
    let mut observed = observer();
    observed.push(terminal(counts()).as_bytes()).unwrap();
    observed
        .push(frame(json!({"candidates":[{"index":1,"finishReason":"STOP"}]})).as_bytes())
        .unwrap();
    assert!(observed.finish().is_err());
    assert!(observed.usage().is_none());
}

#[test]
fn gemini_missing_terminal_parents_or_total_never_become_zero() {
    for missing in [
        "promptTokenCount",
        "candidatesTokenCount",
        "totalTokenCount",
    ] {
        let mut usage = counts();
        usage.as_object_mut().unwrap().remove(missing);
        let mut observed = observer();
        observed.push(terminal(usage).as_bytes()).unwrap();
        assert!(observed.finish().is_err(), "{missing}");
        assert!(observed.usage().is_none());
        assert_eq!(
            observed.canonical_usage().unwrap().completeness,
            UsageCompleteness::Partial
        );
    }
    for value in [
        json!({"usageMetadata":counts()}),
        json!({
        "candidates":[{"finishReason":"FINISH_REASON_UNSPECIFIED"}],"usageMetadata":counts()}),
    ] {
        let mut observed = observer();
        observed.push(frame(value).as_bytes()).unwrap();
        assert_eq!(
            observed.finish(),
            Err(StreamObservationError::MissingTerminator)
        );
        assert!(observed.usage().is_none());
    }
    let mut observed = observer();
    observed
        .push(frame(json!({"usageMetadata":counts()})).as_bytes())
        .unwrap();
    observed
        .push(frame(json!({"candidates":[{"finishReason":"STOP"}]})).as_bytes())
        .unwrap();
    assert!(
        observed.finish().is_err(),
        "pre-terminal totals cannot become final at EOF"
    );
}

#[test]
fn gemini_truncation_abort_and_late_data_preserve_partial_evidence() {
    for suffix in ["data: {", "data: {}\n", ": unfinished"] {
        let mut observed = observer();
        observed
            .push((terminal(counts()) + suffix).as_bytes())
            .unwrap();
        assert!(observed.finish().is_err());
        assert!(observed.usage().is_none());
        assert_eq!(
            observed.canonical_usage().unwrap().completeness,
            UsageCompleteness::Partial
        );
    }
    let mut observed = observer();
    observed.push(terminal(counts()).as_bytes()).unwrap();
    observed.close_on_abort();
    assert!(observed.usage().is_none());
    assert_eq!(observed.finish(), Err(StreamObservationError::Closed));

    for suffix in [
        terminal(counts()),
        frame(json!({"candidates":[{"content":{"parts":[{"text":"late"}]}}]})),
        "data: [DONE]\n\n".into(),
        "event: error\ndata: {}\n\n".into(),
    ] {
        let mut observed = observer();
        observed.push(terminal(counts()).as_bytes()).unwrap();
        assert!(observed.push(suffix.as_bytes()).is_err());
        assert!(observed.finish().is_err());
        assert_eq!(observed.canonical_usage().unwrap().tokens_out, 4);
        assert!(observed.usage().is_none());
    }
}

#[test]
fn gemini_counter_regressions_and_identity_drift_are_sticky() {
    let mut invalid = vec![
        frame(json!({"responseId":"other"})),
        frame(json!({"modelVersion":"other"})),
        frame(json!({"candidates":[{"index":64}]})),
        frame(json!({"candidates":[{"index":1},{"index":1}]})),
        frame(json!({"candidates":[{},{}]})),
        frame(json!({"usageMetadata":{"cachedContentTokenCount":12,"promptTokenCount":11}})),
        frame(
            json!({"usageMetadata":{"promptTokenCount":11,"candidatesTokenCount":4,"totalTokenCount":14}}),
        ),
        "data: {\"usageMetadata\":{},\"usageMetadata\":{}}\n\n".into(),
    ];
    for key in [
        "promptTokenCount",
        "candidatesTokenCount",
        "cachedContentTokenCount",
        "thoughtsTokenCount",
        "toolUsePromptTokenCount",
        "totalTokenCount",
    ] {
        invalid.push(frame(json!({"usageMetadata":{key:0}})));
        for bad in [
            Value::Null,
            json!(-1),
            json!(1.5),
            json!("1"),
            json!(50_000_000_001_u64),
        ] {
            invalid.push(frame(json!({"usageMetadata":{key:bad}})));
        }
    }
    for suffix in invalid {
        let mut observed = observer();
        observed.push(terminal(counts()).as_bytes()).unwrap();
        let before = observed.canonical_usage();
        assert!(observed.push(suffix.as_bytes()).is_err(), "{suffix}");
        assert_eq!(observed.canonical_usage(), before);
        assert!(observed.finish().is_err());
        assert!(observed.usage().is_none());
    }
}

#[test]
fn gemini_all_candidates_must_finish_and_indices_are_bounded() {
    for finished in [false, true] {
        let mut observed = observer();
        observed
            .push(
                frame(
                    json!({"candidates":[{"index":0,"finishReason":"STOP"},{"index":1}],
            "usageMetadata":counts()}),
                )
                .as_bytes(),
            )
            .unwrap();
        if finished {
            observed
                .push(
                    frame(json!({"candidates":[{"index":1,"finishReason":"STOP"}],
                "usageMetadata":counts()}))
                    .as_bytes(),
                )
                .unwrap();
        }
        assert_eq!(observed.finish().is_ok(), finished);
        assert_eq!(observed.usage().is_some(), finished);
    }
}

#[test]
fn gemini_error_and_prompt_block_are_distinct_from_unknown_output() {
    let mut observed = observer();
    observed
        .push(frame(json!({"usageMetadata":{"promptTokenCount":3}})).as_bytes())
        .unwrap();
    assert_eq!(
        observed.push(frame(json!({"error":{"code":503,"message":"private"}})).as_bytes()),
        Err(StreamObservationError::ProviderEvent)
    );
    assert!(observed.provider_failed());
    assert_eq!(observed.canonical_usage().unwrap().tokens_in, 3);
    assert!(observed.usage().is_none());
    for output in [None, Some(0), Some(1)] {
        let mut usage = json!({"promptTokenCount":3,"totalTokenCount":3+output.unwrap_or(0)});
        if let Some(output) = output {
            usage["candidatesTokenCount"] = output.into();
        }
        let mut observed = observer();
        observed
            .push(
                frame(json!({"promptFeedback":{"blockReason":"SAFETY"},"usageMetadata":usage}))
                    .as_bytes(),
            )
            .unwrap();
        assert_eq!(observed.finish().is_ok(), output == Some(0));
        assert!(!observed.provider_failed());
    }
}

#[test]
fn gemini_optional_zero_presence_survives_and_nonzero_may_not_disappear() {
    for reasoning in [None, Some(0), Some(2)] {
        let mut initial = json!({"promptTokenCount":11,"cachedContentTokenCount":0});
        if let Some(n) = reasoning {
            initial["thoughtsTokenCount"] = n.into();
        }
        let mut observed = observer();
        observed
            .push(frame(json!({"usageMetadata":initial})).as_bytes())
            .unwrap();
        observed
            .push(
                terminal(
                    json!({"promptTokenCount":11,"candidatesTokenCount":4,"totalTokenCount":15}),
                )
                .as_bytes(),
            )
            .unwrap();
        if reasoning == Some(2) {
            assert!(observed.finish().is_err());
        } else {
            observed.finish().unwrap();
            assert_eq!(
                observed.canonical_usage().unwrap().reasoning_tokens,
                reasoning
            );
        }
    }
    assert!(
        ProtocolStreamUsageObserver::for_endpoint_with_openai_usage_profile(
            EndpointFamilyV1::GeminiGenerateContent,
            OpenAiStreamUsageProfile::TerminalChunk
        )
        .is_err()
    );
    assert!(
        ProtocolStreamUsageObserver::for_endpoint_with_usage_profiles(
            EndpointFamilyV1::GeminiGenerateContent,
            OpenAiStreamUsageProfile::Standard,
            AnthropicUsageProfile::ImplicitCache
        )
        .is_err()
    );
}

#[test]
fn gemini_newer_subsets_only_establish_partial_lower_bounds() {
    for final_prompt in [None, Some(3), Some(10)] {
        let mut observed = observer();
        observed
            .push(frame(json!({"usageMetadata":{"promptTokenCount":3}})).as_bytes())
            .unwrap();
        observed
            .push(frame(json!({"usageMetadata":{"cachedContentTokenCount":8}})).as_bytes())
            .unwrap();
        let partial = observed.canonical_usage().unwrap();
        assert_eq!((partial.tokens_in, partial.cache_read_tokens), (0, 8));
        assert_eq!(partial.completeness, UsageCompleteness::Partial);
        let mut usage =
            json!({"cachedContentTokenCount":8,"candidatesTokenCount":4,"totalTokenCount":14});
        if let Some(prompt) = final_prompt {
            usage["promptTokenCount"] = prompt.into();
        }
        let pushed = observed.push(terminal(usage).as_bytes());
        assert_eq!(pushed.is_ok(), final_prompt != Some(3));
        assert_eq!(observed.finish().is_ok(), final_prompt == Some(10));
        assert_eq!(observed.usage().is_some(), final_prompt == Some(10));
    }
}
