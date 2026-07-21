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
    parse_ollama_usage, parse_openai_usage, ParsedUsage,
};

pub mod anthropic;
pub mod cohere;
pub mod escape_hatch;
pub mod gemini;
pub mod local;
pub mod openai;
pub mod resilience;
pub use anthropic::Anthropic;
pub use cohere::Cohere;
pub use escape_hatch::FnProvider;
pub use gemini::Gemini;
pub use local::Ollama;
pub use openai::OpenAiCompat;
pub use resilience::{CircuitBreaker, ResilientProvider, RetryConfig, TimeoutConfig};

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
}

impl ProviderRequest {
    pub fn new(model: impl Into<String>, body: serde_json::Value) -> Self {
        Self {
            model: model.into(),
            body,
            session_id: None,
        }
    }

    #[must_use]
    pub fn with_session(mut self, session_id: Option<String>) -> Self {
        self.session_id = session_id;
        self
    }
}

/// A completed (non-streaming) response plus the usage measured **at the source**.
#[derive(Debug, Clone)]
pub struct ProviderResponse {
    pub status: u16,
    pub body: serde_json::Value,
    pub usage: ParsedUsage,
}

/// One item of a streaming response: raw bytes to forward verbatim, plus (on the terminal
/// item only) the finalized usage.
#[derive(Debug, Clone)]
pub struct StreamChunk {
    /// Raw upstream bytes, forwarded to the caller unchanged (O(1) pass-through).
    pub data: Bytes,
    /// Present only on the terminal item: the finalized usage counts.
    pub usage: Option<ParsedUsage>,
}

/// A streaming response: a stream of [`StreamChunk`]s ending with a usage-bearing terminal item.
pub type ByteStream = Pin<Box<dyn Stream<Item = Result<StreamChunk, ProviderError>> + Send>>;

#[derive(Debug)]
#[non_exhaustive]
pub enum ProviderError {
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

/// Wrap a provider's byte stream in the metered pass-through: forward every upstream chunk
/// verbatim (O(1) memory, ADR-0047 D9) while running `sniff` over each complete newline-delimited
/// line to accumulate usage; the terminal item carries the finalized usage. `sniff(line, &mut
/// usage)` updates the running accumulator (SSE `data:` lines, Anthropic events, or NDJSON — the
/// per-adapter parser decides).
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
        let mut usage = ParsedUsage::default();
        while let Some(chunk) = upstream.next().await {
            let chunk = chunk.map_err(|e| ProviderError::Transport(e.to_string()))?;
            line_buf.extend_from_slice(&chunk);
            while let Some(pos) = line_buf.iter().position(|&b| b == b'\n') {
                let line: Vec<u8> = line_buf.drain(..=pos).collect();
                sniff(&line, &mut usage);
            }
            yield StreamChunk { data: chunk, usage: None };
        }
        yield StreamChunk { data: Bytes::new(), usage: Some(usage) };
    };
    Box::pin(s)
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
