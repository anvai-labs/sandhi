//! Sandhi core — the metering engine.
//!
//! Neutral **units only**: usage accounting (incl. the prompt-cache split), virtual-key
//! resolution ([`keys`]), budget/rate-limit enforcement ([`budget`]), threshold
//! [`alerts`], and the [`UsageEvent`] wire type emitted through a [`Sink`]. This crate has
//! **no transport opinion** — the provider adapters live in `sandhi-providers` and the
//! reverse-proxy in `sandhi-proxy`.
//!
//! Sandhi *measures*; the commercial layer *prices* (AnvaiOps ADR-0047 D3). Nothing here
//! emits dollars or tier/SKU names.

pub mod alerts;
pub mod budget;
pub mod chat;
pub mod conformance;
pub mod event;
// Generated typify narrow models (ADR-0003 §2/§4 pilot) — regenerated, never hand-edited.
mod generated;
pub mod keys;
pub mod ledger;
pub mod sink;
pub mod stats;
pub mod usage;

pub use alerts::{
    Alert, AlertChannel, AlertRegistry, AlertRule, NoopWebhookSender, SharedAlertRegistry,
    WebhookSender, DEFAULT_COOLDOWN_SECS,
};
pub use budget::{Budget, BudgetExceeded, BudgetLedger, Policy, Window};
pub use chat::*;
pub use event::{billable, billable_parts, Backend, UsageEvent};
pub use keys::{KeyStore, VirtualKey};
pub use ledger::{Denied, EnforcementLedger, InMemoryLedger, LedgerView, Reservation};
pub use sink::{BufferedSink, InMemorySink, JsonlSink, Sink};
pub use stats::{
    Dimension, LatencySummary, RunCostTreeV1, RunUsageNodeV1, UsageAggregateV1, UsageAggregator,
    NONE_KEY, OVERFLOW_KEY, TOTAL_KEY,
};
pub use usage::{
    parse_anthropic_usage, parse_bedrock_usage, parse_cohere_usage, parse_gemini_usage,
    parse_ollama_usage, parse_openai_responses_usage, parse_openai_usage, ParsedUsage,
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
        let keys = KeyStore::new();
        keys.insert(VirtualKey {
            id: "vk_alice".into(),
            subject_id: Some("alice".into()),
            group_id: Some("platform".into()),
            upstream_ref: "anthropic:default".into(),
            ..Default::default()
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
        // D4 billable counts the cache split: 220 fresh in + 40 cache-read + 80 out.
        // The narrow in+out reading was 300 — less than the proxy charges for the same call.
        assert_eq!(got.billable_tokens(), 340);
        // The in-process ledger records the SAME quantity the proxy settles. Before the helpers
        // were unified this read 300 while the proxy charged 340 for an identical call — the
        // in-process path under-counted every cache read.
        assert_eq!(ledger.spent("group:platform"), 340);

        // A second big call is now blocked by the group budget (340 + 800 > 1000).
        assert!(ledger.check("group:platform", 800).is_err());
    }
}

#[cfg(test)]
mod observability_boundary_tests {
    //! TD-0011 D1: a library emits, an application decides.
    //!
    //! `sandhi-core`, `-providers` and `-store` are linked in-process by hosts like Victor. If any
    //! of them could install a subscriber, that host would get Sandhi's logging configuration
    //! imposed on it — and two subscribers in one process means one of them silently wins. The
    //! strongest form of the guarantee is dependency-level: without `tracing-subscriber` they
    //! *cannot* install one, no matter what a future patch tries. `include_str!` resolves at
    //! compile time, so a moved crate breaks the build rather than skipping the check.

    const CORE: &str = include_str!("../Cargo.toml");
    const PROVIDERS: &str = include_str!("../../sandhi-providers/Cargo.toml");
    const STORE: &str = include_str!("../../sandhi-store/Cargo.toml");

    #[test]
    fn libraries_cannot_install_a_tracing_subscriber() {
        for (crate_name, manifest) in [
            ("sandhi-core", CORE),
            ("sandhi-providers", PROVIDERS),
            ("sandhi-store", STORE),
        ] {
            assert!(
                !manifest.contains("tracing-subscriber"),
                "{crate_name} must not depend on tracing-subscriber (TD-0011 D1): a library that \
                 installs a subscriber hijacks its host's logging"
            );
        }
    }

    #[test]
    fn libraries_do_emit_through_the_facade() {
        // The other half of D1: emitting is expected, only installing is not.
        assert!(
            CORE.contains("tracing"),
            "sandhi-core should emit through the tracing facade"
        );
    }

    #[test]
    fn libraries_cannot_pull_an_observability_sdk() {
        // TD-0011 D4 / P3 (Scope 5): OTLP export lives behind a non-default feature in the
        // *binary* only. The same dependency-level reasoning as the subscriber guard applies: a
        // library crate that pulls `opentelemetry*`, `prometheus`, or the `metrics` crate would
        // inflate the binding wheels and drag an exporter/runtime into every in-process host.
        // `opentelemetry` as a substring also covers `opentelemetry_sdk` / `opentelemetry-otlp`.
        const FORBIDDEN: &[&str] = &["opentelemetry", "prometheus", "metrics"];
        for (crate_name, manifest) in [
            ("sandhi-core", CORE),
            ("sandhi-providers", PROVIDERS),
            ("sandhi-store", STORE),
        ] {
            for dep in FORBIDDEN {
                assert!(
                    !manifest.contains(dep),
                    "{crate_name} must not depend on `{dep}` (TD-0011 D1/D4): the observability \
                     SDK belongs to the proxy binary behind a feature, never a library crate"
                );
            }
        }
    }
}
