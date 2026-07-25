//! The usage **aggregate** — the one view over the [`UsageEvent`] atom (TD-0009).
//!
//! There is exactly one atom (an event per logical call) and everything an operator or a consumer
//! reads is a *fold* over it: a total, a ranking, a dashboard row, a per-session counter. A fold
//! defined more than once is a fold that will disagree with itself — #78 fixed three
//! implementations of "billable" that had drifted into a 2× gap between what the ledger charged
//! and what the dashboard showed.
//!
//! So the fold lives here, once, and its output [`UsageAggregateV1`] is a **boundary object**:
//! schema'd, digest-pinned, and drift-gated exactly like `UsageEvent`, because it crosses into the
//! dashboard, the `sandhi` CLI, both language bindings, and later the control plane. `sandhi-store`
//! re-exports the type and computes it in SQL as an *index* over this definition — never as a
//! second definition (TD-0009 D1/D1a/D6).

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::event::{billable_parts, UsageEvent};

/// The attribution dimension an aggregate is grouped by.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum Dimension {
    Subject,
    Group,
    Provider,
    Model,
    VirtualKey,
    Session,
    /// Everything folded into a single row.
    Total,
}

impl Dimension {
    /// Parse the short dimension name the transports speak (`/admin/usage?by=`, `sandhi usage
    /// --by`, both language bindings). One parser, because a dimension name resolved twice is a
    /// name that will eventually mean two things — the same argument that puts the fold here
    /// (TD-0009 D6). `None` for an unknown name; the caller raises its own dialect of error.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        Some(match name.trim().to_ascii_lowercase().as_str() {
            "subject" | "user" => Self::Subject,
            "group" => Self::Group,
            "provider" => Self::Provider,
            "model" => Self::Model,
            "key" | "virtual_key" => Self::VirtualKey,
            "session" => Self::Session,
            "total" => Self::Total,
            _ => return None,
        })
    }

    /// The canonical short name — the inverse of [`Dimension::parse`].
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Subject => "subject",
            Self::Group => "group",
            Self::Provider => "provider",
            Self::Model => "model",
            Self::VirtualKey => "virtual_key",
            Self::Session => "session",
            Self::Total => "total",
        }
    }

    /// The event field this dimension reads. `None` for [`Dimension::Total`].
    #[must_use]
    pub fn key_of(self, e: &UsageEvent) -> Option<String> {
        match self {
            Dimension::Subject => e.subject_id.clone(),
            Dimension::Group => e.group_id.clone(),
            Dimension::Provider => Some(e.provider.clone()),
            Dimension::Model => Some(e.model.clone()),
            Dimension::VirtualKey => e.virtual_key_id.clone(),
            Dimension::Session => e.session_id.clone(),
            Dimension::Total => Some(TOTAL_KEY.to_string()),
        }
    }
}

/// The key used when the dimension's field is absent on an event.
pub const NONE_KEY: &str = "(none)";
/// The key of the grand-total row.
pub const TOTAL_KEY: &str = "total";
/// The key every row beyond the cardinality cap folds into (TD-0009 D2).
pub const OVERFLOW_KEY: &str = "(overflow)";

/// Latency over the calls in an aggregate.
///
/// **Percentiles are approximate** — computed over the sampled calls counted by `samples`, which
/// is carried precisely so a consumer can see the approximation's weight instead of inferring a
/// confidence it does not have. Tokens are exact because budgets enforce on them; latency informs
/// (TD-0009 D3: approximate what informs, never what enforces).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct LatencySummary {
    /// Calls that reported a duration. Zero means the rest of this struct is meaningless.
    pub samples: u64,
    pub p50_ms: u64,
    pub p95_ms: u64,
    /// Time to first token, streams only; `samples` for it may be lower than `samples` above.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttft_p50_ms: Option<u64>,
}

/// One folded row: neutral units only, no dollars (ADR-0001).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct UsageAggregateV1 {
    /// The group key — a subject/group/provider/model/virtual key/session, or one of
    /// [`TOTAL_KEY`] / [`NONE_KEY`] / [`OVERFLOW_KEY`].
    pub key: String,
    pub calls: u64,
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub cache_creation_tokens: u64,
    pub cache_read_tokens: u64,
    pub reasoning_tokens: u64,
    /// The ADR-0005 D4 quantity budgets are enforced on, summed **per call**. Not derivable from
    /// the fields above: the reasoning fold is a per-call decision, so comparing summed reasoning
    /// against summed output gives a different — wrong — answer.
    pub billable_tokens: u64,
    /// Absent when no call in this row reported a duration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latency: Option<LatencySummary>,
}

impl UsageAggregateV1 {
    /// A zeroed row for `key`.
    #[must_use]
    pub fn empty(key: impl Into<String>) -> Self {
        Self {
            key: key.into(),
            ..Self::default()
        }
    }

    /// Fold one event into this row. The only place an aggregate grows.
    pub fn add(&mut self, e: &UsageEvent) {
        self.calls += 1;
        self.tokens_in += e.tokens_in;
        self.tokens_out += e.tokens_out;
        self.cache_creation_tokens += e.cache_creation_tokens;
        self.cache_read_tokens += e.cache_read_tokens;
        self.reasoning_tokens += e.reasoning_tokens.unwrap_or(0);
        self.billable_tokens += billable_parts(
            e.tokens_in,
            e.cache_creation_tokens,
            e.cache_read_tokens,
            e.tokens_out,
            e.reasoning_tokens.unwrap_or(0),
        );
    }
}

/// Default per-dimension key capacity before rows fold into [`OVERFLOW_KEY`].
pub const DEFAULT_CARDINALITY_CAP: usize = 1024;

/// Folds events into per-key aggregates for ONE dimension.
///
/// Bounded by design (TD-0009 D2): `subject_id` and `session_id` are unbounded in a long-lived
/// process, so an unbounded map is a memory leak. Beyond `cap` distinct keys, further *new* keys
/// fold into [`OVERFLOW_KEY`]. The failure mode is losing per-key detail, **never** losing the
/// sum — `total()` stays exact under eviction.
#[derive(Debug, Clone)]
pub struct UsageAggregator {
    dimension: Dimension,
    cap: usize,
    rows: BTreeMap<String, UsageAggregateV1>,
    durations: BTreeMap<String, Vec<u64>>,
    ttfts: BTreeMap<String, Vec<u64>>,
    total: UsageAggregateV1,
    total_durations: Vec<u64>,
    total_ttfts: Vec<u64>,
}

impl UsageAggregator {
    #[must_use]
    pub fn new(dimension: Dimension) -> Self {
        Self::with_cap(dimension, DEFAULT_CARDINALITY_CAP)
    }

    #[must_use]
    pub fn with_cap(dimension: Dimension, cap: usize) -> Self {
        Self {
            dimension,
            cap,
            rows: BTreeMap::new(),
            durations: BTreeMap::new(),
            ttfts: BTreeMap::new(),
            total: UsageAggregateV1::empty(TOTAL_KEY),
            total_durations: Vec::new(),
            total_ttfts: Vec::new(),
        }
    }

    /// Fold one event in.
    pub fn add(&mut self, e: &UsageEvent) {
        let raw = self.dimension.key_of(e).unwrap_or_else(|| NONE_KEY.into());
        // A key already tracked keeps its row even once the cap is reached; only NEW keys
        // beyond the cap fold into overflow, so the common case stays stable rather than
        // thrashing on whichever key arrived last.
        let key = if self.rows.contains_key(&raw) || self.rows.len() < self.cap {
            raw
        } else {
            OVERFLOW_KEY.to_string()
        };
        self.rows
            .entry(key.clone())
            .or_insert_with(|| UsageAggregateV1::empty(&key))
            .add(e);
        self.total.add(e);
        if let Some(d) = e.duration_ms {
            self.durations.entry(key.clone()).or_default().push(d);
            self.total_durations.push(d);
        }
        if let Some(t) = e.time_to_first_token_ms {
            self.ttfts.entry(key).or_default().push(t);
            self.total_ttfts.push(t);
        }
    }

    /// Rows, busiest first by billable tokens (ties broken by key for determinism).
    #[must_use]
    pub fn rows(&self) -> Vec<UsageAggregateV1> {
        let mut out: Vec<UsageAggregateV1> = self
            .rows
            .iter()
            .map(|(k, row)| {
                let mut row = row.clone();
                row.latency = summarize(
                    self.durations.get(k).map(Vec::as_slice).unwrap_or(&[]),
                    self.ttfts.get(k).map(Vec::as_slice).unwrap_or(&[]),
                );
                row
            })
            .collect();
        out.sort_by(|a, b| {
            b.billable_tokens
                .cmp(&a.billable_tokens)
                .then_with(|| a.key.cmp(&b.key))
        });
        out
    }

    /// The grand total — exact even when per-key rows overflowed.
    #[must_use]
    pub fn total(&self) -> UsageAggregateV1 {
        let mut t = self.total.clone();
        t.latency = summarize(&self.total_durations, &self.total_ttfts);
        t
    }
}

impl LatencySummary {
    /// Summarise raw samples. Public because the durable store computes latency from a bounded
    /// SQL sample and must use *this* percentile, not its own — same reason the billable formula
    /// is shared (TD-0009 D6). `None` when nothing reported a duration.
    #[must_use]
    pub fn from_samples(durations: &[u64], ttfts: &[u64]) -> Option<Self> {
        if durations.is_empty() {
            return None;
        }
        let mut d = durations.to_vec();
        d.sort_unstable();
        let mut t = ttfts.to_vec();
        t.sort_unstable();
        Some(Self {
            samples: d.len() as u64,
            p50_ms: percentile(&d, 50),
            p95_ms: percentile(&d, 95),
            ttft_p50_ms: (!t.is_empty()).then(|| percentile(&t, 50)),
        })
    }
}

fn summarize(durations: &[u64], ttfts: &[u64]) -> Option<LatencySummary> {
    LatencySummary::from_samples(durations, ttfts)
}

/// Nearest-rank percentile on a sorted slice (no interpolation — an operator panel does not need
/// it, and nearest-rank always returns an observed value rather than an invented one).
fn percentile(sorted: &[u64], p: u64) -> u64 {
    debug_assert!(!sorted.is_empty());
    let rank = ((p as f64 / 100.0) * sorted.len() as f64).ceil() as usize;
    sorted[rank.saturating_sub(1).min(sorted.len() - 1)]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::Backend;

    fn ev(subject: &str, tin: u64, tout: u64, creation: u64, read: u64) -> UsageEvent {
        UsageEvent::new(
            "r",
            "2026-07-25T00:00:00Z",
            "openai",
            "m",
            Backend::External,
        )
        .with_attribution(Some("vk".into()), Some(subject.into()), Some("g".into()))
        .with_tokens(tin, tout)
        .with_cache(creation, read)
    }

    #[test]
    fn fold_counts_the_d4_billable_quantity() {
        let mut agg = UsageAggregator::new(Dimension::Subject);
        agg.add(&ev("alice", 100, 20, 5, 10));
        agg.add(&ev("alice", 50, 10, 0, 5));
        let rows = agg.rows();
        assert_eq!(rows.len(), 1);
        // (100+5+10+20) + (50+0+5+10) = 135 + 65
        assert_eq!(rows[0].billable_tokens, 200);
        assert_eq!(rows[0].calls, 2);
        assert_eq!(rows[0].cache_creation_tokens, 5);
    }

    #[test]
    fn reasoning_folds_per_call_not_over_the_sums() {
        // Folded (4 <= 10, adds nothing) + unfolded (8 > 3, adds 8) = 20 + 21.
        let mut agg = UsageAggregator::new(Dimension::Total);
        agg.add(&ev("a", 10, 10, 0, 0).with_reasoning(Some(4)));
        agg.add(&ev("a", 10, 3, 0, 0).with_reasoning(Some(8)));
        let total = agg.total();
        assert_eq!(total.billable_tokens, 41);
        // Applying the fold to the summed columns would read 33 — the wrong answer this
        // per-call fold exists to avoid.
        assert_ne!(
            total.tokens_in + total.tokens_out + total.reasoning_tokens,
            41
        );
    }

    #[test]
    fn rows_rank_by_billable_and_break_ties_deterministically() {
        let mut agg = UsageAggregator::new(Dimension::Subject);
        agg.add(&ev("bob", 200, 40, 0, 5));
        agg.add(&ev("alice", 100, 20, 0, 5));
        agg.add(&ev("alice", 50, 10, 0, 5));
        let rows = agg.rows();
        assert_eq!(rows[0].key, "bob"); // 245 > 190
        assert_eq!(rows[0].billable_tokens, 245);
        assert_eq!(rows[1].billable_tokens, 190);
    }

    #[test]
    fn cardinality_cap_drops_detail_but_never_the_total() {
        let mut agg = UsageAggregator::with_cap(Dimension::Session, 2);
        for i in 0..10 {
            let mut e = ev("alice", 10, 5, 0, 0);
            e.session_id = Some(format!("s{i}"));
            agg.add(&e);
        }
        let rows = agg.rows();
        // 2 tracked keys + the overflow row.
        assert_eq!(rows.len(), 3);
        assert!(rows.iter().any(|r| r.key == OVERFLOW_KEY));
        // The sum survives eviction exactly: 10 calls x 15 billable.
        assert_eq!(agg.total().billable_tokens, 150);
        assert_eq!(agg.total().calls, 10);
        assert_eq!(rows.iter().map(|r| r.billable_tokens).sum::<u64>(), 150);
    }

    #[test]
    fn dimension_names_round_trip_through_one_parser() {
        for (name, want) in [
            ("subject", Dimension::Subject),
            ("user", Dimension::Subject),
            ("group", Dimension::Group),
            ("provider", Dimension::Provider),
            ("model", Dimension::Model),
            ("key", Dimension::VirtualKey),
            ("virtual_key", Dimension::VirtualKey),
            ("session", Dimension::Session),
            ("total", Dimension::Total),
            (" Total ", Dimension::Total),
        ] {
            assert_eq!(Dimension::parse(name), Some(want), "{name}");
        }
        assert_eq!(Dimension::parse("cost"), None);
        for d in [
            Dimension::Subject,
            Dimension::Group,
            Dimension::Provider,
            Dimension::Model,
            Dimension::VirtualKey,
            Dimension::Session,
            Dimension::Total,
        ] {
            assert_eq!(Dimension::parse(d.as_str()), Some(d));
        }
    }

    #[test]
    fn missing_dimension_field_lands_in_the_none_row() {
        let mut agg = UsageAggregator::new(Dimension::Session);
        agg.add(&ev("alice", 10, 5, 0, 0)); // no session_id
        assert_eq!(agg.rows()[0].key, NONE_KEY);
    }

    #[test]
    fn latency_is_summarised_only_when_reported() {
        let mut agg = UsageAggregator::new(Dimension::Total);
        agg.add(&ev("a", 10, 5, 0, 0)); // no latency
        assert!(agg.total().latency.is_none());

        for ms in [10_u64, 20, 30, 40, 100] {
            agg.add(&ev("a", 10, 5, 0, 0).with_latency(Some(ms), Some(ms / 2)));
        }
        let l = agg.total().latency.expect("durations were reported");
        assert_eq!(l.samples, 5);
        assert_eq!(l.p50_ms, 30); // nearest-rank over [10,20,30,40,100]
        assert_eq!(l.p95_ms, 100);
        assert_eq!(l.ttft_p50_ms, Some(15));
    }
}
