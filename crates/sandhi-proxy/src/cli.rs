//! `sandhi` — the TD-0003 operator CLI.
//!
//! A thin HTTP client to the proxy's `/admin/*` REST API. It holds no database state of its own;
//! every subcommand is one authenticated call against a running `sandhi-proxy`. Configure the
//! target with `--admin-url` (env `SANDHI_ADMIN_URL`) and `--admin-token` (env
//! `SANDHI_ADMIN_TOKEN`).
//!
//! The arg→HTTP mapping lives in [`admin_request`] so it can be unit-tested without a network.

use std::io::{BufRead, IsTerminal};
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use serde_json::{json, Value};

use sandhi_proxy::admin;

#[derive(Parser, Debug)]
#[command(
    name = "sandhi",
    version,
    about = "Sandhi operator CLI — keys, virtual keys, budgets, usage (TD-0003)"
)]
struct Cli {
    /// Base URL of the sandhi-proxy admin API.
    #[arg(
        long,
        env = "SANDHI_ADMIN_URL",
        default_value = "http://localhost:8787"
    )]
    admin_url: String,

    /// Admin bearer token (distinct from virtual keys).
    #[arg(long, env = "SANDHI_ADMIN_TOKEN")]
    admin_token: Option<String>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Provider credential vault.
    Keys {
        #[command(subcommand)]
        action: KeysAction,
    },
    /// Virtual keys (share / list / revoke).
    Vkeys {
        #[command(subcommand)]
        action: VkeysAction,
    },
    /// Neutral-token budgets.
    Budget {
        #[command(subcommand)]
        action: BudgetAction,
    },
    /// Attribution / usage aggregates.
    Usage {
        /// Dimension to aggregate by.
        #[arg(long, default_value = "subject")]
        by: String,
        /// RFC 3339 lower-bound (inclusive).
        #[arg(long)]
        since: Option<String>,
        /// Output format.
        #[arg(long, default_value = "table")]
        format: Format,
    },
    /// Threshold alert rules (P2).
    Alerts {
        #[command(subcommand)]
        action: AlertsAction,
    },
}

#[derive(Subcommand, Debug)]
enum KeysAction {
    /// Add a provider credential to the vault.
    Add {
        /// Provider slug (anthropic, openai, gemini, …).
        provider: String,
        /// Credential label (default `default`).
        label: Option<String>,
        /// Auth scheme.
        #[arg(long)]
        scheme: Option<String>,
        /// Override the upstream base URL.
        #[arg(long)]
        base_url: Option<String>,
        /// The raw secret. If omitted, read one line from stdin (keeps it out of shell history).
        #[arg(long)]
        secret: Option<String>,
    },
    /// List provider credentials (masked).
    List,
    /// Revoke a provider credential.
    Revoke { provider: String, label: String },
    /// Mint a scoped virtual key (printed once).
    Share {
        /// Upstream credential id (`provider:label`).
        upstream: String,
        #[arg(long)]
        subject: Option<String>,
        #[arg(long)]
        group: Option<String>,
        /// Comma-separated model allowlist.
        #[arg(long)]
        models: Option<String>,
        /// Explicit budget scope (e.g. `group:platform`).
        #[arg(long)]
        budget: Option<String>,
        /// RFC 3339 expiry.
        #[arg(long)]
        expires: Option<String>,
        /// Rate limit (requests/min, stored — enforcement is P2).
        #[arg(long)]
        rate: Option<u32>,
    },
}

#[derive(Subcommand, Debug)]
enum VkeysAction {
    /// List virtual keys (masked).
    List,
    /// Revoke a virtual key by public id.
    Revoke { id: String },
}

#[derive(Subcommand, Debug)]
enum BudgetAction {
    /// Set a neutral-token budget on a scope.
    Set {
        scope: String,
        limit_tokens: u64,
        #[arg(long, default_value = "total")]
        window: String,
        #[arg(long, default_value = "block")]
        policy: String,
        /// Threshold percentages (0–100) that each create a log-channel alert rule (P2). Repeatable.
        #[arg(long)]
        alert: Vec<u8>,
    },
    /// List configured budgets.
    List,
    /// Spent-vs-limit for a scope.
    Usage { scope: String },
}

#[derive(Subcommand, Debug)]
enum AlertsAction {
    /// List threshold alert rules.
    List {
        /// Filter to a scope.
        #[arg(long)]
        scope: Option<String>,
    },
    /// Acknowledge a fired alert by id.
    Ack { id: String },
}

#[derive(clap::ValueEnum, Clone, Debug)]
enum Format {
    Table,
    Json,
}

/// A mapped admin API request (testable without a network).
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct AdminRequest {
    pub method: &'static str,
    pub path: String,
    pub body: Option<String>,
}

/// Pure mapping from a parsed CLI command to the admin API call. Public to the crate's tests.
pub(crate) fn admin_request(base_url: &str, command: &Command) -> AdminRequest {
    let (method, path, body): (&str, String, Option<Value>) = match command {
        Command::Keys {
            action:
                KeysAction::Add {
                    provider,
                    label,
                    scheme,
                    base_url: bu,
                    secret,
                },
        } => {
            let body = json!({
                "provider": provider,
                "label": label,
                "scheme": scheme,
                "base_url": bu,
                "secret": secret,
            });
            ("POST", "/admin/keys".into(), Some(body))
        }
        Command::Keys {
            action: KeysAction::List,
        } => ("GET", "/admin/keys".into(), None),
        Command::Keys {
            action: KeysAction::Revoke { provider, label },
        } => ("DELETE", format!("/admin/keys/{provider}/{label}"), None),
        Command::Keys {
            action:
                KeysAction::Share {
                    upstream,
                    subject,
                    group,
                    models,
                    budget,
                    expires,
                    rate,
                },
        } => {
            let body = json!({
                "upstream": upstream,
                "subject": subject,
                "group": group,
                "models": models.as_deref().map(csv_to_list),
                "budget_scope": budget,
                "expires_at": expires,
                "rate_limit_per_min": rate,
            });
            ("POST", "/admin/keys/share".into(), Some(body))
        }
        Command::Vkeys {
            action: VkeysAction::List,
        } => ("GET", "/admin/keys/virtual".into(), None),
        Command::Vkeys {
            action: VkeysAction::Revoke { id },
        } => ("DELETE", format!("/admin/vkeys/{id}"), None),
        Command::Budget {
            action:
                BudgetAction::Set {
                    scope,
                    limit_tokens,
                    window,
                    policy,
                    alert,
                },
        } => {
            let body = json!({
                "scope": scope,
                "limit_tokens": limit_tokens,
                "window": window,
                "policy": policy,
                "alert_thresholds": if alert.is_empty() { None } else { Some(alert) },
            });
            ("POST", "/admin/budget".into(), Some(body))
        }
        Command::Budget {
            action: BudgetAction::List,
        } => ("GET", "/admin/budget".into(), None),
        Command::Budget {
            action: BudgetAction::Usage { scope },
        } => ("GET", format!("/admin/budget/usage?scope={scope}"), None),
        Command::Usage {
            by,
            since,
            format: _,
        } => {
            let mut path = format!("/admin/usage?by={by}");
            if let Some(since) = since {
                path.push_str(&format!("&since={since}"));
            }
            ("GET", path, None)
        }
        Command::Alerts {
            action: AlertsAction::List { scope },
        } => {
            let path = match scope {
                Some(s) => format!("/admin/alerts?scope={s}"),
                None => "/admin/alerts".into(),
            };
            ("GET", path, None)
        }
        Command::Alerts {
            action: AlertsAction::Ack { id },
        } => ("POST", format!("/admin/alerts/{id}/ack"), None),
    };
    AdminRequest {
        method,
        path: format!("{}{}", base_url.trim_end_matches('/'), path),
        body: body.map(|v| v.to_string()),
    }
}

fn csv_to_list(csv: &str) -> Vec<String> {
    csv.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

fn main() -> ExitCode {
    let cli = Cli::parse();

    let Some(token) = cli.admin_token.clone() else {
        eprintln!("error: --admin-token (or SANDHI_ADMIN_TOKEN) is required");
        return ExitCode::from(2);
    };

    // For `keys add`, fill the secret from stdin when `--secret` is absent.
    let command = fill_secret_from_stdin(cli.command);

    let req = admin_request(&cli.admin_url, &command);
    let response = match execute(&req, &token) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::FAILURE;
        }
    };

    if let Some(err) = response.get("error") {
        eprintln!("admin API error: {err}");
        return ExitCode::FAILURE;
    }
    render(&command, &response, &cli.admin_url);
    ExitCode::SUCCESS
}

fn fill_secret_from_stdin(mut command: Command) -> Command {
    if let Command::Keys {
        action: KeysAction::Add { secret, .. },
    } = &mut command
    {
        if secret.is_none() {
            // Read one line from stdin (keeps the secret out of argv / shell history).
            if !std::io::stdin().is_terminal() {
                if let Some(Ok(line)) = std::io::stdin().lock().lines().next() {
                    *secret = Some(line.trim().to_string());
                }
            }
        }
    }
    command
}

fn execute(req: &AdminRequest, token: &str) -> Result<Value, String> {
    let client = reqwest::blocking::Client::builder()
        .build()
        .map_err(|e| format!("http client: {e}"))?;
    let mut builder = client
        .request(
            req.method.parse().map_err(|e| format!("method: {e}"))?,
            &req.path,
        )
        .bearer_auth(token);
    if let Some(body) = &req.body {
        builder = builder
            .header("content-type", "application/json")
            .body(body.clone());
    }
    let resp = builder.send().map_err(|e| format!("request failed: {e}"))?;
    let status = resp.status();
    let json: Value = resp.json().unwrap_or(json!({}));
    if !status.is_success() {
        return Ok(json); // surfaced as an `error` by render()
    }
    Ok(json)
}

fn render(command: &Command, response: &Value, _base_url: &str) {
    match command {
        Command::Usage { format, .. } => render_usage(response, format),
        Command::Budget {
            action: BudgetAction::List,
        } => render_rows(
            response.get("budgets").cloned().unwrap_or(Value::Null),
            &["scope", "limit_tokens", "window", "policy"],
        ),
        Command::Alerts {
            action: AlertsAction::List { .. },
        } => render_rows(
            response.get("alerts").cloned().unwrap_or(Value::Null),
            &["id", "scope", "threshold_pct", "channel"],
        ),
        _ => print_json(response),
    }
}

fn render_usage(response: &Value, format: &Format) {
    if matches!(format, Format::Json) {
        print_json(response);
        return;
    }
    let buckets = response.get("buckets").and_then(Value::as_array);
    let total = response.get("total");
    if let Some(total) = total {
        println!(
            "total: {} calls, {} in / {} out (cache read {})",
            total.get("calls").and_then(Value::as_u64).unwrap_or(0),
            total.get("tokens_in").and_then(Value::as_u64).unwrap_or(0),
            total.get("tokens_out").and_then(Value::as_u64).unwrap_or(0),
            total
                .get("cache_read_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0),
        );
    }
    if let Some(buckets) = buckets {
        println!();
        for b in buckets {
            println!(
                "{:<28} {:>6} calls  {:>8} in  {:>8} out",
                b.get("key").and_then(Value::as_str).unwrap_or("?"),
                b.get("calls").and_then(Value::as_u64).unwrap_or(0),
                b.get("tokens_in").and_then(Value::as_u64).unwrap_or(0),
                b.get("tokens_out").and_then(Value::as_u64).unwrap_or(0),
            );
        }
    }
}

fn render_rows(value: Value, cols: &[&str]) {
    match value {
        Value::Array(rows) => {
            for r in rows {
                let parts: Vec<String> = cols
                    .iter()
                    .map(|c| {
                        r.get(c)
                            .map(|v| v.to_string().trim_matches('"').to_string())
                            .unwrap_or_default()
                    })
                    .collect();
                println!("{}", parts.join("\t"));
            }
        }
        other => print_json(&other),
    }
}

fn print_json(value: &Value) {
    println!(
        "{}",
        serde_json::to_string_pretty(value).unwrap_or_default()
    );
}

// Silence the unused-import lint for `admin` when only the types are referenced indirectly.
#[allow(unused_imports)]
use admin as _;

#[cfg(test)]
mod tests {
    use super::*;

    fn url() -> &'static str {
        "http://localhost:8787"
    }

    #[test]
    fn keys_add_maps_to_post_admin_keys() {
        let cmd = Command::Keys {
            action: KeysAction::Add {
                provider: "anthropic".into(),
                label: Some("default".into()),
                scheme: Some("api-key".into()),
                base_url: None,
                secret: Some("sk-x".into()),
            },
        };
        let req = admin_request(url(), &cmd);
        assert_eq!(req.method, "POST");
        assert_eq!(req.path, "http://localhost:8787/admin/keys");
        let body: Value = serde_json::from_str(req.body.as_deref().unwrap()).unwrap();
        assert_eq!(body["provider"], "anthropic");
        assert_eq!(body["label"], "default");
        assert_eq!(body["secret"], "sk-x");
    }

    #[test]
    fn keys_share_maps_models_csv_to_list() {
        let cmd = Command::Keys {
            action: KeysAction::Share {
                upstream: "anthropic:default".into(),
                subject: Some("alice".into()),
                group: Some("platform".into()),
                models: Some("claude-x, claude-y".into()),
                budget: None,
                expires: None,
                rate: Some(60),
            },
        };
        let req = admin_request(url(), &cmd);
        let body: Value = serde_json::from_str(req.body.as_deref().unwrap()).unwrap();
        assert_eq!(body["models"], json!(["claude-x", "claude-y"]));
        assert_eq!(body["rate_limit_per_min"], 60);
        assert_eq!(req.path, "http://localhost:8787/admin/keys/share");
    }

    #[test]
    fn budget_set_and_usage_paths() {
        let set_cmd = Command::Budget {
            action: BudgetAction::Set {
                scope: "group:platform".into(),
                limit_tokens: 1000,
                window: "total".into(),
                policy: "block".into(),
                alert: Vec::new(),
            },
        };
        let req = admin_request(url(), &set_cmd);
        assert_eq!(req.method, "POST");
        assert_eq!(req.path, "http://localhost:8787/admin/budget");
        // With no --alert, alert_thresholds is omitted.
        let body: Value = serde_json::from_str(req.body.as_deref().unwrap()).unwrap();
        assert_eq!(body["scope"], "group:platform");
        assert!(body.get("alert_thresholds").unwrap().is_null());

        let usage_cmd = Command::Usage {
            by: "model".into(),
            since: Some("2026-01-01T00:00:00Z".into()),
            format: Format::Json,
        };
        let req = admin_request(url(), &usage_cmd);
        assert_eq!(req.method, "GET");
        assert!(req.path.contains("/admin/usage?by=model"));
        assert!(req.path.contains("since=2026-01-01"));
    }

    #[test]
    fn budget_set_with_alert_flag_emits_thresholds() {
        let set_cmd = Command::Budget {
            action: BudgetAction::Set {
                scope: "group:platform".into(),
                limit_tokens: 1000,
                window: "daily".into(),
                policy: "warn".into(),
                alert: vec![80, 100],
            },
        };
        let req = admin_request(url(), &set_cmd);
        let body: Value = serde_json::from_str(req.body.as_deref().unwrap()).unwrap();
        assert_eq!(body["window"], "daily");
        assert_eq!(body["policy"], "warn");
        assert_eq!(body["alert_thresholds"], json!([80, 100]));
    }

    #[test]
    fn alerts_list_and_ack_paths() {
        let list_cmd = Command::Alerts {
            action: AlertsAction::List {
                scope: Some("group:platform".into()),
            },
        };
        let req = admin_request(url(), &list_cmd);
        assert_eq!(req.method, "GET");
        assert!(req.path.contains("/admin/alerts"));
        assert!(req.path.contains("scope=group:platform"));

        let list_all = Command::Alerts {
            action: AlertsAction::List { scope: None },
        };
        let req = admin_request(url(), &list_all);
        assert_eq!(req.path, "http://localhost:8787/admin/alerts");

        let ack_cmd = Command::Alerts {
            action: AlertsAction::Ack {
                id: "alert_abc".into(),
            },
        };
        let req = admin_request(url(), &ack_cmd);
        assert_eq!(req.method, "POST");
        assert_eq!(req.path, "http://localhost:8787/admin/alerts/alert_abc/ack");
    }

    #[test]
    fn revoke_paths_include_the_id() {
        let cmd = Command::Vkeys {
            action: VkeysAction::Revoke {
                id: "key_abc".into(),
            },
        };
        let req = admin_request(url(), &cmd);
        assert_eq!(req.method, "DELETE");
        assert_eq!(req.path, "http://localhost:8787/admin/vkeys/key_abc");
    }

    #[test]
    fn csv_to_list_trims_and_drops_empties() {
        assert_eq!(csv_to_list(" a , b , ,c "), vec!["a", "b", "c"]);
        assert!(csv_to_list("").is_empty());
    }

    #[test]
    fn help_smoke_test_parses_without_error() {
        // clap's derive Parser builds the help lazily; ensure parsing a real subcommand and the
        // top-level definition do not panic.
        let parsed = Cli::try_parse_from([
            "sandhi",
            "--admin-url",
            "http://x:9",
            "--admin-token",
            "t",
            "usage",
            "--by",
            "provider",
        ])
        .unwrap();
        match parsed.command {
            Command::Usage { by, .. } => assert_eq!(by, "provider"),
            _ => panic!("expected Usage command"),
        }
    }
}
