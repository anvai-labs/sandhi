//! OpenAI Responses API transport.
//!
//! This is deliberately separate from [`crate::OpenAiCompat`]: Responses is an item/event
//! protocol at `/responses`, not the Chat Completions message/chunk protocol.

use crate::{
    error_for_status, metered_passthrough, parse_openai_responses_usage, sse_data_json, ByteStream,
    ParsedUsage, Provider, ProviderError, ProviderRequest, ProviderResponse,
};
use async_trait::async_trait;
use reqwest::header::{HeaderMap, AUTHORIZATION, CONTENT_TYPE, HOST};
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OpenAiResponsesProfile {
    #[default]
    Standard,
    /// ChatGPT subscription backend: requires instructions, item-array input, `store=false`,
    /// and SSE streaming even when the host requested a completed response.
    ChatGptCodex,
}

pub struct OpenAiResponses {
    client: reqwest::Client,
    slug: String,
    base_url: String,
    bearer_token: String,
    headers: HeaderMap,
    profile: OpenAiResponsesProfile,
}

impl OpenAiResponses {
    pub fn new(
        slug: impl Into<String>,
        base_url: impl Into<String>,
        bearer_token: impl Into<String>,
    ) -> Self {
        Self {
            client: crate::default_client(),
            slug: slug.into(),
            base_url: base_url.into(),
            bearer_token: bearer_token.into(),
            headers: HeaderMap::new(),
            profile: OpenAiResponsesProfile::Standard,
        }
    }

    #[must_use]
    pub fn with_headers(mut self, mut headers: HeaderMap) -> Self {
        headers.remove(AUTHORIZATION);
        headers.remove(CONTENT_TYPE);
        headers.remove(HOST);
        self.headers = headers;
        self
    }

    #[must_use]
    pub fn with_profile(mut self, profile: OpenAiResponsesProfile) -> Self {
        self.profile = profile;
        self
    }

    fn responses_url(&self) -> String {
        format!("{}/responses", self.base_url.trim_end_matches('/'))
    }

    fn validate(body: &Value) -> Result<(), ProviderError> {
        let object = body.as_object().ok_or_else(|| {
            ProviderError::InvalidRequest("Responses request must be a JSON object".into())
        })?;
        if !object
            .get("input")
            .is_some_and(|value| value.is_string() || value.is_array())
        {
            return Err(ProviderError::InvalidRequest(
                "Responses request requires string or array input".into(),
            ));
        }
        Ok(())
    }
}

#[async_trait]
impl Provider for OpenAiResponses {
    fn slug(&self) -> &str {
        &self.slug
    }

    async fn complete(&self, req: ProviderRequest) -> Result<ProviderResponse, ProviderError> {
        if self.profile == OpenAiResponsesProfile::ChatGptCodex {
            return Err(ProviderError::InvalidRequest(
                "ChatGPT Codex Responses is streaming-only; use the typed runtime which aggregates its stream"
                    .into(),
            ));
        }
        Self::validate(&req.body)?;
        let mut body = req.body;
        body["stream"] = Value::Bool(false);
        let response = self
            .client
            .post(self.responses_url())
            .bearer_auth(&self.bearer_token)
            .headers(self.headers.clone())
            .json(&body)
            .send()
            .await
            .map_err(|error| ProviderError::Transport(error.to_string()))?;
        let status = response.status().as_u16();
        if !response.status().is_success() {
            return Err(error_for_status(status));
        }
        let body: Value = response
            .json()
            .await
            .map_err(|error| ProviderError::Transport(error.to_string()))?;
        let usage = parse_openai_responses_usage(&body).unwrap_or_default();
        Ok(ProviderResponse {
            status,
            body,
            usage,
            attempts: 1,
        })
    }

    async fn stream(&self, req: ProviderRequest) -> Result<ByteStream, ProviderError> {
        Self::validate(&req.body)?;
        let mut body = req.body;
        body["stream"] = Value::Bool(true);
        let response = self
            .client
            .post(self.responses_url())
            .bearer_auth(&self.bearer_token)
            .headers(self.headers.clone())
            .json(&body)
            .send()
            .await
            .map_err(|error| ProviderError::Transport(error.to_string()))?;
        if !response.status().is_success() {
            return Err(error_for_status(response.status().as_u16()));
        }
        Ok(metered_passthrough(
            response.bytes_stream(),
            sniff_responses_usage_line,
        ))
    }
}

fn sniff_responses_usage_line(line: &[u8], usage: &mut ParsedUsage) {
    let Some(event) = sse_data_json(line) else {
        return;
    };
    let response = event.get("response").unwrap_or(&event);
    if let Some(parsed) = parse_openai_responses_usage(response) {
        *usage = parsed;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::StreamExt;
    use serde_json::json;
    use wiremock::matchers::{body_json, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn complete_uses_responses_path_bearer_and_response_usage_shape() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/responses"))
            .and(header("authorization", "Bearer oauth-token"))
            .and(body_json(
                json!({"model":"gpt-x","input":"hello","stream":false}),
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id":"resp_1", "output":[],
                "usage":{"input_tokens":100,"output_tokens":20,
                    "input_tokens_details":{"cached_tokens":60}}
            })))
            .mount(&server)
            .await;
        let response =
            OpenAiResponses::new("openai", format!("{}/v1", server.uri()), "oauth-token")
                .complete(ProviderRequest::new(
                    "gpt-x",
                    json!({"model":"gpt-x","input":"hello"}),
                ))
                .await
                .unwrap();
        assert_eq!(response.usage.tokens_in, 40);
        assert_eq!(response.usage.cache_read_tokens, 60);
        assert_eq!(response.usage.tokens_out, 20);
    }

    #[tokio::test]
    async fn stream_extracts_terminal_nested_usage() {
        let server = MockServer::start().await;
        let sse = concat!(
            "event: response.output_text.delta\n",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"hi\"}\n\n",
            "event: response.completed\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":12,\"output_tokens\":3,\"input_tokens_details\":{\"cached_tokens\":2}}}}\n\n"
        );
        Mock::given(method("POST"))
            .and(path("/responses"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(sse, "text/event-stream"))
            .mount(&server)
            .await;
        let mut stream = OpenAiResponses::new("openai", server.uri(), "token")
            .stream(ProviderRequest::new(
                "gpt-x",
                json!({"model":"gpt-x","input":[]}),
            ))
            .await
            .unwrap();
        let mut final_usage = None;
        while let Some(chunk) = stream.next().await {
            final_usage = chunk.unwrap().usage.or(final_usage);
        }
        assert_eq!(final_usage.unwrap().tokens_in, 10);
    }
}
