//! Sandhi core — the metering engine.
//!
//! Neutral **units only**: usage accounting (incl. the prompt-cache split), virtual-key
//! resolution, budget/rate-limit enforcement, and the [`UsageEvent`] wire type. This crate
//! has **no transport opinion** — the provider adapters live in `sandhi-providers`, and the
//! reverse-proxy in `sandhi-proxy`.
//!
//! Sandhi *measures*; the commercial layer *prices* (AnvaiOps ADR-0047 D3). Nothing here
//! emits dollars or tier/SKU names.

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
/// Mirrors `schemas/usage-event.v1.schema.json` (the versioned wire contract). Consumers —
/// victor, ProximaDB, AnvaiOps — code against this shape. **No dollars, no tier/SKU names.**
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
}

/// Virtual keys — one shared upstream key fronts many per-user keys (attribution + revocation
/// per person). Resolution + storage land here.
pub mod keys {
    // TODO(sandhi-core): virtual-key issuance, resolution (`vk_…` → subject/group + upstream),
    // and revocation.
}

/// Budgets + rate limits per virtual key / group. Enforcement mechanism only — no pricing.
pub mod budget {
    // TODO(sandhi-core): per-key/per-group budget + rate-limit enforcement.
}

/// Usage accounting + the neutral-event emitter (best-effort, off the critical path).
pub mod accounting {
    // TODO(sandhi-core): finalize usage from a call, build a `super::UsageEvent`, emit to
    // the configured sink (local SQLite/JSONL, or POST to a collector).
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_event_roundtrips_and_omits_null_optionals() {
        let ev = UsageEvent {
            schema_version: UsageEvent::SCHEMA_VERSION.to_string(),
            request_id: "req_1".into(),
            occurred_at: "2026-07-19T00:00:00Z".into(),
            provider: "anthropic".into(),
            model: "claude-x".into(),
            backend: Backend::External,
            virtual_key_id: Some("vk_1".into()),
            subject_id: Some("alice".into()),
            group_id: Some("team".into()),
            route: None,
            session_id: Some("conv_1".into()),
            tokens_in: 10,
            tokens_out: 5,
            cache_creation_tokens: 0,
            cache_read_tokens: 3,
            gpu_seconds: None,
        };
        let json = serde_json::to_string(&ev).unwrap();
        assert!(!json.contains("\"route\""), "null optionals are omitted");
        assert!(!json.contains("gpu_seconds"));
        let back: UsageEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(ev, back);
    }
}
