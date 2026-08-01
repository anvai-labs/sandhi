//! Scope 5 — first-party OTLP export of `gen_ai.*` spans + metrics (TD-0011 P3).
//!
//! Default-off cargo feature `otel-otlp`. Disposable observability layered *beside* the
//! prerequisite-free Prometheus `/metrics` path (P2), which stays the default — a deployment that
//! wants traces opts in deliberately (TD-0011 D4). Only the proxy binary takes the OpenTelemetry
//! deps; the library crates never do (D1, enforced by a compile-time guard in `sandhi-core`).
//!
//! ## Attribution-leak boundary (stricter than TD-0011 D2)
//!
//! D2 permits `subject_id`/`group_id`/`session_id`/`virtual_key_id`/`request_id` on *in-process*
//! `tracing` spans. **This module never exports them** — OTLP sends spans off-process, past the
//! trust boundary, where "bounded lifetime, sampled" no longer protects against PII/cardinality
//! leakage. The boundary is structural, not conventional:
//!
//! 1. The gen_ai span is created *directly* via the OpenTelemetry `Tracer` API. The
//!    `tracing_opentelemetry` bridge is **deliberately not installed** — it would bridge the
//!    proxy's existing `tracing::` events (which carry `scope` = `vk:<id>` and `request_id`) into
//!    exported spans.
//! 2. Span/metric attribute keys are **literals in this file**, produced only by the closed
//!    [`GenAiAttrs`] / [`usage_attributes`] / [`metric_attrs`] helpers. A `UsageEvent` field *name*
//!    can never become an attribute *key*.
//! 3. The recorder only ever observes `UsageV2` + a provider slug + a model name — it never sees
//!    `subject_id`/`group_id`/`session_id`/`virtual_key_id`/`request_id` (those live in
//!    `RequestMetadataV1`, on the accounting atom, outside the cached body).
//!
//! ## Measure-vs-price (ADR-0001)
//!
//! Only neutral units are exported. `gen_ai.*.cost.*` is deliberately **never** emitted.

use sandhi_core::UsageV2;

#[cfg(not(feature = "otel-otlp"))]
mod disabled {
    //! Default-build stubs. The recorder is never constructed (`ProxyState::new` sets
    //! `otel: None`), but the types must exist so `lib.rs`'s unconditional call sites compile
    //! identically whether or not the feature is on.
    use super::UsageV2;
    use std::sync::Arc;

    #[derive(Clone, Default)]
    pub struct OtelRecorder;

    #[derive(Default)]
    pub struct SpanHandle;

    pub struct OtelGuard;

    impl OtelRecorder {
        pub fn start_span(&self, _system: &str, _request_model: &str) -> SpanHandle {
            SpanHandle
        }
        pub fn record_usage(
            &self,
            _span: &mut SpanHandle,
            _system: &str,
            _model: &str,
            _usage: &UsageV2,
        ) {
        }
    }

    /// The feature is compiled out: there is nothing to initialise.
    pub fn init() -> Option<(Arc<OtelRecorder>, OtelGuard)> {
        None
    }
}

#[cfg(feature = "otel-otlp")]
mod enabled {
    use super::UsageV2;
    use opentelemetry::metrics::{Counter, Histogram, Meter, MeterProvider as _};
    use opentelemetry::trace::{Span as _, SpanBuilder, SpanKind, TracerProvider as _};
    use opentelemetry::KeyValue;
    use opentelemetry_otlp::{MetricExporter, Protocol, SpanExporter, WithExportConfig};
    use opentelemetry_sdk::metrics::SdkMeterProvider;
    use opentelemetry_sdk::trace::{SdkTracer, SdkTracerProvider};
    use opentelemetry_sdk::Resource;
    use std::sync::Arc;

    /// The only operation Sandhi proxies is chat completion.
    const OPERATION: &str = "chat";

    // ── attribute allowlists (closed: a forbidden key is unrepresentable) ──────────────────

    /// The allowlisted *request-time* span attributes. The struct's fields ARE the allowlist —
    /// `subject_id`/`group_id`/`session_id`/`virtual_key_id`/`request_id` and the agent cost-tree
    /// ids are not fields, so they can never appear on an exported span. Mirrors `metrics::Labels`
    /// (TD-0011 D2): a forbidden dimension is unrepresentable, not merely discouraged.
    #[derive(Clone, Copy)]
    pub(crate) struct GenAiAttrs<'a> {
        pub(crate) system: &'a str,
        pub(crate) request_model: &'a str,
        pub(crate) operation_name: &'a str,
    }

    impl<'a> GenAiAttrs<'a> {
        pub(crate) fn to_vec(self) -> Vec<KeyValue> {
            // `gen_ai.system` is deprecated → `gen_ai.provider.name` in newer semconv; emit the
            // former, which real collectors parse today, and swap on a future semconv bump.
            vec![
                KeyValue::new("gen_ai.system", self.system.to_owned()),
                KeyValue::new("gen_ai.request.model", self.request_model.to_owned()),
                KeyValue::new("gen_ai.operation.name", self.operation_name.to_owned()),
            ]
        }
    }

    /// The *usage + response* span attributes, set at finalize. Every key is a literal; a
    /// `UsageV2` field name can never surface as an attribute key. `gen_ai.response.id` is the
    /// provider's completion id (`upstream_request_id`), NEVER Sandhi's `request_id`.
    pub(crate) fn usage_attributes(usage: &UsageV2, model: &str) -> Vec<KeyValue> {
        // semconv: "input_tokens SHOULD include cached tokens" — cache_read is cached input that
        // was read. cache_creation stays its own attribute (a write, not consumed input).
        let input = usage.tokens_in + usage.cache_read_tokens;
        let mut attrs = vec![
            KeyValue::new("gen_ai.usage.input_tokens", i64_count(input)),
            KeyValue::new("gen_ai.usage.output_tokens", i64_count(usage.tokens_out)),
            KeyValue::new(
                "gen_ai.usage.cache_creation.input_tokens",
                i64_count(usage.cache_creation_tokens),
            ),
            KeyValue::new(
                "gen_ai.usage.cache_read.input_tokens",
                i64_count(usage.cache_read_tokens),
            ),
            KeyValue::new(
                "gen_ai.usage.reasoning.output_tokens",
                i64_count(usage.reasoning_tokens.unwrap_or(0)),
            ),
            KeyValue::new("gen_ai.response.model", model.to_owned()),
        ];
        if let Some(id) = usage.upstream_request_id.as_deref() {
            attrs.push(KeyValue::new("gen_ai.response.id", id.to_owned()));
        }
        attrs
    }

    /// OTel semconv integer attributes are `i64`; Sandhi counts are `u64`. Saturate at `i64::MAX` —
    /// a token count that overflows `i64` is not a real measurement, and saturating preserves the
    /// "never silently lose a count" posture without a runtime panic.
    fn i64_count(n: u64) -> i64 {
        i64::try_from(n).unwrap_or(i64::MAX)
    }

    /// Metric attributes — a *narrower*, bounded set than the span (system + operation only).
    /// Metric cardinality must stay bounded (TD-0011 D2): `gen_ai.token.type` is the only extra
    /// dimension, and it has exactly two values (`input`, `output`).
    pub(crate) fn metric_attrs(system: &str) -> Vec<KeyValue> {
        vec![
            KeyValue::new("gen_ai.system", system.to_owned()),
            KeyValue::new("gen_ai.operation.name", OPERATION),
        ]
    }

    // ── the recorder ───────────────────────────────────────────────────────────────────────

    /// One `gen_ai.*` operation span + the three gen_ai metrics per logical call. Lives on
    /// `ProxyState` beside `Metrics` — it is disposable observability, NOT a durable `Sink`
    /// (TD-0011 principle 1).
    pub struct OtelRecorder {
        tracer: SdkTracer,
        token_usage: Counter<u64>,
        op_duration: Histogram<f64>,
        ttft: Histogram<f64>,
    }

    impl OtelRecorder {
        /// Build from an injected meter + tracer, so tests can wire in-memory exporters (no
        /// network) and the e2e test can observe exported data.
        pub fn new(meter: Meter, tracer: SdkTracer) -> Self {
            Self {
                tracer,
                token_usage: meter
                    .u64_counter("gen_ai.client.token.usage")
                    .with_unit("{token}")
                    .build(),
                op_duration: meter
                    .f64_histogram("gen_ai.client.operation.duration")
                    .with_unit("s")
                    .build(),
                ttft: meter
                    .f64_histogram("gen_ai.server.time_to_first_token")
                    .with_unit("s")
                    .build(),
            }
        }

        /// Open the gen_ai operation span at dispatch. Request-time attributes only; usage and
        /// response attributes are added when [`Self::record_usage`] closes it.
        pub fn start_span(&self, system: &str, request_model: &str) -> SpanHandle {
            let attrs = GenAiAttrs {
                system,
                request_model,
                operation_name: OPERATION,
            };
            let span = SpanBuilder::from_name(OPERATION)
                .with_kind(SpanKind::Client)
                .with_attributes(attrs.to_vec())
                .start(&self.tracer);
            SpanHandle { span, ended: false }
        }

        /// Set usage/response attributes, end the span, and record the gen_ai metrics. Called from
        /// `RequestAccounting::finalize` immediately beside `metrics.observe_call` — one OTel
        /// sample per logical call. All recording is best-effort: OTel must never fail the request.
        pub fn record_usage(
            &self,
            span: &mut SpanHandle,
            system: &str,
            model: &str,
            usage: &UsageV2,
        ) {
            span.span.set_attributes(usage_attributes(usage, model));

            let base = metric_attrs(system);
            // gen_ai.client.token.usage — input/output only; the cache split has no metric home
            // (it lives on span attributes). This metric is NOT the billable quantity.
            let mut input_attrs = base.clone();
            input_attrs.push(KeyValue::new("gen_ai.token.type", "input"));
            self.token_usage
                .add(usage.tokens_in + usage.cache_read_tokens, &input_attrs);
            let mut output_attrs = base.clone();
            output_attrs.push(KeyValue::new("gen_ai.token.type", "output"));
            self.token_usage.add(usage.tokens_out, &output_attrs);

            if let Some(ms) = usage.duration_ms {
                self.op_duration.record(ms as f64 / 1000.0, &base);
            }
            if let Some(ms) = usage.time_to_first_token_ms {
                self.ttft.record(ms as f64 / 1000.0, &base);
            }

            span.end();
        }
    }

    /// A started gen_ai span, ended exactly once. `Drop` ends it defensively (a panicked/abandoned
    /// finalize still closes the span rather than leaking an open one).
    pub struct SpanHandle {
        span: opentelemetry_sdk::trace::Span,
        ended: bool,
    }

    impl SpanHandle {
        fn end(&mut self) {
            if !self.ended {
                self.ended = true;
                self.span.end();
            }
        }
    }

    impl Drop for SpanHandle {
        fn drop(&mut self) {
            self.end();
        }
    }

    /// Holds the OTel providers; dropping flushes (best-effort). The proxy binary keeps one in
    /// scope for the lifetime of `serve()`, then drops it on shutdown.
    pub struct OtelGuard {
        tracer_provider: SdkTracerProvider,
        meter_provider: SdkMeterProvider,
    }

    impl OtelGuard {
        pub fn shutdown(&self) {
            let _ = self.tracer_provider.shutdown();
            let _ = self.meter_provider.shutdown();
        }
    }

    impl Drop for OtelGuard {
        fn drop(&mut self) {
            self.shutdown();
        }
    }

    /// Build the OTLP pipeline from `SANDHI_OTEL_*` env. Returns `None` when unconfigured, so a
    /// feature-on build with no endpoint behaves identically to the default build. The endpoint is
    /// the OTLP base URL (the exporter appends `/v1/traces` and `/v1/metrics`).
    pub fn init() -> Option<(Arc<OtelRecorder>, OtelGuard)> {
        if std::env::var("SANDHI_OTEL_EXPORT").as_deref() != Ok("otlp") {
            return None;
        }
        let endpoint = std::env::var("SANDHI_OTEL_ENDPOINT")
            .unwrap_or_else(|_| "http://localhost:4318".to_owned());
        let protocol = match std::env::var("SANDHI_OTEL_PROTOCOL").as_deref() {
            Ok("http/json") => Protocol::HttpJson,
            _ => Protocol::HttpBinary,
        };
        let resource = Resource::builder()
            .with_service_name("sandhi-proxy")
            .build();

        let span_exporter = SpanExporter::builder()
            .with_http()
            .with_endpoint(endpoint.as_str())
            .with_protocol(protocol)
            .build()
            .ok()?;
        let tracer_provider = SdkTracerProvider::builder()
            .with_batch_exporter(span_exporter)
            .with_resource(resource.clone())
            .build();
        let tracer = tracer_provider.tracer("sandhi-proxy");

        let metric_exporter = MetricExporter::builder()
            .with_http()
            .with_endpoint(endpoint.as_str())
            .with_protocol(protocol)
            .build()
            .ok()?;
        let meter_provider = SdkMeterProvider::builder()
            .with_periodic_exporter(metric_exporter)
            .with_resource(resource)
            .build();
        let meter = meter_provider.meter("sandhi-proxy");

        let recorder = OtelRecorder::new(meter, tracer);
        Some((
            Arc::new(recorder),
            OtelGuard {
                tracer_provider,
                meter_provider,
            },
        ))
    }
}

#[cfg(not(feature = "otel-otlp"))]
pub use disabled::{init, OtelGuard, OtelRecorder, SpanHandle};
#[cfg(feature = "otel-otlp")]
pub use enabled::{init, OtelGuard, OtelRecorder, SpanHandle};

#[cfg(all(test, feature = "otel-otlp"))]
mod tests {
    use super::enabled::{metric_attrs, usage_attributes, GenAiAttrs, OtelRecorder};
    use opentelemetry::metrics::MeterProvider as _;
    use opentelemetry::trace::TracerProvider as _;
    use opentelemetry::KeyValue;
    use opentelemetry_sdk::metrics::{InMemoryMetricExporter, PeriodicReader, SdkMeterProvider};
    use opentelemetry_sdk::trace::{InMemorySpanExporter, SdkTracerProvider};
    use sandhi_core::{UsageBasis, UsageCompleteness, UsageV2};

    /// Every string that must never appear on an exported span or metric — as a key or a value.
    const FORBIDDEN: &[&str] = &[
        "subject_id",
        "group_id",
        "session_id",
        "virtual_key_id",
        "request_id",
        "scope",
        "run_id",
        "step_id",
        "parent_id",
        "idempotency_key",
        "route",
        "trace_context",
        // measure-vs-price (ADR-0001): never dollars.
        "cost",
        "price",
        "usd",
        "dollar",
        "cents",
    ];

    fn usage_full() -> UsageV2 {
        UsageV2 {
            tokens_in: 40,
            tokens_out: 20,
            cache_creation_tokens: 5,
            cache_read_tokens: 60,
            reasoning_tokens: Some(0),
            upstream_request_id: Some("resp_upstream_1".into()),
            duration_ms: Some(1200),
            time_to_first_token_ms: Some(300),
            completeness: UsageCompleteness::Final,
            basis: UsageBasis::ProviderReported,
            ..Default::default()
        }
    }

    fn key_value_strings(attrs: &[KeyValue]) -> String {
        let mut s = String::new();
        for kv in attrs {
            s.push_str(&format!("{}={:?} ", kv.key.as_str(), kv.value));
        }
        s
    }

    #[test]
    fn request_attr_keys_are_allowlisted() {
        let attrs = GenAiAttrs {
            system: "openai",
            request_model: "gpt-x",
            operation_name: "chat",
        }
        .to_vec();
        let blob = key_value_strings(&attrs);
        for f in FORBIDDEN {
            assert!(
                !blob.contains(f),
                "forbidden token `{f}` leaked into gen_ai request attrs: {blob}"
            );
        }
        let keys: Vec<&str> = attrs.iter().map(|kv| kv.key.as_str()).collect();
        assert_eq!(
            keys,
            &[
                "gen_ai.system",
                "gen_ai.request.model",
                "gen_ai.operation.name"
            ]
        );
    }

    #[test]
    fn usage_attr_keys_are_allowlisted_and_values_correct() {
        let usage = usage_full();
        let attrs = usage_attributes(&usage, "gpt-x");
        let blob = key_value_strings(&attrs);
        for f in FORBIDDEN {
            assert!(
                !blob.contains(f),
                "forbidden token `{f}` in usage attrs: {blob}"
            );
        }
        let map: std::collections::HashMap<&str, &opentelemetry::Value> = attrs
            .iter()
            .map(|kv| (kv.key.as_str(), &kv.value))
            .collect();
        // input = fresh (40) + cache_read (60)
        assert_eq!(map["gen_ai.usage.input_tokens"].to_string(), "100");
        assert_eq!(map["gen_ai.usage.output_tokens"].to_string(), "20");
        assert_eq!(
            map["gen_ai.usage.cache_creation.input_tokens"].to_string(),
            "5"
        );
        assert_eq!(
            map["gen_ai.usage.cache_read.input_tokens"].to_string(),
            "60"
        );
        assert_eq!(map["gen_ai.response.model"].to_string(), "gpt-x");
        // response.id is the UPSTREAM id, never request_id.
        assert_eq!(map["gen_ai.response.id"].to_string(), "resp_upstream_1");
    }

    #[test]
    fn metric_attr_keys_are_allowlisted() {
        let attrs = metric_attrs("anthropic");
        let blob = key_value_strings(&attrs);
        for f in FORBIDDEN {
            assert!(
                !blob.contains(f),
                "forbidden token `{f}` in metric attrs: {blob}"
            );
        }
        assert_eq!(attrs.len(), 2); // system + operation only — the narrowest set
    }

    /// LOAD-BEARING (handoff): the exported gen_ai span carries NO attribution and NO cost, as a
    /// key or a value. Uses an in-memory span exporter; no network.
    #[test]
    fn exported_span_has_no_attribution_or_cost() {
        let exporter = InMemorySpanExporter::default();
        let provider = SdkTracerProvider::builder()
            .with_simple_exporter(exporter.clone())
            .build();
        let tracer = provider.tracer("sandhi-proxy");
        // A no-op meter is enough: this test inspects the span, not metrics.
        let meter = SdkMeterProvider::builder().build().meter("sandhi-proxy");
        let recorder = OtelRecorder::new(meter, tracer);

        let mut span = recorder.start_span("openai", "gpt-x");
        recorder.record_usage(&mut span, "openai", "gpt-x", &usage_full());
        // record_usage ends the span; the SimpleSpanProcessor exports synchronously on end. Read
        // before any shutdown — `SdkTracerProvider::shutdown` resets the in-memory exporter.
        drop(span);
        let spans = exporter.get_finished_spans().expect("span export");
        assert_eq!(spans.len(), 1, "exactly one gen_ai span per call");
        let span = &spans[0];
        let mut blob = span.name.to_string();
        blob.push(' ');
        blob.push_str(&key_value_strings(&span.attributes));
        for f in FORBIDDEN {
            assert!(
                !blob.contains(f),
                "forbidden token `{f}` leaked into exported span: {blob}"
            );
        }
        assert_eq!(span.name, "chat");
        assert_eq!(span.span_kind, opentelemetry::trace::SpanKind::Client);
    }

    /// The exported metric carries NO attribution, NO cost, and `gen_ai.token.type` is exactly
    /// {input, output} (no cache-split dimension leaked).
    #[tokio::test]
    async fn exported_metric_has_no_attribution_or_cost() {
        let exporter = InMemoryMetricExporter::default();
        let provider = SdkMeterProvider::builder()
            .with_reader(PeriodicReader::builder(exporter.clone()).build())
            .build();
        let meter = provider.meter("sandhi-proxy");
        // A tracer that produces no exported spans is fine here.
        let tracer = SdkTracerProvider::builder()
            .with_simple_exporter(InMemorySpanExporter::default())
            .build()
            .tracer("sandhi-proxy");
        let recorder = OtelRecorder::new(meter, tracer);

        let mut span = recorder.start_span("anthropic", "claude-x");
        recorder.record_usage(&mut span, "anthropic", "claude-x", &usage_full());
        drop(span);
        provider.force_flush().ok();

        let collected = exporter.get_finished_metrics().expect("metric export");
        let blob = format!("{collected:?}");
        // `Metric`/`MetricData` fields are `pub(crate)` in the SDK, so we cannot walk attributes by
        // hand — we scan the Debug blob. That collides with the SDK's own `ScopeMetrics` /
        // `InstrumentationScope` type names for the literal "scope", so "scope" is excluded here:
        // as a metric attribute KEY it is prevented structurally by the closed `metric_attrs`
        // helper (unit-tested above) and is scanned properly against the exported span above,
        // whose `SpanData.attributes` we DO walk (no SDK Debug noise there).
        for f in FORBIDDEN.iter().copied().filter(|f| *f != "scope") {
            assert!(
                !blob.contains(f),
                "forbidden token `{f}` leaked into exported metrics: {blob}"
            );
        }
        // The token-usage metric was recorded with both input and output dimensions…
        assert!(
            blob.contains("gen_ai.client.token.usage"),
            "metric missing: {blob}"
        );
        assert!(
            blob.contains("input") && blob.contains("output"),
            "missing token.type dims"
        );
        // …and the cache split has NO metric dimension — only span attributes.
        assert!(
            !blob.contains("cache"),
            "cache split leaked into the metric (token.type) dimension: {blob}"
        );
    }

    #[test]
    fn init_returns_none_when_unconfigured() {
        // The test harness does not set `SANDHI_OTEL_EXPORT`, so a feature-on build with no
        // exporter configured must behave exactly like the default (feature-off) build.
        assert!(super::init().is_none());
    }
}
