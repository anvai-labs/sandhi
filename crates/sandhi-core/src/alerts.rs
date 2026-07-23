//! Threshold alerts for budget scopes (TD-0003 P2, component 3).
//!
//! When a scope's windowed spend crosses a configured percentage of its limit, an [`Alert`] is
//! raised. Delivery is via two channels:
//! - **log** — always, cheap ([`LogAlerter`], or the registry's built-in log line).
//! - **webhook** — a best-effort, non-blocking HTTP POST to a rule-configured URL. The transport
//!   itself lives in the proxy (which owns the HTTP client + a tokio runtime); core defines only
//!   the [`WebhookSender`] trait so the registry stays free of network dependencies.
//!
//! Dedup is via `last_fired_at`: once a rule fires for a scope it is suppressed for a cooldown
//! window (so a busy scope at 85% does not spam). The durable `last_fired_at` is persisted by the
//! proxy (in `sandhi-store`'s `alert_rules` table) so the suppression survives a restart.
//!
//! Measure-vs-price boundary: alerts are over neutral tokens / percentages — no dollars, no SKU.

use std::collections::HashMap;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

/// How an alert is delivered.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum AlertChannel {
    /// Emit a log line (always — every fired alert is logged).
    Log,
    /// POST the alert JSON to this URL (best-effort, non-blocking).
    Webhook { url: String },
}

impl AlertChannel {
    /// The wire spelling stored alongside a rule (`log` / `webhook:<url>`).
    pub fn as_str(&self) -> String {
        match self {
            Self::Log => "log".into(),
            Self::Webhook { url } => format!("webhook:{url}"),
        }
    }

    /// Parse a `log` / `webhook:<url>` / bare `webhook` spelling. `webhook` without a URL parses
    /// to a Log channel (the caller is expected to supply the URL when creating the rule).
    pub fn parse(s: &str) -> Self {
        let s = s.trim();
        match s.to_ascii_lowercase().as_str() {
            "log" => Self::Log,
            _ if s.to_ascii_lowercase().starts_with("webhook:") => {
                let url = s
                    .split_once(':')
                    .map(|(_, u)| u.trim().to_string())
                    .unwrap_or_default();
                if url.is_empty() {
                    Self::Log
                } else {
                    Self::Webhook { url }
                }
            }
            "webhook" => Self::Log,
            _ => Self::Log,
        }
    }
}

/// A threshold alert rule for a budget scope. Neutral tokens only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AlertRule {
    /// Stable public id (`alert_<hex>`).
    pub id: String,
    /// The budget scope this rule watches (e.g. `group:platform`).
    pub scope: String,
    /// Fire when windowed spend reaches this percentage of the limit (0–100, inclusive).
    pub threshold_pct: u8,
    /// Delivery channel.
    pub channel: AlertChannel,
}

/// A fired alert — the payload logged / POSTed to a webhook.
#[derive(Debug, Clone, Serialize)]
pub struct Alert {
    pub rule_id: String,
    pub scope: String,
    pub threshold_pct: u8,
    pub spent: u64,
    pub limit: u64,
    /// Actual percentage at fire time (spent / limit * 100).
    pub pct: u64,
    pub message: String,
    /// RFC 3339 fire timestamp.
    pub fired_at: String,
}

impl Alert {
    fn from_rule(rule: &AlertRule, spent: u64, limit: u64, fired_at: &str) -> Self {
        let pct = (spent * 100).checked_div(limit.max(1)).unwrap_or(100);
        Self {
            rule_id: rule.id.clone(),
            scope: rule.scope.clone(),
            threshold_pct: rule.threshold_pct,
            spent,
            limit,
            pct,
            message: format!(
                "budget alert: scope '{}' at {}% ({} of {} tokens, threshold {}%)",
                rule.scope, pct, spent, limit, rule.threshold_pct
            ),
            fired_at: fired_at.into(),
        }
    }
}

/// Delivers a webhook alert. Implementations live in the proxy (the only place that owns an HTTP
/// client and a tokio runtime); core's registry calls this for [`AlertChannel::Webhook`] rules.
///
/// Implementations MUST be best-effort and non-blocking: a webhook failure must never break a
/// request.
pub trait WebhookSender: Send + Sync {
    fn send(&self, url: &str, alert: &Alert);
}

/// A no-op sender (default): webhook rules degrade to log-only when no transport is wired. Lets the
/// registry operate standalone in core (tests, the FFI path) without a network dependency.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopWebhookSender;

impl WebhookSender for NoopWebhookSender {
    fn send(&self, _url: &str, _alert: &Alert) {}
}

/// In-memory registry of alert rules + per-(scope, rule) `last_fired_at` dedup. Thread-safe (the
/// proxy holds it behind a `Mutex`). The live evaluation engine.
pub struct AlertRegistry {
    rules: Vec<AlertRule>,
    last_fired_at: HashMap<(String, String), String>,
    cooldown_secs: i64,
    webhook_sender: Box<dyn WebhookSender>,
}

impl std::fmt::Debug for AlertRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AlertRegistry")
            .field("rules", &self.rules)
            .field("cooldown_secs", &self.cooldown_secs)
            .finish_non_exhaustive()
    }
}

/// One hour — generous enough that a steady-state-at-threshold scope does not spam, short enough
/// that a window reset re-arms a rule within an operator-relevant window.
pub const DEFAULT_COOLDOWN_SECS: i64 = 3600;

impl Default for AlertRegistry {
    fn default() -> Self {
        Self::new(DEFAULT_COOLDOWN_SECS, Box::new(NoopWebhookSender))
    }
}

impl AlertRegistry {
    /// Build with an explicit cooldown (seconds) and webhook transport.
    pub fn new(cooldown_secs: i64, webhook_sender: Box<dyn WebhookSender>) -> Self {
        Self {
            rules: Vec::new(),
            last_fired_at: HashMap::new(),
            cooldown_secs,
            webhook_sender,
        }
    }

    /// Replace the webhook transport (used by the proxy to inject its tokio-backed sender).
    pub fn set_webhook_sender(&mut self, sender: Box<dyn WebhookSender>) {
        self.webhook_sender = sender;
    }

    pub fn rules(&self) -> &[AlertRule] {
        &self.rules
    }

    pub fn add_rule(&mut self, rule: AlertRule) {
        if !self.rules.iter().any(|r| r.id == rule.id) {
            self.rules.push(rule);
        }
    }

    pub fn remove_rule(&mut self, id: &str) -> bool {
        let before = self.rules.len();
        self.rules.retain(|r| r.id != id);
        self.last_fired_at.retain(|(_, rule_id), _| rule_id != id);
        self.rules.len() != before
    }

    /// Seed `last_fired_at` from durable storage (called on startup so dedup survives restart).
    pub fn set_last_fired_at(&mut self, scope: &str, rule_id: &str, fired_at: Option<&str>) {
        if let Some(ts) = fired_at {
            self.last_fired_at
                .insert((scope.into(), rule_id.into()), ts.into());
        }
    }

    /// The persisted-as-of timestamp for a rule, if it has fired (used to mirror back to storage).
    pub fn last_fired_at(&self, scope: &str, rule_id: &str) -> Option<&str> {
        self.last_fired_at
            .get(&(scope.into(), rule_id.into()))
            .map(String::as_str)
    }

    /// Evaluate a scope's windowed spend against every matching rule. Fires each rule whose
    /// threshold is crossed and whose cooldown has elapsed: logs always, POSTs for webhook
    /// channels (best-effort). Returns the rules that fired this call.
    pub fn evaluate(&mut self, scope: &str, spent: u64, limit: Option<u64>) -> Vec<Alert> {
        let Some(limit) = limit.filter(|l| *l > 0) else {
            return Vec::new();
        };
        let now = now_rfc3339();
        let now_ts = parse_ts(&now).unwrap_or(0);
        let mut fired = Vec::new();
        for rule in self.rules.iter().filter(|r| r.scope == scope) {
            let pct = spent * 100 / limit;
            if pct < u64::from(rule.threshold_pct) {
                continue;
            }
            let key = (scope.to_string(), rule.id.clone());
            if let Some(last) = self.last_fired_at.get(&key).and_then(|s| parse_ts(s)) {
                if now_ts.saturating_sub(last) < self.cooldown_secs {
                    continue; // suppressed by cooldown (last_fired_at dedup)
                }
            }
            let alert = Alert::from_rule(rule, spent, limit, &now);
            // Always log (cheap channel).
            log_alert(&alert);
            // Webhook channel: best-effort, non-blocking.
            if let AlertChannel::Webhook { url } = &rule.channel {
                self.webhook_sender.send(url, &alert);
            }
            self.last_fired_at.insert(key, now.clone());
            fired.push(alert);
        }
        fired
    }
}

/// Emit a single log line for a fired alert (the always-on `log` channel).
fn log_alert(alert: &Alert) {
    eprintln!("sandhi: {} (rule {})", alert.message, alert.rule_id);
}

fn now_rfc3339() -> String {
    use time::format_description::well_known::Rfc3339;
    time::OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_default()
}

/// Parse an RFC 3339 timestamp to Unix seconds. Returns 0 on failure (treated as "long ago").
fn parse_ts(s: &str) -> Option<i64> {
    use time::format_description::well_known::Rfc3339;
    time::OffsetDateTime::parse(s, &Rfc3339)
        .ok()
        .map(|t| t.unix_timestamp())
}

/// A thread-safe handle around an [`AlertRegistry`] — the shape the proxy stores.
pub type SharedAlertRegistry = Mutex<AlertRegistry>;

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(id: &str, scope: &str, pct: u8) -> AlertRule {
        AlertRule {
            id: id.into(),
            scope: scope.into(),
            threshold_pct: pct,
            channel: AlertChannel::Log,
        }
    }

    #[test]
    fn fires_once_at_threshold_then_suppressed_by_cooldown() {
        let mut reg = AlertRegistry::default();
        reg.add_rule(rule("a1", "group:x", 80));

        // At 80% → fires exactly once.
        let fired = reg.evaluate("group:x", 80, Some(100));
        assert_eq!(fired.len(), 1);
        assert_eq!(fired[0].pct, 80);

        // Immediately again at 90% → suppressed by cooldown (last_fired_at dedup).
        let fired = reg.evaluate("group:x", 90, Some(100));
        assert!(fired.is_empty(), "cooldown must suppress re-fire");

        // After rewinding last_fired_at past the cooldown, it fires again.
        let old = now_rfc3339();
        let past = {
            use time::format_description::well_known::Rfc3339;
            time::OffsetDateTime::parse(&old, &Rfc3339)
                .unwrap()
                .checked_sub(time::Duration::seconds(DEFAULT_COOLDOWN_SECS + 60))
                .unwrap()
                .format(&Rfc3339)
                .unwrap()
        };
        reg.set_last_fired_at("group:x", "a1", Some(&past));
        let fired = reg.evaluate("group:x", 90, Some(100));
        assert_eq!(fired.len(), 1);
    }

    #[test]
    fn below_threshold_does_not_fire() {
        let mut reg = AlertRegistry::default();
        reg.add_rule(rule("a1", "group:x", 80));
        assert!(reg.evaluate("group:x", 79, Some(100)).is_empty());
        // Crossing the boundary arms it.
        assert_eq!(reg.evaluate("group:x", 80, Some(100)).len(), 1);
    }

    #[test]
    fn no_limit_means_no_alerts() {
        let mut reg = AlertRegistry::default();
        reg.add_rule(rule("a1", "group:x", 80));
        assert!(reg.evaluate("group:x", 1_000_000, None).is_empty());
        assert!(reg.evaluate("group:x", 1_000_000, Some(0)).is_empty());
    }

    /// A recording webhook sender (test double): records deliveries and never errors. Shared via
    /// Arc so it can be both stored in the registry and inspected by the test.
    #[derive(Default, Clone)]
    struct RecordingSender(std::sync::Arc<Mutex<Vec<(String, String)>>>);
    impl WebhookSender for RecordingSender {
        fn send(&self, url: &str, alert: &Alert) {
            self.0
                .lock()
                .unwrap()
                .push((url.into(), alert.rule_id.clone()));
        }
    }

    #[test]
    fn webhook_channel_uses_sender_and_is_best_effort() {
        let rec = RecordingSender::default();
        let mut reg = AlertRegistry::new(DEFAULT_COOLDOWN_SECS, Box::new(rec.clone()));
        reg.add_rule(AlertRule {
            id: "w1".into(),
            scope: "group:x".into(),
            threshold_pct: 50,
            channel: AlertChannel::Webhook {
                url: "https://hooks.example/x".into(),
            },
        });
        let fired = reg.evaluate("group:x", 60, Some(100));
        assert_eq!(fired.len(), 1);
        let sent = rec.0.lock().unwrap().clone();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].0, "https://hooks.example/x");
        assert_eq!(sent[0].1, "w1");
    }

    #[test]
    fn channel_parse_round_trips() {
        assert_eq!(AlertChannel::parse("log"), AlertChannel::Log);
        match AlertChannel::parse("webhook:https://h") {
            AlertChannel::Webhook { url } => assert_eq!(url, "https://h"),
            _ => panic!("expected webhook"),
        }
        // Bare webhook with no URL degrades to log.
        assert_eq!(AlertChannel::parse("webhook"), AlertChannel::Log);
    }
}
