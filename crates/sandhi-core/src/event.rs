//! The neutral usage-event wire type (mirrors `schemas/usage-event.v1.schema.json`).

use serde::{Deserialize, Serialize};

/// The cost basis of a call's backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Backend {
    /// Provider API — billed in tokens.
    External,
    /// Local vLLM/TGI — billed in GPU-hours; token counts are display-only.
    SelfHosted,
}

/// The neutral, cross-repo boundary object emitted once per model call.
///
/// **No dollars, no tier/SKU names.** Sandhi measures; the commercial layer prices
/// (AnvaiOps ADR-0047 D3). Build with [`UsageEvent::new`] + the `with_*` setters.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UsageEvent {
    /// Wire-contract major version. Breaking changes bump this and coordinate consumers.
    pub schema_version: String,
    pub request_id: String,
    /// RFC 3339 timestamp of completion (usage finalized).
    pub occurred_at: String,
    /// Neutral provider slug (anthropic, openai, gemini, bedrock, cohere, vllm, ollama, byo…).
    pub provider: String,
    pub model: String,
    pub backend: Backend,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub virtual_key_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub route: Option<String>,
    /// Conversation/session key for prompt-cache + KV affinity (ADR-0047 D9). Preserved
    /// end-to-end; never collapsed to a single value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,

    pub tokens_in: u64,
    pub tokens_out: u64,
    #[serde(default)]
    pub cache_creation_tokens: u64,
    #[serde(default)]
    pub cache_read_tokens: u64,
    /// Self-hosted backends only: GPU-seconds (the cost basis there).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gpu_seconds: Option<f64>,
}

impl UsageEvent {
    /// The wire-contract major version this build emits.
    pub const SCHEMA_VERSION: &'static str = "1";

    /// A new event with zero counts and no attribution — fill via the `with_*` setters.
    pub fn new(
        request_id: impl Into<String>,
        occurred_at: impl Into<String>,
        provider: impl Into<String>,
        model: impl Into<String>,
        backend: Backend,
    ) -> Self {
        Self {
            schema_version: Self::SCHEMA_VERSION.to_string(),
            request_id: request_id.into(),
            occurred_at: occurred_at.into(),
            provider: provider.into(),
            model: model.into(),
            backend,
            virtual_key_id: None,
            subject_id: None,
            group_id: None,
            route: None,
            session_id: None,
            tokens_in: 0,
            tokens_out: 0,
            cache_creation_tokens: 0,
            cache_read_tokens: 0,
            gpu_seconds: None,
        }
    }

    /// Total billable tokens (fresh input + output). The neutral quantity budgets meter on.
    pub fn billable_tokens(&self) -> u64 {
        self.tokens_in + self.tokens_out
    }

    #[must_use]
    pub fn with_attribution(
        mut self,
        virtual_key_id: Option<String>,
        subject_id: Option<String>,
        group_id: Option<String>,
    ) -> Self {
        self.virtual_key_id = virtual_key_id;
        self.subject_id = subject_id;
        self.group_id = group_id;
        self
    }

    #[must_use]
    pub fn with_session(mut self, session_id: Option<String>) -> Self {
        self.session_id = session_id;
        self
    }

    #[must_use]
    pub fn with_route(mut self, route: Option<String>) -> Self {
        self.route = route;
        self
    }

    #[must_use]
    pub fn with_tokens(mut self, tokens_in: u64, tokens_out: u64) -> Self {
        self.tokens_in = tokens_in;
        self.tokens_out = tokens_out;
        self
    }

    #[must_use]
    pub fn with_cache(mut self, creation: u64, read: u64) -> Self {
        self.cache_creation_tokens = creation;
        self.cache_read_tokens = read;
        self
    }

    #[must_use]
    pub fn with_gpu_seconds(mut self, gpu_seconds: Option<f64>) -> Self {
        self.gpu_seconds = gpu_seconds;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrips_and_omits_null_optionals() {
        let ev = UsageEvent::new(
            "req_1",
            "2026-07-19T00:00:00Z",
            "anthropic",
            "claude-x",
            Backend::External,
        )
        .with_attribution(
            Some("vk_1".into()),
            Some("alice".into()),
            Some("team".into()),
        )
        .with_session(Some("conv_1".into()))
        .with_tokens(10, 5)
        .with_cache(0, 3);

        assert_eq!(ev.billable_tokens(), 15);
        let json = serde_json::to_string(&ev).unwrap();
        assert!(!json.contains("\"route\""), "null optionals are omitted");
        assert!(!json.contains("gpu_seconds"));
        let back: UsageEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(ev, back);
    }
}
