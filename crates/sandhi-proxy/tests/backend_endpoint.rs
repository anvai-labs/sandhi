//! Explicit endpoint binding must not depend on provider names or weaken Codex leases.
use sandhi_core::{ChatRequestV1, EndpointFamilyV1, MessageContent};
use sandhi_providers::{ProviderError, ProviderFamily, ProviderRuntime};
use sandhi_proxy::{
    backend::{BackendEndpoint, BackendProfile},
    build_provider_handle, build_provider_handle_for_endpoint,
};
use sandhi_store::CredentialScheme;
use serde_json::json;
use wiremock::{
    matchers::{header, method, path},
    Mock, MockServer, ResponseTemplate,
};

fn lease(expires_at: u64) -> String {
    json!({"access_token":"synthetic-access","account_id":"synthetic-account", "expires_at":expires_at}).to_string()
}

fn request() -> ChatRequestV1 {
    serde_json::from_value(json!({"model":"fixture","max_output_tokens":64,
        "messages":[{"role":"system","content":"Be precise"},{"role":"user","content":"hello"}]}))
    .unwrap()
}

#[test]
fn explicit_family_is_independent_of_provider_name() {
    for (provider, family, expected) in [
        (
            "openai",
            EndpointFamilyV1::OpenaiResponses,
            ProviderFamily::OpenAiResponses,
        ),
        (
            "custom-anthropic",
            EndpointFamilyV1::AnthropicMessages,
            ProviderFamily::Anthropic,
        ),
        (
            "local-model",
            EndpointFamilyV1::OpenaiChatCompletions,
            ProviderFamily::OpenAiCompat,
        ),
        (
            "anthropic",
            EndpointFamilyV1::OpenaiResponses,
            ProviderFamily::OpenAiResponses,
        ),
    ] {
        let handle = build_provider_handle_for_endpoint(
            &ProviderRuntime::new(),
            provider,
            None,
            "synthetic-key",
            CredentialScheme::ApiKey,
            None,
            BackendEndpoint::standard(family),
        )
        .unwrap();
        assert_eq!(handle.family(), expected);
        assert!(handle.raw_forwarder().is_some());
    }
}

#[test]
fn legacy_defaults_keep_supported_families_and_reject_unimplemented_oauth() {
    for (provider, expected) in [
        ("openai", ProviderFamily::OpenAiCompat),
        ("anthropic", ProviderFamily::Anthropic),
        ("gemini", ProviderFamily::Gemini),
        ("local-model", ProviderFamily::OpenAiCompat),
    ] {
        let handle = build_provider_handle(
            &ProviderRuntime::new(),
            provider,
            None,
            "synthetic-key",
            CredentialScheme::ApiKey,
            None,
        )
        .unwrap();
        assert_eq!(handle.family(), expected);
    }
    for provider in ["anthropic", "gemini", "local-model"] {
        assert!(build_provider_handle(
            &ProviderRuntime::new(),
            provider,
            None,
            "synthetic-unsupported-oauth",
            CredentialScheme::Oauth,
            None
        )
        .is_none());
    }
}

#[test]
fn unsupported_family_and_profile_auth_combinations_are_rejected() {
    for (family, profile, scheme) in [
        (
            EndpointFamilyV1::OpenaiResponses,
            BackendProfile::OpenaiTerminalChunkUsage,
            CredentialScheme::ApiKey,
        ),
        (
            EndpointFamilyV1::AnthropicMessages,
            BackendProfile::OpenaiTerminalChunkUsage,
            CredentialScheme::ApiKey,
        ),
        (
            EndpointFamilyV1::OpenaiChatCompletions,
            BackendProfile::OpenaiTerminalChunkUsage,
            CredentialScheme::Oauth,
        ),
        (
            EndpointFamilyV1::OpenaiChatCompletions,
            BackendProfile::AnthropicImplicitCache,
            CredentialScheme::ApiKey,
        ),
        (
            EndpointFamilyV1::AnthropicMessages,
            BackendProfile::AnthropicImplicitCache,
            CredentialScheme::Oauth,
        ),
        (
            EndpointFamilyV1::BedrockConverse,
            BackendProfile::Standard,
            CredentialScheme::ApiKey,
        ),
        (
            EndpointFamilyV1::HostCallback,
            BackendProfile::Standard,
            CredentialScheme::ApiKey,
        ),
        (
            EndpointFamilyV1::OpenaiChatCompletions,
            BackendProfile::ChatGptCodex,
            CredentialScheme::Oauth,
        ),
        (
            EndpointFamilyV1::AnthropicMessages,
            BackendProfile::ChatGptCodex,
            CredentialScheme::Oauth,
        ),
        (
            EndpointFamilyV1::OpenaiResponses,
            BackendProfile::ChatGptCodex,
            CredentialScheme::ApiKey,
        ),
        (
            EndpointFamilyV1::OpenaiResponses,
            BackendProfile::Standard,
            CredentialScheme::Oauth,
        ),
        (
            EndpointFamilyV1::AnthropicMessages,
            BackendProfile::Standard,
            CredentialScheme::Oauth,
        ),
    ] {
        assert!(
            build_provider_handle_for_endpoint(
                &ProviderRuntime::new(),
                "openai",
                None,
                &lease(4_000_000_000),
                scheme,
                None,
                BackendEndpoint {
                    endpoint_family: family,
                    endpoint_profile: profile
                }
            )
            .is_none(),
            "{family:?}/{profile:?}/{scheme:?}"
        );
    }
}

#[tokio::test]
async fn explicit_codex_binding_retains_origin_account_and_expiry_constraints() {
    let endpoint = BackendEndpoint {
        endpoint_family: EndpointFamilyV1::OpenaiResponses,
        endpoint_profile: BackendProfile::ChatGptCodex,
    };
    let make = |base, secret: &str| {
        build_provider_handle_for_endpoint(
            &ProviderRuntime::new(),
            "custom-subscription",
            base,
            secret,
            CredentialScheme::Oauth,
            None,
            endpoint,
        )
    };
    let handle = make(None, &lease(4_000_000_000)).unwrap();
    assert_eq!(handle.family(), ProviderFamily::OpenAiResponses);
    assert!(handle.raw_forwarder().is_none());
    assert!(handle.supports_streaming_deadlines());
    assert!(make(Some("https://other.invalid"), &lease(4_000_000_000)).is_none());
    assert!(make(None, &lease(1)).is_none());
    assert!(make(
        None,
        &json!({"access_token":"access","account_id":"account",
        "expires_at":4_000_000_000_u64,"refresh_token":"forbidden"})
        .to_string()
    )
    .is_none());
    let expired = handle.with_credential_expiry(1);
    assert!(matches!(
        expired.complete(request()).await,
        Err(ProviderError::Auth)
    ));
    assert!(matches!(
        expired.stream(request()).await,
        Err(ProviderError::Auth)
    ));
    // Legacy selection still chooses the constrained profile for OpenAI OAuth.
    let legacy = build_provider_handle(
        &ProviderRuntime::new(),
        "openai",
        None,
        &lease(4_000_000_000),
        CredentialScheme::Oauth,
        None,
    )
    .unwrap();
    assert_eq!(legacy.family(), ProviderFamily::OpenAiResponses);
    assert!(legacy.raw_forwarder().is_none());
}

#[tokio::test]
async fn explicit_binding_dispatches_to_its_native_protocol_and_auth_scheme() {
    for family in [
        EndpointFamilyV1::OpenaiResponses,
        EndpointFamilyV1::AnthropicMessages,
        EndpointFamilyV1::GeminiGenerateContent,
    ] {
        let upstream = MockServer::start().await;
        let anthropic = family == EndpointFamilyV1::AnthropicMessages;
        let gemini = family == EndpointFamilyV1::GeminiGenerateContent;
        let response = if anthropic {
            json!({"id":"msg_fixture","model":"fixture","content":[{"type":"text","text":"reply"}],
                "stop_reason":"end_turn","usage":{"input_tokens":3,"output_tokens":2,"cache_creation_input_tokens":0,"cache_read_input_tokens":0}})
        } else if gemini {
            json!({"candidates":[{"content":{"parts":[{"text":"reply"}]},"finishReason":"STOP"}],
                "usageMetadata":{"promptTokenCount":3,"candidatesTokenCount":2}})
        } else {
            json!({"id":"resp_fixture","model":"fixture","status":"completed",
                "output":[{"type":"message","content":[{"type":"output_text","text":"reply"}]}],
                "usage":{"input_tokens":3,"output_tokens":2}})
        };
        Mock::given(method("POST"))
            .and(path(if anthropic {
                "/v1/messages"
            } else if gemini {
                "/models/fixture:generateContent"
            } else {
                "/responses"
            }))
            .and(header("authorization", "Bearer synthetic-bearer"))
            .and(header("user-agent", "sandhi-endpoint-test"))
            .respond_with(ResponseTemplate::new(200).set_body_json(response))
            .expect(1)
            .mount(&upstream)
            .await;
        let handle = build_provider_handle_for_endpoint(
            &ProviderRuntime::new(),
            "custom-backend",
            Some(&upstream.uri()),
            "synthetic-bearer",
            CredentialScheme::Bearer,
            Some("sandhi-endpoint-test"),
            BackendEndpoint::standard(family),
        )
        .unwrap();
        let reply = handle.complete(request()).await.unwrap();
        assert_eq!(
            reply.output.content,
            Some(MessageContent::Text("reply".into()))
        );
        let requests = upstream.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1);
        assert!(!requests[0].headers.contains_key("x-api-key"));
        assert!(!requests[0].headers.contains_key("x-goog-api-key"));
        let body: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
        if !gemini {
            // Gemini carries the model in the already-asserted native URL path.
            assert_eq!(body["model"], "fixture");
        }
        assert!(body
            .get(if anthropic {
                "messages"
            } else if gemini {
                "contents"
            } else {
                "input"
            })
            .is_some());
    }
}

#[test]
fn endpoint_configuration_is_strict_and_defaults_only_the_profile() {
    let endpoint: BackendEndpoint =
        serde_json::from_value(json!({"endpoint_family":"openai_responses"})).unwrap();
    assert_eq!(endpoint.endpoint_profile, BackendProfile::Standard);
    assert!(
        serde_json::from_value::<BackendEndpoint>(json!({"endpoint_profile":"standard"})).is_err()
    );
    assert!(serde_json::from_value::<BackendEndpoint>(
        json!({"endpoint_family":"openai_responses",
        "endpoint_profile":"unverified_subscription"})
    )
    .is_err());
    assert!(serde_json::from_value::<BackendEndpoint>(
        json!({"endpoint_family":"openai_responses",
        "endpoint_profiel":"chatgpt_codex"})
    )
    .is_err());
}
