//! Sandhi unified provider transport (AnvaiOps ADR-0047 D10).
//!
//! One Rust implementation of the provider wire layer that victor, ProximaDB, and AnvaiOps
//! all delegate to — because usage/cache-token parsing is provider-specific and must be
//! **single-sourced at the point of the call**, where metering trust is decided.
//!
//! Patterns: **adapter** (per provider), **strategy** (routing/fallback), **factory** (from
//! config), **decorator** (metering + circuit-breaker + retry — later, wrapped around each
//! adapter). OpenAI-compatibility covers ~20 providers, so the real adapter surface is small.
//!
//! Adapters return raw counts ([`ParsedUsage`]) + the response; the **caller** (proxy /
//! middleware) assembles the neutral [`sandhi_core::UsageEvent`] with request id, timestamp,
//! and attribution — the adapter never fabricates those.

use async_trait::async_trait;
use bytes::Bytes;
use futures_core::Stream;
use std::pin::Pin;

// The usage parsers are metering primitives — they live in `sandhi-core` (no transport deps).
pub use sandhi_core::usage::{
    parse_anthropic_usage, parse_bedrock_usage, parse_cohere_usage, parse_gemini_usage,
    parse_ollama_usage, parse_openai_responses_usage, parse_openai_usage, ParsedUsage,
};

pub mod anthropic;
mod anthropic_typed;
pub mod catalog;
pub mod cohere;
mod cohere_typed;
pub mod escape_hatch;
pub mod gemini;
mod gemini_typed;
pub mod local;
mod ollama_typed;
pub mod openai;
pub mod openai_responses;
mod openai_responses_typed;
pub mod openai_roles;
pub mod raw;
pub mod resilience;
pub mod typed;
pub use anthropic::{Anthropic, AnthropicAuthScheme};
pub use catalog::{
    openai_compat_descriptor, provider_descriptor, resolve_openai_compat_provider,
    ModelEndpointRoute, OpenAiCompatProviderSpec, OPENAI_COMPAT_PROVIDER_SPECS,
};
pub use cohere::Cohere;
pub use escape_hatch::FnProvider;
pub use gemini::{Gemini, GeminiAuthScheme};
pub use local::Ollama;
pub use openai::OpenAiCompat;
pub use openai_responses::{OpenAiResponses, OpenAiResponsesProfile};
pub use openai_roles::{validate_openai_chat_messages, OpenAiChatRole};
pub mod metering;
pub use metering::MeteredProvider;
pub use resilience::{CircuitBreaker, ResilientProvider, RetryConfig, TimeoutConfig};
pub use typed::{
    ChatEventStream, ChatProvider, ProviderFamily, ProviderHandle, ProviderRuntime,
    ProviderTransportConfig,
};

/// Shared HTTP client for the in-repo adapters: a 10s TCP/TLS connect bound as
/// defense-in-depth under the decorator's per-attempt timeouts. Policy timeouts
/// (whole-call / stream-setup / idle) live in [`TimeoutConfig`], not here.
pub(crate) fn default_client() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(10))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
}

/// AWS Bedrock — the usage parser is [`sandhi_core::usage::parse_bedrock_usage`]. Native
/// transport needs AWS **SigV4** request signing (a dedicated follow-up); until then, front
/// Bedrock with an OpenAI-compatible gateway and use [`OpenAiCompat`].
pub mod bedrock {}

/// A model request. `body` is the provider-native JSON, forwarded prefix-exact so prompt
/// caches keep hitting (ADR-0047 D9). `session_id` is the conversation key for cache/KV
/// affinity — preserved end-to-end, never flattened.
#[derive(Debug, Clone)]
pub struct ProviderRequest {
    pub model: String,
    pub body: serde_json::Value,
    pub session_id: Option<String>,
    /// Who this call is for (metering decorator input). Never enters the wire body —
    /// attribution rides outside the cached prompt (ADR-0001 §4); adapters ignore it.
    pub attribution: Attribution,
}

/// Per-call attribution consumed by the metering decorator. Carried on the request (not the
/// decorator constructor) because one provider instance serves many virtual keys in the proxy.
#[derive(Debug, Clone, Default)]
pub struct Attribution {
    pub virtual_key_id: Option<String>,
    pub subject_id: Option<String>,
    pub group_id: Option<String>,
    pub route: Option<String>,
}

impl ProviderRequest {
    pub fn new(model: impl Into<String>, body: serde_json::Value) -> Self {
        Self {
            model: model.into(),
            body,
            session_id: None,
            attribution: Attribution::default(),
        }
    }

    #[must_use]
    pub fn with_session(mut self, session_id: Option<String>) -> Self {
        self.session_id = session_id;
        self
    }

    #[must_use]
    pub fn with_attribution(mut self, attribution: Attribution) -> Self {
        self.attribution = attribution;
        self
    }
}

/// A completed (non-streaming) response plus the usage measured **at the source**.
#[derive(Debug, Clone)]
pub struct ProviderResponse {
    pub status: u16,
    pub body: serde_json::Value,
    pub usage: ParsedUsage,
    /// Upstream attempts made for this logical call, including the successful attempt.
    pub attempts: u32,
}

/// One item of a streaming response: raw bytes to forward verbatim, plus (on the terminal
/// item only) the finalized usage.
#[derive(Debug, Clone)]
pub struct StreamChunk {
    /// Raw upstream bytes, forwarded to the caller unchanged (O(1) pass-through).
    pub data: Bytes,
    /// Present only on the terminal item: the finalized usage counts.
    pub usage: Option<ParsedUsage>,
    /// Upstream stream-setup attempts made for this logical call.
    pub attempts: u32,
}

/// A streaming response: a stream of [`StreamChunk`]s ending with a usage-bearing terminal item.
pub type ByteStream = Pin<Box<dyn Stream<Item = Result<StreamChunk, ProviderError>> + Send>>;

#[derive(Debug)]
#[non_exhaustive]
pub enum ProviderError {
    /// The caller supplied a malformed provider-native request. Never retry.
    InvalidRequest(String),
    /// 401 / 403 — bad or missing credential.
    Auth,
    /// 429 — provider rate limit.
    RateLimited,
    /// Any other non-success status.
    Upstream(u16),
    /// Network / TLS / decode failure before or during the response.
    Transport(String),
    /// The circuit breaker is open (upstream failing) — the call was not attempted.
    CircuitOpen,
    /// The call (or stream setup / idle gap) exceeded the configured bound. Carries the bound
    /// for a self-describing message. Retryable — a timeout is a transient bet, like a 503.
    Timeout(std::time::Duration),
}

impl std::fmt::Display for ProviderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProviderError::InvalidRequest(e) => write!(f, "invalid request: {e}"),
            ProviderError::Auth => write!(f, "auth failed (401/403)"),
            ProviderError::RateLimited => write!(f, "rate limited (429)"),
            ProviderError::Upstream(s) => write!(f, "upstream status {s}"),
            ProviderError::Transport(e) => write!(f, "transport error: {e}"),
            ProviderError::CircuitOpen => write!(f, "circuit open (upstream failing)"),
            ProviderError::Timeout(d) => write!(f, "timed out after {}s", d.as_secs_f32()),
        }
    }
}

impl std::error::Error for ProviderError {}

/// Map a non-success HTTP status to a [`ProviderError`].
pub(crate) fn error_for_status(status: u16) -> ProviderError {
    match status {
        401 | 403 => ProviderError::Auth,
        429 => ProviderError::RateLimited,
        s => ProviderError::Upstream(s),
    }
}

/// The adapter contract every provider implements. The metering/resilience **decorator** will
/// wrap this so accounting + circuit-breaker + retry apply uniformly (a later milestone).
#[async_trait]
pub trait Provider: Send + Sync {
    /// Neutral provider slug (e.g. `anthropic`, `openai`).
    fn slug(&self) -> &str;

    /// Non-streaming call. Extracts the full usage breakdown (incl. the cache split) from the
    /// provider's real response — never estimated.
    async fn complete(&self, req: ProviderRequest) -> Result<ProviderResponse, ProviderError>;

    /// Streaming call: SSE pass-through (O(1) memory), usage finalized from the terminal block.
    async fn stream(&self, req: ProviderRequest) -> Result<ByteStream, ProviderError>;
}

/// Hard cap on the single-line buffer inside the metered pass-through sniffer (TD-0006). Past
/// this bound the line is flushed (forwarded) without further sniffing — a giant tool-call delta
/// cannot blow memory. 64 KiB is generous for SSE/NDJSON lines (typical delta ≈ 1–4 KiB) while
/// keeping the worst-case buffer small.
const LINE_SNIFF_BUDGET: usize = 64 * 1024;

/// Wrap a provider's byte stream in the metered pass-through: forward every upstream chunk
/// verbatim (O(1) forwarding, ADR-0047 D9) while running `sniff` over each complete newline-
/// delimited line to accumulate usage; the terminal item carries the finalized usage.
///
/// **Improvements over the original O(n²) / unbounded implementation (TD-0006):**
///
/// - **Bounded line buffer** — a single line exceeding [`LINE_SNIFF_BUDGET`] is flushed without
///   sniffing so memory stays bounded. A huge tool-call delta or a single-JSON-array stream
///   (Gemini non-`?alt=sse`) cannot exhaust memory.
/// - **O(n) scan** — tracks the last-scanned position so only newly-arrived bytes are searched
///   for `\n` on each chunk. The original rescanned the entire accumulated buffer on every chunk
///   (O(chunks²)).
/// - **`contains("usage")` guard** — skips the JSON parse inside `sniff` for lines that cannot
///   carry a usage object. Every known provider embeds the substring `"usage"` in usage-bearing
///   events (OpenAI/Anthropic/Cohere: `"usage"`; Gemini: `"usageMetadata"`). Non-usage lines
///   are forwarded without the parse overhead.
/// - **Transport-shape-aware final flush** — on stream end, sniffs any remaining buffered bytes.
///   This handles NDJSON without a trailing newline and, critically, the single-JSON-array
///   transport (Gemini's non-SSE stream: one `[{…},{…}]` with no line boundaries). If the
///   remaining buffer is within the budget, the sniff closure gets one final shot at extraction;
///   otherwise usage degrades gracefully to the default (zero) rather than blowing memory.
///
/// `sniff(line, &mut usage)` updates the running accumulator (SSE `data:` lines, Anthropic
/// events, NDJSON, or the terminal JSON array — the per-adapter parser decides).
pub(crate) fn metered_passthrough<S>(
    mut upstream: S,
    mut sniff: impl FnMut(&[u8], &mut ParsedUsage) + Send + 'static,
) -> ByteStream
where
    S: Stream<Item = reqwest::Result<Bytes>> + Send + Unpin + 'static,
{
    use futures_util::StreamExt;
    let s = async_stream::try_stream! {
        let mut line_buf: Vec<u8> = Vec::new();
        // Position in line_buf up to which we've already searched for '\n'. Only bytes after
        // this offset are rescanned on each new chunk — O(chunks) not O(chunks²).
        let mut searched_to: usize = 0;
        let mut usage = ParsedUsage::default();
        while let Some(chunk) = upstream.next().await {
            let chunk = chunk.map_err(|e| ProviderError::Transport(e.to_string()))?;
            line_buf.extend_from_slice(&chunk);
            // Scan only the newly-arrived portion for line boundaries.
            while searched_to < line_buf.len() {
                let Some(rel) = line_buf[searched_to..].iter().position(|&b| b == b'\n') else {
                    searched_to = line_buf.len();
                    break;
                };
                let nl = searched_to + rel;
                // Drain the complete line (including the newline) and sniff it.
                let line: Vec<u8> = line_buf.drain(..=nl).collect();
                // After draining the buffer shifts; reset the search cursor to the new start.
                searched_to = 0;
                // Guard: skip the JSON parse for lines that can't carry a usage object.
                if line_contains_usage(&line) {
                    sniff(&line, &mut usage);
                }
            }
            // Bounded memory: if no newline was found and the buffer exceeds the sniff budget,
            // the current line is too large (a giant tool-call delta, or a single-JSON-array
            // without the `?alt=sse` flag). Flush it without sniffing so memory stays bounded.
            // The bytes were already forwarded verbatim via `chunk` above.
            if line_buf.len() > LINE_SNIFF_BUDGET {
                line_buf.clear();
                searched_to = 0;
            }
            yield StreamChunk { data: chunk, usage: None, attempts: 1 };
        }
        // Transport-shape awareness: on stream end, sniff any remaining buffered bytes. Handles
        // NDJSON without a trailing newline and the single-JSON-array transport (Gemini's
        // non-`?alt=sse` stream). If within the budget, the sniff closure gets one final shot;
        // otherwise we degrade gracefully (default zero usage) rather than blowing memory.
        if !line_buf.is_empty()
            && line_buf.len() <= LINE_SNIFF_BUDGET
            && line_contains_usage(&line_buf)
        {
            sniff(&line_buf, &mut usage);
        }
        yield StreamChunk { data: Bytes::new(), usage: Some(usage), attempts: 1 };
    };
    Box::pin(s)
}

/// Check whether a byte slice could carry a usage object — a cheap pre-filter that avoids the
/// JSON parse for lines that cannot. Every known provider embeds one of these substrings in
/// usage-bearing events:
/// - OpenAI / Anthropic / Cohere: `"usage"`
/// - Gemini: `"usageMetadata"`
/// - Ollama: `"eval_count"` (`prompt_eval_count` / `eval_count`)
fn line_contains_usage(line: &[u8]) -> bool {
    contains_substring(line, b"usage") || contains_substring(line, b"eval_count")
}

fn contains_substring(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

/// Extract the JSON payload from an SSE `data: {...}` line (skipping `[DONE]`), for the
/// per-adapter sniffers.
pub(crate) fn sse_data_json(line: &[u8]) -> Option<serde_json::Value> {
    let s = std::str::from_utf8(line).ok()?.trim();
    let payload = s.strip_prefix("data:")?.trim();
    if payload == "[DONE]" {
        return None;
    }
    serde_json::from_str(payload).ok()
}

/// Test helper: drive `chunks` (a pre-split byte stream) through the production streaming
/// primitive (`metered_passthrough` + the adapter's real `sniff`) and return the finalized usage
/// from the terminal item. Shared by the per-provider chunk-boundary / forward-compat property
/// tests (TD-0001 W1) so each exercises the exact production path.
#[cfg(test)]
pub(crate) async fn accumulate_usage(
    chunks: Vec<Bytes>,
    sniff: impl FnMut(&[u8], &mut ParsedUsage) + Send + 'static,
) -> ParsedUsage {
    use futures_util::StreamExt;
    let upstream = futures_util::stream::iter(
        chunks
            .into_iter()
            .map(Ok::<Bytes, reqwest::Error>)
            .collect::<Vec<_>>(),
    );
    let mut out = metered_passthrough(Box::pin(upstream), sniff);
    let mut final_usage = None;
    while let Some(item) = out.next().await {
        let c = item.unwrap();
        if c.usage.is_some() {
            final_usage = c.usage;
        }
    }
    final_usage.expect("terminal item carries usage")
}

#[cfg(test)]
mod metered_passthrough_tests {
    use super::*;
    use bytes::Bytes;
    use futures_util::StreamExt;

    /// Drive chunks through `metered_passthrough` and return (finalized usage, all forwarded data).
    async fn drive(
        chunks: Vec<Bytes>,
        sniff: impl FnMut(&[u8], &mut ParsedUsage) + Send + 'static,
    ) -> (ParsedUsage, Vec<u8>) {
        let upstream = futures_util::stream::iter(
            chunks
                .into_iter()
                .map(Ok::<Bytes, reqwest::Error>)
                .collect::<Vec<_>>(),
        );
        let mut out = metered_passthrough(Box::pin(upstream), sniff);
        let mut final_usage = ParsedUsage::default();
        let mut forwarded = Vec::new();
        while let Some(item) = out.next().await {
            let c = item.unwrap();
            forwarded.extend_from_slice(&c.data);
            if let Some(u) = c.usage {
                final_usage = u;
            }
        }
        (final_usage, forwarded)
    }

    /// Bounded-sniffer: a single-JSON-array (Gemini non-`?alt=sse` shape) with no line
    /// boundaries must not blow the buffer. The whole response accumulates in the line buffer;
    /// on stream end the final flush sniffs it (within budget) and usage is extracted.
    #[tokio::test]
    async fn single_json_array_within_budget_extracts_usage_on_final_flush() {
        // Gemini non-SSE stream shape: one JSON array, no newlines.
        let array = br#"[{"candidates":[{"content":{"parts":[{"text":"hi"}]}}],"usageMetadata":{"promptTokenCount":10,"candidatesTokenCount":5,"cachedContentTokenCount":3}}]"#;
        let chunks: Vec<Bytes> = array.chunks(16).map(Bytes::copy_from_slice).collect();

        // A sniffer that understands non-SSE JSON (not the SSE-specific sse_data_json).
        fn sniff(line: &[u8], usage: &mut ParsedUsage) {
            if let Ok(v) = serde_json::from_slice::<serde_json::Value>(line) {
                if let Some(arr) = v.as_array() {
                    // Gemini puts usageMetadata on the last array element.
                    if let Some(last) = arr.last() {
                        if let Some(meta) = last.get("usageMetadata") {
                            usage.tokens_in = meta
                                .get("promptTokenCount")
                                .and_then(serde_json::Value::as_u64)
                                .unwrap_or(0)
                                - meta
                                    .get("cachedContentTokenCount")
                                    .and_then(serde_json::Value::as_u64)
                                    .unwrap_or(0);
                            usage.tokens_out = meta
                                .get("candidatesTokenCount")
                                .and_then(serde_json::Value::as_u64)
                                .unwrap_or(0);
                            usage.cache_read_tokens = meta
                                .get("cachedContentTokenCount")
                                .and_then(serde_json::Value::as_u64)
                                .unwrap_or(0);
                        }
                    }
                }
            }
        }

        let (usage, forwarded) = drive(chunks, sniff).await;
        // All bytes forwarded verbatim.
        assert_eq!(forwarded, array.to_vec());
        // Usage extracted from the final flush (the array has no line boundaries).
        assert_eq!(usage.tokens_in, 7); // 10 - 3 cached
        assert_eq!(usage.tokens_out, 5);
        assert_eq!(usage.cache_read_tokens, 3);
    }

    /// Bounded-sniffer: a single SSE line exceeding LINE_SNIFF_BUDGET must not blow the buffer.
    /// The line is flushed (forwarded) without sniffing — usage degrades gracefully to default.
    #[tokio::test]
    async fn huge_single_sse_line_does_not_blow_buffer() {
        // Build a single SSE line far exceeding LINE_SNIFF_BUDGET (64 KiB).
        let padding = "x".repeat(LINE_SNIFF_BUDGET + 10_000);
        let sse_line = format!(
            "data: {{\"choices\":[{{\"delta\":{{\"content\":\"{padding}\"}}}}],\"usage\":{{\"prompt_tokens\":42,\"completion_tokens\":7}}}}\n"
        );
        let body = sse_line.as_bytes().to_vec();
        // Feed as small chunks to exercise the bounded-buffer path.
        let chunks: Vec<Bytes> = body.chunks(1024).map(Bytes::copy_from_slice).collect();

        fn sniff(line: &[u8], usage: &mut ParsedUsage) {
            if let Some(v) = sse_data_json(line) {
                if v.get("usage").is_some_and(|u| !u.is_null()) {
                    if let Some(u) = parse_openai_usage(&v) {
                        *usage = u;
                    }
                }
            }
        }

        let (usage, forwarded) = drive(chunks, sniff).await;
        // All bytes forwarded verbatim — no data loss.
        assert_eq!(forwarded, body);
        // Usage gracefully defaults to zero (the oversize line was flushed without sniffing).
        assert_eq!(usage, ParsedUsage::default());
    }

    /// The `contains("usage")` guard correctly identifies usage-bearing lines and skips others.
    #[test]
    fn line_contains_usage_guard() {
        assert!(line_contains_usage(
            b"data: {\"usage\":{\"prompt_tokens\":10}}\n"
        ));
        assert!(line_contains_usage(
            b"data: {\"usageMetadata\":{\"promptTokenCount\":10}}\n"
        ));
        assert!(!line_contains_usage(
            b"data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n"
        ));
        assert!(!line_contains_usage(b"event: ping\n"));
        assert!(!line_contains_usage(b"\n"));
    }

    /// Transport-shape awareness: a single JSON object without a trailing newline (NDJSON's
    /// final line, or a non-streaming body forwarded through the stream path) still extracts
    /// usage via the final flush.
    #[tokio::test]
    async fn no_trailing_newline_extracts_usage_on_final_flush() {
        // One NDJSON line with usage — no trailing newline so it never hits the line-boundary
        // path; it stays in line_buf until the final flush.
        let body = br#"{"id":"b","usage":{"prompt_tokens":10,"completion_tokens":5}}"#;
        let chunks: Vec<Bytes> = vec![Bytes::copy_from_slice(body)];

        fn sniff(line: &[u8], usage: &mut ParsedUsage) {
            if let Ok(v) = serde_json::from_slice::<serde_json::Value>(line) {
                if v.get("usage").is_some_and(|u| !u.is_null()) {
                    if let Some(u) = parse_openai_usage(&v) {
                        *usage = u;
                    }
                }
            }
        }

        let (usage, _) = drive(chunks, sniff).await;
        assert_eq!(usage.tokens_in, 10);
        assert_eq!(usage.tokens_out, 5);
    }

    /// Normal SSE still works: chunk-boundary invariance preserved (the improvement doesn't
    /// regress the existing property tested by per-adapter suites).
    #[tokio::test]
    async fn normal_sse_still_extracts_usage_across_chunk_boundaries() {
        let sse = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"he\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"llo\"}}]}\n\n",
            "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":5}}\n\n",
            "data: [DONE]\n\n"
        )
        .as_bytes();
        // Split at every offset — usage must be invariant.
        fn sniff(line: &[u8], usage: &mut ParsedUsage) {
            if let Some(v) = sse_data_json(line) {
                if v.get("usage").is_some_and(|u| !u.is_null()) {
                    if let Some(u) = parse_openai_usage(&v) {
                        *usage = u;
                    }
                }
            }
        }
        for split in 0..=sse.len() {
            let chunks = vec![
                Bytes::copy_from_slice(&sse[..split]),
                Bytes::copy_from_slice(&sse[split..]),
            ];
            let (usage, _) = drive(chunks, sniff).await;
            assert_eq!(usage.tokens_in, 10, "split {split}");
            assert_eq!(usage.tokens_out, 5, "split {split}");
        }
    }
}
