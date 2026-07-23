//! Budgets + rate limits — the **enforcement mechanism** (AnvaiOps ADR-0047 D4). Caps are in
//! neutral **tokens**, not dollars; pricing is a downstream commercial concern.
//!
//! A [`BudgetLedger`] tracks spend per scope (a virtual-key id or a group id) and answers "would
//! this call exceed the cap?" before the call, then records the actual usage after.
//!
//! TD-0003 P2 adds:
//! - **Windows** ([`Window`]) — the spend counter resets at `daily` / `monthly` / `total`
//!   boundaries (wall-clock based; a rolling 24h / 30d approximation for daily/monthly).
//! - **Policy** ([`Policy`]) — `Block` (hard reject, today's behavior) or `Warn` (soft cap: the
//!   request proceeds; a threshold alert subsystem does the notifying).
//! - **Reserve-then-reconcile against the projected max** — already present in P1; P2 preserves
//!   it so concurrent in-flight reservations cannot overspend a near-exhausted budget.
//!
//! The ledger is the **live enforcement surface** (in-memory). Durable spent-by-window aggregates
//! come from `usage_events` in `sandhi-store`; a proxy restart re-derives the window from those
//! events (the in-memory counter simply resets). No dollars anywhere.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

/// The window over which a budget's spend accrues before it resets. Neutral tokens only.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Window {
    /// Resets roughly every 24h (rolling).
    Daily,
    /// Resets roughly every 30 days (rolling). A precise calendar-month boundary is a follow-up;
    /// the rolling approximation is deterministic and cheap to enforce.
    Monthly,
    /// Never resets — the lifetime cap (P1 behavior).
    #[default]
    Total,
}

impl Window {
    /// Parse the wire/CLI spelling (`daily` / `monthly` / `total`). Unknown → `Total`.
    pub fn parse(s: &str) -> Self {
        match s.to_ascii_lowercase().as_str() {
            "daily" => Self::Daily,
            "monthly" => Self::Monthly,
            _ => Self::Total,
        }
    }

    /// The rolling duration after which the spend counter resets. `Total` never elapses.
    pub fn duration(self) -> Option<time::Duration> {
        match self {
            Self::Daily => Some(time::Duration::seconds(86_400)),
            Self::Monthly => Some(time::Duration::seconds(86_400 * 30)),
            Self::Total => None,
        }
    }

    /// Lowercase wire name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Daily => "daily",
            Self::Monthly => "monthly",
            Self::Total => "total",
        }
    }
}

/// How a scope reacts when a request's projected spend exceeds its cap.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Policy {
    /// Hard reject — the request is not forwarded (today's behavior).
    #[default]
    Block,
    /// Soft cap — the request is forwarded and a threshold alert is expected to fire (the alert
    /// subsystem, not the ledger, does the notifying).
    Warn,
}

impl Policy {
    /// Parse the wire/CLI spelling (`block` / `warn`). Unknown → `Block`.
    pub fn parse(s: &str) -> Self {
        match s.to_ascii_lowercase().as_str() {
            "warn" => Self::Warn,
            _ => Self::Block,
        }
    }

    /// Lowercase wire name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Block => "block",
            Self::Warn => "warn",
        }
    }
}

/// A per-scope cap.
#[derive(Debug, Clone, Copy, Default)]
pub struct Budget {
    /// Max billable tokens over the ledger's window. `None` = unlimited.
    pub token_limit: Option<u64>,
    /// The window over which the counter accrues (TD-0003 P2).
    pub window: Window,
    /// The enforcement policy (TD-0003 P2).
    pub policy: Policy,
}

impl Budget {
    /// A plain `total` / `block` budget (the P1 shape — preserves existing callers).
    pub fn tokens(limit: u64) -> Self {
        Self {
            token_limit: Some(limit),
            window: Window::Total,
            policy: Policy::Block,
        }
    }

    /// A fully-specified budget (window + policy).
    pub fn with(limit: u64, window: Window, policy: Policy) -> Self {
        Self {
            token_limit: Some(limit),
            window,
            policy,
        }
    }
}

/// Raised when a call would exceed a scope's budget under the `Block` policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BudgetExceeded {
    pub scope: String,
    pub limit: u64,
    pub spent: u64,
    pub requested: u64,
}

/// Tracks spend per scope and enforces caps. In-memory — the **live enforcement surface**. Durable
/// aggregates come from `usage_events` in `sandhi-store`; window-reset state lives here and is
/// re-derived from events on restart (or simply reset — see TD-0003 P2 notes).
#[derive(Debug, Default)]
pub struct BudgetLedger {
    limits: HashMap<String, Budget>,
    spent: HashMap<String, u64>,
    reserved: HashMap<String, u64>,
    /// Wall-clock instant the current window started for this scope (lazily set on first use).
    window_started_at: HashMap<String, OffsetDateTime>,
}

impl BudgetLedger {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_limit(&mut self, scope: impl Into<String>, budget: Budget) {
        let scope = scope.into();
        // (Re)setting a limit starts a fresh window for the scope.
        self.window_started_at
            .insert(scope.clone(), OffsetDateTime::now_utc());
        self.limits.insert(scope, budget);
    }

    /// The configured limit for a scope, if any.
    pub fn limit_of(&self, scope: &str) -> Option<u64> {
        self.limits.get(scope).and_then(|b| b.token_limit)
    }

    /// The configured window for a scope (defaults to `Total` when unset).
    pub fn window_of(&self, scope: &str) -> Window {
        self.limits.get(scope).map(|b| b.window).unwrap_or_default()
    }

    /// The configured policy for a scope (defaults to `Block` when unset).
    pub fn policy_of(&self, scope: &str) -> Policy {
        self.limits.get(scope).map(|b| b.policy).unwrap_or_default()
    }

    /// Spent tokens in the current window for this scope. Refreshes the window first (so a
    /// boundary that elapsed since the last mutation is observed).
    pub fn spent(&mut self, scope: &str) -> u64 {
        self.refresh(scope);
        self.spent.get(scope).copied().unwrap_or(0)
    }

    /// Tokens held by in-flight requests for this scope.
    pub fn reserved(&self, scope: &str) -> u64 {
        self.reserved.get(scope).copied().unwrap_or(0)
    }

    /// Would `add` billable tokens exceed the scope's cap? Honors the policy: a `Warn` scope never
    /// rejects (it allows + expects the alert subsystem to notify). An unset/unlimited scope always
    /// passes. Enforcement only — no pricing.
    pub fn check(&mut self, scope: &str, add: u64) -> Result<(), BudgetExceeded> {
        self.refresh(scope);
        let Some(budget) = self.limits.get(scope).copied() else {
            return Ok(());
        };
        let Some(limit) = budget.token_limit else {
            return Ok(());
        };
        let spent = self.spent.get(scope).copied().unwrap_or(0);
        let reserved = self.reserved.get(scope).copied().unwrap_or(0);
        let projected = spent.saturating_add(reserved).saturating_add(add);
        if projected > limit && budget.policy == Policy::Block {
            return Err(BudgetExceeded {
                scope: scope.to_string(),
                limit,
                spent,
                requested: add,
            });
        }
        Ok(())
    }

    /// Atomically check and reserve an upper bound for one in-flight request. Under the `Warn`
    /// policy the reservation is always accepted (the request proceeds).
    pub fn reserve(&mut self, scope: &str, tokens: u64) -> Result<(), BudgetExceeded> {
        self.check(scope, tokens)?;
        *self.reserved.entry(scope.to_string()).or_insert(0) += tokens;
        Ok(())
    }

    /// Replace an in-flight reservation with the actual finalized billable usage.
    pub fn reconcile(&mut self, scope: &str, reserved: u64, actual: u64) {
        self.refresh(scope);
        let held = self.reserved.entry(scope.to_string()).or_insert(0);
        *held = held.saturating_sub(reserved);
        self.record_internal(scope, actual);
    }

    /// Release a reservation when a call fails before any billable usage is reported.
    pub fn release(&mut self, scope: &str, reserved: u64) {
        let held = self.reserved.entry(scope.to_string()).or_insert(0);
        *held = held.saturating_sub(reserved);
    }

    /// Record actual usage after a call (call once the real token counts are known).
    pub fn record(&mut self, scope: &str, tokens: u64) {
        self.refresh(scope);
        self.record_internal(scope, tokens);
    }

    fn record_internal(&mut self, scope: &str, tokens: u64) {
        *self.spent.entry(scope.to_string()).or_insert(0) += tokens;
    }

    /// If the scope's window has elapsed, reset its spent counter (reserved is in-flight and kept)
    /// and start a new window. Cheap no-op for `Total` / unset scopes.
    fn refresh(&mut self, scope: &str) {
        let Some(budget) = self.limits.get(scope).copied() else {
            return;
        };
        let Some(duration) = budget.window.duration() else {
            return; // Total: never resets.
        };
        let now = OffsetDateTime::now_utc();
        let started = self
            .window_started_at
            .entry(scope.to_string())
            .or_insert(now);
        if now - *started >= duration {
            self.spent.insert(scope.to_string(), 0);
            *started = now;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::Duration;

    #[test]
    fn unset_scope_is_unlimited() {
        let mut ledger = BudgetLedger::new();
        assert!(ledger.check("vk_1", 1_000_000).is_ok());
    }

    #[test]
    fn enforces_cap_and_tracks_spend() {
        let mut ledger = BudgetLedger::new();
        ledger.set_limit("group:team", Budget::tokens(100));

        assert!(ledger.check("group:team", 60).is_ok());
        ledger.record("group:team", 60);
        assert_eq!(ledger.spent("group:team"), 60);

        // 60 spent + 50 would be 110 > 100 → blocked (block policy).
        let err = ledger.check("group:team", 50).unwrap_err();
        assert_eq!(err.spent, 60);
        assert_eq!(err.limit, 100);
        assert_eq!(err.requested, 50);

        // 60 + 40 == 100 is allowed (cap is inclusive).
        assert!(ledger.check("group:team", 40).is_ok());
    }

    #[test]
    fn reservations_prevent_concurrent_oversubscription_and_reconcile() {
        // Reserve-then-reconcile against the projected max: two in-flight requests that together
        // would exceed the cap cannot both reserve, so concurrent calls can't overspend.
        let mut ledger = BudgetLedger::new();
        ledger.set_limit("group:team", Budget::tokens(100));
        ledger.reserve("group:team", 60).unwrap();
        assert_eq!(ledger.reserved("group:team"), 60);
        // 60 reserved + 50 = 110 > 100 → the second reservation is refused before dispatch.
        assert!(ledger.reserve("group:team", 50).is_err());

        ledger.reconcile("group:team", 60, 40);
        assert_eq!(ledger.reserved("group:team"), 0);
        assert_eq!(ledger.spent("group:team"), 40);
        // After the first call reconciles at 40, a 60 reservation fits (40 + 60 == 100).
        ledger.reserve("group:team", 60).unwrap();
        ledger.release("group:team", 60);
        assert_eq!(ledger.reserved("group:team"), 0);
    }

    #[test]
    fn warn_policy_allows_over_limit_without_rejecting() {
        let mut ledger = BudgetLedger::new();
        ledger.set_limit("group:soft", Budget::with(100, Window::Total, Policy::Warn));
        // Reserve up to the limit.
        assert!(ledger.reserve("group:soft", 60).is_ok());
        // Projected spend exceeds the cap (60 + 60 = 120 > 100) but Warn allows it.
        assert!(ledger.reserve("group:soft", 60).is_ok());
        // A block-policy scope with the same spend would reject.
        ledger.set_limit(
            "group:hard",
            Budget::with(100, Window::Total, Policy::Block),
        );
        assert!(ledger.reserve("group:hard", 60).is_ok());
        assert!(ledger.reserve("group:hard", 60).is_err());
    }

    #[test]
    fn daily_window_resets_at_boundary() {
        let mut ledger = BudgetLedger::new();
        ledger.set_limit("group:d", Budget::with(1000, Window::Daily, Policy::Block));
        ledger.record("group:d", 800);
        assert_eq!(ledger.spent("group:d"), 800);

        // Force the window into the past (2 days ago) and record again → counter resets, then adds.
        *ledger.window_started_at.get_mut("group:d").unwrap() =
            OffsetDateTime::now_utc() - Duration::days(2);
        ledger.record("group:d", 50);
        assert_eq!(ledger.spent("group:d"), 50);

        // A pre-flight check now sees only the post-reset spend (check is read-only: each call is
        // independent against the current counter).
        assert!(ledger.check("group:d", 950).is_ok()); // 50 + 950 = 1000 <= 1000
        assert!(ledger.check("group:d", 951).is_err()); // 50 + 951 > 1000
    }

    #[test]
    fn monthly_window_resets_after_30_days() {
        let mut ledger = BudgetLedger::new();
        ledger.set_limit(
            "group:m",
            Budget::with(10_000, Window::Monthly, Policy::Block),
        );
        ledger.record("group:m", 5_000);
        // 29 days later → no reset.
        *ledger.window_started_at.get_mut("group:m").unwrap() =
            OffsetDateTime::now_utc() - Duration::days(29);
        assert_eq!(ledger.spent("group:m"), 5_000);
        // 31 days later → reset.
        *ledger.window_started_at.get_mut("group:m").unwrap() =
            OffsetDateTime::now_utc() - Duration::days(31);
        assert_eq!(ledger.spent("group:m"), 0);
    }

    #[test]
    fn total_window_never_resets() {
        let mut ledger = BudgetLedger::new();
        ledger.set_limit("group:t", Budget::with(1000, Window::Total, Policy::Block));
        ledger.record("group:t", 900);
        // Even a year later, total does not reset.
        *ledger.window_started_at.get_mut("group:t").unwrap() =
            OffsetDateTime::now_utc() - Duration::days(365);
        assert_eq!(ledger.spent("group:t"), 900);
    }

    #[test]
    fn window_and_policy_parse_round_trip() {
        assert_eq!(Window::parse("daily"), Window::Daily);
        assert_eq!(Window::parse("MONTHLY"), Window::Monthly);
        assert_eq!(Window::parse("nope"), Window::Total);
        assert_eq!(Policy::parse("warn"), Policy::Warn);
        assert_eq!(Policy::parse("x"), Policy::Block);
        assert_eq!(Window::Daily.as_str(), "daily");
        assert_eq!(Policy::Warn.as_str(), "warn");
    }

    #[test]
    fn accessors_default_for_unset_scope() {
        let ledger = BudgetLedger::new();
        assert_eq!(ledger.limit_of("none"), None);
        assert_eq!(ledger.window_of("none"), Window::Total);
        assert_eq!(ledger.policy_of("none"), Policy::Block);
    }
}
