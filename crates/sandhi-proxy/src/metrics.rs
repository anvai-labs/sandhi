//! Prometheus-format metrics for the gateway (TD-0011 P2).
//!
//! Hand-rolled on purpose, for two reasons.
//!
//! **The label discipline becomes a type, not a convention.** TD-0011 D2 closes the label set
//! because a metric keyed by `subject_id` or `session_id` is a memory leak with a dashboard
//! attached. A registry that accepts `&[(&str, &str)]` can only *test* that rule; [`Labels`] makes
//! a forbidden dimension **unrepresentable** — there is no field to put it in. The accompanying
//! test then guards the rendered output as a second line of defence.
//!
//! **No new dependencies.** `prometheus`/`metrics-exporter-prometheus` would pull a stack into the
//! proxy for what amounts to a few atomics and a text formatter. The repo already keeps SQLite out
//! of the bindings for the same reason (TD-0009 D1).
//!
//! Counters are unsampled (D6): anything derived from a settled call must reconcile with the meter.
//! Nothing here recounts tokens — [`Metrics::observe_call`] is handed the same `billable` quantity
//! the ledger settled (D3), so a metric can never disagree with what was charged.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::Arc;
use std::sync::Mutex;

/// Which plane served a request (ADR-0004 D1) — the adoption signal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Plane {
    Transparent,
    Translation,
}

impl Plane {
    fn as_str(self) -> &'static str {
        match self {
            Plane::Transparent => "transparent",
            Plane::Translation => "translation",
        }
    }
}

/// The complete, closed set of metric dimensions (TD-0011 D2).
///
/// Every field here is bounded by the catalog or by the code — never by traffic. There is
/// deliberately no field for `subject_id`, `group_id`, `session_id`, `virtual_key_id`,
/// `request_id`, or a budget scope: a scope may be `vk:<id>`, which is per-key and therefore
/// unbounded, so it is excluded for the same reason as the rest. Per-subject attribution lives in
/// the usage aggregate (TD-0009), which is bounded by an explicit cap with an overflow bucket.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Labels {
    pub provider: String,
    pub model: String,
    pub dialect: &'static str,
    pub plane: Plane,
    pub outcome: &'static str,
}

impl Labels {
    fn render(&self) -> String {
        format!(
            "provider=\"{}\",model=\"{}\",dialect=\"{}\",plane=\"{}\",outcome=\"{}\"",
            escape(&self.provider),
            escape(&self.model),
            self.dialect,
            self.plane.as_str(),
            self.outcome,
        )
    }
}

/// Prometheus label values escape backslash, double-quote and newline.
fn escape(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

/// Fixed duration buckets in milliseconds. Chosen for model calls, where sub-100ms is unheard of
/// and multi-second is normal — the default Prometheus buckets would waste most of their
/// resolution below 1s and lose all of it above 10s.
const DURATION_BUCKETS_MS: &[u64] = &[
    100, 250, 500, 1_000, 2_500, 5_000, 10_000, 30_000, 60_000, 120_000,
];

#[derive(Debug, Default, Clone)]
struct Histogram {
    /// Cumulative counts, parallel to `DURATION_BUCKETS_MS`, plus a final `+Inf` slot.
    buckets: Vec<u64>,
    sum: u64,
    count: u64,
}

impl Histogram {
    fn new() -> Self {
        Self {
            buckets: vec![0; DURATION_BUCKETS_MS.len() + 1],
            sum: 0,
            count: 0,
        }
    }

    fn record(&mut self, value_ms: u64) {
        if self.buckets.is_empty() {
            *self = Self::new();
        }
        // Prometheus buckets are cumulative: a value counts in its own bucket and every wider one.
        for (i, edge) in DURATION_BUCKETS_MS.iter().enumerate() {
            if value_ms <= *edge {
                self.buckets[i] += 1;
            }
        }
        *self.buckets.last_mut().expect("+Inf slot exists") += 1;
        self.sum += value_ms;
        self.count += 1;
    }
}

/// The token dimension a counter is measuring. Kept separate from [`Labels`] because it is a
/// property of the measurement, not of the request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum TokenKind {
    FreshInput,
    CacheCreation,
    CacheRead,
    Output,
    Reasoning,
    /// The ADR-0005 D4 quantity the ledger settled — the one a budget is enforced on.
    Billable,
}

impl TokenKind {
    fn as_str(self) -> &'static str {
        match self {
            TokenKind::FreshInput => "fresh_input",
            TokenKind::CacheCreation => "cache_creation",
            TokenKind::CacheRead => "cache_read",
            TokenKind::Output => "output",
            TokenKind::Reasoning => "reasoning",
            TokenKind::Billable => "billable",
        }
    }
}

/// One settled call's measurements, as the accounting path already computed them.
#[derive(Debug, Clone, Copy, Default)]
pub struct CallMeasurements {
    pub fresh_input: u64,
    pub cache_creation: u64,
    pub cache_read: u64,
    pub output: u64,
    pub reasoning: u64,
    /// Passed in, never recomputed here (TD-0011 D3).
    pub billable: u64,
    /// Whether any part of `billable` came from the byte fallback rather than the provider
    /// (TD-0013 D5). Lets an operator answer "how much of my settled spend was guessed?", which
    /// until now was visible only per-event in the sink.
    pub estimated: bool,
    pub duration_ms: Option<u64>,
    pub ttft_ms: Option<u64>,
}

#[derive(Debug, Default)]
struct Inner {
    requests: BTreeMap<Labels, u64>,
    tokens: BTreeMap<(Labels, TokenKind), u64>,
    duration: BTreeMap<Labels, Histogram>,
    ttft: BTreeMap<Labels, Histogram>,
    /// Enforcement counters, labelled by policy only — a budget scope is unbounded (see [`Labels`]).
    denied: BTreeMap<&'static str, u64>,
    rate_limited: BTreeMap<Labels, u64>,
    /// Billable tokens settled from an estimate rather than a provider measurement.
    estimated_tokens: BTreeMap<Labels, u64>,
    admitted_unmetered: u64,
    /// TD-0021 P4: a retry whose (vkey, idempotency-key) matched the window —
    /// the logical call was metered once; the duplicate event was dropped.
    idempotent_replays: u64,
    /// TD-0021 P4: a call carried an idempotency-key but dedup was unavailable
    /// (volatile arm) — counted, per D3's fail-toward-counting, and visible here.
    idempotent_fallbacks: u64,
    leases_reclaimed: u64,
    settle_failures: u64,
    settle_overshoot: u64,
    settle_overshoot_tokens: u64,
}

/// The gateway's metric registry. Cheap to share: one mutex around a few maps, touched once per
/// settled call rather than per byte.
#[derive(Debug, Default)]
pub struct Metrics {
    inner: Mutex<Inner>,
    /// Open streaming response bodies. Outside the registry lock on purpose: touched twice per
    /// stream by the body's own lifetime. TD-0014 D6 — no bound ships unobservable.
    streams_open: AtomicI64,
    /// Open TCP connections (TD-0014 P3). Same reasoning, same lifetime discipline.
    connections_open: AtomicI64,
    /// Connections refused by the total or per-IP connection caps (TD-0014 P3).
    /// An operator seeing a pinned `sandhi_connections_open` plus a rising shed
    /// counter is looking at finding-3's wedge or a genuine flood.
    connections_shed: AtomicU64,
}

impl Metrics {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// One streaming response body opened. Paired with [`stream_closed`](Self::stream_closed) by
    /// the body's drop — including client disconnects and graceful-drain cancellation, which is
    /// the point: those are the exits a per-request counter never sees.
    pub fn stream_opened(&self) {
        self.streams_open.fetch_add(1, Ordering::AcqRel);
    }

    pub fn stream_closed(&self) {
        self.streams_open.fetch_sub(1, Ordering::AcqRel);
    }

    /// Open a stream and return the drop guard that closes it. Prefer this over the pair —
    /// the guard is what makes disconnects and drain-cancellations balance the count.
    ///
    /// Takes `&Arc<Self>` and owns the Arc: the guard lives inside a streaming generator that
    /// also mutates request accounting, so a borrow would alias.
    pub fn stream_open_guard(self: &Arc<Self>) -> StreamOpenGuard {
        self.stream_opened();
        StreamOpenGuard {
            metrics: Arc::clone(self),
        }
    }

    /// Open a TCP connection and return the drop guard that closes its count.
    /// Lifetime discipline identical to the streams gauge: the guard lives with
    /// the connection task, so aborts and disconnects balance the count.
    pub fn connection_open_guard(self: &Arc<Self>) -> ConnectionOpenGuard {
        self.connections_open.fetch_add(1, Ordering::AcqRel);
        ConnectionOpenGuard {
            metrics: Arc::clone(self),
        }
    }

    /// One connection refused by a connection-level cap.
    pub fn connection_shed(&self) {
        self.connections_shed.fetch_add(1, Ordering::Relaxed);
    }

    /// Record one settled call. Called from the accounting path, which already holds the settled
    /// `billable` — so this cannot drift from what the ledger charged.
    pub fn observe_call(&self, labels: &Labels, m: CallMeasurements) {
        let Ok(mut inner) = self.inner.lock() else {
            return; // never let telemetry fail a request
        };
        *inner.requests.entry(labels.clone()).or_default() += 1;
        for (kind, value) in [
            (TokenKind::FreshInput, m.fresh_input),
            (TokenKind::CacheCreation, m.cache_creation),
            (TokenKind::CacheRead, m.cache_read),
            (TokenKind::Output, m.output),
            (TokenKind::Reasoning, m.reasoning),
            (TokenKind::Billable, m.billable),
        ] {
            if value > 0 {
                *inner.tokens.entry((labels.clone(), kind)).or_default() += value;
            }
        }
        if m.estimated && m.billable > 0 {
            *inner.estimated_tokens.entry(labels.clone()).or_default() += m.billable;
        }
        if let Some(ms) = m.duration_ms {
            inner
                .duration
                .entry(labels.clone())
                .or_insert_with(Histogram::new)
                .record(ms);
        }
        if let Some(ms) = m.ttft_ms {
            inner
                .ttft
                .entry(labels.clone())
                .or_insert_with(Histogram::new)
                .record(ms);
        }
    }

    /// A request refused by the per-key rate limiter (TD-0012 D6). Uses the standard bounded
    /// label set — never the virtual key, which is unbounded.
    pub fn record_rate_limited(&self, labels: &Labels) {
        if let Ok(mut inner) = self.inner.lock() {
            *inner.rate_limited.entry(labels.clone()).or_default() += 1;
        }
    }

    /// A reservation refused by a hard cap (`policy` is `block` or `warn`).
    pub fn record_denied(&self, policy: &'static str) {
        if let Ok(mut inner) = self.inner.lock() {
            *inner.denied.entry(policy).or_default() += 1;
        }
    }

    /// Admitted without a lease because the ledger errored under a `Warn` policy (ADR-0005 D6).
    pub fn record_admitted_unmetered(&self) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.admitted_unmetered += 1;
        }
    }

    /// TD-0021 P4 D1: a retry matched the dedup window — its duplicate usage event
    /// was dropped; the logical call was metered once (visible, not silent).
    pub fn record_idempotent_replay(&self) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.idempotent_replays += 1;
        }
    }

    /// TD-0021 P4 D3: dedup unavailable — the call was counted (the fallback the
    /// TD's acceptance criterion requires "counted in a metric").
    pub fn record_idempotent_fallback(&self) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.idempotent_fallbacks += 1;
        }
    }

    /// Expired leases reclaimed — the crash-recovery signal.
    pub fn record_leases_reclaimed(&self, count: u64) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.leases_reclaimed += count;
        }
    }

    /// A settle whose measured usage exceeded the ceiling it was admitted against (TD-0013 D6).
    ///
    /// The full amount is still settled — discarding it would lose a real measurement, which is
    /// the defect this TD exists to remove. What this counter buys is visibility: a sustained rate
    /// means reservation ceilings are systematically too tight, so caps are being enforced a call
    /// later than intended. `overshoot` is how far past the ceiling the call landed.
    pub fn observe_settle_overshoot(&self, overshoot: u64) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.settle_overshoot += 1;
            inner.settle_overshoot_tokens = inner.settle_overshoot_tokens.saturating_add(overshoot);
        }
    }

    /// A durable settle that did not land, leaving capacity reserved until its TTL.
    pub fn record_settle_failure(&self) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.settle_failures += 1;
        }
    }

    /// Render the Prometheus text exposition format.
    #[must_use]
    pub fn render(&self) -> String {
        let Ok(inner) = self.inner.lock() else {
            return String::new();
        };
        let mut out = String::new();

        out.push_str("# HELP sandhi_requests_total Model calls served, by transport dimension.\n");
        out.push_str("# TYPE sandhi_requests_total counter\n");
        for (labels, value) in &inner.requests {
            let _ = writeln!(out, "sandhi_requests_total{{{}}} {value}", labels.render());
        }

        out.push_str(
            "# HELP sandhi_tokens_total Neutral token units by kind — units only, never currency (ADR-0001).\n",
        );
        out.push_str("# TYPE sandhi_tokens_total counter\n");
        for ((labels, kind), value) in &inner.tokens {
            let _ = writeln!(
                out,
                "sandhi_tokens_total{{{},kind=\"{}\"}} {value}",
                labels.render(),
                kind.as_str()
            );
        }

        render_histogram(
            &mut out,
            "sandhi_request_duration_ms",
            "Wall-clock duration of a logical call, measured at the adapter boundary.",
            &inner.duration,
        );
        render_histogram(
            &mut out,
            "sandhi_time_to_first_token_ms",
            "Streams only: milliseconds to the first delivered item.",
            &inner.ttft,
        );

        let streams_open = self.streams_open.load(Ordering::Acquire);
        out.push_str("# HELP sandhi_streams_open Streaming response bodies currently open.\n");
        out.push_str("# TYPE sandhi_streams_open gauge\n");
        let _ = writeln!(out, "sandhi_streams_open {streams_open}");

        let connections_open = self.connections_open.load(Ordering::Acquire);
        out.push_str("# HELP sandhi_connections_open TCP connections currently served.\n");
        out.push_str("# TYPE sandhi_connections_open gauge\n");
        let _ = writeln!(out, "sandhi_connections_open {connections_open}");

        let connections_shed = self.connections_shed.load(Ordering::Acquire);
        out.push_str(
            "# HELP sandhi_connections_shed_total Connections refused by the connection caps.\n",
        );
        out.push_str("# TYPE sandhi_connections_shed_total counter\n");
        let _ = writeln!(out, "sandhi_connections_shed_total {connections_shed}");

        out.push_str(
            "# HELP sandhi_rate_limited_total Requests refused by the per-key rate limiter.\n",
        );
        out.push_str("# TYPE sandhi_rate_limited_total counter\n");
        for (labels, value) in &inner.rate_limited {
            let _ = writeln!(
                out,
                "sandhi_rate_limited_total{{{}}} {value}",
                labels.render()
            );
        }

        out.push_str(
            "# HELP sandhi_estimated_tokens_total Billable tokens settled from a byte estimate \
             rather than a provider measurement.\n",
        );
        out.push_str("# TYPE sandhi_estimated_tokens_total counter\n");
        for (labels, value) in &inner.estimated_tokens {
            let _ = writeln!(
                out,
                "sandhi_estimated_tokens_total{{{}}} {value}",
                labels.render()
            );
        }

        out.push_str(
            "# HELP sandhi_reservations_denied_total Calls refused before dispatch by a cap.\n",
        );
        out.push_str("# TYPE sandhi_reservations_denied_total counter\n");
        for (policy, value) in &inner.denied {
            let _ = writeln!(
                out,
                "sandhi_reservations_denied_total{{policy=\"{policy}\"}} {value}"
            );
        }

        for (name, help, value) in [
            (
                "sandhi_idempotent_replays_total",
                "Retries whose (vkey, idempotency-key) matched the window: one logical call, one event (TD-0021 P4 D1).",
                inner.idempotent_replays,
            ),
            (
                "sandhi_idempotent_fallbacks_total",
                "Calls carrying an idempotency-key metered without dedup (volatile arm): counted per D3, not dropped.",
                inner.idempotent_fallbacks,
            ),
            (
                "sandhi_admitted_unmetered_total",
                "Calls admitted without a lease after a ledger error under a Warn policy (fail-open).",
                inner.admitted_unmetered,
            ),
            (
                "sandhi_leases_reclaimed_total",
                "Expired leases reclaimed; a sustained rate means leases are leaking.",
                inner.leases_reclaimed,
            ),
            (
                "sandhi_settle_failures_total",
                "Durable settles that did not land, leaving capacity reserved until the lease TTL.",
                inner.settle_failures,
            ),
            (
                "sandhi_settle_overshoot_total",
                "Settles whose measured usage exceeded the ceiling they were admitted against.",
                inner.settle_overshoot,
            ),
            (
                "sandhi_settle_overshoot_tokens_total",
                "Tokens settled beyond the reserved ceiling; a sustained rate means ceilings are too tight and caps bind a call later than intended.",
                inner.settle_overshoot_tokens,
            ),
        ] {
            let _ = writeln!(out, "# HELP {name} {help}");
            let _ = writeln!(out, "# TYPE {name} counter");
            let _ = writeln!(out, "{name} {value}");
        }

        out
    }
}

fn render_histogram(
    out: &mut String,
    name: &str,
    help: &str,
    series: &BTreeMap<Labels, Histogram>,
) {
    let _ = writeln!(out, "# HELP {name} {help}");
    let _ = writeln!(out, "# TYPE {name} histogram");
    for (labels, hist) in series {
        let rendered = labels.render();
        for (i, edge) in DURATION_BUCKETS_MS.iter().enumerate() {
            let _ = writeln!(
                out,
                "{name}_bucket{{{rendered},le=\"{edge}\"}} {}",
                hist.buckets[i]
            );
        }
        let _ = writeln!(
            out,
            "{name}_bucket{{{rendered},le=\"+Inf\"}} {}",
            hist.buckets[DURATION_BUCKETS_MS.len()]
        );
        let _ = writeln!(out, "{name}_sum{{{rendered}}} {}", hist.sum);
        let _ = writeln!(out, "{name}_count{{{rendered}}} {}", hist.count);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn labels() -> Labels {
        Labels {
            provider: "openai".into(),
            model: "gpt-mock".into(),
            dialect: "openai",
            plane: Plane::Transparent,
            outcome: "success",
        }
    }

    /// TD-0011 D2's second line of defence. The first is that `Labels` has no field for a forbidden
    /// dimension, so this cannot regress without someone adding one — at which point this fails.
    #[test]
    fn idempotency_counters_render() {
        let m = Metrics::new();
        m.record_idempotent_replay();
        m.record_idempotent_replay();
        m.record_idempotent_fallback();
        let out = m.render();
        assert!(out.contains("sandhi_idempotent_replays_total 2"));
        assert!(out.contains("sandhi_idempotent_fallbacks_total 1"));
    }

    #[test]
    fn rendered_output_contains_no_unbounded_label() {
        let metrics = Metrics::new();
        metrics.observe_call(
            &labels(),
            CallMeasurements {
                fresh_input: 40,
                cache_read: 60,
                output: 20,
                billable: 120,
                duration_ms: Some(1_200),
                ttft_ms: Some(300),
                ..Default::default()
            },
        );
        metrics.record_denied("block");
        let text = metrics.render();

        for forbidden in [
            "subject_id",
            "group_id",
            "session_id",
            "virtual_key_id",
            "request_id",
            "scope",
            "vk_",
        ] {
            assert!(
                !text.contains(forbidden),
                "'{forbidden}' must never appear in metrics output (TD-0011 D2):\n{text}"
            );
        }
    }

    #[test]
    fn no_dollars_anywhere() {
        // ADR-0001: this repo emits neutral units. A price label would be the boundary breaking.
        let metrics = Metrics::new();
        metrics.observe_call(&labels(), CallMeasurements::default());
        let text = metrics.render().to_lowercase();
        for forbidden in ["cost", "price", "usd", "dollar", "cents"] {
            assert!(
                !text.contains(forbidden),
                "'{forbidden}' leaked into metrics"
            );
        }
    }

    #[test]
    fn billable_is_recorded_as_given_not_recomputed() {
        // TD-0011 D3: the accounting path passes the settled quantity; metrics must not re-derive
        // it, or the dashboard can disagree with the ledger (the #78 defect).
        let metrics = Metrics::new();
        metrics.observe_call(
            &labels(),
            CallMeasurements {
                fresh_input: 40,
                cache_read: 60,
                output: 20,
                billable: 999, // deliberately inconsistent with the parts
                ..Default::default()
            },
        );
        let text = metrics.render();
        assert!(
            text.contains("kind=\"billable\"} 999"),
            "billable must be recorded verbatim:\n{text}"
        );
    }

    #[test]
    fn histogram_buckets_are_cumulative_and_totals_agree() {
        let metrics = Metrics::new();
        for ms in [50_u64, 300, 3_000, 200_000] {
            metrics.observe_call(
                &labels(),
                CallMeasurements {
                    duration_ms: Some(ms),
                    ..Default::default()
                },
            );
        }
        let text = metrics.render();
        // 50 <= 100, so the first bucket holds exactly one sample.
        assert!(text.contains("le=\"100\"} 1"), "{text}");
        // 50 and 300 are both <= 500.
        assert!(text.contains("le=\"500\"} 2"), "{text}");
        // Everything lands in +Inf, including the 200s outlier beyond the last edge.
        assert!(text.contains("le=\"+Inf\"} 4"), "{text}");
        assert!(text.contains("_count{") && text.contains("} 4"), "{text}");
        // The sum series exists and carries the same bounded labels as the buckets.
        assert!(text.contains("_sum{provider=\"openai\""), "{text}");
        assert!(
            text.contains("_sum{provider=\"openai\",model=\"gpt-mock\""),
            "{text}"
        );
    }

    #[test]
    fn label_values_are_escaped() {
        let metrics = Metrics::new();
        metrics.observe_call(
            &Labels {
                provider: "weird\"provider".into(),
                model: "model\\x".into(),
                dialect: "openai",
                plane: Plane::Translation,
                outcome: "success",
            },
            CallMeasurements::default(),
        );
        let text = metrics.render();
        // A raw quote would break the exposition format and could inject a fake series.
        assert!(text.contains("weird\\\"provider"), "{text}");
        assert!(text.contains("model\\\\x"), "{text}");
    }
}

/// Drop guard for one open streaming body: decrements [`Metrics::stream_closed`] on drop.
#[derive(Debug)]
pub struct StreamOpenGuard {
    metrics: Arc<Metrics>,
}

impl Drop for StreamOpenGuard {
    fn drop(&mut self) {
        self.metrics.stream_closed();
    }
}

/// Drop guard for one open TCP connection: decrements
/// [`Metrics::connection_open_guard`] on drop.
#[derive(Debug)]
pub struct ConnectionOpenGuard {
    metrics: Arc<Metrics>,
}

impl Drop for ConnectionOpenGuard {
    fn drop(&mut self) {
        self.metrics.connections_open.fetch_sub(1, Ordering::AcqRel);
    }
}
