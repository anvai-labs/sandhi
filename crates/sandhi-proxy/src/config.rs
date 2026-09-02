//! Declarative desired-state config (`SANDHI_CONFIG`, e.g. `config/sandhi.json`) — meant to be
//! committed to git and reviewed like any other change. Backs `GET /admin/config` (diff/preview)
//! and `POST /admin/config/apply` (execute).
//!
//! **Additive-only by design**: apply creates and updates what's declared, and never deletes or
//! revokes anything missing from the file. A config edit removing an entry must never silently
//! cut off a live credential, budget, alert rule, or virtual key — that stays a deliberate,
//! separate action (the dashboard's Revoke/Ack buttons, or the `sandhi` CLI), not a side effect
//! of editing JSON.
//!
//! **No secrets in the file.** Provider credentials and alert webhook URLs are both effectively
//! bearer credentials — the config never carries them inline, only the *name* of an environment
//! variable to resolve them from at apply time (`secret_env` / `webhook_env`). An absent or unset
//! env var resolves to "no secret" (keyless provider / plain `log` alert channel), not an error —
//! that's the common case for a local Ollama upstream.
//!
//! Virtual keys are declared as *intent* (who should have access, at what scope), never as a
//! fixed value: the real `vk_...` secret only ever exists at mint time, stored server-side as a
//! hash. Applying a vkey entry either mints a new key (if none matching its identity exists yet)
//! or is a no-op (if one does) — it can never "update" a vkey's secret in place.
//!
//! Listener TLS is the one startup-only section: preview/apply reports it as
//! `restart required`, while process bootstrap validates and activates it before bind. Live
//! certificate replacement belongs to TD-0017 P2 and is not smuggled into desired-state apply.

use serde::Deserialize;
use std::path::PathBuf;

use sandhi_store::{AlertRuleRecord, VirtualKeyRecord};

#[derive(Debug, Clone, Deserialize, Default, PartialEq)]
pub struct SandhiFileConfig {
    /// Optional listener TLS configuration. Paths are reviewable configuration,
    /// while the private-key bytes remain outside this file.
    #[serde(default)]
    pub tls: Option<TlsEntry>,
    #[serde(default)]
    pub providers: Vec<ProviderEntry>,
    #[serde(default)]
    pub budgets: Vec<BudgetEntry>,
    #[serde(default)]
    pub alerts: Vec<AlertEntry>,
    #[serde(default)]
    pub vkeys: Vec<VkeyEntry>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct TlsEntry {
    /// PEM certificate chain, leaf first.
    pub cert: PathBuf,
    /// PEM PKCS#1, PKCS#8, or SEC1 private key matching the leaf certificate.
    pub key: PathBuf,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct ProviderEntry {
    pub provider: String,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub scheme: Option<String>,
    /// Env var to resolve the real secret from at apply time. Never the secret itself.
    #[serde(default)]
    pub secret_env: Option<String>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct BudgetEntry {
    pub scope: String,
    pub limit_tokens: u64,
    #[serde(default)]
    pub window: Option<String>,
    #[serde(default)]
    pub policy: Option<String>,
    #[serde(default)]
    pub alert_thresholds: Option<Vec<u8>>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct AlertEntry {
    pub scope: String,
    pub threshold_pct: u8,
    /// Env var holding a webhook URL. Absent/unset -> the plain `log` channel.
    #[serde(default)]
    pub webhook_env: Option<String>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct VkeyEntry {
    pub upstream: String,
    #[serde(default)]
    pub subject: Option<String>,
    #[serde(default)]
    pub group: Option<String>,
    #[serde(default)]
    pub models: Option<Vec<String>>,
    #[serde(default)]
    pub rate_limit_per_min: Option<u32>,
}

/// Resolve `channel` env var to a webhook string, or `"log"` when absent/unset. Mirrors
/// `SetBudgetRequest`'s zero-config default so an entry with no `webhook_env` behaves the same
/// as one that never mentioned alerting at all.
pub fn resolve_channel(
    webhook_env: &Option<String>,
    getenv: impl Fn(&str) -> Option<String>,
) -> String {
    match webhook_env.as_deref().and_then(&getenv) {
        Some(url) if !url.is_empty() => format!("webhook:{url}"),
        _ => "log".to_string(),
    }
}

/// Resolve `secret_env` to a secret value, or `""` (keyless) when absent/unset — the common case
/// for a local Ollama upstream, which has no auth at all.
pub fn resolve_secret(
    secret_env: &Option<String>,
    getenv: impl Fn(&str) -> Option<String>,
) -> String {
    secret_env.as_deref().and_then(getenv).unwrap_or_default()
}

/// An alert entry is already satisfied by an existing rule with the same (scope, threshold,
/// channel) — `POST /admin/alerts` has no upsert semantics (each call inserts a fresh row with a
/// new id), so apply must dedup itself rather than rely on the server.
pub fn alert_already_applied(
    existing: &[AlertRuleRecord],
    entry: &AlertEntry,
    channel: &str,
) -> bool {
    existing.iter().any(|r| {
        r.scope == entry.scope && r.threshold_pct == entry.threshold_pct && r.channel == channel
    })
}

/// A vkey entry is already satisfied by any *non-revoked* key matching its declared identity
/// (upstream, subject, group) — `POST /admin/keys/share` always mints a brand-new secret+id
/// regardless of whether an equivalent key exists, so apply must dedup itself. A revoked key
/// does NOT count as satisfying the entry (re-applying after a manual revoke re-mints — the
/// additive-only rule governs deletion, not re-creation of something the operator deliberately
/// revoked and now wants declared again).
pub fn vkey_already_applied(existing: &[VirtualKeyRecord], entry: &VkeyEntry) -> bool {
    existing.iter().any(|k| {
        k.revoked_at.is_none()
            && k.upstream_ref == entry.upstream
            && k.subject_id.as_deref() == entry.subject.as_deref()
            && k.group_id.as_deref() == entry.group.as_deref()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
        move |k| {
            pairs
                .iter()
                .find(|(n, _)| *n == k)
                .map(|(_, v)| v.to_string())
        }
    }

    #[test]
    fn parses_a_full_config_from_json() {
        let json = r#"{
            "tls": {"cert":"/run/tls/fullchain.pem","key":"/run/tls/privkey.pem"},
            "providers": [{"provider":"ollama","label":"default","base_url":"http://localhost:11434","secret_env":null}],
            "budgets": [{"scope":"group:platform","limit_tokens":1000000,"window":"monthly","policy":"block"}],
            "alerts": [{"scope":"group:platform","threshold_pct":90,"webhook_env":"SLACK_URL"}],
            "vkeys": [{"upstream":"ollama:default","subject":"alice","group":"platform","rate_limit_per_min":60}]
        }"#;
        let cfg: SandhiFileConfig = serde_json::from_str(json).unwrap();
        assert_eq!(
            cfg.tls.as_ref().map(|tls| tls.cert.as_path()),
            Some(std::path::Path::new("/run/tls/fullchain.pem"))
        );
        assert_eq!(cfg.providers.len(), 1);
        assert_eq!(cfg.budgets[0].limit_tokens, 1_000_000);
        assert_eq!(cfg.alerts[0].threshold_pct, 90);
        assert_eq!(cfg.vkeys[0].subject.as_deref(), Some("alice"));
    }

    #[test]
    fn missing_sections_default_to_empty_not_an_error() {
        let cfg: SandhiFileConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(cfg, SandhiFileConfig::default());
    }

    #[test]
    fn tls_certificate_and_key_are_an_atomic_pair() {
        let missing_key = r#"{"tls":{"cert":"/run/tls/fullchain.pem"}}"#;
        assert!(
            serde_json::from_str::<SandhiFileConfig>(missing_key).is_err(),
            "enabling TLS with only half of the identity must fail configuration parsing"
        );
    }

    #[test]
    fn secret_env_resolves_from_environment() {
        let getenv = env(&[("MY_KEY", "sk-real-secret")]);
        assert_eq!(
            resolve_secret(&Some("MY_KEY".into()), getenv),
            "sk-real-secret"
        );
    }

    #[test]
    fn absent_secret_env_is_keyless_not_an_error() {
        assert_eq!(resolve_secret(&None, |_| None), "");
    }

    #[test]
    fn unset_secret_env_is_keyless_not_an_error() {
        // Named, but not actually present in the environment — same as absent, never a hard error.
        assert_eq!(resolve_secret(&Some("NOT_SET".into()), |_| None), "");
    }

    #[test]
    fn webhook_env_resolves_to_webhook_channel() {
        let getenv = env(&[("SLACK", "https://hooks.slack.com/xyz")]);
        assert_eq!(
            resolve_channel(&Some("SLACK".into()), getenv),
            "webhook:https://hooks.slack.com/xyz"
        );
    }

    #[test]
    fn absent_webhook_env_defaults_to_log_channel() {
        assert_eq!(resolve_channel(&None, |_| None), "log");
    }

    fn alert_record(scope: &str, pct: u8, channel: &str) -> AlertRuleRecord {
        AlertRuleRecord {
            id: "alert_1".into(),
            scope: scope.into(),
            threshold_pct: pct,
            channel: channel.into(),
            created_at: "2026-01-01T00:00:00Z".into(),
            last_fired_at: None,
            acked_at: None,
        }
    }

    #[test]
    fn identical_alert_is_already_applied() {
        let existing = vec![alert_record("group:platform", 90, "log")];
        let entry = AlertEntry {
            scope: "group:platform".into(),
            threshold_pct: 90,
            webhook_env: None,
        };
        assert!(alert_already_applied(&existing, &entry, "log"));
    }

    #[test]
    fn different_threshold_is_not_already_applied() {
        let existing = vec![alert_record("group:platform", 90, "log")];
        let entry = AlertEntry {
            scope: "group:platform".into(),
            threshold_pct: 80,
            webhook_env: None,
        };
        assert!(!alert_already_applied(&existing, &entry, "log"));
    }

    fn vkey_record(
        upstream: &str,
        subject: Option<&str>,
        group: Option<&str>,
        revoked: bool,
    ) -> VirtualKeyRecord {
        VirtualKeyRecord {
            id: "key_1".into(),
            secret_hash: "hash".into(),
            upstream_ref: upstream.into(),
            subject_id: subject.map(String::from),
            group_id: group.map(String::from),
            models: None,
            budget_scope: None,
            expires_at: None,
            rate_limit_per_min: None,
            created_at: "2026-01-01T00:00:00Z".into(),
            revoked_at: if revoked {
                Some("2026-01-02T00:00:00Z".into())
            } else {
                None
            },
        }
    }

    #[test]
    fn matching_active_vkey_is_already_applied() {
        let existing = vec![vkey_record(
            "ollama:default",
            Some("alice"),
            Some("platform"),
            false,
        )];
        let entry = VkeyEntry {
            upstream: "ollama:default".into(),
            subject: Some("alice".into()),
            group: Some("platform".into()),
            models: None,
            rate_limit_per_min: None,
        };
        assert!(vkey_already_applied(&existing, &entry));
    }

    #[test]
    fn revoked_vkey_does_not_satisfy_the_entry_re_applies() {
        // A deliberate revoke should be re-mintable by re-applying config, not silently ignored.
        let existing = vec![vkey_record(
            "ollama:default",
            Some("alice"),
            Some("platform"),
            true,
        )];
        let entry = VkeyEntry {
            upstream: "ollama:default".into(),
            subject: Some("alice".into()),
            group: Some("platform".into()),
            models: None,
            rate_limit_per_min: None,
        };
        assert!(!vkey_already_applied(&existing, &entry));
    }

    #[test]
    fn different_subject_is_not_already_applied() {
        let existing = vec![vkey_record(
            "ollama:default",
            Some("alice"),
            Some("platform"),
            false,
        )];
        let entry = VkeyEntry {
            upstream: "ollama:default".into(),
            subject: Some("bob".into()),
            group: Some("platform".into()),
            models: None,
            rate_limit_per_min: None,
        };
        assert!(!vkey_already_applied(&existing, &entry));
    }
}
