//! Sandhi core — the metering engine.
//!
//! Neutral **units only**: usage accounting (incl. the prompt-cache split), virtual-key
//! resolution ([`keys`]), budget/rate-limit enforcement ([`budget`]), and the [`UsageEvent`]
//! wire type emitted through a [`Sink`]. This crate has **no transport opinion** — the
//! provider adapters live in `sandhi-providers` and the reverse-proxy in `sandhi-proxy`.
//!
//! Sandhi *measures*; the commercial layer *prices* (AnvaiOps ADR-0047 D3). Nothing here
//! emits dollars or tier/SKU names.

pub mod budget;
pub mod event;
pub mod keys;
pub mod sink;
pub mod usage;

pub use budget::{Budget, BudgetExceeded, BudgetLedger};
pub use event::{Backend, UsageEvent};
pub use keys::{KeyStore, VirtualKey};
pub use sink::{InMemorySink, JsonlSink, Sink};
pub use usage::{
    parse_anthropic_usage, parse_bedrock_usage, parse_cohere_usage, parse_gemini_usage,
    parse_ollama_usage, parse_openai_usage, ParsedUsage,
};

#[cfg(test)]
mod flow_tests {
    //! End-to-end: resolve a virtual key → budget-check → (call happens) → build the event
    //! from real counts → emit → record budget. This is the metering flow the proxy/middleware
    //! runs around every call.
    use super::*;
    use std::sync::Arc;

    #[test]
    fn shared_key_call_is_attributed_metered_and_budgeted() {
        // One shared upstream key fronts a per-user virtual key.
        let mut keys = KeyStore::new();
        keys.insert(VirtualKey {
            id: "vk_alice".into(),
            subject_id: Some("alice".into()),
            group_id: Some("platform".into()),
            upstream_ref: "anthropic:default".into(),
        });

        let mut ledger = BudgetLedger::new();
        ledger.set_limit("group:platform", Budget::tokens(1000));

        let sink = Arc::new(InMemorySink::new());

        // A call arrives presenting vk_alice.
        let vk = keys.resolve("vk_alice").expect("known key");
        let scope = format!("group:{}", vk.group_id.as_deref().unwrap_or("none"));

        // Pre-flight budget check (estimate 300).
        ledger.check(&scope, 300).expect("within budget");

        // ... the upstream call happens; real usage comes back (fresh 220 in, 80 out, 40 cached).
        let event = UsageEvent::new(
            "req_42",
            "2026-07-19T12:00:00Z",
            "anthropic",
            "claude-x",
            Backend::External,
        )
        .with_attribution(
            Some(vk.id.clone()),
            vk.subject_id.clone(),
            vk.group_id.clone(),
        )
        .with_session(Some("conv_7".into()))
        .with_tokens(220, 80)
        .with_cache(0, 40);

        // Emit (best-effort) + record the real spend.
        sink.emit(&event);
        ledger.record(&scope, event.billable_tokens());

        // Attribution + metering landed correctly.
        let got = &sink.events()[0];
        assert_eq!(got.subject_id.as_deref(), Some("alice"));
        assert_eq!(got.virtual_key_id.as_deref(), Some("vk_alice"));
        assert_eq!(got.session_id.as_deref(), Some("conv_7"));
        assert_eq!(got.cache_read_tokens, 40);
        assert_eq!(got.billable_tokens(), 300);
        assert_eq!(ledger.spent("group:platform"), 300);

        // A second big call is now blocked by the group budget (300 + 800 > 1000).
        assert!(ledger.check("group:platform", 800).is_err());
    }
}
