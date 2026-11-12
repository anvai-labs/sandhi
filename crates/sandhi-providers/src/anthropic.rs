//! Anthropic adapter — the Messages API. Validates the prompt-cache split
//! (`cache_creation_input_tokens` / `cache_read_input_tokens`) the meter depends on.

use crate::{
    error_for_response, metered_passthrough, ByteStream, ParsedUsage, Provider, ProviderError,
    ProviderRequest, ProviderResponse,
};
use async_trait::async_trait;
use sandhi_core::usage::u64_at;
use serde_json::Value;

const ANTHROPIC_VERSION: &str = "2023-06-01";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AnthropicAuthScheme {
    #[default]
    ApiKey,
    Bearer,
}

/// The Anthropic Messages provider. Authentication is explicit: API keys use `x-api-key`, while
/// subscription OAuth uses `Authorization: Bearer`. The two credential headers are never sent
/// together.
pub struct Anthropic {
    client: reqwest::Client,
    base_url: String,
    api_key: String,
    version: String,
    auth_scheme: AnthropicAuthScheme,
    headers: http::HeaderMap,
    usage_profile: sandhi_core::usage::AnthropicUsageProfile,
}

impl Anthropic {
    pub fn new(base_url: impl Into<String>, api_key: impl Into<String>) -> Self {
        Self {
            client: crate::default_client(),
            base_url: base_url.into(),
            api_key: api_key.into(),
            version: ANTHROPIC_VERSION.to_string(),
            auth_scheme: AnthropicAuthScheme::ApiKey,
            headers: http::HeaderMap::new(),
            usage_profile: Default::default(),
        }
    }

    /// The hosted Anthropic API (`https://api.anthropic.com`).
    pub fn hosted(api_key: impl Into<String>) -> Self {
        Self::new("https://api.anthropic.com", api_key)
    }

    #[must_use]
    pub fn with_auth_scheme(mut self, auth_scheme: AnthropicAuthScheme) -> Self {
        self.auth_scheme = auth_scheme;
        self
    }

    /// Explicit response-usage convention for qualified Messages streaming.
    #[must_use]
    pub fn with_usage_profile(
        mut self,
        profile: sandhi_core::usage::AnthropicUsageProfile,
    ) -> Self {
        self.usage_profile = profile;
        self
    }

    /// Caller-supplied provider headers, transport-owned names stripped (TD-0022 D3: this
    /// family previously dropped `ProviderTransportConfig::headers` entirely, so gateway-path
    /// wire headers never reached the upstream).
    #[must_use]
    pub fn with_headers(mut self, headers: http::HeaderMap) -> Self {
        self.headers = crate::strip_transport_owned(headers);
        self
    }

    fn messages_url(&self) -> String {
        format!("{}/v1/messages", self.base_url.trim_end_matches('/'))
    }

    fn authenticate(&self, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match self.auth_scheme {
            AnthropicAuthScheme::ApiKey => request.header("x-api-key", &self.api_key),
            AnthropicAuthScheme::Bearer => request.bearer_auth(&self.api_key),
        }
    }
}

#[async_trait]
impl Provider for Anthropic {
    fn slug(&self) -> &str {
        "anthropic"
    }

    async fn complete(&self, req: ProviderRequest) -> Result<ProviderResponse, ProviderError> {
        let attempt_context = req.attempt_context.clone();
        let model = req.model.clone();
        let mut body = req.body;
        if let Some(obj) = body.as_object_mut() {
            obj.insert("stream".into(), Value::Bool(false));
        }
        let request = self
            .client
            .post(self.messages_url())
            .header("anthropic-version", &self.version)
            .headers(crate::merge_call_headers(&self.headers, &req.extra_headers))
            .json(&body);
        crate::attempt::observe_complete_qualified(
            attempt_context,
            self.slug(),
            Some(&model),
            sandhi_core::EndpointFamilyV1::AnthropicMessages,
            self.usage_profile,
            |facts| async move {
                let resp = self
                    .authenticate(request)
                    .send()
                    .await
                    .map_err(|e| ProviderError::Transport(e.to_string()))?;
                let status = resp.status().as_u16();
                facts.record_headers(status, resp.headers(), Some("anthropic-request-id"));
                if !resp.status().is_success() {
                    return Err(error_for_response(resp, Some("anthropic-request-id")).await);
                }
                let body = crate::decode_response_json(resp).await?;
                let (response_usage, observed_usage) =
                    crate::buffered_usage(sandhi_core::CacheReadFamily::Anthropic, &body);
                Ok((
                    ProviderResponse {
                        status,
                        body,
                        usage: response_usage,
                        attempts: 1,
                    },
                    observed_usage,
                ))
            },
        )
        .await
    }

    async fn stream_qualified(
        &self,
        req: ProviderRequest,
        _delivery: crate::QualifiedStreamDelivery,
    ) -> Result<crate::QualifiedProviderStream, ProviderError> {
        let context = req.attempt_context.clone();
        let model = req.model.clone();
        let mut body = req.body;
        body.as_object_mut()
            .ok_or_else(|| {
                ProviderError::InvalidRequest("Messages request must be a JSON object".into())
            })?
            .insert("stream".into(), Value::Bool(true));
        let request = self
            .client
            .post(self.messages_url())
            .header("anthropic-version", &self.version)
            .headers(crate::merge_call_headers(&self.headers, &req.extra_headers))
            .json(&body);
        crate::attempt::observe_qualified_stream(
            context,
            self.slug(),
            Some(&model),
            sandhi_core::EndpointFamilyV1::AnthropicMessages,
            Default::default(),
            self.usage_profile,
            |facts| async move {
                let response = self
                    .authenticate(request)
                    .send()
                    .await
                    .map_err(|error| ProviderError::Transport(error.to_string()))?;
                facts.record_headers(
                    response.status().as_u16(),
                    response.headers(),
                    Some("anthropic-request-id"),
                );
                if !response.status().is_success() {
                    return Err(error_for_response(response, Some("anthropic-request-id")).await);
                }
                let request_id =
                    crate::provider_request_id(response.headers(), Some("anthropic-request-id"));
                use futures_util::StreamExt;
                let raw: crate::raw::RawChunkStream =
                    Box::pin(response.bytes_stream().map(|chunk| {
                        chunk.map_err(|error| ProviderError::Transport(error.to_string()))
                    }));
                Ok((raw, request_id))
            },
        )
        .await
    }

    async fn stream(&self, req: ProviderRequest) -> Result<ByteStream, ProviderError> {
        let attempt_context = req.attempt_context.clone();
        let model = req.model.clone();
        let mut body = req.body;
        if let Some(obj) = body.as_object_mut() {
            obj.insert("stream".into(), Value::Bool(true));
        }
        let request = self
            .client
            .post(self.messages_url())
            .header("anthropic-version", &self.version)
            .headers(crate::merge_call_headers(&self.headers, &req.extra_headers))
            .json(&body);
        crate::attempt::observe_stream(
            attempt_context,
            self.slug(),
            Some(&model),
            |facts| async move {
                let resp = self
                    .authenticate(request)
                    .send()
                    .await
                    .map_err(|e| ProviderError::Transport(e.to_string()))?;
                facts.record_headers(
                    resp.status().as_u16(),
                    resp.headers(),
                    Some("anthropic-request-id"),
                );
                if !resp.status().is_success() {
                    return Err(error_for_response(resp, Some("anthropic-request-id")).await);
                }
                // Forward every upstream chunk verbatim (O(1) memory, ADR-0047 D9) while sniffing each
                // complete line for usage. `metered_passthrough` is the single shared streaming
                // primitive — the chunk-boundary property test exercises this exact path.
                Ok(metered_passthrough(
                    Box::pin(resp.bytes_stream()),
                    sniff_usage_line,
                ))
            },
        )
        .await
    }
}

/// Merge cumulative usage from Messages SSE. Hosted Anthropic reports input/cache
/// on message_start; compatible endpoints can replace placeholder counts in a
/// message_delta. Missing fields must retain prior measurements in either frame.
pub(crate) fn sniff_usage_line(line: &[u8], acc: &mut ParsedUsage) -> bool {
    let Ok(s) = std::str::from_utf8(line) else {
        return false;
    };
    let Some(payload) = s.trim().strip_prefix("data:") else {
        return false;
    };
    let Ok(v) = serde_json::from_str::<Value>(payload.trim()) else {
        return false;
    };
    crate::observe_stream_cache(sandhi_core::CacheReadFamily::Anthropic, &v, acc);
    let usage = match v.get("type").and_then(Value::as_str) {
        Some("message_start") => v.pointer("/message/usage"),
        Some("message_delta") => v.get("usage"),
        _ => None,
    };
    let Some(usage) = usage.filter(|value| value.is_object()) else {
        return false;
    };
    for (field, count) in [
        ("input_tokens", &mut acc.tokens_in),
        (
            "cache_creation_input_tokens",
            &mut acc.cache_creation_tokens,
        ),
        ("cache_read_input_tokens", &mut acc.cache_read_tokens),
        ("output_tokens", &mut acc.tokens_out),
    ] {
        if let Some(value) = usage
            .get(field)
            .and_then(Value::as_u64)
            .filter(|value| *value == u64_at(usage, field))
        {
            *count = value;
        }
    }
    acc.reasoning_included = Some(true);
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use futures_util::StreamExt;
    use serde_json::json;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use http::header::{HeaderName, HeaderValue};

    const EXPECTED: ParsedUsage = ParsedUsage {
        cache_read_observation: Some(sandhi_core::CacheReadObservation {
            status: sandhi_core::CacheReadStatus::Reported,
            source: sandhi_core::CacheReadSource::OriginUsage,
        }),
        reasoning_included: Some(true),
        tokens_in: 1024,
        tokens_out: 256,
        cache_creation_tokens: 2048,
        cache_read_tokens: 4096,
        reasoning_tokens: 0,
        duration_ms: None,
        time_to_first_token_ms: None,
    };

    #[test]
    fn terminal_delta_can_supply_input_and_cache_after_placeholder_start() {
        let mut usage = ParsedUsage::default();
        assert!(sniff_usage_line(br#"data: {"type":"message_start","message":{"usage":{"input_tokens":0,"output_tokens":0}}}"#, &mut usage));
        assert!(sniff_usage_line(br#"data: {"type":"message_delta","usage":{"input_tokens":5,"cache_read_input_tokens":3776,"cache_creation_input_tokens":30,"output_tokens":67}}"#, &mut usage));
        assert_eq!(
            (
                usage.tokens_in,
                usage.cache_read_tokens,
                usage.cache_creation_tokens,
                usage.tokens_out
            ),
            (5, 3776, 30, 67)
        );
        // A later output-only update must not erase the input/cache evidence.
        assert!(sniff_usage_line(
            br#"data: {"type":"message_delta","usage":{"output_tokens":70}}"#,
            &mut usage
        ));
        assert_eq!(
            (
                usage.tokens_in,
                usage.cache_read_tokens,
                usage.cache_creation_tokens,
                usage.tokens_out
            ),
            (5, 3776, 30, 70)
        );
    }

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

    /// TD-0022 D2/D3 + review finding: static and per-call headers ride for this family, and
    /// the family credential header can NEVER be caller-supplied — reqwest appends same-named
    /// values added after a header map, so an unstripped `x-api-key` would put a second,
    /// attacker-supplied credential on the wire next to the real one.
    #[tokio::test]
    async fn headers_ride_and_the_credential_header_is_never_caller_suppliable() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .and(header("x-api-key", "REAL-KEY"))
            .and(header("anthropic-version", "2023-06-01"))
            .and(header("x-custom", "static-value"))
            .and(header("x-sandhi-step-id", "step-7"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "msg_1", "type": "message", "role": "assistant",
                "content": [{"type": "text", "text": "hi"}],
                "usage": {"input_tokens": 3, "output_tokens": 1}
            })))
            .mount(&server)
            .await;

        let mut statics = http::HeaderMap::new();
        statics.insert(
            HeaderName::from_static("x-custom"),
            HeaderValue::from_static("static-value"),
        );
        // Attacker-controlled attempts — all transport-owned, all must vanish.
        statics.insert(
            HeaderName::from_static("x-api-key"),
            HeaderValue::from_static("attacker-static"),
        );
        let provider = Anthropic::new(server.uri(), "REAL-KEY").with_headers(statics);

        let mut request = ProviderRequest::new(
            "claude-test",
            json!({ "messages": [{"role": "user", "content": "hi"}] }),
        );
        let mut call = http::HeaderMap::new();
        call.insert(
            HeaderName::from_static("x-sandhi-step-id"),
            HeaderValue::from_static("step-7"),
        );
        call.insert(
            HeaderName::from_static("x-api-key"),
            HeaderValue::from_static("attacker-call"),
        );
        call.insert(
            HeaderName::from_static("anthropic-version"),
            HeaderValue::from_static("1999-01-01"),
        );
        request.extra_headers = call;
        provider.complete(request).await.unwrap();

        // The header matchers above are the positive assertions; this pins the negative one:
        // exactly ONE x-api-key, and it is the transport's.
        let sent = &server.received_requests().await.unwrap()[0];
        assert_eq!(
            sent.headers.get_all("x-api-key").iter().count(),
            1,
            "no second, caller-supplied credential header on the wire"
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
    async fn bearer_auth_never_leaks_into_x_api_key() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .and(header("authorization", "Bearer oauth-test"))
            .and(header("anthropic-version", "2023-06-01"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "content": [{ "type": "text", "text": "hi" }],
                "usage": { "input_tokens": 1, "output_tokens": 1 }
            })))
            .mount(&server)
            .await;

        Anthropic::new(server.uri(), "oauth-test")
            .with_auth_scheme(AnthropicAuthScheme::Bearer)
            .complete(ProviderRequest::new("claude-x", json!({ "messages": [] })))
            .await
            .unwrap();

        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1);
        assert!(!requests[0].headers.contains_key("x-api-key"));
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
