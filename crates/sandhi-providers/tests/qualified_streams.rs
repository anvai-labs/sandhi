//! Qualified typed source APIs; broader HTTP activation is a separate checkpoint.
use futures_util::StreamExt;
use sandhi_core::{ChatRequestV1, ChatStreamEventV1 as Event, UsageCompleteness};
use sandhi_providers::{AttemptContext, AttemptOutcome, AttemptPhase, ProviderRuntime};
use serde_json::{json, Value};
use wiremock::{matchers::method, Mock, MockServer, ResponseTemplate};

fn request(native: bool) -> ChatRequestV1 {
    serde_json::from_value(
        json!({"model":"fixture","max_output_tokens":64,"include_native_stream":native,
        "messages":[{"role":"developer","content":"Code."},{"role":"user","content":"hello"}]}),
    )
    .unwrap()
}

#[tokio::test]
async fn qualified_typed_stream_matrix_retains_canonical_counts_and_refuses_replay() {
    for profile in ["chat", "responses", "codex"] {
        for native in [false, true] {
            for mode in [
                "final",
                "missing",
                "malformed",
                "post_terminal",
                "truncated",
                "missing_terminal",
            ] {
                let server = MockServer::start().await;
                let mut usage = if profile == "chat" {
                    json!({"prompt_tokens":10,"completion_tokens":4,"prompt_tokens_details":{"cached_tokens":6,"cache_write_tokens":2},"completion_tokens_details":{"reasoning_tokens":0}})
                } else {
                    json!({"input_tokens":10,"output_tokens":4,"input_tokens_details":{"cached_tokens":6,"cache_write_tokens":2},"output_tokens_details":{"reasoning_tokens":0}})
                };
                if mode == "malformed" {
                    usage[if profile == "chat" {
                        "completion_tokens"
                    } else {
                        "output_tokens"
                    }] = json!("bad");
                }
                let mut wire = if profile == "chat" {
                    format!(
                        "data: {}\n\n",
                        json!({"object":"chat.completion.chunk","choices":[{"index":0,"delta":{"content":"reply"},"finish_reason":"stop"}]})
                    )
                } else {
                    format!(
                        "data: {}\n\n",
                        json!({"type":"response.output_text.delta","delta":"reply"})
                    )
                };
                if profile == "chat" {
                    if mode != "missing" {
                        wire.push_str(&format!(
                            "data: {}\n\n",
                            json!({"object":"chat.completion.chunk","choices":[],"usage":usage})
                        ));
                    }
                    if mode != "missing_terminal" {
                        wire.push_str("data: [DONE]\n\n");
                    }
                } else if mode != "missing_terminal" {
                    let mut response = json!({"id":"resp","status":"completed","usage":usage});
                    if mode == "missing" {
                        response.as_object_mut().unwrap().remove("usage");
                    }
                    wire.push_str(&format!(
                        "data: {}\n\n",
                        json!({"type":"response.completed","response":response})
                    ));
                }
                if mode == "post_terminal" {
                    wire.push_str("data: {}\n\n");
                }
                if mode == "truncated" {
                    wire.push_str("data: {");
                }
                Mock::given(method("POST"))
                    .respond_with(
                        ResponseTemplate::new(200)
                            .set_body_raw(wire, "text/event-stream")
                            .insert_header("x-request-id", "origin"),
                    )
                    .expect(1)
                    .mount(&server)
                    .await;
                let runtime = ProviderRuntime::new();
                let provider = match profile {
                    "chat" => runtime.openai_compat(
                        "models",
                        server.uri(),
                        "synthetic",
                        Default::default(),
                        Some(0),
                        None,
                        None,
                        None,
                    ),
                    "responses" => runtime.openai_responses(
                        "models",
                        server.uri(),
                        "synthetic",
                        Default::default(),
                        Some(0),
                        None,
                        None,
                        None,
                    ),
                    _ => runtime
                        .chatgpt_responses(
                            "models",
                            server.uri(),
                            "synthetic",
                            Default::default(),
                            Some(0),
                            None,
                            None,
                            None,
                        )
                        .with_credential_expiry(4_000_000_000),
                };
                assert!(provider.supports_owned_streaming());
                provider
                    .preflight_owned_streaming(&request(native))
                    .unwrap();
                let (context, receiver) =
                    AttemptContext::single_attempt("execution", "e".repeat(64)).unwrap();
                let stream = provider
                    .stream_with_qualified_usage_and_attempts(
                        request(native),
                        Default::default(),
                        context.clone(),
                    )
                    .await
                    .unwrap();
                let events: Vec<_> = stream.collect().await;
                assert_eq!(
                    events.iter().any(Result::is_err),
                    mode != "final",
                    "{profile}/{native}/{mode}"
                );
                assert_eq!(
                    events
                        .iter()
                        .any(|event| matches!(event, Ok(Event::NativeFrame { .. }))),
                    native,
                    "{profile}/{mode}"
                );
                assert!(events
                    .iter()
                    .any(|event| matches!(event,Ok(Event::TextDelta{delta})if delta=="reply")));
                let finals: Vec<_> = events
                    .iter()
                    .filter_map(|event| match event {
                        Ok(Event::Usage { usage })
                            if usage.completeness == UsageCompleteness::Final =>
                        {
                            Some(usage)
                        }
                        _ => None,
                    })
                    .collect();
                if mode == "final" {
                    assert_eq!(finals.len(), 1);
                    assert_eq!(
                        (
                            finals[0].tokens_in,
                            finals[0].cache_read_tokens,
                            finals[0].cache_creation_tokens,
                            finals[0].tokens_out
                        ),
                        (2, 6, 2, 4)
                    );
                    assert_eq!(finals[0].reasoning_tokens, Some(0));
                } else if matches!(mode, "missing" | "malformed") {
                    assert!(
                        finals.is_empty(),
                        "missing/malformed totals cannot become measured counts"
                    );
                }
                let evidence = context.qualified_stream_evidence().unwrap();
                assert_eq!(
                    evidence.snapshot.outcome,
                    Some(
                        if mode == "final" || (profile == "chat" && mode == "missing") {
                            AttemptOutcome::Success
                        } else {
                            AttemptOutcome::IncompleteStream
                        }
                    ),
                    "{profile}/{native}/{mode}"
                );
                if let Some(usage) = &evidence.usage {
                    assert_eq!(
                        usage.completeness,
                        if mode == "final" {
                            UsageCompleteness::Final
                        } else {
                            UsageCompleteness::Partial
                        }
                    );
                    assert_eq!(usage.cache_read_tokens, 6);
                    assert_eq!(usage.reasoning_tokens, Some(0));
                    assert_eq!(usage.upstream_request_id.as_deref(), Some("origin"));
                } else {
                    assert!(matches!(mode, "missing" | "malformed" | "missing_terminal"));
                }
                let observed = receiver.drain();
                assert_eq!(observed.len(), 2);
                assert!(matches!(
                    &observed[1].phase,
                    AttemptPhase::Terminal {
                        provider_status: Some(200),
                        ..
                    }
                ));
                assert_eq!(context.dispatch_count(), 1);
                assert!(provider
                    .stream_with_qualified_usage_and_attempts(
                        request(native),
                        Default::default(),
                        context
                    )
                    .await
                    .is_err());
                let calls = server.received_requests().await.unwrap();
                assert_eq!(calls.len(), 1);
                let body: Value = calls[0].body_json().unwrap();
                assert_eq!(body["stream"], true);
                if profile == "chat" {
                    assert_eq!(body["stream_options"]["include_usage"], true);
                }
                if profile == "codex" {
                    assert!(provider.raw_forwarder().is_none());
                    assert_eq!(body["store"], false);
                }
            }
        }
    }
}
