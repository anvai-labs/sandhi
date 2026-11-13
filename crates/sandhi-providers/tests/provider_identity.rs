use futures_util::StreamExt;
use sandhi_core::{ChatRequestV1, EndpointFamilyV1};
use sandhi_providers::{AttemptContext, ProviderOptions, ProviderRuntime};
use serde_json::json;
use wiremock::{matchers::method, Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn configured_native_identity_reaches_dispatch_and_keeps_old_namespace() {
    for (protocol, scope, body) in [
        (
            "anthropic_messages",
            "upstream:anthropic",
            json!({"id":"r","type":"message","model":"fixture","role":"assistant","content":[{"type":"text","text":"reply"}],"stop_reason":"end_turn","usage":{"input_tokens":2,"output_tokens":3,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}),
        ),
        (
            "gemini_generate_content",
            "upstream:gemini",
            json!({"candidates":[{"index":0,"content":{"role":"model","parts":[{"text":"reply"}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":2,"cachedContentTokenCount":0,"candidatesTokenCount":3,"totalTokenCount":5}}),
        ),
    ] {
        let origin = MockServer::start().await;
        let reply = body.clone();
        Mock::given(method("POST")).respond_with(move |request:&wiremock::Request| {
            let request:serde_json::Value=serde_json::from_slice(&request.body).unwrap();
            if protocol=="anthropic_messages" && request["stream"]==true {
                let mut start=reply.clone();start["content"]=json!([]);start["stop_reason"]=serde_json::Value::Null;start["usage"]["output_tokens"]=json!(0);
                let events=[json!({"type":"message_start","message":start}),json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":"reply"}}),json!({"type":"content_block_stop","index":0}),json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":3}}),json!({"type":"message_stop"})];
                ResponseTemplate::new(200).set_body_raw(events.into_iter().map(|e|format!("data: {e}\n\n")).collect::<String>(),"text/event-stream")
            } else {ResponseTemplate::new(200).set_body_json(reply.clone())}
        }).mount(&origin).await;
        let handle = ProviderRuntime::new()
            .configured_provider(ProviderOptions {
                provider: "configured-vendor".into(),
                model: "fixture".into(),
                api_key: "synthetic".into(),
                base_url: Some(origin.uri()),
                headers: Default::default(),
                max_retries: Some(0),
                timeout_secs: None,
                stream_setup_timeout_secs: None,
                stream_idle_timeout_secs: None,
                auth_scheme: None,
                protocol: Some(protocol.into()),
                endpoint_profile: None,
                peer_role: None,
            })
            .unwrap()
            .handle;
        assert_eq!(handle.slug(), "configured-vendor");
        assert_eq!(handle.upstream_budget_scope(), scope);
        let changed = handle
            .clone()
            .with_provider_identity("renamed-vendor")
            .unwrap()
            .with_family(sandhi_providers::ProviderFamily::OpenAiCompat);
        assert_eq!(changed.upstream_budget_scope(), scope);
        for native in [false, true] {
            let request:ChatRequestV1=serde_json::from_value(json!({"model":"fixture","max_output_tokens":16,"messages":[{"role":"user","content":"hello"}],"include_native_response":native})).unwrap();
            let (context, rx) = AttemptContext::channel("synthetic-identity", 8).unwrap();
            handle
                .complete_with_attempts(request, Default::default(), context)
                .await
                .unwrap();
            let events = rx.drain();
            assert_eq!(events.len(), 2);
            assert!(events.iter().all(
                |e| e.provider == "configured-vendor" && e.model.as_deref() == Some("fixture")
            ));
        }
        if protocol == "anthropic_messages" {
            let request=serde_json::from_value(json!({"model":"fixture","max_output_tokens":16,"messages":[{"role":"user","content":"hello"}]})).unwrap();
            let (context, rx) = AttemptContext::channel("synthetic-stream-identity", 8).unwrap();
            let events = handle
                .stream_with_attempts(request, Default::default(), context)
                .await
                .unwrap()
                .collect::<Vec<_>>()
                .await;
            assert!(events.iter().all(Result::is_ok));
            assert_eq!(
                rx.drain()
                    .iter()
                    .filter(|event| event.provider == "configured-vendor")
                    .count(),
                2
            );
        }
        assert_eq!(
            handle.family().endpoint_family(),
            if protocol == "anthropic_messages" {
                EndpointFamilyV1::AnthropicMessages
            } else {
                EndpointFamilyV1::GeminiGenerateContent
            }
        );
    }
}
