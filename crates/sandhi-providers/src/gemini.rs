//! Google Gemini adapter — `generateContent` / `streamGenerateContent`. The model rides in the
//! URL path; auth is the `x-goog-api-key` header; usage is in `usageMetadata`.

use crate::{
    error_for_status, metered_passthrough, sse_data_json, ByteStream, ParsedUsage, Provider,
    ProviderError, ProviderRequest, ProviderResponse,
};
use async_trait::async_trait;
use sandhi_core::parse_gemini_usage;
use serde_json::Value;

/// The Google Generative Language provider. POSTs to `{base_url}/models/{model}:{method}`.
pub struct Gemini {
    client: reqwest::Client,
    base_url: String,
    api_key: String,
}

impl Gemini {
    pub fn new(base_url: impl Into<String>, api_key: impl Into<String>) -> Self {
        Self {
            client: reqwest::Client::new(),
            base_url: base_url.into(),
            api_key: api_key.into(),
        }
    }

    /// The hosted Gemini API (`https://generativelanguage.googleapis.com/v1beta`).
    pub fn hosted(api_key: impl Into<String>) -> Self {
        Self::new("https://generativelanguage.googleapis.com/v1beta", api_key)
    }

    fn url(&self, model: &str, method: &str) -> String {
        format!(
            "{}/models/{model}:{method}",
            self.base_url.trim_end_matches('/')
        )
    }
}

#[async_trait]
impl Provider for Gemini {
    fn slug(&self) -> &str {
        "gemini"
    }

    async fn complete(&self, req: ProviderRequest) -> Result<ProviderResponse, ProviderError> {
        let resp = self
            .client
            .post(self.url(&req.model, "generateContent"))
            .header("x-goog-api-key", &self.api_key)
            .json(&req.body)
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
        let usage = parse_gemini_usage(&body).unwrap_or_default();
        Ok(ProviderResponse {
            status,
            body,
            usage,
        })
    }

    async fn stream(&self, req: ProviderRequest) -> Result<ByteStream, ProviderError> {
        let url = format!("{}?alt=sse", self.url(&req.model, "streamGenerateContent"));
        let resp = self
            .client
            .post(url)
            .header("x-goog-api-key", &self.api_key)
            .json(&req.body)
            .send()
            .await
            .map_err(|e| ProviderError::Transport(e.to_string()))?;
        if !resp.status().is_success() {
            return Err(error_for_status(resp.status().as_u16()));
        }
        Ok(metered_passthrough(resp.bytes_stream(), sniff_usage_line))
    }
}

/// Accumulate usage from a Gemini `streamGenerateContent` SSE line: the chunk carrying
/// `usageMetadata` (typically the final one) holds the full counts; last wins.
fn sniff_usage_line(line: &[u8], usage: &mut ParsedUsage) {
    if let Some(v) = sse_data_json(line) {
        if let Some(u) = parse_gemini_usage(&v) {
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
    /// offset — a `usageMetadata` field straddling two `Bytes` chunks still parses.
    #[tokio::test]
    async fn stream_usage_invariant_across_every_chunk_boundary() {
        let body: &[u8] = include_bytes!("../tests/fixtures/gemini/stream.sse");
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

    /// Forward-compat property (TD-0001 W1): unknown fields leave the meter unperturbed.
    #[tokio::test]
    async fn stream_usage_ignores_unknown_fields() {
        let body: &[u8] = include_bytes!("../tests/fixtures/gemini/stream_forward_compat.sse");
        assert_eq!(
            crate::accumulate_usage(vec![Bytes::copy_from_slice(body)], sniff_usage_line).await,
            EXPECTED
        );
    }
    use serde_json::json;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn complete_parses_usage_metadata_and_sends_api_key() {
        let server = MockServer::start().await;
        let body = json!({
            "candidates": [{ "content": { "parts": [{ "text": "hi" }] } }],
            "usageMetadata": { "promptTokenCount": 100, "candidatesTokenCount": 30, "cachedContentTokenCount": 40 }
        });
        Mock::given(method("POST"))
            .and(path("/models/gemini-x:generateContent"))
            .and(header("x-goog-api-key", "gk-test"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(&server)
            .await;

        let p = Gemini::new(server.uri(), "gk-test");
        let out = p
            .complete(ProviderRequest::new("gemini-x", json!({ "contents": [] })))
            .await
            .unwrap();
        assert_eq!(out.usage.tokens_in, 60); // 100 - 40 cached
        assert_eq!(out.usage.tokens_out, 30);
        assert_eq!(out.usage.cache_read_tokens, 40);
    }

    #[tokio::test]
    async fn stream_forwards_bytes_and_finalizes_usage() {
        let server = MockServer::start().await;
        let sse = concat!(
            "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"he\"}]}}]}\n\n",
            "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"llo\"}]}}],\"usageMetadata\":{\"promptTokenCount\":10,\"candidatesTokenCount\":5,\"cachedContentTokenCount\":2}}\n\n"
        );
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(sse),
            )
            .mount(&server)
            .await;

        let p = Gemini::new(server.uri(), "gk");
        let mut stream = p
            .stream(ProviderRequest::new("gemini-x", json!({ "contents": [] })))
            .await
            .unwrap();

        let mut forwarded = Vec::new();
        let mut usage = None;
        while let Some(item) = stream.next().await {
            let chunk = item.unwrap();
            forwarded.extend_from_slice(&chunk.data);
            if chunk.usage.is_some() {
                usage = chunk.usage;
            }
        }
        assert!(String::from_utf8(forwarded).unwrap().contains("llo"));
        let u = usage.unwrap();
        assert_eq!(u.tokens_in, 8); // 10 - 2 cached
        assert_eq!(u.tokens_out, 5);
    }
}
