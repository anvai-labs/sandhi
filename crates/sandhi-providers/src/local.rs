//! Local Ollama adapter — native `/api/chat`. Ollama streams **NDJSON** (not SSE); usage is
//! `prompt_eval_count` / `eval_count` on the final line. (vLLM and other OpenAI-compatible local
//! servers use [`crate::OpenAiCompat`] pointed at `http://localhost:.../v1`.)

use crate::{
    error_for_status, metered_passthrough, ByteStream, Provider, ProviderError, ProviderRequest,
    ProviderResponse,
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
            client: reqwest::Client::new(),
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
        Ok(metered_passthrough(resp.bytes_stream(), |line, usage| {
            if let Some(v) = std::str::from_utf8(line)
                .ok()
                .and_then(|s| serde_json::from_str::<Value>(s.trim()).ok())
            {
                if let Some(u) = parse_ollama_usage(&v) {
                    *usage = u;
                }
            }
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use wiremock::matchers::{method, path};
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
