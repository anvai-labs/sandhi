//! The neutral usage-event wire type (mirrors `schemas/usage-event.v1.schema.json`).

use serde::{Deserialize, Serialize};

use crate::chat::{UsageCompleteness, UsageV2};

/// The single billable-token definition (ADR-0005 D4), used identically by reserve, settle,
/// and the durable aggregate — closing the budget-vs-event divergence.
///
/// Every measured dimension is counted at neutral weight 1 and the cache split stays visible
/// as **distinct terms** (`fresh_input` + `cache_creation` + `cache_read` + output +
/// reasoning) rather than flattening to `tokens_in + tokens_out`; the downstream pricer
/// applies its own weights to the preserved split. Still neutral tokens — no dollars.
///
/// Reasoning invariant (D4): each adapter either folds `reasoning_tokens` into `tokens_out`
/// (OpenAI and Anthropic both do) or reports them separately, in which case this function
/// adds them. Detection is total: reasoning that *cannot* be contained in `tokens_out`
/// (`reasoning_tokens > tokens_out`) is treated as unfolded and added; otherwise it is
/// assumed folded and not double-counted.
#[must_use]
pub fn billable(u: &UsageV2) -> u64 {
    let reasoning = u.reasoning_tokens.unwrap_or(0);
    let unfolded_reasoning = if reasoning > u.tokens_out {
        reasoning
    } else {
        0
    };
    u.tokens_in + u.cache_creation_tokens + u.cache_read_tokens + u.tokens_out + unfolded_reasoning
}

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

    // --- ADR-0005 D7 neutral identity (attribution metadata, never pricing) ---
    /// Caller-supplied key for at-most-once semantics across retries of one logical call.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<String>,
    /// Agent-run identifier; groups every call one run makes (cost-tree root).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    /// Step within a run; child dimension under `run_id`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step_id: Option<String>,
    /// Parent step/run for nested agents, so an agent's cost tree is reconstructable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    /// W3C `traceparent` value, linking the event into distributed traces.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace_context: Option<String>,

    pub tokens_in: u64,
    pub tokens_out: u64,
    #[serde(default)]
    pub cache_creation_tokens: u64,
    #[serde(default)]
    pub cache_read_tokens: u64,
    /// Whether token counts are final, partial, or unavailable for this logical call.
    #[serde(default)]
    pub usage_completeness: UsageCompleteness,
    /// Number of upstream attempts made by the runtime for this logical call.
    #[serde(default = "one")]
    pub attempts: u32,
    /// Stable terminal outcome such as `success`, `error`, or `cancelled`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<String>,
    /// Provider-supplied request identifier when one was returned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream_request_id: Option<String>,
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
            idempotency_key: None,
            run_id: None,
            step_id: None,
            parent_id: None,
            trace_context: None,
            tokens_in: 0,
            tokens_out: 0,
            cache_creation_tokens: 0,
            cache_read_tokens: 0,
            usage_completeness: UsageCompleteness::Unavailable,
            attempts: 1,
            outcome: None,
            upstream_request_id: None,
            gpu_seconds: None,
        }
    }

    /// Legacy narrow quantity (fresh input + output only). Superseded by the crate-level
    /// [`billable`] (ADR-0005 D4), which counts the cache split + reasoning; the request-path
    /// integration migrates budget recording onto that definition. Kept unchanged here so
    /// existing callers' accounting is not silently altered (Phase-0 zero-behavior rule).
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

    /// Neutral identity (ADR-0005 D7): idempotency + agent cost-tree + trace linkage.
    #[must_use]
    pub fn with_identity(
        mut self,
        idempotency_key: Option<String>,
        run_id: Option<String>,
        step_id: Option<String>,
        parent_id: Option<String>,
        trace_context: Option<String>,
    ) -> Self {
        self.idempotency_key = idempotency_key;
        self.run_id = run_id;
        self.step_id = step_id;
        self.parent_id = parent_id;
        self.trace_context = trace_context;
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
    pub fn with_measurement(
        mut self,
        completeness: UsageCompleteness,
        attempts: u32,
        outcome: Option<String>,
        upstream_request_id: Option<String>,
    ) -> Self {
        self.usage_completeness = completeness;
        self.attempts = attempts.max(1);
        self.outcome = outcome;
        self.upstream_request_id = upstream_request_id;
        self
    }

    #[must_use]
    pub fn with_gpu_seconds(mut self, gpu_seconds: Option<f64>) -> Self {
        self.gpu_seconds = gpu_seconds;
        self
    }
}

const fn one() -> u32 {
    1
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
        // ADR-0005 D7 identity fields are additive: absent stays absent on the wire.
        assert!(!json.contains("run_id"));
        assert!(!json.contains("idempotency_key"));
        let back: UsageEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(ev, back);
    }

    #[test]
    fn identity_fields_round_trip_and_old_events_still_deserialize() {
        let ev = UsageEvent::new(
            "req_2",
            "2026-07-23T00:00:00Z",
            "openai",
            "gpt-5",
            Backend::External,
        )
        .with_identity(
            Some("idem-1".into()),
            Some("run-7".into()),
            Some("step-3".into()),
            Some("run-6".into()),
            Some("00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01".into()),
        );
        let json = serde_json::to_string(&ev).unwrap();
        let back: UsageEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(back.run_id.as_deref(), Some("run-7"));
        assert_eq!(back.parent_id.as_deref(), Some("run-6"));
        assert_eq!(ev, back);

        // Back-compat: a pre-identity event (no new fields) still deserializes.
        let legacy = serde_json::json!({
            "schema_version": "1",
            "request_id": "req_old",
            "occurred_at": "2026-07-01T00:00:00Z",
            "provider": "anthropic",
            "model": "claude-x",
            "backend": "external",
            "tokens_in": 1,
            "tokens_out": 2
        });
        let old: UsageEvent = serde_json::from_value(legacy).unwrap();
        assert_eq!(old.idempotency_key, None);
        assert_eq!(old.trace_context, None);
    }

    fn usage(tokens_in: u64, tokens_out: u64, creation: u64, read: u64) -> UsageV2 {
        UsageV2 {
            tokens_in,
            tokens_out,
            cache_creation_tokens: creation,
            cache_read_tokens: read,
            ..UsageV2::default()
        }
    }

    #[test]
    fn billable_counts_every_cache_dimension_once() {
        // fresh 100 + creation 40 + read 60 + out 20 — the split stays visible, none dropped.
        assert_eq!(billable(&usage(100, 20, 40, 60)), 220);
        // The old flattened definition would have said 120 (the budget-vs-event divergence).
        assert_eq!(
            usage(100, 20, 40, 60).tokens_in + usage(100, 20, 40, 60).tokens_out,
            120
        );
        assert_eq!(billable(&usage(0, 0, 0, 0)), 0);
    }

    #[test]
    fn billable_reasoning_is_folded_or_added_never_double_counted() {
        // Folded (OpenAI/Anthropic): reasoning ≤ tokens_out → contained, not re-added.
        let mut folded = usage(10, 100, 0, 0);
        folded.reasoning_tokens = Some(80);
        assert_eq!(billable(&folded), 110);
        // Unfolded: reasoning cannot fit in tokens_out → added as its own term.
        let mut unfolded = usage(10, 100, 0, 0);
        unfolded.reasoning_tokens = Some(250);
        assert_eq!(billable(&unfolded), 360);
        // Absent reasoning → no contribution.
        assert_eq!(billable(&usage(10, 100, 0, 0)), 110);
    }
}
