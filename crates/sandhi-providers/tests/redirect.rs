//! Provider prompts stay at the configured endpoint even when it returns a redirect.

use sandhi_providers::{OpenAiCompat, Provider, ProviderRequest};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn openai_compatible_complete_refuses_cross_origin_redirect() {
    let source = MockServer::start().await;
    let receiver = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(307)
                .insert_header("Location", format!("{}/sink", receiver.uri())),
        )
        .mount(&source)
        .await;
    Mock::given(method("POST"))
        .and(path("/sink"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&receiver)
        .await;

    let provider = OpenAiCompat::new("inferflux", source.uri(), "dummy-key");
    let request = ProviderRequest::new(
        "dummy-model",
        serde_json::json!({"model": "dummy-model", "messages": [{"role": "user", "content": "private prompt"}]}),
    );
    assert!(provider.complete(request).await.is_err());
    assert!(receiver.received_requests().await.unwrap().is_empty());
}
