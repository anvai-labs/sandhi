//! OpenAI-compatible adapter — OpenAI proper plus the ~20 providers that speak the Chat
//! Completions wire format (Groq, Together, Fireworks, DeepSeek, Mistral, Qwen, xAI,
//! OpenRouter, vLLM, LM Studio, Ollama, Cerebras…). One adapter, many providers.

use crate::embed::{
    parse_openai_embeddings, EmbedRequest, EmbedResponse, EmbedUsage, EmbeddingProvider,
};
use crate::parse_openai_usage;
use crate::{
    error_for_status, ByteStream, ParsedUsage, Provider, ProviderError, ProviderRequest,
    ProviderResponse, StreamChunk,
};
use async_trait::async_trait;
use bytes::Bytes;
use futures_util::StreamExt;
use serde_json::{json, Value};

/// An OpenAI-compatible provider. `base_url` is the API base (e.g. `https://api.openai.com/v1`);
/// the adapter POSTs to `{base_url}/chat/completions` with `Authorization: Bearer <key>`.
pub struct OpenAiCompat {
    client: reqwest::Client,
    slug: String,
    base_url: String,
    api_key: String,
}

impl OpenAiCompat {
    pub fn new(
        slug: impl Into<String>,
        base_url: impl Into<String>,
        api_key: impl Into<String>,
    ) -> Self {
        Self {
            client: reqwest::Client::new(),
            slug: slug.into(),
            base_url: base_url.into(),
            api_key: api_key.into(),
        }
    }

    /// OpenAI proper (`https://api.openai.com/v1`), slug `openai`.
    pub fn openai(api_key: impl Into<String>) -> Self {
        Self::new("openai", "https://api.openai.com/v1", api_key)
    }

    fn chat_url(&self) -> String {
        format!("{}/chat/completions", self.base_url.trim_end_matches('/'))
    }

    fn embeddings_url(&self) -> String {
        format!("{}/embeddings", self.base_url.trim_end_matches('/'))
    }
}

#[async_trait]
impl EmbeddingProvider for OpenAiCompat {
    fn slug(&self) -> &str {
        &self.slug
    }

    async fn embed(&self, req: EmbedRequest) -> Result<EmbedResponse, ProviderError> {
        let body = json!({ "model": req.model, "input": req.input });
        let resp = self
            .client
            .post(self.embeddings_url())
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
        let embeddings = parse_openai_embeddings(&body);
        let usage = body.get("usage").map(|u| EmbedUsage {
            input_tokens: u.get("prompt_tokens").and_then(Value::as_u64).unwrap_or(0),
            total_tokens: u.get("total_tokens").and_then(Value::as_u64).unwrap_or(0),
        });
        Ok(EmbedResponse {
            status,
            embeddings,
            usage,
        })
    }
}

#[async_trait]
impl Provider for OpenAiCompat {
    fn slug(&self) -> &str {
        &self.slug
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
        let usage = parse_openai_usage(&body).unwrap_or_default();
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
            // Ask for usage in the terminal SSE chunk.
            obj.insert("stream_options".into(), json!({ "include_usage": true }));
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
        let mut upstream = resp.bytes_stream();
        let s = async_stream::try_stream! {
            let mut line_buf: Vec<u8> = Vec::new();
            let mut final_usage: Option<ParsedUsage> = None;
            while let Some(chunk) = upstream.next().await {
                let chunk = chunk.map_err(|e| ProviderError::Transport(e.to_string()))?;
                line_buf.extend_from_slice(&chunk);
                while let Some(pos) = line_buf.iter().position(|&b| b == b'\n') {
                    let line: Vec<u8> = line_buf.drain(..=pos).collect();
                    if let Some(u) = sniff_usage_line(&line) {
                        final_usage = Some(u);
                    }
                }
                // Forward raw upstream bytes verbatim (O(1) pass-through).
                yield StreamChunk { data: chunk, usage: None };
            }
            // Terminal item carries the finalized usage.
            yield StreamChunk { data: Bytes::new(), usage: Some(final_usage.unwrap_or_default()) };
        };
        Ok(Box::pin(s))
    }
}

/// Extract usage from a single `data: {json}` SSE line, if it carries a `usage` object.
fn sniff_usage_line(line: &[u8]) -> Option<ParsedUsage> {
    let s = std::str::from_utf8(line).ok()?.trim();
    let payload = s.strip_prefix("data:")?.trim();
    if payload == "[DONE]" {
        return None;
    }
    let v: Value = serde_json::from_str(payload).ok()?;
    if v.get("usage").is_some_and(|u| !u.is_null()) {
        parse_openai_usage(&v)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::StreamExt;
    use serde_json::json;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn complete_parses_cache_split_and_sends_bearer_auth() {
        let server = MockServer::start().await;
        let body = json!({
            "choices": [{ "message": { "content": "hi" } }],
            "usage": {
                "prompt_tokens": 100, "completion_tokens": 20,
                "prompt_tokens_details": { "cached_tokens": 60 }
            }
        });
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .and(header("authorization", "Bearer sk-test"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(&server)
            .await;

        let p = OpenAiCompat::new("openai", server.uri(), "sk-test");
        let out = p
            .complete(ProviderRequest::new("gpt-x", json!({ "messages": [] })))
            .await
            .unwrap();

        assert_eq!(out.status, 200);
        assert_eq!(out.usage.tokens_in, 40); // 100 total - 60 cached
        assert_eq!(out.usage.cache_read_tokens, 60);
        assert_eq!(out.usage.tokens_out, 20);
    }

    #[tokio::test]
    async fn maps_401_to_auth_and_429_to_rate_limited() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(header("authorization", "Bearer bad"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server)
            .await;
        let p = OpenAiCompat::new("openai", server.uri(), "bad");
        let err = p
            .complete(ProviderRequest::new("m", json!({})))
            .await
            .unwrap_err();
        assert!(matches!(err, ProviderError::Auth));

        let server2 = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(429))
            .mount(&server2)
            .await;
        let p2 = OpenAiCompat::new("openai", server2.uri(), "k");
        let err2 = p2
            .complete(ProviderRequest::new("m", json!({})))
            .await
            .unwrap_err();
        assert!(matches!(err2, ProviderError::RateLimited));
    }

    #[tokio::test]
    async fn embed_parses_vectors_and_usage() {
        let server = MockServer::start().await;
        let body = json!({
            "object": "list",
            "data": [
                { "index": 0, "embedding": [0.1, 0.2, 0.3] },
                { "index": 1, "embedding": [0.4, 0.5, 0.6] }
            ],
            "usage": { "prompt_tokens": 42, "total_tokens": 42 }
        });
        Mock::given(method("POST"))
            .and(path("/embeddings"))
            .and(header("authorization", "Bearer sk-test"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(&server)
            .await;

        let p = OpenAiCompat::new("openai", server.uri(), "sk-test");
        let out = p
            .embed(EmbedRequest::new(
                "text-embedding-3-small",
                vec!["a".into(), "b".into()],
            ))
            .await
            .unwrap();

        assert_eq!(out.status, 200);
        assert_eq!(out.embeddings.len(), 2);
        assert_eq!(out.embeddings[0], vec![0.1, 0.2, 0.3]);
        assert_eq!(out.embeddings[1], vec![0.4, 0.5, 0.6]);
        let usage = out.usage.unwrap();
        assert_eq!(usage.input_tokens, 42);
        assert_eq!(usage.total_tokens, 42);
    }

    #[tokio::test]
    async fn stream_forwards_bytes_and_finalizes_usage() {
        let server = MockServer::start().await;
        let sse = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"he\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"llo\"}}]}\n\n",
            "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":5,\"prompt_tokens_details\":{\"cached_tokens\":4}}}\n\n",
            "data: [DONE]\n\n"
        );
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(sse),
            )
            .mount(&server)
            .await;

        let p = OpenAiCompat::new("openai", server.uri(), "k");
        let mut stream = p
            .stream(ProviderRequest::new("m", json!({ "messages": [] })))
            .await
            .unwrap();

        let mut forwarded: Vec<u8> = Vec::new();
        let mut final_usage: Option<ParsedUsage> = None;
        while let Some(item) = stream.next().await {
            let chunk = item.unwrap();
            forwarded.extend_from_slice(&chunk.data);
            if chunk.usage.is_some() {
                final_usage = chunk.usage;
            }
        }

        let text = String::from_utf8(forwarded).unwrap();
        assert!(text.contains("he") && text.contains("llo") && text.contains("[DONE]"));
        let u = final_usage.unwrap();
        assert_eq!(u.tokens_in, 6); // 10 - 4 cached
        assert_eq!(u.tokens_out, 5);
        assert_eq!(u.cache_read_tokens, 4);
    }
}
