//! Local Ollama adapter — native `/api/chat`. Ollama streams **NDJSON** (not SSE); usage is
//! `prompt_eval_count` / `eval_count` on the final line. (vLLM and other OpenAI-compatible local
//! servers use [`crate::OpenAiCompat`] pointed at `http://localhost:.../v1`.)

use crate::{
    error_for_status, metered_passthrough, ByteStream, ParsedUsage, Provider, ProviderError,
    ProviderRequest, ProviderResponse,
};
use async_trait::async_trait;
use sandhi_core::parse_ollama_usage;
use serde_json::Value;

/// A local Ollama server. Optional bearer for secured deployments.
pub struct Ollama {
    client: reqwest::Client,
    base_url: String,
    api_key: Option<String>,
}

impl Ollama {
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            client: crate::default_client(),
            base_url: base_url.into(),
            api_key: None,
        }
    }

    /// The default local Ollama endpoint (`http://localhost:11434`).
    pub fn local() -> Self {
        Self::new("http://localhost:11434")
    }

    #[must_use]
    pub fn with_api_key(mut self, api_key: impl Into<String>) -> Self {
        self.api_key = Some(api_key.into());
        self
    }

    fn chat_url(&self) -> String {
        format!("{}/api/chat", self.base_url.trim_end_matches('/'))
    }

    fn post(&self, body: &Value) -> reqwest::RequestBuilder {
        let mut rb = self.client.post(self.chat_url()).json(body);
        if let Some(key) = &self.api_key {
            rb = rb.bearer_auth(key);
        }
        rb
    }
}

#[async_trait]
impl Provider for Ollama {
    fn slug(&self) -> &str {
        "ollama"
    }

    async fn complete(&self, req: ProviderRequest) -> Result<ProviderResponse, ProviderError> {
        let mut body = req.body;
        if let Some(obj) = body.as_object_mut() {
            obj.insert("stream".into(), Value::Bool(false));
        }
        let resp = self
            .post(&body)
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
        let usage = parse_ollama_usage(&body).unwrap_or_default();
        Ok(ProviderResponse {
            status,
            body,
            usage,
            attempts: 1,
        })
    }

    async fn stream(&self, req: ProviderRequest) -> Result<ByteStream, ProviderError> {
        let mut body = req.body;
        if let Some(obj) = body.as_object_mut() {
            obj.insert("stream".into(), Value::Bool(true));
        }
        let resp = self
            .post(&body)
            .send()
            .await
            .map_err(|e| ProviderError::Transport(e.to_string()))?;
        if !resp.status().is_success() {
            return Err(error_for_status(resp.status().as_u16()));
        }
        // NDJSON: each line is a complete JSON object; the final one carries the eval counts.
        Ok(metered_passthrough(resp.bytes_stream(), sniff_usage_line))
    }
}

/// Accumulate usage from an Ollama NDJSON line: the final object (`done: true`) carries
/// `prompt_eval_count` / `eval_count`; last wins.
pub(crate) fn sniff_usage_line(line: &[u8], usage: &mut ParsedUsage) {
    if let Some(v) = std::str::from_utf8(line)
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(s.trim()).ok())
    {
        if let Some(u) = parse_ollama_usage(&v) {
            *usage = u;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use serde_json::json;
    use wiremock::matchers::{method, path};

    const EXPECTED: ParsedUsage = ParsedUsage {
        tokens_in: 512,
        tokens_out: 128,
        cache_creation_tokens: 0,
        cache_read_tokens: 0,
    };

    /// Chunk-boundary property (TD-0001 W1): finalized usage is invariant across every split
    /// offset — the final NDJSON line's eval counts straddling two `Bytes` chunks still parse.
    #[tokio::test]
    async fn stream_usage_invariant_across_every_chunk_boundary() {
        let body: &[u8] = include_bytes!("../tests/fixtures/ollama/stream.ndjson");
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
        let body: &[u8] = include_bytes!("../tests/fixtures/ollama/stream_forward_compat.ndjson");
        assert_eq!(
            crate::accumulate_usage(vec![Bytes::copy_from_slice(body)], sniff_usage_line).await,
            EXPECTED
        );
    }
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn complete_reads_eval_counts() {
        let server = MockServer::start().await;
        let body = json!({
            "message": { "role": "assistant", "content": "hi" },
            "done": true, "prompt_eval_count": 26, "eval_count": 14
        });
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(&server)
            .await;

        let p = Ollama::new(server.uri());
        let out = p
            .complete(ProviderRequest::new("llama3", json!({ "messages": [] })))
            .await
            .unwrap();
        assert_eq!(out.usage.tokens_in, 26);
        assert_eq!(out.usage.tokens_out, 14);
    }
}
