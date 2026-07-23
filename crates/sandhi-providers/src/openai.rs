//! OpenAI-compatible adapter — OpenAI proper plus the ~20 providers that speak the Chat
//! Completions wire format (Groq, Together, Fireworks, DeepSeek, Mistral, Qwen, xAI,
//! OpenRouter, vLLM, LM Studio, Ollama, Cerebras…). One adapter, many providers.

use crate::{
    error_for_status, metered_passthrough, sse_data_json, ByteStream, ParsedUsage, Provider,
    ProviderError, ProviderRequest, ProviderResponse,
};
use crate::{parse_openai_usage, validate_openai_chat_messages};
use async_trait::async_trait;
use reqwest::header::{HeaderMap, AUTHORIZATION, CONTENT_TYPE, HOST};
use serde_json::{json, Value};

/// An OpenAI-compatible provider. `base_url` is the API base (e.g. `https://api.openai.com/v1`);
/// the adapter POSTs to `{base_url}/chat/completions` with `Authorization: Bearer <key>`.
pub struct OpenAiCompat {
    client: reqwest::Client,
    slug: String,
    base_url: String,
    api_key: String,
    headers: HeaderMap,
}

impl OpenAiCompat {
    pub fn new(
        slug: impl Into<String>,
        base_url: impl Into<String>,
        api_key: impl Into<String>,
    ) -> Self {
        Self {
            client: crate::default_client(),
            slug: slug.into(),
            base_url: base_url.into(),
            api_key: api_key.into(),
            headers: HeaderMap::new(),
        }
    }

    /// OpenAI proper (`https://api.openai.com/v1`), slug `openai`.
    pub fn openai(api_key: impl Into<String>) -> Self {
        Self::new("openai", "https://api.openai.com/v1", api_key)
    }

    /// Add caller-supplied provider headers while protecting transport-owned headers.
    /// OpenRouter's `HTTP-Referer` / `X-Title` are the motivating case.
    #[must_use]
    pub fn with_headers(mut self, mut headers: HeaderMap) -> Self {
        headers.remove(AUTHORIZATION);
        headers.remove(CONTENT_TYPE);
        headers.remove(HOST);
        self.headers = headers;
        self
    }

    fn chat_url(&self) -> String {
        format!("{}/chat/completions", self.base_url.trim_end_matches('/'))
    }
}

#[async_trait]
impl Provider for OpenAiCompat {
    fn slug(&self) -> &str {
        &self.slug
    }

    async fn complete(&self, req: ProviderRequest) -> Result<ProviderResponse, ProviderError> {
        validate_openai_chat_messages(&req.body)?;
        let mut body = req.body;
        if let Some(obj) = body.as_object_mut() {
            obj.insert("stream".into(), Value::Bool(false));
        }
        let resp = self
            .client
            .post(self.chat_url())
            .bearer_auth(&self.api_key)
            .headers(self.headers.clone())
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
            attempts: 1,
        })
    }

    async fn stream(&self, req: ProviderRequest) -> Result<ByteStream, ProviderError> {
        validate_openai_chat_messages(&req.body)?;
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
            .headers(self.headers.clone())
            .json(&body)
            .send()
            .await
            .map_err(|e| ProviderError::Transport(e.to_string()))?;
        if !resp.status().is_success() {
            return Err(error_for_status(resp.status().as_u16()));
        }
        // Forward every upstream chunk verbatim (O(1) pass-through) while sniffing each complete
        // line for the terminal usage object; `metered_passthrough` is the shared streaming
        // primitive (the chunk-boundary property test exercises this exact path).
        Ok(metered_passthrough(resp.bytes_stream(), sniff_usage_line))
    }
}

/// Accumulate usage from an OpenAI Chat Completions SSE line. With `stream_options.include_usage`
/// the terminal chunk carries the `usage` object while earlier chunks send `"usage": null` — the
/// null guard prevents a non-final chunk from zeroing the counts; last usage-bearing line wins.
pub(crate) fn sniff_usage_line(line: &[u8], usage: &mut ParsedUsage) {
    let Some(v) = sse_data_json(line) else {
        return;
    };
    if v.get("usage").is_some_and(|u| !u.is_null()) {
        if let Some(u) = parse_openai_usage(&v) {
            *usage = u;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use futures_util::StreamExt;

    const EXPECTED: ParsedUsage = ParsedUsage {
        tokens_in: 200,
        tokens_out: 250,
        cache_creation_tokens: 0,
        cache_read_tokens: 800,
    };

    /// Chunk-boundary property (TD-0001 W1): finalized usage is invariant across every split
    /// offset — a `usage` field straddling two `Bytes` chunks still parses.
    #[tokio::test]
    async fn stream_usage_invariant_across_every_chunk_boundary() {
        let body: &[u8] = include_bytes!("../tests/fixtures/openai/stream.sse");
        for k in 0..=body.len() {
            let chunks = vec![
                Bytes::copy_from_slice(&body[..k]),
                Bytes::copy_from_slice(&body[k..]),
            ];
            assert_eq!(
                crate::accumulate_usage(chunks, sniff_usage_line).await,
                EXPECTED,
                "split at offset {k}"
            );
        }
        let one_byte: Vec<Bytes> = body.iter().map(|b| Bytes::copy_from_slice(&[*b])).collect();
        assert_eq!(
            crate::accumulate_usage(one_byte, sniff_usage_line).await,
            EXPECTED,
            "one byte per chunk"
        );
    }

    /// Forward-compat property (TD-0001 W1): unknown fields + `"usage": null` chunks leave the
    /// meter unperturbed.
    #[tokio::test]
    async fn stream_usage_ignores_unknown_fields() {
        let body: &[u8] = include_bytes!("../tests/fixtures/openai/stream_forward_compat.sse");
        assert_eq!(
            crate::accumulate_usage(vec![Bytes::copy_from_slice(body)], sniff_usage_line).await,
            EXPECTED
        );
    }
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
    async fn forwards_custom_headers_but_not_transport_owned_headers() {
        use reqwest::header::{HeaderName, HeaderValue};
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(header("authorization", "Bearer real-key"))
            .and(header("http-referer", "https://victor.example"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "choices": [{"message": {"content": "ok"}}]
            })))
            .mount(&server)
            .await;
        let mut headers = HeaderMap::new();
        headers.insert(
            HeaderName::from_static("http-referer"),
            HeaderValue::from_static("https://victor.example"),
        );
        headers.insert(AUTHORIZATION, HeaderValue::from_static("Bearer attacker"));
        OpenAiCompat::new("openrouter", server.uri(), "real-key")
            .with_headers(headers)
            .complete(ProviderRequest::new("m", json!({})))
            .await
            .unwrap();
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
