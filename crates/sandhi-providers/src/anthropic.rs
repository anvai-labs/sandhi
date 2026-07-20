//! Anthropic adapter — the Messages API. Validates the prompt-cache split
//! (`cache_creation_input_tokens` / `cache_read_input_tokens`) the meter depends on.

use crate::parse_anthropic_usage;
use crate::{
    error_for_status, metered_passthrough, ByteStream, ParsedUsage, Provider, ProviderError,
    ProviderRequest, ProviderResponse,
};
use async_trait::async_trait;
use sandhi_core::usage::u64_at;
use serde_json::Value;

const ANTHROPIC_VERSION: &str = "2023-06-01";

/// The Anthropic Messages provider. POSTs to `{base_url}/v1/messages` with `x-api-key` +
/// `anthropic-version` headers.
pub struct Anthropic {
    client: reqwest::Client,
    base_url: String,
    api_key: String,
    version: String,
}

impl Anthropic {
    pub fn new(base_url: impl Into<String>, api_key: impl Into<String>) -> Self {
        Self {
            client: reqwest::Client::new(),
            base_url: base_url.into(),
            api_key: api_key.into(),
            version: ANTHROPIC_VERSION.to_string(),
        }
    }

    /// The hosted Anthropic API (`https://api.anthropic.com`).
    pub fn hosted(api_key: impl Into<String>) -> Self {
        Self::new("https://api.anthropic.com", api_key)
    }

    fn messages_url(&self) -> String {
        format!("{}/v1/messages", self.base_url.trim_end_matches('/'))
    }
}

#[async_trait]
impl Provider for Anthropic {
    fn slug(&self) -> &str {
        "anthropic"
    }

    async fn complete(&self, req: ProviderRequest) -> Result<ProviderResponse, ProviderError> {
        let mut body = req.body;
        if let Some(obj) = body.as_object_mut() {
            obj.insert("stream".into(), Value::Bool(false));
        }
        let resp = self
            .client
            .post(self.messages_url())
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", &self.version)
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
        let usage = parse_anthropic_usage(&body).unwrap_or_default();
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
            .post(self.messages_url())
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", &self.version)
            .json(&body)
            .send()
            .await
            .map_err(|e| ProviderError::Transport(e.to_string()))?;
        if !resp.status().is_success() {
            return Err(error_for_status(resp.status().as_u16()));
        }
        // Forward every upstream chunk verbatim (O(1) memory, ADR-0047 D9) while sniffing each
        // complete line for usage. `metered_passthrough` is the single shared streaming
        // primitive — the chunk-boundary property test exercises this exact path.
        Ok(metered_passthrough(
            Box::pin(resp.bytes_stream()),
            sniff_usage_line,
        ))
    }
}

/// Accumulate usage from Anthropic SSE lines: input + cache from `message_start`, output from
/// `message_delta` (cumulative).
fn sniff_usage_line(line: &[u8], acc: &mut ParsedUsage) {
    let Ok(s) = std::str::from_utf8(line) else {
        return;
    };
    let Some(payload) = s.trim().strip_prefix("data:") else {
        return;
    };
    let Ok(v) = serde_json::from_str::<Value>(payload.trim()) else {
        return;
    };
    match v.get("type").and_then(Value::as_str) {
        Some("message_start") => {
            if let Some(u) = v.get("message").and_then(|m| m.get("usage")) {
                acc.tokens_in = u64_at(u, "input_tokens");
                acc.cache_creation_tokens = u64_at(u, "cache_creation_input_tokens");
                acc.cache_read_tokens = u64_at(u, "cache_read_input_tokens");
            }
        }
        Some("message_delta") => {
            if let Some(u) = v.get("usage") {
                acc.tokens_out = u64_at(u, "output_tokens");
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use futures_util::StreamExt;
    use serde_json::json;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const EXPECTED: ParsedUsage = ParsedUsage {
        tokens_in: 1024,
        tokens_out: 256,
        cache_creation_tokens: 2048,
        cache_read_tokens: 4096,
    };

    /// Drive an SSE byte-stream (pre-split into `chunks`) through the production streaming
    /// primitive (`metered_passthrough` + the real `sniff_usage_line`) and return the finalized
    /// usage from the terminal item.
    async fn accumulate(chunks: Vec<Bytes>) -> ParsedUsage {
        let upstream = futures_util::stream::iter(
            chunks
                .into_iter()
                .map(Ok::<Bytes, reqwest::Error>)
                .collect::<Vec<_>>(),
        );
        let mut out = metered_passthrough(Box::pin(upstream), sniff_usage_line);
        let mut final_usage = None;
        while let Some(item) = out.next().await {
            let c = item.unwrap();
            if c.usage.is_some() {
                final_usage = c.usage;
            }
        }
        final_usage.expect("terminal item carries usage")
    }

    /// Chunk-boundary property (ADR-0003 §5 / TD-0001 W1): the finalized usage must be invariant
    /// no matter where the byte stream is split — a `usage` field straddling two `Bytes` chunks
    /// must still parse. Covers every 2-way split offset plus the one-byte-per-chunk worst case.
    #[tokio::test]
    async fn stream_usage_invariant_across_every_chunk_boundary() {
        let sse: &[u8] = include_bytes!("../tests/fixtures/anthropic/stream_cache_split.sse");
        for k in 0..=sse.len() {
            let chunks = vec![
                Bytes::copy_from_slice(&sse[..k]),
                Bytes::copy_from_slice(&sse[k..]),
            ];
            assert_eq!(accumulate(chunks).await, EXPECTED, "split at offset {k}");
        }
        let one_byte: Vec<Bytes> = sse.iter().map(|b| Bytes::copy_from_slice(&[*b])).collect();
        assert_eq!(accumulate(one_byte).await, EXPECTED, "one byte per chunk");
    }

    /// Forward-compat property (ADR-0003 §5 / TD-0001 W1): unknown event types and unknown usage
    /// fields must not fault or perturb the meter.
    #[tokio::test]
    async fn stream_usage_ignores_unknown_events_and_fields() {
        let sse: &[u8] = include_bytes!("../tests/fixtures/anthropic/stream_forward_compat.sse");
        assert_eq!(
            accumulate(vec![Bytes::copy_from_slice(sse)]).await,
            EXPECTED
        );
    }

    #[tokio::test]
    async fn complete_sends_headers_and_parses_cache_split() {
        let server = MockServer::start().await;
        let body = json!({
            "content": [{ "type": "text", "text": "hi" }],
            "usage": {
                "input_tokens": 120, "output_tokens": 45,
                "cache_creation_input_tokens": 300, "cache_read_input_tokens": 900
            }
        });
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .and(header("x-api-key", "ak-test"))
            .and(header("anthropic-version", "2023-06-01"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(&server)
            .await;

        let p = Anthropic::new(server.uri(), "ak-test");
        let out = p
            .complete(ProviderRequest::new("claude-x", json!({ "messages": [] })))
            .await
            .unwrap();

        assert_eq!(out.usage.tokens_in, 120);
        assert_eq!(out.usage.tokens_out, 45);
        assert_eq!(out.usage.cache_creation_tokens, 300);
        assert_eq!(out.usage.cache_read_tokens, 900);
    }

    #[tokio::test]
    async fn stream_finalizes_usage_from_start_and_delta() {
        let server = MockServer::start().await;
        let sse = concat!(
            "event: message_start\n",
            "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":120,\"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":30}}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"delta\":{\"text\":\"hi\"}}\n\n",
            "event: message_delta\n",
            "data: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":64}}\n\n",
            "event: message_stop\n",
            "data: {\"type\":\"message_stop\"}\n\n"
        );
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(sse),
            )
            .mount(&server)
            .await;

        let p = Anthropic::new(server.uri(), "ak-test");
        let mut stream = p
            .stream(ProviderRequest::new("claude-x", json!({ "messages": [] })))
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
        assert!(text.contains("message_start") && text.contains("message_stop"));
        let u = final_usage.unwrap();
        assert_eq!(u.tokens_in, 120);
        assert_eq!(u.tokens_out, 64);
        assert_eq!(u.cache_read_tokens, 30);
    }
}
