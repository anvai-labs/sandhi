//! Cohere v2 chat adapter. POSTs to `{base_url}/v2/chat` with `Authorization: Bearer`; usage is
//! in `usage.billed_units` (no prompt-cache split).

use crate::embed::{EmbedRequest, EmbedResponse, EmbedUsage, EmbeddingProvider};
use crate::{
    error_for_status, metered_passthrough, sse_data_json, ByteStream, Provider, ProviderError,
    ProviderRequest, ProviderResponse,
};
use async_trait::async_trait;
use sandhi_core::parse_cohere_usage;
use serde_json::{json, Value};

/// The Cohere provider.
pub struct Cohere {
    client: reqwest::Client,
    base_url: String,
    api_key: String,
}

impl Cohere {
    pub fn new(base_url: impl Into<String>, api_key: impl Into<String>) -> Self {
        Self {
            client: reqwest::Client::new(),
            base_url: base_url.into(),
            api_key: api_key.into(),
        }
    }

    /// The hosted Cohere API (`https://api.cohere.com`).
    pub fn hosted(api_key: impl Into<String>) -> Self {
        Self::new("https://api.cohere.com", api_key)
    }

    fn chat_url(&self) -> String {
        format!("{}/v2/chat", self.base_url.trim_end_matches('/'))
    }

    fn embed_url(&self) -> String {
        format!("{}/v2/embed", self.base_url.trim_end_matches('/'))
    }
}

#[async_trait]
impl EmbeddingProvider for Cohere {
    fn slug(&self) -> &str {
        "cohere"
    }

    async fn embed(&self, req: EmbedRequest) -> Result<EmbedResponse, ProviderError> {
        // Cohere v2 requires input_type + embedding_types; we request float vectors.
        let body = json!({
            "model": req.model,
            "texts": req.input,
            "input_type": req.input_type.as_deref().unwrap_or("search_document"),
            "embedding_types": ["float"],
        });
        let resp = self
            .client
            .post(self.embed_url())
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .await
            .map_err(|e| ProviderError::Transport(e.to_string()))?;
        let status = resp.status().as_u16();
        if !resp.status().is_success() {
            return Err(error_for_status(status));
        }
        let body: Value = resp
            .json()
            .await
            .map_err(|e| ProviderError::Transport(e.to_string()))?;
        // v2 shape: { "embeddings": { "float": [[...]] }, "meta": { "billed_units": { "input_tokens": N } } }
        let embeddings = body
            .pointer("/embeddings/float")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(Value::as_array)
                    .map(|nums| {
                        nums.iter()
                            .filter_map(|n| n.as_f64().map(|f| f as f32))
                            .collect()
                    })
                    .collect()
            })
            .unwrap_or_default();
        let input_tokens = body
            .pointer("/meta/billed_units/input_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let usage = (input_tokens > 0).then_some(EmbedUsage {
            input_tokens,
            total_tokens: input_tokens,
        });
        Ok(EmbedResponse {
            status,
            embeddings,
            usage,
        })
    }
}

#[async_trait]
impl Provider for Cohere {
    fn slug(&self) -> &str {
        "cohere"
    }

    async fn complete(&self, req: ProviderRequest) -> Result<ProviderResponse, ProviderError> {
        let mut body = req.body;
        if let Some(obj) = body.as_object_mut() {
            obj.insert("stream".into(), Value::Bool(false));
        }
        let resp = self
            .client
            .post(self.chat_url())
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .await
            .map_err(|e| ProviderError::Transport(e.to_string()))?;
        let status = resp.status().as_u16();
        if !resp.status().is_success() {
            return Err(error_for_status(status));
        }
        let body: Value = resp
            .json()
            .await
            .map_err(|e| ProviderError::Transport(e.to_string()))?;
        let usage = parse_cohere_usage(&body).unwrap_or_default();
        Ok(ProviderResponse {
            status,
            body,
            usage,
        })
    }

    async fn stream(&self, req: ProviderRequest) -> Result<ByteStream, ProviderError> {
        let mut body = req.body;
        if let Some(obj) = body.as_object_mut() {
            obj.insert("stream".into(), Value::Bool(true));
        }
        let resp = self
            .client
            .post(self.chat_url())
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .await
            .map_err(|e| ProviderError::Transport(e.to_string()))?;
        if !resp.status().is_success() {
            return Err(error_for_status(resp.status().as_u16()));
        }
        Ok(metered_passthrough(resp.bytes_stream(), |line, usage| {
            if let Some(v) = sse_data_json(line) {
                // Cohere carries usage on the `message-end` event under `delta.usage`.
                let obj = v
                    .get("usage")
                    .or_else(|| v.get("delta").and_then(|d| d.get("usage")));
                if let Some(uo) = obj {
                    if let Some(u) = parse_cohere_usage(&json!({ "usage": uo })) {
                        *usage = u;
                    }
                }
            }
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn complete_parses_billed_units() {
        let server = MockServer::start().await;
        let body = json!({
            "message": { "content": [{ "text": "hi" }] },
            "usage": { "billed_units": { "input_tokens": 42, "output_tokens": 9 } }
        });
        Mock::given(method("POST"))
            .and(path("/v2/chat"))
            .and(header("authorization", "Bearer co-test"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(&server)
            .await;

        let p = Cohere::new(server.uri(), "co-test");
        let out = p
            .complete(ProviderRequest::new("command-r", json!({ "messages": [] })))
            .await
            .unwrap();
        assert_eq!(out.usage.tokens_in, 42);
        assert_eq!(out.usage.tokens_out, 9);
    }

    #[tokio::test]
    async fn embed_parses_float_vectors_and_billed_input_tokens() {
        let server = MockServer::start().await;
        let body = json!({
            "id": "abc",
            "embeddings": { "float": [[1.0, 2.0], [3.0, 4.0]] },
            "texts": ["a", "b"],
            "meta": { "billed_units": { "input_tokens": 17 } }
        });
        Mock::given(method("POST"))
            .and(path("/v2/embed"))
            .and(header("authorization", "Bearer co-test"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(&server)
            .await;

        let p = Cohere::new(server.uri(), "co-test");
        let out = p
            .embed(EmbedRequest::new(
                "embed-english-v3.0",
                vec!["a".into(), "b".into()],
            ))
            .await
            .unwrap();

        assert_eq!(out.status, 200);
        assert_eq!(out.embeddings, vec![vec![1.0, 2.0], vec![3.0, 4.0]]);
        assert_eq!(out.usage.unwrap().input_tokens, 17);
    }
}
