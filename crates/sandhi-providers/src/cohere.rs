//! Cohere v2 chat adapter. POSTs to `{base_url}/v2/chat` with `Authorization: Bearer`; usage is
//! in `usage.billed_units` (no prompt-cache split).

use crate::{
    error_for_response, metered_passthrough, sse_data_json, ByteStream, ParsedUsage, Provider,
    ProviderError, ProviderRequest, ProviderResponse,
};
use async_trait::async_trait;
use sandhi_core::parse_cohere_usage;
use serde_json::{json, Value};

/// The Cohere provider.
pub struct Cohere {
    client: reqwest::Client,
    base_url: String,
    api_key: String,
    headers: http::HeaderMap,
}

impl Cohere {
    pub fn new(base_url: impl Into<String>, api_key: impl Into<String>) -> Self {
        Self {
            client: crate::default_client(),
            base_url: base_url.into(),
            api_key: api_key.into(),
            headers: http::HeaderMap::new(),
        }
    }

    /// Caller-supplied provider headers, transport-owned names stripped (TD-0022 D3: this
    /// family previously dropped `ProviderTransportConfig::headers` entirely).
    #[must_use]
    pub fn with_headers(mut self, headers: http::HeaderMap) -> Self {
        self.headers = crate::strip_transport_owned(headers);
        self
    }

    /// The hosted Cohere API (`https://api.cohere.com`).
    pub fn hosted(api_key: impl Into<String>) -> Self {
        Self::new("https://api.cohere.com", api_key)
    }

    fn chat_url(&self) -> String {
        format!("{}/v2/chat", self.base_url.trim_end_matches('/'))
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
            .headers(crate::merge_call_headers(&self.headers, &req.extra_headers))
            .json(&body)
            .send()
            .await
            .map_err(|e| ProviderError::Transport(e.to_string()))?;
        let status = resp.status().as_u16();
        if !resp.status().is_success() {
            return Err(error_for_response(resp, None).await);
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
            attempts: 1,
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
            .headers(crate::merge_call_headers(&self.headers, &req.extra_headers))
            .json(&body)
            .send()
            .await
            .map_err(|e| ProviderError::Transport(e.to_string()))?;
        if !resp.status().is_success() {
            return Err(error_for_response(resp, None).await);
        }
        Ok(metered_passthrough(resp.bytes_stream(), sniff_usage_line))
    }
}

/// Accumulate usage from a Cohere v2 chat SSE line. Cohere carries usage on the `message-end`
/// event under `delta.usage` (falling back to a top-level `usage`); last wins.
pub(crate) fn sniff_usage_line(line: &[u8], usage: &mut ParsedUsage) -> bool {
    let Some(v) = sse_data_json(line) else {
        return false;
    };
    let obj = v
        .get("usage")
        .or_else(|| v.get("delta").and_then(|d| d.get("usage")));
    if let Some(uo) = obj {
        if let Some(u) = parse_cohere_usage(&json!({ "usage": uo })) {
            *usage = u;
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use http::header::{HeaderName, HeaderValue};
    use wiremock::matchers::{header, method, path};

    const EXPECTED: ParsedUsage = ParsedUsage {
        tokens_in: 300,
        tokens_out: 120,
        cache_creation_tokens: 0,
        cache_read_tokens: 0,
        reasoning_tokens: 0,
    };

    /// Chunk-boundary property (TD-0001 W1): finalized usage is invariant across every split
    /// offset — the `message-end` `delta.usage` straddling two `Bytes` chunks still parses.
    #[tokio::test]
    async fn stream_usage_invariant_across_every_chunk_boundary() {
        let body: &[u8] = include_bytes!("../tests/fixtures/cohere/stream.sse");
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

    /// Forward-compat property (TD-0001 W1): unknown event types + unknown fields leave the meter
    /// unperturbed.
    #[tokio::test]
    async fn stream_usage_ignores_unknown_events_and_fields() {
        let body: &[u8] = include_bytes!("../tests/fixtures/cohere/stream_forward_compat.sse");
        assert_eq!(
            crate::accumulate_usage(vec![Bytes::copy_from_slice(body)], sniff_usage_line).await,
            EXPECTED
        );
    }
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

    /// TD-0022 D3: this family previously dropped transport headers entirely. Static and
    /// per-call headers must ride, transport-owned names (incl. the family credential) must
    /// never be caller-suppliable.
    #[tokio::test]
    async fn headers_ride_and_the_credential_is_never_caller_suppliable() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v2/chat"))
            .and(header("authorization", "Bearer co-real"))
            .and(header("x-custom", "static-value"))
            .and(header("x-sandhi-step-id", "step-7"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "message": { "content": [{ "text": "hi" }] },
                "usage": { "billed_units": { "input_tokens": 1, "output_tokens": 1 } }
            })))
            .mount(&server)
            .await;

        let mut statics = http::HeaderMap::new();
        statics.insert(
            HeaderName::from_static("x-custom"),
            HeaderValue::from_static("static-value"),
        );
        statics.insert(
            HeaderName::from_static("authorization"),
            HeaderValue::from_static("Bearer attacker-static"),
        );
        let provider = Cohere::new(server.uri(), "co-real").with_headers(statics);

        let mut request = ProviderRequest::new("command-r", json!({ "messages": [] }));
        let mut call = http::HeaderMap::new();
        call.insert(
            HeaderName::from_static("x-sandhi-step-id"),
            HeaderValue::from_static("step-7"),
        );
        call.insert(
            HeaderName::from_static("authorization"),
            HeaderValue::from_static("Bearer attacker-call"),
        );
        request.extra_headers = call;
        provider.complete(request).await.unwrap();

        // The matchers gate the positive behavior; this pins exactly one credential value.
        let sent = &server.received_requests().await.unwrap()[0];
        assert_eq!(
            sent.headers.get_all("authorization").iter().count(),
            1,
            "no attacker-supplied Authorization may ride alongside the real key"
        );
    }
}
