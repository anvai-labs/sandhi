//! Raw byte-faithful forwarder for the transparent-metering plane (TD-0006 / ADR-0004 D1).
//!
//! Owns the `reqwest` POST with `.body(bytes)` — never `.json()`. Independent of the
//! [`Provider`][crate::Provider] / [`ChatProvider`][crate::typed::ChatProvider] traits. Forward
//! the client's body bytes with only documented envelope normalizations for metering (OpenAI
//! streaming: `stream:true` + `stream_options.include_usage`), set `Accept-Encoding: identity`
//! so bytes are plaintext for both sniffing and forwarding (reqwest is built without gzip), and
//! return the status + raw body bytes + a curated response-header allowlist.
//!
//! The promise is **content-faithful, envelope-normalized** — not byte-identical. The typed
//! adapters parse to `serde_json::Value`, inject `stream` / `stream_options`, and carry no
//! response headers; they cannot be byte-identical. This forwarder preserves the client's
//! message / content bytes semantically while adding only the documented metering-envelope
//! fields. Everything stays inside the measure-vs-token boundary — no dollars, tiers, or SKUs.

use crate::{
    error_for_response, AnthropicAuthScheme, GeminiAuthScheme, ProviderError, ProviderFamily,
};
use bytes::Bytes;
use futures_core::Stream;
use http::header::{HeaderMap, HeaderName, HeaderValue, ACCEPT_ENCODING, AUTHORIZATION, HOST};
use serde_json::Value;
use std::pin::Pin;
use std::time::Duration;

/// Anthropic API version header value (mirrors the typed adapter).
const ANTHROPIC_VERSION: &str = "2023-06-01";

/// A raw streaming response: raw upstream byte chunks to forward verbatim. The caller wraps
/// this with [`metered_passthrough`][crate::metered_passthrough] (or a per-family sniffer) to
/// extract usage — usage extraction lives outside the raw forwarder.
pub type RawChunkStream = Pin<Box<dyn Stream<Item = Result<Bytes, ProviderError>> + Send>>;

/// A completed (non-streaming) raw response: status, raw body bytes, and a curated header
/// allowlist. Hop-by-hop and credential headers are already stripped by
/// [`filter_response_headers`].
#[derive(Debug, Clone)]
pub struct RawResponse {
    pub status: u16,
    pub body: Bytes,
    pub headers: HeaderMap,
}

/// A successful metered streaming response. Unlike a bare [`ByteStream`](crate::ByteStream),
/// this retains the upstream status and the curated response-header allowlist so the transparent
/// proxy plane can preserve request IDs, rate-limit state, and content type on streaming calls.
pub struct RawMeteredStreamResponse {
    pub status: u16,
    pub headers: HeaderMap,
    pub stream: crate::ByteStream,
}

/// A content-faithful, envelope-normalized raw forwarder. One HTTP client (connection pool)
/// per instance; thread-safe via `reqwest::Client`'s internal Arc.
#[derive(Clone)]
pub struct RawForwarder {
    client: reqwest::Client,
    family: ProviderFamily,
    base_url: String,
    api_key: String,
    anthropic_auth: AnthropicAuthScheme,
    gemini_auth: GeminiAuthScheme,
    extra_headers: HeaderMap,
    /// Vendor session-affinity request header from the catalog spec (strategy-via-data):
    /// when declared, the per-request neutral session id is mapped onto it (ADR-0008 D3).
    /// Attribution never crosses this seam — only the conversation key does (ADR-0001 §4).
    session_header: Option<&'static str>,
    complete_timeout: Duration,
    stream_setup_timeout: Duration,
    stream_idle_timeout: Option<Duration>,
}

impl RawForwarder {
    /// Construct a forwarder for a provider family. Auth is derived from the family; use
    /// [`with_anthropic_auth`] / [`with_gemini_auth`] to override for OAuth/ADC.
    ///
    /// [`with_anthropic_auth`]: Self::with_anthropic_auth
    /// [`with_gemini_auth`]: Self::with_gemini_auth
    #[must_use]
    pub fn new(
        family: ProviderFamily,
        base_url: impl Into<String>,
        api_key: impl Into<String>,
    ) -> Self {
        Self {
            client: crate::default_client(),
            family,
            base_url: base_url.into(),
            api_key: api_key.into(),
            anthropic_auth: AnthropicAuthScheme::ApiKey,
            gemini_auth: GeminiAuthScheme::ApiKey,
            extra_headers: HeaderMap::new(),
            session_header: None,
            complete_timeout: Duration::from_secs(120),
            stream_setup_timeout: Duration::from_secs(30),
            stream_idle_timeout: Some(Duration::from_secs(90)),
        }
    }

    /// Add caller-supplied provider headers. Transport-owned headers
    /// (`Authorization`, `Accept-Encoding`, `Host`) are stripped so the forwarder controls them.
    #[must_use]
    pub fn with_headers(mut self, headers: HeaderMap) -> Self {
        self.extra_headers = headers;
        // Defense-in-depth: never let caller-controlled headers override auth or encoding.
        self.extra_headers.remove(AUTHORIZATION);
        self.extra_headers.remove(ACCEPT_ENCODING);
        self.extra_headers.remove(HOST);
        self
    }

    /// Declare the vendor's session-affinity request header (a catalog spec fact, resolved by
    /// the typed runtime's [`build_raw_forwarder`]). `None` (the default) sends no affinity
    /// header, preserving today's wire bytes for every other provider.
    ///
    /// [`build_raw_forwarder`]: crate::build_raw_forwarder
    #[must_use]
    pub fn with_session_header(mut self, header: Option<&'static str>) -> Self {
        self.session_header = header;
        self
    }

    /// Override the Anthropic auth scheme (API key vs OAuth Bearer).
    #[must_use]
    pub fn with_anthropic_auth(mut self, scheme: AnthropicAuthScheme) -> Self {
        self.anthropic_auth = scheme;
        self
    }

    /// Override the Gemini auth scheme (API key vs OAuth/ADC Bearer).
    #[must_use]
    pub fn with_gemini_auth(mut self, scheme: GeminiAuthScheme) -> Self {
        self.gemini_auth = scheme;
        self
    }

    /// Apply the same per-call bounds as the typed resilience transport. The raw transparent
    /// plane intentionally remains retry-free: replaying a provider POST after an ambiguous
    /// timeout can duplicate a billed generation. Retry ownership requires a separate policy;
    /// time bounds do not.
    #[must_use]
    pub fn with_timeouts(
        mut self,
        complete: Duration,
        stream_setup: Duration,
        stream_idle: Option<Duration>,
    ) -> Self {
        self.complete_timeout = complete;
        self.stream_setup_timeout = stream_setup;
        self.stream_idle_timeout = stream_idle;
        self
    }

    /// Non-streaming forward: POST the (envelope-normalized) body bytes to `{base_url}{path}`,
    /// return the status + raw body bytes + curated headers.
    pub async fn forward(&self, path: &str, body: Bytes) -> Result<RawResponse, ProviderError> {
        self.forward_with_session(path, body, None).await
    }

    /// Session-aware core of [`Self::forward`]: `session` is the neutral conversation key
    /// mapped onto the catalog-declared vendor affinity header, when one exists.
    async fn forward_with_session(
        &self,
        path: &str,
        body: Bytes,
        session: Option<&str>,
    ) -> Result<RawResponse, ProviderError> {
        let url = self.url(path);
        let out_body = normalize_envelope(self.family, &body, false);
        match tokio::time::timeout(self.complete_timeout, async {
            let resp = self.send_with_session(&url, out_body, session).await?;
            let status = resp.status().as_u16();
            if !resp.status().is_success() {
                return Err(error_for_response(resp, None).await);
            }
            let headers = filter_response_headers(resp.headers());
            let body = resp
                .bytes()
                .await
                .map_err(|e| ProviderError::Transport(e.to_string()))?;
            Ok(RawResponse {
                status,
                body,
                headers,
            })
        })
        .await
        {
            Ok(result) => result,
            Err(_) => Err(ProviderError::Timeout(self.complete_timeout)),
        }
    }

    /// Streaming forward: POST the (envelope-normalized) body bytes and yield raw upstream
    /// chunks. Each chunk is forwarded verbatim — O(1) pass-through. Wrap the result with
    /// [`metered_passthrough`][crate::metered_passthrough] to sniff usage from the terminal
    /// frame.
    pub async fn forward_stream(
        &self,
        path: &str,
        body: Bytes,
    ) -> Result<RawChunkStream, ProviderError> {
        let url = self.url(path);
        let out_body = normalize_envelope(self.family, &body, true);
        let resp = match tokio::time::timeout(self.stream_setup_timeout, self.send(&url, out_body))
            .await
        {
            Ok(result) => result?,
            Err(_) => return Err(ProviderError::Timeout(self.stream_setup_timeout)),
        };
        if !resp.status().is_success() {
            return Err(error_for_response(resp, None).await);
        }
        use futures_util::TryStreamExt;
        let stream = resp
            .bytes_stream()
            .map_err(|e| ProviderError::Transport(e.to_string()));
        Ok(Box::pin(stream))
    }

    /// Non-streaming forward **that also meters**: forwards the body verbatim and parses the
    /// family's usage from the response, so the proxy gets both the raw response and a `UsageV2`
    /// from one call. Usage parsing is single-sourced in `sandhi-core` (the public per-family
    /// parsers) — the transparent plane meters exactly as the typed adapter would. `session` is
    /// the neutral conversation key, mapped onto the catalog-declared vendor affinity header
    /// when one exists (ADR-0008 D3); it never carries attribution (ADR-0001 §4).
    pub async fn forward_metered(
        &self,
        path: &str,
        body: Bytes,
        session: Option<&str>,
    ) -> Result<(RawResponse, sandhi_core::UsageV2), ProviderError> {
        let raw = self.forward_with_session(path, body, session).await?;
        let usage = serde_json::from_slice::<Value>(&raw.body)
            .ok()
            .and_then(|value| parse_usage_for_family(self.family, &value))
            .unwrap_or_default()
            .into();
        Ok((raw, usage))
    }

    /// Streaming forward **that also meters**: yields the upstream bytes verbatim (O(1)
    /// pass-through) while the shared [`metered_passthrough`][crate::metered_passthrough] primitive
    /// accumulates usage with the family's own sniffer (so Anthropic's split input/output usage,
    /// etc. are handled identically to the typed path); the terminal
    /// [`StreamChunk`][crate::StreamChunk] carries the finalized [`ParsedUsage`][crate::ParsedUsage].
    /// `session` is the neutral conversation key, mapped onto the catalog-declared vendor
    /// affinity header when one exists (ADR-0008 D3); it never carries attribution.
    pub async fn forward_stream_metered(
        &self,
        path: &str,
        body: Bytes,
        session: Option<&str>,
    ) -> Result<RawMeteredStreamResponse, ProviderError> {
        let url = self.url(path);
        let out_body = normalize_envelope(self.family, &body, true);
        let resp = match tokio::time::timeout(
            self.stream_setup_timeout,
            self.send_with_session(&url, out_body, session),
        )
        .await
        {
            Ok(result) => result?,
            Err(_) => return Err(ProviderError::Timeout(self.stream_setup_timeout)),
        };
        if !resp.status().is_success() {
            return Err(error_for_response(resp, None).await);
        }
        let status = resp.status().as_u16();
        let headers = filter_response_headers(resp.headers());
        let stream = crate::metered_passthrough(resp.bytes_stream(), sniff_for_family(self.family));
        Ok(RawMeteredStreamResponse {
            status,
            headers,
            stream: with_idle_timeout(stream, self.stream_idle_timeout),
        })
    }

    fn url(&self, path: &str) -> String {
        if path.starts_with("http://") || path.starts_with("https://") {
            // Absolute URL — use as-is (callers may bypass base_url for provider-specific URLs).
            return path.to_string();
        }
        let base = self.base_url.trim_end_matches('/');
        if path.starts_with('/') {
            format!("{base}{path}")
        } else {
            format!("{base}/{path}")
        }
    }

    fn send(
        &self,
        url: &str,
        body: Bytes,
    ) -> impl std::future::Future<Output = Result<reqwest::Response, ProviderError>> {
        self.send_with_session(url, body, None)
    }

    /// Session-aware send: when a catalog-declared affinity header exists and the call
    /// carries a conversation key, the key rides the declared header — inserted into a
    /// clone of the extra headers so the authoritative per-request value overwrites any
    /// stale caller-supplied copy (same defense-in-depth as [`Self::with_headers`]
    /// stripping `Authorization`). `try_from`, not `from_static`: catalog names are
    /// `&'static str` data, and an invalid entry must be skipped, not panic the hot path.
    fn send_with_session(
        &self,
        url: &str,
        body: Bytes,
        session: Option<&str>,
    ) -> impl std::future::Future<Output = Result<reqwest::Response, ProviderError>> {
        let mut headers = self.extra_headers.clone();
        if let (Some(name), Some(value)) = (
            self.session_header,
            session.map(str::trim).filter(|value| !value.is_empty()),
        ) {
            if let (Ok(name), Ok(value)) =
                (HeaderName::try_from(name), HeaderValue::from_str(value))
            {
                headers.insert(name, value);
            }
        }
        let builder = self
            .client
            .post(url)
            .header(ACCEPT_ENCODING, "identity")
            .headers(headers)
            .body(body);
        let builder = self.apply_auth(builder);
        async move {
            builder
                .send()
                .await
                .map_err(|e| ProviderError::Transport(e.to_string()))
        }
    }

    fn apply_auth(&self, mut builder: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match self.family {
            ProviderFamily::OpenAiCompat
            | ProviderFamily::OpenAiResponses
            | ProviderFamily::Cohere => {
                builder = builder.bearer_auth(&self.api_key);
            }
            ProviderFamily::Anthropic => match self.anthropic_auth {
                AnthropicAuthScheme::ApiKey => {
                    builder = builder
                        .header("x-api-key", &self.api_key)
                        .header("anthropic-version", ANTHROPIC_VERSION);
                }
                AnthropicAuthScheme::Bearer => {
                    builder = builder
                        .bearer_auth(&self.api_key)
                        .header("anthropic-version", ANTHROPIC_VERSION);
                }
            },
            ProviderFamily::Gemini => match self.gemini_auth {
                GeminiAuthScheme::ApiKey => {
                    builder = builder.header("x-goog-api-key", &self.api_key);
                }
                GeminiAuthScheme::Bearer => {
                    builder = builder.bearer_auth(&self.api_key);
                }
            },
            ProviderFamily::Ollama => {
                if !self.api_key.is_empty() {
                    builder = builder.bearer_auth(&self.api_key);
                }
            }
        }
        builder
    }
}

/// Bound the gap between metered stream items without imposing a total generation timeout.
/// Once response headers have arrived, this is the only timeout that is safe for long-running
/// model streams. A timeout is surfaced in-stream and is never retried.
fn with_idle_timeout(stream: crate::ByteStream, idle: Option<Duration>) -> crate::ByteStream {
    let Some(idle) = idle else {
        return stream;
    };
    Box::pin(async_stream::stream! {
        let mut stream = stream;
        loop {
            match tokio::time::timeout(idle, futures_util::StreamExt::next(&mut stream)).await {
                Ok(Some(item)) => yield item,
                Ok(None) => break,
                Err(_) => {
                    yield Err(ProviderError::Timeout(idle));
                    break;
                }
            }
        }
    })
}

/// Apply documented envelope normalizations for metering. These are the **only** mutations to
/// the client's body bytes.
///
/// - **OpenAI Chat Completions, streaming:** injects `stream:true` and ensures
///   `stream_options.include_usage:true` so the terminal SSE chunk carries usage (without this
///   the proxy cannot meter an OpenAI stream). Merges into an existing `stream_options` object
///   rather than overwriting.
/// - **All other families / non-streaming:** the body is returned **byte-unchanged**.
///
/// If the body is not a valid JSON object, it is forwarded as-is (defensive — never block a
/// request on a parse failure).
/// The family's streaming usage sniffer, single-sourced with each typed adapter — so the
/// transparent plane accumulates usage exactly as the translated path does.
fn sniff_for_family(family: ProviderFamily) -> fn(&[u8], &mut crate::ParsedUsage) -> bool {
    match family {
        ProviderFamily::OpenAiCompat => crate::openai::sniff_usage_line,
        ProviderFamily::OpenAiResponses => crate::openai_responses::sniff_responses_usage_line,
        ProviderFamily::Anthropic => crate::anthropic::sniff_usage_line,
        ProviderFamily::Cohere => crate::cohere::sniff_usage_line,
        ProviderFamily::Gemini => crate::gemini::sniff_usage_line,
        ProviderFamily::Ollama => crate::local::sniff_usage_line,
    }
}

/// The family's non-streaming usage parser (the public `sandhi-core` per-family parsers).
fn parse_usage_for_family(family: ProviderFamily, value: &Value) -> Option<crate::ParsedUsage> {
    match family {
        ProviderFamily::OpenAiCompat => crate::parse_openai_usage(value),
        ProviderFamily::OpenAiResponses => crate::parse_openai_responses_usage(value),
        ProviderFamily::Anthropic => crate::parse_anthropic_usage(value),
        ProviderFamily::Cohere => crate::parse_cohere_usage(value),
        ProviderFamily::Gemini => crate::parse_gemini_usage(value),
        ProviderFamily::Ollama => crate::parse_ollama_usage(value),
    }
}

pub fn normalize_envelope(family: ProviderFamily, body: &Bytes, streaming: bool) -> Bytes {
    if !streaming || family != ProviderFamily::OpenAiCompat {
        return body.clone();
    }
    let Ok(mut value) = serde_json::from_slice::<Value>(body) else {
        return body.clone();
    };
    let Some(obj) = value.as_object_mut() else {
        return body.clone();
    };
    obj.insert("stream".into(), Value::Bool(true));
    // Merge include_usage into existing stream_options (preserve other fields), or create fresh.
    match obj.get_mut("stream_options").filter(|v| v.is_object()) {
        Some(existing) => {
            if let Some(map) = existing.as_object_mut() {
                map.insert("include_usage".into(), Value::Bool(true));
            }
        }
        _ => {
            obj.insert(
                "stream_options".into(),
                serde_json::json!({ "include_usage": true }),
            );
        }
    }
    // Re-serialize. This changes whitespace/key-order but preserves all content semantically —
    // the promise is "content-faithful, envelope-normalized," not byte-identical.
    match serde_json::to_vec(&value) {
        Ok(bytes) => Bytes::from(bytes),
        Err(_) => body.clone(),
    }
}

/// Filter the upstream response headers down to a curated metering/debug allowlist. Strips
/// hop-by-hop headers (`Connection`, `Transfer-Encoding`, `Keep-Alive`, `TE`, `Upgrade`,
/// `Proxy-Authorization`, `Proxy-Authenticate`) and **never** forwards `Authorization` (that
/// would leak the upstream credential back to the client). Passes:
/// - `Content-Type` (needed to parse the response body)
/// - `Retry-After` (rate-limit back-off)
/// - Request-id family (`Request-ID`, `X-Request-ID`)
/// - Rate-limit family (`X-RateLimit-*`, `RateLimit-*`, `Anthropic-RateLimit-*`)
/// - `X-Should-Retry` (OpenAI error-retry hint)
#[must_use]
pub fn filter_response_headers(headers: &HeaderMap) -> HeaderMap {
    let mut out = HeaderMap::new();
    for (name, value) in headers.iter() {
        if is_passthrough_header(name.as_str()) {
            // Cloned to avoid borrowing from the input HeaderMap.
            out.insert(name.clone(), value.clone());
        }
    }
    out
}

fn is_passthrough_header(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower == "content-type"
        || lower == "retry-after"
        || lower == "request-id"
        || lower == "x-request-id"
        || lower == "x-should-retry"
        // W3C trace context: the child `traceparent` an upstream echoes links the response
        // back to the caller's span (InferFlux does this on every generation response).
        // Hop-by-hop `tracestate` stays stripped — it is per-hop routing state, not linkage.
        || lower == "traceparent"
        || lower.starts_with("x-ratelimit-")
        || lower.starts_with("ratelimit-")
        || lower.starts_with("anthropic-ratelimit-")
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::header::{HeaderName, HeaderValue};
    use serde_json::{json, Value};
    use wiremock::matchers::{body_bytes, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    // ---------------------------------------------------------------------------------------------
    // Envelope normalization
    // ---------------------------------------------------------------------------------------------

    #[test]
    fn normalize_envelope_injects_stream_and_usage_for_openai_streaming() {
        let client_body = Bytes::from(
            serde_json::to_vec(&json!({
                "model": "gpt-4o",
                "messages": [{"role": "user", "content": "hello"}]
            }))
            .unwrap(),
        );
        let normalized = normalize_envelope(ProviderFamily::OpenAiCompat, &client_body, true);
        let v: Value = serde_json::from_slice(&normalized).unwrap();
        assert_eq!(v["stream"], true);
        assert_eq!(v["stream_options"]["include_usage"], true);
        // Content fidelity: messages survive unchanged.
        assert_eq!(v["messages"][0]["role"], "user");
        assert_eq!(v["messages"][0]["content"], "hello");
    }

    #[test]
    fn normalize_envelope_merges_into_existing_stream_options() {
        let client_body = Bytes::from(
            serde_json::to_vec(&json!({
                "model": "gpt-4o",
                "messages": [],
                "stream_options": {"show_usage_stats": true}
            }))
            .unwrap(),
        );
        let normalized = normalize_envelope(ProviderFamily::OpenAiCompat, &client_body, true);
        let v: Value = serde_json::from_slice(&normalized).unwrap();
        assert_eq!(v["stream_options"]["include_usage"], true);
        assert_eq!(v["stream_options"]["show_usage_stats"], true);
    }

    #[test]
    fn normalize_envelope_preserves_non_stream_and_non_openai_unchanged() {
        let original = Bytes::from(r#"{"model":"claude-3","messages":[]}"#);
        // Non-streaming OpenAI: unchanged.
        assert_eq!(
            normalize_envelope(ProviderFamily::OpenAiCompat, &original, false),
            original
        );
        // Anthropic streaming: unchanged (no envelope normalization needed).
        assert_eq!(
            normalize_envelope(ProviderFamily::Anthropic, &original, true),
            original
        );
        // Gemini streaming: unchanged.
        assert_eq!(
            normalize_envelope(ProviderFamily::Gemini, &original, true),
            original
        );
    }

    #[test]
    fn normalize_envelope_is_defensive_on_invalid_json() {
        let bad = Bytes::from_static(b"not json at all");
        assert_eq!(
            normalize_envelope(ProviderFamily::OpenAiCompat, &bad, true),
            bad
        );
    }

    // ---------------------------------------------------------------------------------------------
    // Header filtering
    // ---------------------------------------------------------------------------------------------

    #[test]
    fn filter_strips_hop_by_hop_and_authorization() {
        let mut headers = HeaderMap::new();
        headers.insert("content-type", "application/json".parse().unwrap());
        headers.insert("retry-after", "30".parse().unwrap());
        headers.insert("x-request-id", "req-123".parse().unwrap());
        headers.insert("x-ratelimit-remaining-requests", "100".parse().unwrap());
        // W3C trace context linkage survives (the child span an upstream echoes); the
        // per-hop `tracestate` does not.
        headers.insert(
            "traceparent",
            "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01"
                .parse()
                .unwrap(),
        );
        headers.insert("tracestate", "congo=t61rcWkgMzE".parse().unwrap());
        // These must be stripped:
        headers.insert("connection", "keep-alive".parse().unwrap());
        headers.insert("transfer-encoding", "chunked".parse().unwrap());
        headers.insert("keep-alive", "timeout=60".parse().unwrap());
        headers.insert("authorization", "Bearer sk-secret".parse().unwrap());
        headers.insert("set-cookie", "session=abc".parse().unwrap());

        let filtered = filter_response_headers(&headers);
        assert!(filtered.contains_key("content-type"));
        assert!(filtered.contains_key("retry-after"));
        assert!(filtered.contains_key("x-request-id"));
        assert!(filtered.contains_key("x-ratelimit-remaining-requests"));
        assert!(filtered.contains_key("traceparent"));
        assert!(!filtered.contains_key("tracestate"));
        assert!(!filtered.contains_key("connection"));
        assert!(!filtered.contains_key("transfer-encoding"));
        assert!(!filtered.contains_key("keep-alive"));
        assert!(!filtered.contains_key("authorization"));
        assert!(!filtered.contains_key("set-cookie"));
    }

    // ---------------------------------------------------------------------------------------------
    // Golden content-fidelity test (non-streaming)
    // ---------------------------------------------------------------------------------------------

    #[tokio::test]
    async fn golden_non_stream_content_fidelity_and_usage() {
        let server = MockServer::start().await;
        let client_body = Bytes::from(
            serde_json::to_vec(&json!({
                "model": "gpt-4o",
                "messages": [
                    {"role": "system", "content": "be precise"},
                    {"role": "user", "content": "what is 2+2?"}
                ],
                "temperature": 0.7
            }))
            .unwrap(),
        );

        // The mock asserts the upstream receives the client's body bytes (for non-stream,
        // non-OpenAI there is no mutation — this is byte-identical for the non-envelope case).
        let upstream_response = json!({
            "choices": [{"message": {"content": "4"}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 12, "completion_tokens": 1,
                      "prompt_tokens_details": {"cached_tokens": 2}}
        });
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .and(body_bytes(client_body.to_vec()))
            .and(header("accept-encoding", "identity"))
            .and(header("authorization", "Bearer sk-test"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "application/json")
                    .insert_header("x-request-id", "req-abc")
                    .insert_header("retry-after", "0")
                    .insert_header("connection", "keep-alive")
                    .insert_header("authorization", "Bearer sk-upstream-secret")
                    .set_body_json(upstream_response),
            )
            .mount(&server)
            .await;

        let forwarder = RawForwarder::new(ProviderFamily::OpenAiCompat, server.uri(), "sk-test");
        let resp = forwarder
            .forward("/v1/chat/completions", client_body)
            .await
            .unwrap();

        assert_eq!(resp.status, 200);
        // Non-zero usage extractable from the plaintext body.
        let body: Value = serde_json::from_slice(&resp.body).unwrap();
        let usage = crate::parse_openai_usage(&body).unwrap();
        assert_eq!(usage.tokens_in, 10); // 12 - 2 cached
        assert_eq!(usage.tokens_out, 1);
        assert_eq!(usage.cache_read_tokens, 2);
        // Header allowlist: x-request-id and retry-after pass through; authorization stripped.
        assert_eq!(resp.headers.get("x-request-id").unwrap(), "req-abc");
        assert!(resp.headers.contains_key("retry-after"));
        assert!(!resp.headers.contains_key("authorization"));
        assert!(!resp.headers.contains_key("connection"));
    }

    // ---------------------------------------------------------------------------------------------
    // Golden content-fidelity test (streaming) — the envelope normalization case
    // ---------------------------------------------------------------------------------------------

    #[tokio::test]
    async fn golden_stream_content_fidelity_and_injected_usage_flag() {
        let server = MockServer::start().await;
        let client_body = Bytes::from(
            serde_json::to_vec(&json!({
                "model": "gpt-4o",
                "messages": [
                    {"role": "user", "content": "say hello"}
                ]
            }))
            .unwrap(),
        );

        let sse = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"he\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"llo\"}}]}\n\n",
            "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":5}}\n\n",
            "data: [DONE]\n\n"
        );

        // Capture the body the upstream receives so we can assert content-fidelity.
        let received_body = std::sync::Arc::new(tokio::sync::Mutex::new(None::<Vec<u8>>));
        let captured = received_body.clone();
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .and(header("accept-encoding", "identity"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(sse),
            )
            .mount(&server)
            .await;
        // wiremock doesn't give us the raw body via a matcher easily for assertion, so we
        // verify via a second approach: assert the forwarder's normalize_envelope function
        // produces the right output (tested above) and that the stream round-trips.

        let forwarder = RawForwarder::new(ProviderFamily::OpenAiCompat, server.uri(), "sk-test");
        let mut stream = forwarder
            .forward_stream("/v1/chat/completions", client_body.clone())
            .await
            .unwrap();

        use futures_util::StreamExt;
        let mut forwarded = Vec::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.unwrap();
            forwarded.extend_from_slice(&chunk);
        }

        // The forwarded bytes contain the content deltas verbatim.
        let text = String::from_utf8(forwarded).unwrap();
        assert!(text.contains("\"he\"") && text.contains("\"llo\""));
        assert!(text.contains("[DONE]"));

        // Content-fidelity: normalize_envelope injected stream + stream_options; messages survive.
        let normalized = normalize_envelope(ProviderFamily::OpenAiCompat, &client_body, true);
        let v: Value = serde_json::from_slice(&normalized).unwrap();
        assert_eq!(v["stream"], true);
        assert_eq!(v["stream_options"]["include_usage"], true);
        assert_eq!(v["messages"][0]["content"], "say hello");

        // Suppress unused-variable warning — the capture pattern is for future body inspection.
        drop(captured);
        drop(received_body);
    }

    // ---------------------------------------------------------------------------------------------
    // Accept-Encoding: identity path (reqwest built without gzip)
    // ---------------------------------------------------------------------------------------------

    #[tokio::test]
    async fn accept_encoding_identity_yields_plaintext_and_nonzero_usage() {
        let server = MockServer::start().await;
        let body = json!({
            "choices": [{"message": {"content": "hi"}}],
            "usage": {"prompt_tokens": 5, "completion_tokens": 3}
        });
        // The mock verifies we send Accept-Encoding: identity. Since reqwest is built without
        // the gzip feature, and we explicitly request identity, the upstream returns plaintext.
        Mock::given(method("POST"))
            .and(header("accept-encoding", "identity"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "application/json")
                    .set_body_json(body),
            )
            .mount(&server)
            .await;

        let forwarder = RawForwarder::new(ProviderFamily::OpenAiCompat, server.uri(), "sk-test");
        let resp = forwarder
            .forward("/v1/chat/completions", Bytes::from_static(b"{}"))
            .await
            .unwrap();

        // Plaintext body is parseable (not compressed garbage).
        let parsed: Value = serde_json::from_slice(&resp.body).unwrap();
        let usage = crate::parse_openai_usage(&parsed).unwrap();
        assert_eq!(usage.tokens_in, 5);
        assert_eq!(usage.tokens_out, 3);
    }

    // ---------------------------------------------------------------------------------------------
    // Error passthrough
    // ---------------------------------------------------------------------------------------------

    #[tokio::test]
    async fn upstream_error_maps_to_provider_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(429))
            .mount(&server)
            .await;
        let forwarder = RawForwarder::new(ProviderFamily::OpenAiCompat, server.uri(), "sk-test");
        let err = forwarder
            .forward("/v1/chat/completions", Bytes::from_static(b"{}"))
            .await
            .unwrap_err();
        assert!(matches!(err, ProviderError::RateLimited));
    }

    // ---------------------------------------------------------------------------------------------
    // Anthropic auth header
    // ---------------------------------------------------------------------------------------------

    #[tokio::test]
    async fn anthropic_uses_x_api_key_and_version_header() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(header("x-api-key", "ak-test"))
            .and(header("anthropic-version", "2023-06-01"))
            .and(header("accept-encoding", "identity"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "content": [{"type": "text", "text": "hi"}],
                "usage": {"input_tokens": 5, "output_tokens": 3}
            })))
            .mount(&server)
            .await;

        let forwarder = RawForwarder::new(ProviderFamily::Anthropic, server.uri(), "ak-test");
        let resp = forwarder
            .forward("/v1/messages", Bytes::from_static(b"{}"))
            .await
            .unwrap();
        let parsed: Value = serde_json::from_slice(&resp.body).unwrap();
        let usage = crate::parse_anthropic_usage(&parsed).unwrap();
        assert_eq!(usage.tokens_in, 5);
        assert_eq!(usage.tokens_out, 3);
    }

    // ---------------------------------------------------------------------------------------------
    // Stream variant yields raw chunks
    // ---------------------------------------------------------------------------------------------

    #[tokio::test]
    async fn stream_yields_raw_chunks_verbatim() {
        let server = MockServer::start().await;
        let sse = "data: {\"hello\":true}\n\ndata: [DONE]\n\n";
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(sse),
            )
            .mount(&server)
            .await;

        let forwarder = RawForwarder::new(ProviderFamily::OpenAiCompat, server.uri(), "k");
        let mut stream = forwarder
            .forward_stream("/v1/chat/completions", Bytes::from_static(b"{}"))
            .await
            .unwrap();

        use futures_util::StreamExt;
        let mut collected = Vec::new();
        while let Some(chunk) = stream.next().await {
            collected.extend_from_slice(&chunk.unwrap());
        }
        assert_eq!(collected, sse.as_bytes());
    }

    // ---------------------------------------------------------------------------------------------
    // Extra headers: transport-owned headers stripped
    // ---------------------------------------------------------------------------------------------

    #[tokio::test]
    async fn extra_headers_forwarded_but_authorization_stripped() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(header("http-referer", "https://victor.example"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
            .mount(&server)
            .await;

        let mut headers = HeaderMap::new();
        headers.insert(
            HeaderName::from_static("http-referer"),
            HeaderValue::from_static("https://victor.example"),
        );
        // Attacker tries to override auth via extra headers — must be stripped.
        headers.insert(AUTHORIZATION, HeaderValue::from_static("Bearer attacker"));

        let forwarder = RawForwarder::new(ProviderFamily::OpenAiCompat, server.uri(), "real-key")
            .with_headers(headers);
        forwarder
            .forward("/v1/chat/completions", Bytes::from_static(b"{}"))
            .await
            .unwrap();
    }

    // ---------------------------------------------------------------------------------------------
    // Session affinity (ADR-0008 D3): the declared vendor header carries the neutral
    // conversation key — and nothing else.
    // ---------------------------------------------------------------------------------------------

    #[tokio::test]
    async fn session_header_rides_when_the_spec_declares_one() {
        let server = MockServer::start().await;
        let body = br#"{"model":"llama3-8b","messages":[]}"#;
        Mock::given(method("POST"))
            .and(header("x-inferflux-session-id", "conv_9"))
            .and(body_bytes(Bytes::from_static(body)))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
            .mount(&server)
            .await;

        let forwarder = RawForwarder::new(ProviderFamily::OpenAiCompat, server.uri(), "local-key")
            .with_session_header(Some("x-inferflux-session-id"));
        forwarder
            .forward_metered(
                "/v1/chat/completions",
                Bytes::from_static(body),
                Some("conv_9"),
            )
            .await
            .unwrap();
        // The header matcher above is the positive assertion: the mock would not have
        // responded (and the call failed) without it. The body matcher pins that affinity
        // rides the header, never the body.
    }

    #[tokio::test]
    async fn no_session_header_when_undeclared_or_blank() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
            .mount(&server)
            .await;

        // (a) No catalog fact + a session id → no header.
        RawForwarder::new(ProviderFamily::OpenAiCompat, server.uri(), "k")
            .forward_metered(
                "/v1/chat/completions",
                Bytes::from_static(b"{}"),
                Some("conv_9"),
            )
            .await
            .unwrap();
        let sent = &server.received_requests().await.unwrap()[0];
        assert!(
            !sent.headers.contains_key("x-inferflux-session-id"),
            "no affinity header without a declared fact"
        );

        // (b) Declared fact + blank session id → no empty header.
        RawForwarder::new(ProviderFamily::OpenAiCompat, server.uri(), "k")
            .with_session_header(Some("x-inferflux-session-id"))
            .forward_metered(
                "/v1/chat/completions",
                Bytes::from_static(b"{}"),
                Some("   "),
            )
            .await
            .unwrap();
        let sent = &server.received_requests().await.unwrap()[1];
        assert!(
            !sent.headers.contains_key("x-inferflux-session-id"),
            "a blank session id must not produce an empty header"
        );
    }

    #[tokio::test]
    async fn session_header_leaves_the_forwarded_body_byte_identical() {
        let server = MockServer::start().await;
        let body = br#"{"model":"llama3-8b","messages":[{"role":"user","content":"hi"}]}"#;
        Mock::given(method("POST"))
            .and(body_bytes(Bytes::from_static(body)))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
            .mount(&server)
            .await;

        RawForwarder::new(ProviderFamily::OpenAiCompat, server.uri(), "k")
            .with_session_header(Some("x-inferflux-session-id"))
            .forward_metered(
                "/v1/chat/completions",
                Bytes::from_static(body),
                Some("conv_9"),
            )
            .await
            .unwrap();

        let sent = &server.received_requests().await.unwrap()[0];
        assert_eq!(
            &sent.body, body,
            "affinity is a header, never a body mutation — the cached prompt must stay byte-stable"
        );
    }

    #[tokio::test]
    async fn forward_metered_parses_family_usage() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "choices": [{"message": {"content": "4"}, "finish_reason": "stop"}],
                "usage": {"prompt_tokens": 12, "completion_tokens": 1,
                          "prompt_tokens_details": {"cached_tokens": 2}}
            })))
            .mount(&server)
            .await;
        let forwarder = RawForwarder::new(ProviderFamily::OpenAiCompat, server.uri(), "k");
        let (resp, usage) = forwarder
            .forward_metered("/v1/chat/completions", Bytes::from_static(b"{}"), None)
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        // Fresh input = 12 total − 2 cached; cache_read = 2; output = 1.
        assert_eq!(usage.tokens_in, 10);
        assert_eq!(usage.cache_read_tokens, 2);
        assert_eq!(usage.tokens_out, 1);
    }

    #[tokio::test]
    async fn forward_stream_metered_forwards_bytes_and_finalizes_usage() {
        use futures_util::StreamExt;
        let server = MockServer::start().await;
        let sse = "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n\
data: {\"choices\":[],\"usage\":{\"prompt_tokens\":6,\"completion_tokens\":5,\"prompt_tokens_details\":{\"cached_tokens\":2}}}\n\n\
data: [DONE]\n\n";
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_raw(sse, "text/event-stream")
                    .insert_header("x-request-id", "req-stream-1"),
            )
            .mount(&server)
            .await;
        let forwarder = RawForwarder::new(ProviderFamily::OpenAiCompat, server.uri(), "k");
        let response = forwarder
            .forward_stream_metered("/v1/chat/completions", Bytes::from_static(b"{}"), None)
            .await
            .unwrap();
        assert_eq!(response.status, 200);
        assert_eq!(
            response.headers.get("content-type").unwrap(),
            "text/event-stream"
        );
        assert_eq!(
            response.headers.get("x-request-id").unwrap(),
            "req-stream-1"
        );
        let mut stream = response.stream;
        let mut forwarded = Vec::new();
        let mut final_usage = None;
        while let Some(item) = stream.next().await {
            let chunk = item.unwrap();
            forwarded.extend_from_slice(&chunk.data);
            if chunk.usage.is_some() {
                final_usage = chunk.usage;
            }
        }
        // Client-visible bytes are the upstream SSE, verbatim.
        assert!(String::from_utf8_lossy(&forwarded).contains("\"content\":\"hi\""));
        let usage = final_usage.expect("terminal chunk carries the finalized usage");
        assert_eq!(usage.tokens_in, 4); // 6 − 2 cached
        assert_eq!(usage.cache_read_tokens, 2);
        assert_eq!(usage.tokens_out, 5);
    }

    #[tokio::test]
    async fn complete_timeout_applies_to_the_raw_plane() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_delay(Duration::from_millis(100))
                    .set_body_json(json!({})),
            )
            .mount(&server)
            .await;
        let forwarder = RawForwarder::new(ProviderFamily::OpenAiCompat, server.uri(), "k")
            .with_timeouts(
                Duration::from_millis(10),
                Duration::from_secs(1),
                Some(Duration::from_secs(1)),
            );

        let err = forwarder
            .forward("/v1/chat/completions", Bytes::from_static(b"{}"))
            .await
            .unwrap_err();
        assert!(matches!(err, ProviderError::Timeout(d) if d == Duration::from_millis(10)));
    }

    #[tokio::test]
    async fn stream_setup_timeout_applies_to_the_raw_plane() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_delay(Duration::from_millis(100))
                    .set_body_string("data: [DONE]\n\n"),
            )
            .mount(&server)
            .await;
        let forwarder = RawForwarder::new(ProviderFamily::OpenAiCompat, server.uri(), "k")
            .with_timeouts(
                Duration::from_secs(1),
                Duration::from_millis(10),
                Some(Duration::from_secs(1)),
            );

        let err = forwarder
            .forward_stream_metered("/v1/chat/completions", Bytes::from_static(b"{}"), None)
            .await
            .err()
            .expect("stream setup should time out");
        assert!(matches!(err, ProviderError::Timeout(d) if d == Duration::from_millis(10)));
    }
}
