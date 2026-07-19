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

pub mod usage;
pub use usage::{parse_anthropic_usage, parse_openai_usage, ParsedUsage};

pub mod anthropic;
pub mod openai;
pub use anthropic::Anthropic;
pub use openai::OpenAiCompat;

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
pub enum ProviderError {
    /// 401 / 403 — bad or missing credential.
    Auth,
    /// 429 — provider rate limit.
    RateLimited,
    /// Any other non-success status.
    Upstream(u16),
    /// Network / TLS / decode failure before or during the response.
    Transport(String),
}

impl std::fmt::Display for ProviderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProviderError::Auth => write!(f, "auth failed (401/403)"),
            ProviderError::RateLimited => write!(f, "rate limited (429)"),
            ProviderError::Upstream(s) => write!(f, "upstream status {s}"),
            ProviderError::Transport(e) => write!(f, "transport error: {e}"),
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

/// Register a **host-language adapter** (a Python/TS callback) so a consumer's custom /
/// air-gapped / community providers work without a Rust contribution (ADR-0047 D10).
pub mod escape_hatch {
    // TODO(sandhi-providers): a `Provider` impl that dispatches to a host-language callback,
    // exposed through the bindings so victor's Python custom-provider path keeps working.
}

// Remaining adapters — first-implementation follow-ups (OpenAI-compat + Anthropic are live).
pub mod bedrock {
    // TODO: AWS Bedrock.
}
pub mod cohere {
    // TODO: Cohere.
}
pub mod gemini {
    // TODO: Google Gemini.
}
pub mod local {
    // TODO: local vLLM / Ollama (self-hosted backend; GPU-seconds cost basis).
}
