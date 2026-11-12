//! Messages source transport, typed projection and independent usage ownership.
use futures_util::StreamExt;
use sandhi_core::{usage::AnthropicUsageProfile, ChatStreamEventV1 as Event, UsageCompleteness};
use sandhi_providers::{AttemptContext, AttemptOutcome, ProviderRuntime};
use serde_json::{json, Value};
use wiremock::{matchers::method, Mock, MockServer, ResponseTemplate};

fn frame(value: Value) -> String {
    format!(
        "event: {}\ndata: {value}\n\n",
        value["type"].as_str().unwrap()
    )
}
fn wire(implicit: bool, mode: &str) -> String {
    let mut usage = json!({"input_tokens":2,"output_tokens":1,"cache_read_input_tokens":6,
        "cache_creation_input_tokens":2,"output_tokens_details":{"thinking_tokens":0}});
    if implicit {
        usage
            .as_object_mut()
            .unwrap()
            .remove("cache_creation_input_tokens");
    }
    if mode == "missing_input" {
        usage.as_object_mut().unwrap().remove("input_tokens");
    }
    let mut out = frame(
        json!({"type":"message_start","message":{"id":"msg_fixture","type":"message","role":"assistant","model":"fixture","content":[],"usage":usage}}),
    );
    out += &frame(
        json!({"type":"content_block_start","index":0,"content_block":if mode=="opaque" {json!({"type":"thinking","thinking":"re","signature":"opaque"})} else {json!({"type":"text","text":"re"})}}),
    );
    out += &frame(
        json!({"type":"content_block_delta","index":0,"delta":if mode=="opaque" {json!({"type":"thinking_delta","thinking":"ply"})} else {json!({"type":"text_delta","text":"ply"})}}),
    );
    out += &frame(json!({"type":"content_block_stop","index":0}));
    if mode == "provider_error" {
        out += &frame(
            json!({"type":"error","error":{"type":"overloaded_error","message":"synthetic"}}),
        );
        return out;
    }
    out += &frame(
        json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":
        if mode=="missing_output" {json!({})} else if mode=="malformed" {json!({"output_tokens":"4"})} else {json!({"output_tokens":4})}}),
    );
    if mode != "missing_terminal" {
        out += &frame(json!({"type":"message_stop"}));
    }
    if mode == "post_terminal" {
        out += &frame(json!({"type":"ping"}));
    }
    if mode == "truncated" {
        out += "data: {";
    }
    out
}

#[tokio::test]
async fn messages_profiles_share_one_attempt_on_both_streaming_planes() {
    for implicit in [false, true] {
        for mode in [
            "final",
            "missing_input",
            "missing_output",
            "malformed",
            "missing_terminal",
            "post_terminal",
            "truncated",
            "provider_error",
        ] {
            for native in [false, true] {
                let server = MockServer::start().await;
                let response = wire(implicit, mode);
                Mock::given(method("POST"))
                    .respond_with(
                        ResponseTemplate::new(200)
                            .insert_header("anthropic-request-id", "origin-messages")
                            .set_body_raw(response.clone(), "text/event-stream"),
                    )
                    .expect(2)
                    .mount(&server)
                    .await;
                let provider = ProviderRuntime::new().anthropic_with_usage_profile(
                    server.uri(),
                    "synthetic",
                    sandhi_providers::AnthropicAuthScheme::ApiKey,
                    Default::default(),
                    Some(0),
                    None,
                    None,
                    None,
                    if implicit {
                        AnthropicUsageProfile::ImplicitCache
                    } else {
                        AnthropicUsageProfile::SeparateCacheWrites
                    },
                );
                assert!(provider.supports_owned_streaming());
                let request = serde_json::from_value(
                    json!({"model":"fixture","max_output_tokens":64,"include_native_stream":native,
                    "messages":[{"role":"user","content":"hello"}]}),
                )
                .unwrap();
                let (context, receiver) =
                    AttemptContext::single_attempt("typed-messages", "e".repeat(64)).unwrap();
                let events: Vec<_> = provider
                    .stream_with_qualified_usage_and_attempts(
                        request,
                        Default::default(),
                        context.clone(),
                    )
                    .await
                    .unwrap()
                    .collect()
                    .await;
                assert_eq!(
                    events.iter().any(Result::is_err),
                    mode != "final",
                    "{implicit}/{native}/{mode}"
                );
                assert_eq!(
                    events
                        .iter()
                        .any(|e| matches!(e, Ok(Event::NativeFrame { .. }))),
                    native
                );
                assert_eq!(
                    events
                        .iter()
                        .filter_map(|e| match e {
                            Ok(Event::TextDelta { delta }) => Some(delta.as_str()),
                            _ => None,
                        })
                        .collect::<String>(),
                    "reply"
                );
                let finals: Vec<_> = events
                    .iter()
                    .filter_map(|e| match e {
                        Ok(Event::Usage { usage })
                            if usage.completeness == UsageCompleteness::Final =>
                        {
                            Some(usage)
                        }
                        _ => None,
                    })
                    .collect();
                assert_eq!(finals.len(), usize::from(mode == "final"));
                assert_eq!(
                    events.iter().any(|e| matches!(e, Ok(Event::Finish { .. }))),
                    mode == "final"
                );
                let evidence = context.qualified_stream_evidence().unwrap();
                let usage = evidence.usage.unwrap();
                assert_eq!(
                    usage.completeness,
                    if mode == "final" {
                        UsageCompleteness::Final
                    } else {
                        UsageCompleteness::Partial
                    }
                );
                assert_eq!(usage.cache_read_tokens, 6);
                assert_eq!(usage.cache_creation_tokens, if implicit { 0 } else { 2 });
                assert_eq!(usage.reasoning_tokens, Some(0));
                assert_eq!(
                    usage.upstream_request_id.as_deref(),
                    Some("origin-messages")
                );
                assert_eq!(
                    evidence.snapshot.outcome,
                    Some(if mode == "final" {
                        AttemptOutcome::Success
                    } else if mode == "provider_error" {
                        // The source already reported a rejection before the
                        // native projector returned; dropping is not cancellation.
                        AttemptOutcome::ProviderRejected
                    } else {
                        AttemptOutcome::IncompleteStream
                    })
                );
                assert_eq!(receiver.drain().len(), 2);
                assert_eq!(context.dispatch_count(), 1);

                let (context, receiver) =
                    AttemptContext::single_attempt("raw-messages", "e".repeat(64)).unwrap();
                let raw = provider
                    .raw_forwarder()
                    .unwrap()
                    .clone()
                    .with_metered_attempt_context("anthropic", None, context.clone());
                let source = raw
                    .forward_stream_qualified_with_headers(
                        "/v1/messages",
                        bytes::Bytes::from_static(b"{\"model\":\"fixture\",\"stream\":true}"),
                        None,
                        None,
                        &Default::default(),
                    )
                    .await
                    .unwrap();
                let observation = source.observation;
                let forwarded: Vec<_> = source.stream.collect().await;
                let bytes: Vec<_> = forwarded.into_iter().flat_map(Result::unwrap).collect();
                assert_eq!(
                    bytes,
                    response.as_bytes(),
                    "transparent response bytes stay unchanged"
                );
                assert_eq!(
                    observation.evidence().usage.unwrap().completeness,
                    if mode == "final" {
                        UsageCompleteness::Final
                    } else {
                        UsageCompleteness::Partial
                    }
                );
                assert_eq!(receiver.drain().len(), 2);
                assert_eq!(context.dispatch_count(), 1);
                assert_eq!(server.received_requests().await.unwrap().len(), 2);
            }
        }
    }
}

#[tokio::test]
async fn portable_refusal_drains_same_messages_source_and_preserves_final_counts() {
    for native in [false, true] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(200).set_body_raw(wire(false, "opaque"), "text/event-stream"),
            )
            .expect(1)
            .mount(&server)
            .await;
        let provider = ProviderRuntime::new().anthropic(
            server.uri(),
            "synthetic",
            Default::default(),
            Default::default(),
            Some(0),
            None,
            None,
            None,
        );
        let request=serde_json::from_value(json!({"model":"fixture","max_output_tokens":64,"include_native_stream":native,"messages":[{"role":"user","content":"hello"}]})).unwrap();
        let (context, receiver) =
            AttemptContext::single_attempt("messages-refusal", "f".repeat(64)).unwrap();
        let events = provider
            .stream_with_qualified_usage_and_attempts(request, Default::default(), context.clone())
            .await
            .unwrap()
            .collect::<Vec<_>>()
            .await;
        assert_eq!(events.iter().any(Result::is_err), !native);
        assert_eq!(
            events
                .iter()
                .any(|event| matches!(event, Ok(Event::Finish { .. }))),
            native
        );
        let evidence = context.qualified_stream_evidence().unwrap();
        assert_eq!(evidence.snapshot.outcome, Some(AttemptOutcome::Success));
        assert!(evidence.snapshot.qualification_error.is_none());
        let usage = evidence.usage.unwrap();
        assert_eq!(usage.completeness, UsageCompleteness::Final);
        assert_eq!(
            (
                usage.tokens_in,
                usage.cache_read_tokens,
                usage.cache_creation_tokens,
                usage.tokens_out
            ),
            (2, 6, 2, 4)
        );
        assert_eq!(context.dispatch_count(), 1);
        assert_eq!(receiver.drain().len(), 2);
    }
}
