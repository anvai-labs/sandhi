//! Sandhi unified provider transport (AnvaiOps ADR-0047 D10).
//!
//! One Rust implementation of the provider wire layer that victor, ProximaDB, and AnvaiOps
//! all delegate to — because usage/cache-token parsing is provider-specific and must be
//! **single-sourced at the point of the call**, where metering trust is decided.
//!
//! Patterns: **adapter** (per provider), **strategy** (routing/fallback), **factory** (from
//! config), **decorator** (metering + circuit-breaker + retry wrapped around each adapter).
//!
//! OpenAI-compatibility covers ~20 providers (Groq, Together, Fireworks, DeepSeek, Mistral,
//! Qwen, xAI, OpenRouter, vLLM, LM Studio, Ollama, Cerebras…), so the real adapter surface is
//! ~5–6: `openai_compat`, `anthropic`, `gemini`, `bedrock`, `cohere`, `local`.

use async_trait::async_trait;
use sandhi_core::UsageEvent;

/// A model request, transport-neutral. `body` is the provider-native JSON, forwarded
/// prefix-exact so prompt caches keep hitting (ADR-0047 D9).
#[derive(Debug, Clone)]
pub struct ProviderRequest {
    pub model: String,
    pub body: serde_json::Value,
    pub stream: bool,
    /// Conversation/session key for cache + KV affinity — preserved, never flattened.
    pub session_id: Option<String>,
}

/// A completed (non-streaming) response plus the usage measured **at the source**.
#[derive(Debug, Clone)]
pub struct ProviderResponse {
    pub body: serde_json::Value,
    pub usage: UsageEvent,
}

#[derive(Debug)]
pub enum ProviderError {
    Auth,
    RateLimited,
    Upstream(u16),
    Transport(String),
}

/// The adapter contract every provider implements. The metering/resilience **decorator**
/// wraps this so accounting + circuit-breaker + retry are applied uniformly.
#[async_trait]
pub trait Provider: Send + Sync {
    /// Neutral provider slug (e.g. `anthropic`, `openai`).
    fn slug(&self) -> &str;

    /// Non-streaming call. Extracts the full usage breakdown (incl. the cache split) from the
    /// provider's real response — never estimated.
    async fn complete(&self, req: ProviderRequest) -> Result<ProviderResponse, ProviderError>;

    // TODO(sandhi-providers): `stream()` — SSE pass-through (O(1) memory), usage finalized
    // from the terminal usage block.
}

/// Register a **host-language adapter** (a Python/TS callback) so a consumer's custom /
/// air-gapped / community providers work without a Rust contribution (ADR-0047 D10).
pub mod escape_hatch {
    //! TODO(sandhi-providers): a `Provider` impl that dispatches to a host-language callback,
    //! exposed through the bindings so victor's Python custom-provider path keeps working.
}

// Adapter stubs — first implementation milestones (OpenAI-compat first, then Anthropic).
pub mod openai_compat {
    //! TODO: covers ~20 providers via the OpenAI Chat Completions spec; reads cached tokens
    //! from `prompt_tokens_details.cached_tokens`.
}
pub mod anthropic {
    //! TODO: Messages API; assembles the cache split from `message_start` + `message_delta`.
}
pub mod gemini {
    //! TODO: Google Gemini.
}
pub mod bedrock {
    //! TODO: AWS Bedrock.
}
pub mod cohere {
    //! TODO: Cohere.
}
pub mod local {
    //! TODO: local vLLM / Ollama (self-hosted backend; GPU-seconds cost basis).
}
