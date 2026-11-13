//! Typed display reasoning extends the current portable contract without changing native fidelity.
use sandhi_core::{ChatRequestV1, EndpointFamilyV1 as F};
use sandhi_providers::{ProviderOptions, ProviderRuntime};
use serde_json::{json, Value};
use wiremock::{matchers::method, Mock, MockServer, ResponseTemplate};

fn fixture(family: F) -> (&'static str, &'static str, Value) {
    match family {
        F::OpenaiChatCompletions => (
            "chat_completions",
            "openai",
            json!({"id":"chat","model":"fixture","choices":[{"index":0,"message":{"role":"assistant","content":"answer","reasoning_content":"materialized"},"finish_reason":"stop"}],"usage":{"prompt_tokens":10,"completion_tokens":4}}),
        ),
        F::AnthropicMessages => (
            "anthropic_messages",
            "anthropic",
            json!({"id":"message","type":"message","role":"assistant","model":"fixture","content":[{"type":"thinking","thinking":"materialized","signature":""},{"type":"text","text":"answer"}],"stop_reason":"end_turn","usage":{"input_tokens":10,"output_tokens":4,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}),
        ),
        F::OpenaiResponses => (
            "responses",
            "openai_responses",
            json!({"id":"response","status":"completed","model":"fixture","output":[{"type":"reasoning","summary":[{"type":"summary_text","text":"materialized"}]},{"type":"message","role":"assistant","content":[{"type":"output_text","text":"answer"}]}],"usage":{"input_tokens":10,"output_tokens":4}}),
        ),
        _ => (
            "gemini_generate_content",
            "gemini",
            json!({"candidates":[{"content":{"role":"model","parts":[{"text":"materialized","thought":true},{"text":"answer"}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":10,"candidatesTokenCount":4,"thoughtsTokenCount":0,"totalTokenCount":14}}),
        ),
    }
}

#[tokio::test]
async fn buffered_reasoning_uses_existing_fidelity_owner_and_retains_extension_compatibility() {
    for family in [
        F::OpenaiChatCompletions,
        F::AnthropicMessages,
        F::OpenaiResponses,
        F::GeminiGenerateContent,
    ] {
        for native in [false, true] {
            for mode in ["valid", "opaque", "malformed"] {
                let (protocol, envelope, mut body) = fixture(family);
                if mode == "opaque" {
                    match family {
                        F::OpenaiChatCompletions => {
                            body["choices"][0]["message"]["audio"] = json!({"data":"opaque"})
                        }
                        F::AnthropicMessages => body["content"][0]["signature"] = json!("opaque"),
                        F::OpenaiResponses => {
                            body["output"][0]["encrypted_content"] = json!("opaque")
                        }
                        _ => {
                            body["candidates"][0]["content"]["parts"][0]["thoughtSignature"] =
                                json!("opaque")
                        }
                    }
                } else if mode == "malformed" {
                    match family {
                        F::OpenaiChatCompletions => {
                            body["choices"][0]["message"]["reasoning"] = json!("conflicting alias")
                        }
                        F::AnthropicMessages => body["content"][0]["thinking"] = json!(17),
                        F::OpenaiResponses => body["output"][0]["summary"][0]["text"] = json!(17),
                        _ => body["candidates"][0]["content"]["parts"][0]["text"] = json!(17),
                    }
                }
                let server = MockServer::start().await;
                Mock::given(method("POST"))
                    .respond_with(ResponseTemplate::new(200).set_body_json(body.clone()))
                    .expect(1)
                    .mount(&server)
                    .await;
                let handle = ProviderRuntime::new()
                    .configured_provider(ProviderOptions {
                        provider: "fixture".into(),
                        model: "fixture".into(),
                        api_key: "synthetic".into(),
                        base_url: Some(server.uri()),
                        headers: Default::default(),
                        max_retries: Some(0),
                        timeout_secs: None,
                        stream_setup_timeout_secs: None,
                        stream_idle_timeout_secs: None,
                        auth_scheme: None,
                        protocol: Some(protocol.into()),
                        endpoint_profile: Some("standard".into()),
                        peer_role: None,
                    })
                    .unwrap()
                    .handle;
                let request:ChatRequestV1=serde_json::from_value(json!({"model":"fixture","max_output_tokens":64,"include_native_response":native,"messages":[{"role":"user","content":"hello"}]})).unwrap();
                let result = handle
                    .complete_with_qualified_usage(request, Default::default())
                    .await;
                if !native && mode != "valid" {
                    assert!(
                        matches!(
                            result,
                            Err(sandhi_providers::ProviderError::OutputProjection(_))
                        ),
                        "{family:?}/{mode}"
                    );
                } else {
                    let result = result.unwrap();
                    let response = result.response;
                    assert_eq!(
                        response.output.reasoning.as_deref(),
                        if mode == "valid" {
                            Some("materialized")
                        } else {
                            None
                        },
                        "{family:?}/{native}/{mode}"
                    );
                    assert_eq!(response.extensions.get(envelope), native.then_some(&body));
                    if !native {
                        assert_eq!(
                            response.extensions.get("reasoning"),
                            Some(&json!("materialized"))
                        );
                    }
                    let usage = result.usage.unwrap();
                    assert_eq!((usage.tokens_in, usage.tokens_out), (10, 4));
                }
                assert_eq!(server.received_requests().await.unwrap().len(), 1);
            }
        }
    }
}

#[tokio::test]
async fn codex_buffered_terminal_summary_uses_the_same_typed_display_field() {
    for (native, opaque) in [(false, false), (true, false), (false, true), (true, true)] {
        let (_, _, mut body) = fixture(F::OpenaiResponses);
        if opaque {
            body["output"][0]["encrypted_content"] = json!("opaque");
        }
        let server = MockServer::start().await;
        let wire = format!(
            "data: {}\n\ndata: {}\n\n",
            json!({"type":"response.created","response":{"id":"response","model":"fixture","status":"in_progress","output":[]}}),
            json!({"type":"response.completed","response":body})
        );
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(wire, "text/event-stream"))
            .expect(1)
            .mount(&server)
            .await;
        let handle = ProviderRuntime::new().chatgpt_responses(
            "fixture",
            server.uri(),
            "synthetic",
            Default::default(),
            Some(0),
            None,
            None,
            None,
        );
        let request:ChatRequestV1=serde_json::from_value(json!({"model":"fixture","include_native_response":native,"messages":[{"role":"developer","content":"Be precise"},{"role":"user","content":"hello"}]})).unwrap();
        let result = handle
            .complete_with_qualified_usage(request, Default::default())
            .await;
        if opaque && !native {
            assert!(matches!(
                result,
                Err(sandhi_providers::ProviderError::OutputProjection(_))
            ));
            assert_eq!(server.received_requests().await.unwrap().len(), 1);
            continue;
        }
        let response = result.unwrap().response;
        assert_eq!(
            response.output.reasoning.as_deref(),
            if opaque { None } else { Some("materialized") }
        );
        assert_eq!(
            response.output.content,
            Some(sandhi_core::MessageContent::Text("answer".into()))
        );
        if native {
            assert_eq!(response.extensions["openai_responses"], body);
        } else {
            assert_eq!(response.extensions["reasoning"], json!("materialized"));
        }
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }
}
