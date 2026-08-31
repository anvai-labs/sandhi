//! OpenAI-compatible adapter — OpenAI proper plus the ~20 providers that speak the Chat
//! Completions wire format (Groq, Together, Fireworks, DeepSeek, Mistral, Qwen, xAI,
//! OpenRouter, vLLM, LM Studio, Ollama, Cerebras…). One adapter, many providers.

use crate::{
    error_for_response, metered_passthrough, sse_data_json, ByteStream, ParsedUsage, Provider,
    ProviderError, ProviderRequest, ProviderResponse,
};
use crate::{parse_openai_usage, validate_openai_chat_messages};
use async_trait::async_trait;
use http::header::{HeaderMap, HeaderName, HeaderValue};
use serde_json::{json, Value};

/// An OpenAI-compatible provider. `base_url` is the API base (e.g. `https://api.openai.com/v1`);
/// the adapter POSTs to `{base_url}/chat/completions` with `Authorization: Bearer <key>`.
pub struct OpenAiCompat {
    client: reqwest::Client,
    slug: String,
    base_url: String,
    api_key: String,
    headers: HeaderMap,
    /// Vendor request-id header from the catalog spec (strategy-via-data): set for
    /// slugs whose id header deviates from the standard (e.g. moonshot).
    request_id_header: Option<&'static str>,
    /// Vendor session-affinity request header from the catalog spec (strategy-via-data):
    /// set for slugs that key KV/prefix-cache reuse per conversation (e.g. inferflux).
    session_header: Option<&'static str>,
}

impl OpenAiCompat {
    pub fn new(
        slug: impl Into<String>,
        base_url: impl Into<String>,
        api_key: impl Into<String>,
    ) -> Self {
        let slug = slug.into();
        let spec = crate::resolve_openai_compat_provider(&slug);
        Self {
            client: crate::default_client(),
            slug,
            base_url: base_url.into(),
            api_key: api_key.into(),
            headers: HeaderMap::new(),
            request_id_header: spec.and_then(|spec| spec.request_id_header),
            session_header: spec.and_then(|spec| spec.session_header),
        }
    }

    /// OpenAI proper (`https://api.openai.com/v1`), slug `openai`.
    pub fn openai(api_key: impl Into<String>) -> Self {
        Self::new("openai", "https://api.openai.com/v1", api_key)
    }

    /// Add caller-supplied provider headers while protecting transport-owned headers.
    /// OpenRouter's `HTTP-Referer` / `X-Title` are the motivating case.
    #[must_use]
    pub fn with_headers(mut self, headers: HeaderMap) -> Self {
        // Single-sourced strip (TD-0022 D2) — also drops a caller-supplied
        // `Accept-Encoding: gzip`, which would corrupt byte metering (reqwest builds without
        // decompression) and family credential headers.
        self.headers = crate::strip_transport_owned(headers);
        self
    }

    fn chat_url(&self) -> String {
        format!("{}/chat/completions", self.base_url.trim_end_matches('/'))
    }

    /// Per-request headers derived from the neutral `ProviderRequest`: the transport's static
    /// headers, overlaid with the call's wire headers (TD-0022 D2 — transport-owned names
    /// stripped by the shared merge), overlaid last with the conversation-affinity key mapped
    /// onto the catalog-declared vendor header (ADR-0008 D3) so the affinity value stays
    /// authoritative. `req.attribution` is deliberately unread: subject/group attribution
    /// rides outside the cached prompt (ADR-0001 §4) and must never reach a provider header
    /// or body. `try_from`, not `from_static`: catalog names are `&'static str` data, and a
    /// future entry with invalid casing must be skipped, not panic the hot path.
    fn request_headers(&self, req: &ProviderRequest) -> HeaderMap {
        let mut out = crate::merge_call_headers(&self.headers, &req.extra_headers);
        let Some(name) = self.session_header else {
            return out;
        };
        // Sanitized, not skipped: a session id carrying a control byte (raw FFI input that
        // bypassed core derivation) must still engage the affinity header — silently
        // dropping it would disable session affinity for the whole conversation.
        let Some(value) = req
            .session_id
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(sandhi_core::sanitize_affinity_value)
        else {
            return out;
        };
        if let (Ok(name), Ok(value)) = (HeaderName::try_from(name), HeaderValue::from_str(&value)) {
            out.insert(name, value);
        }
        out
    }
}

#[async_trait]
impl Provider for OpenAiCompat {
    fn slug(&self) -> &str {
        &self.slug
    }

    async fn complete(&self, req: ProviderRequest) -> Result<ProviderResponse, ProviderError> {
        validate_openai_chat_messages(&req.body)?;
        let headers = self.request_headers(&req);
        let mut body = req.body;
        if let Some(obj) = body.as_object_mut() {
            obj.insert("stream".into(), Value::Bool(false));
        }
        let mut request = self.client.post(self.chat_url()).headers(headers);
        if !self.api_key.is_empty() {
            request = request.bearer_auth(&self.api_key);
        }
        let resp = request
            .json(&body)
            .send()
            .await
            .map_err(|e| ProviderError::Transport(e.to_string()))?;
        let status = resp.status().as_u16();
        if !resp.status().is_success() {
            return Err(error_for_response(resp, self.request_id_header).await);
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
        let headers = self.request_headers(&req);
        let mut body = req.body;
        if let Some(obj) = body.as_object_mut() {
            obj.insert("stream".into(), Value::Bool(true));
            // Ask for usage in the terminal SSE chunk — by *merging*, not replacing. Inserting a
            // fresh object dropped every sibling the client had set (TD-0013 P4); the raw plane
            // has always merged here, and metering must not cost the caller a request field.
            match obj.get_mut("stream_options").and_then(Value::as_object_mut) {
                Some(options) => {
                    options.insert("include_usage".into(), Value::Bool(true));
                }
                None => {
                    obj.insert("stream_options".into(), json!({ "include_usage": true }));
                }
            }
        }
        let mut request = self.client.post(self.chat_url()).headers(headers);
        if !self.api_key.is_empty() {
            request = request.bearer_auth(&self.api_key);
        }
        let resp = request
            .json(&body)
            .send()
            .await
            .map_err(|e| ProviderError::Transport(e.to_string()))?;
        if !resp.status().is_success() {
            return Err(error_for_response(resp, self.request_id_header).await);
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
pub(crate) fn sniff_usage_line(line: &[u8], usage: &mut ParsedUsage) -> bool {
    let Some(v) = sse_data_json(line) else {
        return false;
    };
    if v.get("usage").is_some_and(|u| !u.is_null()) {
        if let Some(u) = parse_openai_usage(&v) {
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
    use futures_util::StreamExt;

    const EXPECTED: ParsedUsage = ParsedUsage {
        tokens_in: 200,
        tokens_out: 250,
        cache_creation_tokens: 0,
        cache_read_tokens: 800,
        reasoning_tokens: 0,
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
    /// meter unperturbed. `completion_tokens_details.reasoning_tokens` graduated from an
    /// ignored-unknown to a parsed field — the fixture's value is now expected, proving both
    /// properties at once.
    #[tokio::test]
    async fn stream_usage_ignores_unknown_fields() {
        let body: &[u8] = include_bytes!("../tests/fixtures/openai/stream_forward_compat.sse");
        assert_eq!(
            crate::accumulate_usage(vec![Bytes::copy_from_slice(body)], sniff_usage_line).await,
            ParsedUsage {
                reasoning_tokens: 33,
                ..EXPECTED
            }
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
        use http::header::{HeaderName, HeaderValue};
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
        headers.insert("authorization", HeaderValue::from_static("Bearer attacker"));
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

    /// TD-0013 P4: asking for usage must not cost the caller a request field.
    ///
    /// The typed plane inserted a fresh `stream_options` object, silently dropping every sibling
    /// the client had set. The raw plane has always merged (see
    /// `normalize_envelope_merges_into_existing_stream_options`), so the two planes disagreed on
    /// the same request — and the typed one lost data.
    #[tokio::test]
    async fn stream_merges_into_the_clients_stream_options() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string("data: [DONE]\n\n"),
            )
            .mount(&server)
            .await;

        let provider = OpenAiCompat::new("openai", server.uri(), "k");
        let request = ProviderRequest::new(
            "m",
            json!({
                "model": "m",
                "messages": [],
                "stream_options": {"show_usage_stats": true}
            }),
        );
        let mut stream = provider.stream(request).await.unwrap();
        while stream.next().await.is_some() {}

        let sent: serde_json::Value =
            serde_json::from_slice(&server.received_requests().await.unwrap()[0].body).unwrap();
        assert_eq!(
            sent["stream_options"]["include_usage"], true,
            "metering still opts in"
        );
        assert_eq!(
            sent["stream_options"]["show_usage_stats"], true,
            "the client's sibling field must survive — the raw plane already preserves it"
        );
    }

    /// Session affinity (ADR-0008 D3): the catalog-declared vendor header carries the neutral
    /// `session_id` — and only that. Attribution set on the request must never reach a header
    /// or the body (ADR-0001 §4).
    #[tokio::test]
    async fn inferflux_session_header_rides_out_of_band() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(header("x-inferflux-session-id", "conv_42"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
            .mount(&server)
            .await;

        let provider = OpenAiCompat::new("inferflux", server.uri(), "local-key");
        let mut request = ProviderRequest::new("llama3-8b", json!({ "messages": [] }));
        request.session_id = Some("conv_42".into());
        request.attribution = crate::Attribution {
            subject_id: Some("alice".into()),
            group_id: Some("platform".into()),
            ..Default::default()
        };
        provider.complete(request).await.unwrap();

        let sent = &server.received_requests().await.unwrap()[0];
        let body: serde_json::Value = serde_json::from_slice(&sent.body).unwrap();
        assert!(
            !sent.headers.contains_key("x-sandhi-subject-id")
                && !sent.headers.contains_key("x-sandhi-group-id"),
            "attribution must stay out-of-band"
        );
        assert!(
            body.get("session_id").is_none()
                && body.get("subject_id").is_none()
                && body.get("group_id").is_none(),
            "the wire body must carry no attribution or session keys"
        );
    }

    /// TD-0022 D2: per-call wire headers overlay the static set and transport-owned names are
    /// stripped from the per-call side. (The affinity header's authority over per-call spoofs
    /// is separate — `request_headers` inserts it after this merge — and is pinned by the
    /// adapter-level session tests above.)
    #[test]
    fn merge_call_headers_overlays_and_strips_transport_owned_names() {
        let mut base = HeaderMap::new();
        base.insert(
            HeaderName::from_static("http-referer"),
            HeaderValue::from_static("https://victor.example"),
        );
        let mut call = HeaderMap::new();
        call.insert(
            HeaderName::from_static("x-sandhi-step-id"),
            HeaderValue::from_static("step-7"),
        );
        // Attacker-controlled overrides must be dropped — the vaulted credential and the
        // framing are not per-call state.
        call.insert("authorization", HeaderValue::from_static("Bearer attacker"));
        call.insert("content-type", HeaderValue::from_static("text/plain"));
        call.insert("host", HeaderValue::from_static("evil.example"));
        // Family credential headers and the Anthropic protocol version are transport-owned
        // too: reqwest appends same-named values added after a header map, so an unstripped
        // x-api-key here would put a second, attacker-supplied credential on the wire.
        call.insert(
            HeaderName::from_static("x-api-key"),
            HeaderValue::from_static("attacker-key"),
        );
        call.insert(
            HeaderName::from_static("x-goog-api-key"),
            HeaderValue::from_static("attacker-key"),
        );
        call.insert(
            HeaderName::from_static("anthropic-version"),
            HeaderValue::from_static("2020-01-01"),
        );

        let merged = crate::merge_call_headers(&base, &call);
        assert_eq!(merged["http-referer"], "https://victor.example");
        assert_eq!(merged["x-sandhi-step-id"], "step-7");
        assert!(!merged.contains_key("authorization"));
        assert!(!merged.contains_key("content-type"));
        assert!(!merged.contains_key("host"));
        assert!(!merged.contains_key("x-api-key"));
        assert!(!merged.contains_key("x-goog-api-key"));
        assert!(!merged.contains_key("anthropic-version"));
    }

    #[test]
    fn strip_transport_owned_removes_every_transport_owned_name() {
        let mut headers = HeaderMap::new();
        for name in ["authorization", "content-type", "host"] {
            headers.insert(name, HeaderValue::from_static("x"));
        }
        headers.insert(
            HeaderName::from_static("x-custom"),
            HeaderValue::from_static("kept"),
        );
        let stripped = crate::strip_transport_owned(headers);
        assert!(stripped.len() == 1 && stripped["x-custom"] == "kept");
    }

    #[tokio::test]
    async fn slugs_without_a_session_header_fact_send_none() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
            .mount(&server)
            .await;

        let provider = OpenAiCompat::new("openai", server.uri(), "k");
        let mut request = ProviderRequest::new("m", json!({ "messages": [] }));
        request.session_id = Some("conv_42".into());
        provider.complete(request).await.unwrap();

        let sent = &server.received_requests().await.unwrap()[0];
        assert!(
            !sent.headers.contains_key("x-inferflux-session-id"),
            "no vendor affinity header without a catalog fact"
        );
    }

    #[tokio::test]
    async fn blank_session_id_sends_no_header() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
            .mount(&server)
            .await;

        let provider = OpenAiCompat::new("inferflux", server.uri(), "local-key");
        let mut request = ProviderRequest::new("llama3-8b", json!({ "messages": [] }));
        request.session_id = Some("   ".into());
        provider.complete(request).await.unwrap();

        let sent = &server.received_requests().await.unwrap()[0];
        assert!(
            !sent.headers.contains_key("x-inferflux-session-id"),
            "a blank session id must not produce an empty header"
        );
    }

    /// TD-0022 D2 + ADR-0008 D3: the affinity header is inserted AFTER the per-call merge,
    /// so a per-call spoof of the vendor affinity name (any FFI `wire_headers_json` caller)
    /// must never win over the authoritative session value.
    #[tokio::test]
    async fn per_call_spoof_cannot_override_the_affinity_header() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(header("x-inferflux-session-id", "conv_42"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
            .mount(&server)
            .await;

        let provider = OpenAiCompat::new("inferflux", server.uri(), "local-key");
        let mut request = ProviderRequest::new("llama3-8b", json!({ "messages": [] }));
        request.session_id = Some("conv_42".into());
        let mut call = http::HeaderMap::new();
        call.insert(
            HeaderName::from_static("x-inferflux-session-id"),
            HeaderValue::from_static("spoofed-session"),
        );
        request.extra_headers = call;
        provider.complete(request).await.unwrap();

        let sent = &server.received_requests().await.unwrap()[0];
        assert_eq!(
            sent.headers
                .get("x-inferflux-session-id")
                .and_then(|value| value.to_str().ok()),
            Some("conv_42"),
            "the session-derived affinity value must survive per-call spoofs"
        );
        assert_eq!(
            sent.headers
                .get_all("x-inferflux-session-id")
                .iter()
                .count(),
            1,
            "exactly one affinity header, no spoofed duplicate"
        );
    }

    /// A session id carrying a control byte (raw FFI input that bypassed core derivation)
    /// still engages the affinity header: the value is sanitized, never silently dropped.
    #[tokio::test]
    async fn control_bytes_in_session_id_still_send_the_affinity_header() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(header("x-inferflux-session-id", "acct-42"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
            .mount(&server)
            .await;

        let provider = OpenAiCompat::new("inferflux", server.uri(), "local-key");
        let mut request = ProviderRequest::new("llama3-8b", json!({ "messages": [] }));
        request.session_id = Some("acct\n42".into());
        provider.complete(request).await.unwrap();

        let sent = &server.received_requests().await.unwrap()[0];
        assert_eq!(
            sent.headers
                .get("x-inferflux-session-id")
                .and_then(|value| value.to_str().ok()),
            Some("acct-42"),
            "control bytes are mapped to '-', not used to silently disable affinity"
        );
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
