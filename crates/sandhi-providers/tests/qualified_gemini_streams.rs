//! One Gemini physical source owner for raw and typed SSE, both credential forms.
use futures_util::StreamExt;
use sandhi_core::{ChatStreamEventV1 as Event, UsageCompleteness};
use sandhi_providers::{AttemptContext, AttemptOutcome, GeminiAuthScheme, ProviderRuntime};
use serde_json::json;
use wiremock::{matchers::method, Mock, MockServer, ResponseTemplate};

fn wire(mode: &str) -> String {
    let mut first = json!({"responseId":"gemini-fixture","modelVersion":"fixture",
        "candidates":[{"index":0,"content":{"role":"model","parts":[{"text":"reply"}]}}],
        "usageMetadata":{"promptTokenCount":7,"cachedContentTokenCount":6,"candidatesTokenCount":1}});
    let mut last = json!({"candidates":[{"index":0,"finishReason":"STOP"}],
        "usageMetadata":{"promptTokenCount":7,"cachedContentTokenCount":6,"candidatesTokenCount":4,
            "thoughtsTokenCount":2,"toolUsePromptTokenCount":1,"totalTokenCount":14}});
    match mode {
        "signature" => {
            first["candidates"][0]["content"]["parts"][0]["thoughtSignature"] =
                "opaque-state".into()
        }
        "media" => {
            first["candidates"][0]["content"]["parts"] =
                json!([{"inlineData":{"mimeType":"image/png","data":"opaque-state"}}])
        }
        "missing_input" => {
            last["usageMetadata"]
                .as_object_mut()
                .unwrap()
                .remove("promptTokenCount");
        }
        "missing_output" => {
            last["usageMetadata"]
                .as_object_mut()
                .unwrap()
                .remove("candidatesTokenCount");
        }
        "missing_total" => {
            last["usageMetadata"]
                .as_object_mut()
                .unwrap()
                .remove("totalTokenCount");
        }
        "malformed" => last["usageMetadata"]["candidatesTokenCount"] = "4".into(),
        "missing_terminal" => {
            last["candidates"][0]
                .as_object_mut()
                .unwrap()
                .remove("finishReason");
        }
        "regressed" => {
            last["usageMetadata"]["candidatesTokenCount"] = 0.into();
            last["usageMetadata"]["totalTokenCount"] = 10.into();
        }
        "provider_error" => last = json!({"error":{"code":503,"message":"synthetic"}}),
        _ => {}
    }
    let mut wire = format!("data: {first}\n\ndata: {last}\n\n");
    if mode == "post_terminal" {
        wire += "data: {\"candidates\":[{\"index\":0}]}\n\n";
    }
    if mode == "truncated" {
        wire += "data: {";
    }
    wire
}

#[tokio::test]
async fn gemini_profiles_share_one_physical_attempt_and_preserve_actual_source_counts() {
    for auth in [GeminiAuthScheme::ApiKey, GeminiAuthScheme::Bearer] {
        for native in [false, true] {
            for mode in [
                "final",
                "signature",
                "media",
                "missing_input",
                "missing_output",
                "missing_total",
                "malformed",
                "missing_terminal",
                "post_terminal",
                "truncated",
                "provider_error",
                "regressed",
            ] {
                let server = MockServer::start().await;
                let response = wire(mode);
                Mock::given(method("POST"))
                    .respond_with(
                        ResponseTemplate::new(200)
                            .insert_header("x-request-id", "origin-gemini")
                            .set_body_raw(response.clone(), "text/event-stream"),
                    )
                    .expect(2)
                    .mount(&server)
                    .await;
                let provider = ProviderRuntime::new().gemini(
                    server.uri(),
                    "synthetic",
                    auth,
                    Default::default(),
                    Some(0),
                    None,
                    None,
                    None,
                );
                assert!(provider.supports_owned_streaming());
                let request =
                    serde_json::from_value(json!({"model":"fixture","max_output_tokens":64,
                    "include_native_stream":native,"messages":[{"role":"user","content":"hello"}]}))
                    .unwrap();
                let (context, receiver) =
                    AttemptContext::single_attempt("typed-gemini", "f".repeat(64)).unwrap();
                let events = provider
                    .stream_with_qualified_usage_and_attempts(
                        request,
                        Default::default(),
                        context.clone(),
                    )
                    .await
                    .unwrap()
                    .collect::<Vec<_>>()
                    .await;
                let source_final = matches!(mode, "final" | "signature" | "media");
                let clean = source_final && (native || mode == "final");
                assert_eq!(
                    events.iter().any(Result::is_err),
                    !clean,
                    "{auth:?}/{native}/{mode}"
                );
                assert_eq!(
                    events
                        .iter()
                        .any(|e| matches!(e, Ok(Event::NativeFrame { .. }))),
                    native
                );
                let finals = events
                    .iter()
                    .filter_map(|e| match e {
                        Ok(Event::Usage { usage })
                            if usage.completeness == UsageCompleteness::Final =>
                        {
                            Some(usage)
                        }
                        _ => None,
                    })
                    .collect::<Vec<_>>();
                assert_eq!(finals.len(), usize::from(clean));
                assert_eq!(
                    events.iter().any(|e| matches!(e, Ok(Event::Finish { .. }))),
                    clean
                );
                let evidence = context.qualified_stream_evidence().unwrap();
                let usage = evidence.usage.unwrap();
                assert_eq!(
                    usage.completeness,
                    if source_final {
                        UsageCompleteness::Final
                    } else {
                        UsageCompleteness::Partial
                    }
                );
                assert_eq!(usage.cache_read_tokens, 6);
                if source_final {
                    assert_eq!(
                        (usage.tokens_in, usage.tokens_out, usage.reasoning_tokens),
                        (2, 4, Some(2))
                    );
                    assert_eq!(usage.reasoning_included, Some(false));
                }
                assert_eq!(usage.upstream_request_id.as_deref(), Some("origin-gemini"));
                assert_eq!(
                    evidence.snapshot.outcome,
                    Some(if source_final {
                        AttemptOutcome::Success
                    } else if mode == "provider_error" {
                        AttemptOutcome::ProviderRejected
                    } else {
                        AttemptOutcome::IncompleteStream
                    })
                );
                assert_eq!(receiver.drain().len(), 2);
                assert_eq!(context.dispatch_count(), 1);

                let (context, receiver) =
                    AttemptContext::single_attempt("raw-gemini", "f".repeat(64)).unwrap();
                let raw = provider
                    .raw_forwarder()
                    .unwrap()
                    .clone()
                    .with_metered_attempt_context("gemini", None, context.clone());
                let source = raw.forward_stream_qualified_with_headers("/v1beta/models/fixture:streamGenerateContent?alt=sse",
                    bytes::Bytes::from_static(b"{\"contents\":[{\"role\":\"user\",\"parts\":[{\"text\":\"hello\"}]}]}"),
                    None,None,&Default::default()).await.unwrap();
                let observation = source.observation;
                let bytes = source
                    .stream
                    .collect::<Vec<_>>()
                    .await
                    .into_iter()
                    .flat_map(Result::unwrap)
                    .collect::<Vec<_>>();
                assert_eq!(bytes, response.as_bytes());
                assert_eq!(
                    observation.evidence().usage.unwrap().completeness,
                    if source_final {
                        UsageCompleteness::Final
                    } else {
                        UsageCompleteness::Partial
                    }
                );
                assert_eq!(context.dispatch_count(), 1);
                assert_eq!(receiver.drain().len(), 2);
                let calls = server.received_requests().await.unwrap();
                assert_eq!(calls.len(), 2);
                for call in calls {
                    assert!(call
                        .url
                        .path()
                        .ends_with("/models/fixture:streamGenerateContent"));
                    assert_eq!(call.url.query(), Some("alt=sse"));
                    assert_eq!(
                        call.headers.contains_key("x-goog-api-key"),
                        auth == GeminiAuthScheme::ApiKey
                    );
                    assert_eq!(
                        call.headers.contains_key("authorization"),
                        auth == GeminiAuthScheme::Bearer
                    );
                }
            }
        }
    }
}

#[tokio::test]
async fn bare_gemini_qualified_setup_refuses_unsafe_models_and_nonobjects_before_dispatch() {
    use sandhi_providers::{
        Gemini, Provider, ProviderError, ProviderRequest, QualifiedStreamDelivery,
    };
    let server = MockServer::start().await;
    let provider = Gemini::new(server.uri(), "synthetic");
    for (model, body) in [
        ("fixture?alt=json", json!({})),
        ("fixture#fragment", json!({})),
        ("../fixture", json!({})),
        ("fixture", json!([])),
    ] {
        let (context, receiver) =
            AttemptContext::single_attempt("invalid-gemini", "a".repeat(64)).unwrap();
        let mut request = ProviderRequest::new(model, body);
        request.attempt_context = Some(context.clone());
        let result = provider
            .stream_qualified(request, QualifiedStreamDelivery::Streaming)
            .await;
        assert!(matches!(result, Err(ProviderError::InvalidRequest(_))));
        assert_eq!(context.dispatch_count(), 0);
        assert!(receiver.drain().is_empty());
    }
    assert!(server.received_requests().await.unwrap().is_empty());
}
