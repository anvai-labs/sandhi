//! One real transport invocation for qualified usage; no model retries.
use sandhi_core::{ChatRequestV1, EndpointFamilyV1, UsageCompleteness};
use sandhi_providers::{
    AttemptContext, AttemptOutcome, AttemptPhase, ProviderError, ProviderRuntime,
};
use serde_json::{json, Value};
use wiremock::{matchers::method, Mock, MockServer, ResponseTemplate};

fn request() -> ChatRequestV1 {
    serde_json::from_value(json!({"model":"fixture","max_output_tokens":64,
        "messages":[{"role":"user","content":"hello"}],"include_native_response":false}))
    .unwrap()
}

#[tokio::test]
async fn qualified_buffered_families_use_one_send_and_preserve_unknown_and_zero() {
    for family in [
        EndpointFamilyV1::AnthropicMessages,
        EndpointFamilyV1::OpenaiResponses,
        EndpointFamilyV1::GeminiGenerateContent,
    ] {
        for mode in [
            "missing",
            "null",
            "malformed",
            "zero",
            "split",
            "inconsistent",
        ] {
            let server = MockServer::start().await;
            let (mut body, path, usage_key, good) = match family {
                EndpointFamilyV1::AnthropicMessages => (
                    json!({"id":"msg_fixture","type":"message","role":"assistant","model":"fixture","content":[{"type":"text","text":"reply"}],"stop_reason":"end_turn"}),
                    "/v1/messages",
                    "usage",
                    json!({"input_tokens":10,"output_tokens":20,"cache_read_input_tokens":60,"cache_creation_input_tokens":30,"output_tokens_details":{"thinking_tokens":10}}),
                ),
                EndpointFamilyV1::OpenaiResponses => (
                    json!({"id":"resp_fixture","object":"response","status":"completed","model":"fixture","output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"reply"}]}]}),
                    "/responses",
                    "usage",
                    json!({"input_tokens":100,"output_tokens":20,"total_tokens":120,"input_tokens_details":{"cached_tokens":60,"cache_write_tokens":30},"output_tokens_details":{"reasoning_tokens":10}}),
                ),
                _ => (
                    json!({"candidates":[{"content":{"role":"model","parts":[{"text":"reply"}]},"finishReason":"STOP"}]}),
                    "/models/fixture:generateContent",
                    "usageMetadata",
                    json!({"promptTokenCount":100,"cachedContentTokenCount":60,"candidatesTokenCount":20,"toolUsePromptTokenCount":5,"thoughtsTokenCount":10,"totalTokenCount":135}),
                ),
            };
            let mut usage = good.clone();
            if mode == "zero" {
                fn zero(value: &mut Value) {
                    match value {
                        Value::Number(_) => *value = json!(0),
                        Value::Object(map) => map.values_mut().for_each(zero),
                        _ => {}
                    }
                }
                zero(&mut usage);
            } else if mode == "inconsistent" {
                let field = match family {
                    EndpointFamilyV1::AnthropicMessages => "output_tokens",
                    EndpointFamilyV1::OpenaiResponses => "input_tokens",
                    _ => "promptTokenCount",
                };
                usage[field] = json!(1);
            } else if mode == "malformed" {
                usage[if family == EndpointFamilyV1::GeminiGenerateContent {
                    "candidatesTokenCount"
                } else {
                    "output_tokens"
                }] = json!("bad");
            } else if mode == "null" {
                usage = Value::Null;
            }
            if mode != "missing" {
                body[usage_key] = usage;
            }
            Mock::given(method("POST"))
                .respond_with(
                    ResponseTemplate::new(200)
                        .insert_header("x-request-id", "fixture-provider-id")
                        .set_body_json(body.clone()),
                )
                .mount(&server)
                .await;
            let runtime = ProviderRuntime::new();
            let handle = match family {
                EndpointFamilyV1::AnthropicMessages => runtime.anthropic(
                    server.uri(),
                    "fixture",
                    sandhi_providers::AnthropicAuthScheme::ApiKey,
                    Default::default(),
                    Some(0),
                    None,
                    None,
                    None,
                ),
                EndpointFamilyV1::OpenaiResponses => runtime.openai_responses(
                    "models",
                    server.uri(),
                    "fixture",
                    Default::default(),
                    Some(0),
                    None,
                    None,
                    None,
                ),
                _ => runtime.gemini(
                    server.uri(),
                    "fixture",
                    sandhi_providers::GeminiAuthScheme::ApiKey,
                    Default::default(),
                    Some(0),
                    None,
                    None,
                    None,
                ),
            };
            assert!(handle.supports_owned_buffered());
            handle.preflight_owned_buffered(&request()).unwrap();
            let id = "a".repeat(64);
            let (context, receiver) = AttemptContext::single_attempt("execution", &id).unwrap();
            let result = handle
                .complete_with_qualified_usage_and_attempts(
                    request(),
                    Default::default(),
                    context.clone(),
                )
                .await
                .unwrap();
            assert_eq!(
                result.usage.is_ok(),
                matches!(mode, "split" | "zero"),
                "{family:?}/{mode}"
            );
            assert!(result.response.extensions.is_empty());
            if let Ok(usage) = result.usage {
                assert_eq!(usage.completeness, UsageCompleteness::Final);
                assert_eq!(
                    usage.reasoning_tokens,
                    Some(if mode == "zero" { 0 } else { 10 })
                );
                assert_eq!(
                    sandhi_core::billable(&usage),
                    if mode == "zero" {
                        0
                    } else if family == EndpointFamilyV1::GeminiGenerateContent {
                        135
                    } else {
                        120
                    }
                );
            }
            assert!(matches!(
                handle
                    .complete_with_qualified_usage_and_attempts(
                        request(),
                        Default::default(),
                        context
                    )
                    .await,
                Err(ProviderError::InvalidRequest(_))
            ));
            let observations = receiver.drain();
            assert_eq!(observations.len(), 2);
            assert!(observations
                .iter()
                .all(|o| o.attempt_id == id && o.attempt_ordinal == 1));
            assert!(matches!(
                observations[1].phase,
                AttemptPhase::Terminal {
                    outcome: AttemptOutcome::Success,
                    provider_status: Some(200),
                    ..
                }
            ));
            let calls = server.received_requests().await.unwrap();
            assert_eq!(calls.len(), 1);
            assert_eq!(calls[0].url.path(), path);
            // A separate raw-plane invocation retains exactly the original bytes.
            let (context, receiver) =
                AttemptContext::single_attempt("raw-execution", "b".repeat(64)).unwrap();
            let raw = handle
                .raw_forwarder()
                .unwrap()
                .clone()
                .with_metered_attempt_context("models", Some("fixture".into()), context)
                .forward_qualified_with_headers(
                    path,
                    bytes::Bytes::from_static(b"{\"model\":\"fixture\"}"),
                    None,
                    None,
                    &Default::default(),
                )
                .await
                .unwrap();
            assert_eq!(
                serde_json::from_slice::<Value>(&raw.response.body).unwrap(),
                body
            );
            assert_eq!(raw.usage.is_ok(), matches!(mode, "split" | "zero"));
            if let Ok(usage) = raw.usage {
                assert_eq!(
                    usage.upstream_request_id.as_deref(),
                    Some("fixture-provider-id")
                );
            }
            assert_eq!(receiver.drain().len(), 2);
            assert_eq!(server.received_requests().await.unwrap().len(), 2);
        }
    }
}

#[tokio::test]
async fn qualified_buffered_preflight_refuses_async_responses_and_unqualified_profiles_without_sending(
) {
    let server = MockServer::start().await;
    let runtime = ProviderRuntime::new();
    let handle = runtime.openai_responses(
        "models",
        server.uri(),
        "fixture",
        Default::default(),
        Some(0),
        None,
        None,
        None,
    );
    let mut asynchronous = request();
    asynchronous
        .extensions
        .insert("openai_responses".into(), json!({"background":true}));
    assert!(handle.preflight_owned_buffered(&asynchronous).is_err());
    assert!(handle
        .complete_with_qualified_usage(asynchronous, Default::default())
        .await
        .is_err());
    let codex = runtime.chatgpt_responses(
        "models",
        server.uri(),
        "fixture",
        Default::default(),
        Some(0),
        None,
        None,
        None,
    );
    assert!(codex.supports_owned_buffered());
    assert!(codex.preflight_owned_buffered(&request()).is_err());
    assert!(codex
        .complete_with_qualified_usage(request(), Default::default())
        .await
        .is_err());
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn typed_and_raw_qualification_refuse_ambiguous_usage_without_replaying() {
    let server = MockServer::start().await;
    let body = r#"{"choices":[{"message":{"content":"reply"}}],"usage":{"prompt_tokens":1,"completion_tokens":1},"u\u0073age":{"prompt_tokens":10,"completion_tokens":2}}"#;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_string(body))
        .mount(&server)
        .await;
    let handle = ProviderRuntime::new().openai_compat(
        "models",
        server.uri(),
        "fixture",
        Default::default(),
        Some(0),
        None,
        None,
        None,
    );
    let (context, receiver) = AttemptContext::single_attempt("execution", "d".repeat(64)).unwrap();
    assert!(matches!(
        handle
            .complete_with_qualified_usage_and_attempts(request(), Default::default(), context)
            .await,
        Err(ProviderError::OutputProjection(_))
    ));
    assert!(matches!(
        receiver.drain()[1].phase,
        AttemptPhase::Terminal {
            outcome: AttemptOutcome::TransportError,
            provider_status: Some(200),
            ..
        }
    ));
    let raw = handle
        .raw_forwarder()
        .unwrap()
        .forward_qualified_with_headers(
            "/chat/completions",
            bytes::Bytes::from_static(b"{}"),
            None,
            None,
            &Default::default(),
        )
        .await
        .unwrap();
    assert_eq!(raw.response.body, body);
    assert!(matches!(
        raw.usage,
        Err(sandhi_core::usage::BufferedUsageError::InvalidResponse)
    ));
    assert_eq!(server.received_requests().await.unwrap().len(), 2);
}
