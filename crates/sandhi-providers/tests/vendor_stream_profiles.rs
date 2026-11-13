//! Explicit endpoint contracts apply equally to transparent and typed source observations.
use futures_util::StreamExt;
use sandhi_core::{ChatRequestV1, ChatStreamEventV1 as Event, UsageCompleteness};
use sandhi_providers::{AttemptContext, ProviderOptions, ProviderRuntime};
use serde_json::json;
use wiremock::{matchers::method, Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn explicit_vendor_profiles_preserve_raw_wire_and_typed_counts_without_replay() {
    for (profile, omit_object) in [
        ("zai_final_choice_usage", true),
        ("deepseek_final_choice_usage", false),
    ] {
        for mode in ["raw", "raw_postfinal", "portable", "native", "standard"] {
            let server = MockServer::start().await;
            let mut content = json!({"object":"chat.completion.chunk","choices":[{"index":0,"delta":{"content":"reply"},"finish_reason":null}]});
            let mut terminal = json!({"object":"chat.completion.chunk","choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":10,"completion_tokens":4,"total_tokens":14,"prompt_tokens_details":{"cached_tokens":6,"cache_write_tokens":2},"completion_tokens_details":{"reasoning_tokens":0}}});
            if omit_object {
                content.as_object_mut().unwrap().remove("object");
                terminal.as_object_mut().unwrap().remove("object");
            }
            let extra = if mode == "raw_postfinal" {
                format!("data: {content}\n\n")
            } else {
                String::new()
            };
            let wire = format!("data: {content}\n\ndata: {terminal}\n\n{extra}data: [DONE]\n\n");
            Mock::given(method("POST"))
                .respond_with(
                    ResponseTemplate::new(200).set_body_raw(wire.clone(), "text/event-stream"),
                )
                .expect(1)
                .mount(&server)
                .await;
            let handle = ProviderRuntime::new()
                .configured_provider(ProviderOptions {
                    // An unrelated label proves the endpoint profile owns selection.
                    provider: if mode == "standard" {
                        "deepseek"
                    } else {
                        "custom-label"
                    }
                    .into(),
                    model: "fixture".into(),
                    api_key: "synthetic".into(),
                    base_url: Some(server.uri()),
                    headers: Default::default(),
                    max_retries: Some(0),
                    timeout_secs: None,
                    stream_setup_timeout_secs: None,
                    stream_idle_timeout_secs: None,
                    auth_scheme: None,
                    protocol: Some("chat_completions".into()),
                    endpoint_profile: Some(
                        if mode == "standard" {
                            "standard"
                        } else {
                            profile
                        }
                        .into(),
                    ),
                    peer_role: None,
                })
                .unwrap()
                .handle;
            let (context, receiver) =
                AttemptContext::single_attempt("vendor-profile", "f".repeat(64)).unwrap();
            if mode.starts_with("raw") {
                let response = handle
                    .raw_forwarder()
                    .unwrap()
                    .clone()
                    .with_metered_attempt_context(
                        "custom-label",
                        Some("fixture".into()),
                        context.clone(),
                    )
                    .forward_stream_qualified_with_headers(
                        "/v1/chat/completions",
                        bytes::Bytes::from_static(
                            br#"{"model":"fixture","stream":true,"messages":[]}"#,
                        ),
                        None,
                        None,
                        &Default::default(),
                    )
                    .await
                    .unwrap();
                let chunks: Vec<_> = response.stream.collect().await;
                let bytes: Vec<_> = chunks
                    .into_iter()
                    .flat_map(|chunk| chunk.unwrap().to_vec())
                    .collect();
                assert_eq!(bytes, wire.as_bytes());
                assert_eq!(
                    response
                        .observation
                        .snapshot()
                        .qualification_error
                        .is_some(),
                    mode == "raw_postfinal"
                );
                assert_eq!(
                    response.observation.evidence().usage.unwrap().completeness,
                    if mode == "raw_postfinal" {
                        UsageCompleteness::Partial
                    } else {
                        UsageCompleteness::Final
                    }
                );
                assert_eq!(response.observation.snapshot().usage.unwrap().tokens_out, 4);
            } else {
                let request: ChatRequestV1 = serde_json::from_value(json!({"model":"fixture","max_output_tokens":64,"include_native_stream":mode=="native","messages":[{"role":"user","content":"hello"}]})).unwrap();
                let events: Vec<_> = handle
                    .stream_with_qualified_usage_and_attempts(
                        request.clone(),
                        Default::default(),
                        context.clone(),
                    )
                    .await
                    .unwrap()
                    .collect()
                    .await;
                assert_eq!(events.iter().any(Result::is_err), mode == "standard");
                if mode != "standard" {
                    assert!(events.iter().any(
                        |event| matches!(event, Ok(Event::TextDelta { delta }) if delta=="reply")
                    ));
                    assert_eq!(
                        events
                            .iter()
                            .any(|event| matches!(event, Ok(Event::NativeFrame { .. }))),
                        mode == "native"
                    );
                    let usage = events
                        .iter()
                        .find_map(|event| match event {
                            Ok(Event::Usage { usage })
                                if usage.completeness == UsageCompleteness::Final =>
                            {
                                Some(usage)
                            }
                            _ => None,
                        })
                        .unwrap();
                    assert_eq!(
                        (
                            usage.tokens_in,
                            usage.tokens_out,
                            usage.cache_read_tokens,
                            usage.cache_creation_tokens
                        ),
                        (2, 4, 6, 2)
                    );
                    assert_eq!(usage.reasoning_tokens, Some(0));
                }
                assert!(handle
                    .stream_with_qualified_usage_and_attempts(request, Default::default(), context)
                    .await
                    .is_err());
            }
            assert_eq!(server.received_requests().await.unwrap().len(), 1);
            assert_eq!(receiver.drain().len(), 2);
        }
    }
}
