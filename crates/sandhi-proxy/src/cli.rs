//! `sandhi` — the TD-0003 operator CLI.
//!
//! A thin HTTP client to the proxy's `/admin/*` REST API. It holds no database state of its own;
//! every subcommand is one authenticated call against a running `sandhi-proxy`. Configure the
//! target with `--admin-url` (env `SANDHI_ADMIN_URL`) and `--admin-token` (env
//! `SANDHI_ADMIN_TOKEN`).
//!
//! The arg→HTTP mapping lives in [`admin_request`] so it can be unit-tested without a network.

use std::io::{BufRead, IsTerminal, Read};
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use serde_json::{json, Value};

use sandhi_proxy::admin;
use sandhi_store::diagnostics::{
    DiagnosticQuery, DiagnosticSelector, DEFAULT_LIMIT, MAX_REQUEST_BYTES, MAX_RESPONSE_BYTES,
};

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
    /// Print bounded persisted usage evidence as JSON (not an anonymized export).
    #[command(group(clap::ArgGroup::new("diagnostic_selector")
        .args(["request", "session", "run"]).required(true).multiple(false)))]
    Diagnose(DiagnoseArgs),
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
        /// Dimension to aggregate by (`subject`, `group`, `provider`, `model`, `key`,
        /// `session`, `run`).
        #[arg(long, default_value = "subject")]
        by: String,
        /// RFC 3339 lower-bound (inclusive).
        #[arg(long)]
        since: Option<String>,
        /// Show the cost tree of ONE run id (ADR-0005 D7) instead of an aggregate:
        /// per-step spend assembled by parent, with subtree-inclusive rollups.
        #[arg(long, conflicts_with_all = ["by", "since"])]
        run: Option<String>,
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

#[derive(clap::Args, Debug)]
#[group(skip)]
struct DiagnoseArgs {
    /// Match a persisted request ID exactly (IDs need not be unique).
    #[arg(long)]
    request: Option<String>,
    /// Match a session ID exactly.
    #[arg(long)]
    session: Option<String>,
    /// Match a run ID exactly.
    #[arg(long)]
    run: Option<String>,
    /// Maximum rows, from 1 through 500; response bytes are bounded separately.
    #[arg(long, default_value_t = DEFAULT_LIMIT)]
    limit: usize,
}

impl DiagnoseArgs {
    fn query(&self) -> Result<DiagnosticQuery, &'static str> {
        let selector = match (&self.request, &self.session, &self.run) {
            (Some(value), None, None) => DiagnosticSelector::Request(value.clone()),
            (None, Some(value), None) => DiagnosticSelector::Session(value.clone()),
            (None, None, Some(value)) => DiagnosticSelector::Run(value.clone()),
            _ => return Err("exactly one diagnostic selector is required"),
        };
        let query = DiagnosticQuery {
            selector,
            limit: self.limit,
        };
        query.validate()?;
        Ok(query)
    }
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
    /// Register an existing exact vault reference with a read grant; never reads a secret from stdin.
    Reference {
        provider: String,
        label: Option<String>,
        #[arg(long)]
        scheme: Option<String>,
        #[arg(long)]
        base_url: Option<String>,
    },
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
        /// Rate limit in requests/minute, enforced per proxy process.
        ///
        /// With N replicas the effective limit is N x this value — the limiter is in-memory
        /// (TD-0012 D2), the same single-node caveat the budget ledger carries.
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
        // Diagnose validates its query and never enters the legacy unbounded executor.
        Command::Diagnose(_) => unreachable!("separate diagnostic request path"),
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
            action:
                KeysAction::Reference {
                    provider,
                    label,
                    scheme,
                    base_url,
                },
        } => (
            "POST",
            "/admin/keys/reference".into(),
            Some(json!({
                "provider": provider, "label": label, "scheme": scheme, "base_url": base_url,
            })),
        ),
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
            run,
            format: _,
        } => match run {
            Some(run_id) => ("GET", format!("/admin/usage/run/{run_id}"), None),
            None => {
                let mut path = format!("/admin/usage?by={by}");
                if let Some(since) = since {
                    path.push_str(&format!("&since={since}"));
                }
                ("GET", path, None)
            }
        },
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
    let args: Vec<_> = std::env::args_os().collect();
    let cli = match Cli::try_parse_from(&args) {
        Ok(cli) => cli,
        Err(error) => {
            // Clap can echo invalid values (including identifiers or credentials). Only
            // diagnose changes error presentation; ordinary help/version remain available.
            if args.iter().any(|arg| arg == "diagnose") && error.use_stderr() {
                eprintln!("error: invalid diagnostic arguments; see sandhi diagnose --help");
                return ExitCode::from(2);
            }
            error.exit();
        }
    };

    let Some(token) = cli.admin_token.clone() else {
        eprintln!("error: --admin-token (or SANDHI_ADMIN_TOKEN) is required");
        return ExitCode::from(2);
    };

    if let Command::Diagnose(args) = &cli.command {
        return match diagnostic_request(&cli.admin_url, args)
            .and_then(|request| execute_diagnostic(&request, &token))
        {
            Ok(response) => {
                print_json(&response);
                ExitCode::SUCCESS
            }
            Err(error) => {
                eprintln!("error: {error}");
                ExitCode::FAILURE
            }
        };
    }

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
        // A failed multi-write can still have committed a budget or minted a key. Preserve
        // that structured result for the operator while keeping the command's exit nonzero.
        if response.get("budget_applied").is_some()
            || response.get("failures").is_some()
            || response.get("reconcile_before_retry").is_some()
        {
            print_json(&response);
        }
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
        if secret.is_none() && !std::io::stdin().is_terminal() {
            // Read one line from stdin (keeps the secret out of argv / shell history). A
            // piped-but-empty stdin (e.g. `printf '' | sandhi keys add ollama ...` for a
            // keyless local upstream) yields zero lines, not an empty one — treat that as
            // a deliberate empty secret rather than leaving `None`, which serializes as
            // JSON `null` against the admin API's required `String` field and gets
            // rejected with a non-JSON body (see `execute`'s parse-failure handling).
            let line = match std::io::stdin().lock().lines().next() {
                Some(Ok(line)) => line,
                _ => String::new(),
            };
            *secret = Some(line.trim().to_string());
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
    // Do NOT default a parse failure to `json!({})` — that shape has no "error" key, so
    // main()'s `response.get("error")` check falls through and the CLI reports SUCCESS
    // (exit 0) while silently discarding whatever the server actually said, including a
    // non-JSON rejection body (e.g. Axum's plain-text 4xx for a malformed request). Surface
    // the real body so a bad request is never mistaken for an empty-but-successful one.
    let text = resp
        .text()
        .map_err(|e| format!("reading response body: {e}"))?;
    let json: Value = serde_json::from_str(&text).map_err(|e| {
        format!("server returned a non-JSON response (status {status}): {e}\nbody: {text}")
    })?;
    if !status.is_success() {
        return Ok(json); // surfaced as an `error` by render()
    }
    Ok(json)
}

fn diagnostic_request(base_url: &str, args: &DiagnoseArgs) -> Result<AdminRequest, &'static str> {
    let query = args.query()?;
    let body = serde_json::to_string(&query).map_err(|_| "invalid diagnostic request")?;
    if body.len() > MAX_REQUEST_BYTES {
        return Err("diagnostic request exceeds byte limit");
    }
    Ok(AdminRequest {
        method: "POST",
        path: format!("{}/admin/usage/diagnostics", base_url.trim_end_matches('/')),
        body: Some(body),
    })
}

/// Read at most the byte budget plus one overflow probe byte, even without Content-Length.
/// No JSON parsing, raw response display, or unbounded `.text()` occurs on this path.
fn read_diagnostic_response(reader: impl Read) -> Result<Value, &'static str> {
    let mut bytes = Vec::new();
    reader
        .take((MAX_RESPONSE_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| "could not read diagnostic response")?;
    if bytes.len() > MAX_RESPONSE_BYTES {
        return Err("diagnostic response exceeds byte limit");
    }
    let value: Value = serde_json::from_slice(&bytes).map_err(|_| "invalid diagnostic response")?;
    if !value.is_object() || value.get("error").is_some() {
        return Err("invalid diagnostic response");
    }
    Ok(value)
}

fn execute_diagnostic(req: &AdminRequest, token: &str) -> Result<Value, &'static str> {
    let client = reqwest::blocking::Client::builder()
        // Redirects must not forward the selector to an unrelated endpoint.
        .redirect(reqwest::redirect::Policy::none())
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|_| "could not initialize diagnostic client")?;
    let response = client
        .post(&req.path)
        .bearer_auth(token)
        .header("content-type", "application/json")
        .body(req.body.clone().ok_or("invalid diagnostic request")?)
        .send()
        .map_err(|_| "diagnostic request failed")?;
    if !response.status().is_success() {
        // Never read or relay an error body, including JSON with no `error` key.
        return Err("diagnostic request rejected");
    }
    if response
        .content_length()
        .is_some_and(|length| length > MAX_RESPONSE_BYTES as u64)
    {
        return Err("diagnostic response exceeds byte limit");
    }
    read_diagnostic_response(response)
}

fn render(command: &Command, response: &Value, _base_url: &str) {
    match command {
        Command::Usage {
            format,
            run: Some(_),
            ..
        } => render_run_tree(response, format),
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

/// `p50/p95 ms` over the calls that reported a duration, or `—`. Absent latency is unknown, not
/// zero — printing `0 ms` would read as "instant" for a provider that never reported timing.
fn latency_cell(b: &Value) -> String {
    match b.get("latency") {
        Some(l) if l.get("samples").and_then(Value::as_u64).unwrap_or(0) > 0 => format!(
            "{}/{} ms",
            l.get("p50_ms").and_then(Value::as_u64).unwrap_or(0),
            l.get("p95_ms").and_then(Value::as_u64).unwrap_or(0),
        ),
        _ => "—".to_string(),
    }
}

fn cache_read_cell(row: &Value) -> String {
    let calls = row.get("calls").and_then(Value::as_u64).unwrap_or(0);
    let coverage = row
        .get("cache_read_coverage")
        .cloned()
        .and_then(|v| serde_json::from_value::<sandhi_core::CacheReadCoverage>(v).ok())
        .map(|c| c.normalized(calls));
    let Some(c) = coverage else {
        return "unknown".into();
    };
    if calls == 0 {
        return "—".into();
    }
    let count = row
        .get("cache_read_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    if c.reported == calls {
        return format!("{count} ({calls}/{calls} reported)");
    }
    let label = if c.absent == calls {
        "not reported"
    } else if c.malformed == calls {
        "malformed"
    } else if c.unsupported == calls {
        "unsupported"
    } else if c.unknown == calls {
        "unknown"
    } else {
        "mixed reporting"
    };
    format!(
        "{label} ({}/{} reported; numeric total {count})",
        c.reported, calls
    )
}

fn render_usage(response: &Value, format: &Format) {
    if matches!(format, Format::Json) {
        print_json(response);
        return;
    }
    let buckets = response.get("buckets").and_then(Value::as_array);
    let total = response.get("total");
    let u64_at = |v: &Value, k: &str| v.get(k).and_then(Value::as_u64).unwrap_or(0);
    if let Some(total) = total {
        // `billable` is the ADR-0005 D4 quantity budgets are enforced on, so it is what an
        // operator needs to reconcile against a cap — the in/out columns alone under-report it
        // whenever a prompt cache is in play.
        println!(
            "total: {} calls, {} in / {} out (cache write {}, cache read {}) — {} billable",
            u64_at(total, "calls"),
            u64_at(total, "tokens_in"),
            u64_at(total, "tokens_out"),
            u64_at(total, "cache_creation_tokens"),
            cache_read_cell(total),
            u64_at(total, "billable_tokens"),
        );
    }
    if let Some(buckets) = buckets {
        println!();
        for b in buckets {
            println!(
                "{:<28} {:>6} calls  {:>8} in  {:>8} out  {:>10} billable  {:>16}  cache read {}",
                b.get("key").and_then(Value::as_str).unwrap_or("?"),
                u64_at(b, "calls"),
                u64_at(b, "tokens_in"),
                u64_at(b, "tokens_out"),
                u64_at(b, "billable_tokens"),
                latency_cell(b),
                cache_read_cell(b),
            );
        }
    }
}

/// Render one run's cost tree: the grand total, then each node indented under its parent with
/// own vs subtree-inclusive (`rollup`) billable tokens — the ADR-0005 D4 quantity.
fn render_run_tree(response: &Value, format: &Format) {
    if matches!(format, Format::Json) {
        print_json(response);
        return;
    }
    let Some(run) = response.get("run") else {
        print_json(response);
        return;
    };
    let u64_at = |v: &Value, k: &str| v.get(k).and_then(Value::as_u64).unwrap_or(0);
    if let Some(total) = run.get("total") {
        println!(
            "run {}: {} calls, {} in / {} out (cache write {}, cache read {}) — {} billable",
            run.get("run_id").and_then(Value::as_str).unwrap_or("?"),
            u64_at(total, "calls"),
            u64_at(total, "tokens_in"),
            u64_at(total, "tokens_out"),
            u64_at(total, "cache_creation_tokens"),
            cache_read_cell(total),
            u64_at(total, "billable_tokens"),
        );
    }
    if let Some(roots) = run.get("roots").and_then(Value::as_array) {
        println!();
        for node in roots {
            render_run_node(node, 0);
        }
    }
}

fn render_run_node(node: &Value, depth: usize) {
    let u64_in = |k: &str, f: &str| {
        node.get(k)
            .and_then(|v| v.get(f))
            .and_then(Value::as_u64)
            .unwrap_or(0)
    };
    println!(
        "{:indent$}{:<28} {:>6} calls  {:>10} own  {:>10} rollup",
        "",
        node.get("step_id").and_then(Value::as_str).unwrap_or("?"),
        u64_in("own", "calls"),
        u64_in("own", "billable_tokens"),
        u64_in("rollup", "billable_tokens"),
        indent = depth * 2
    );
    if let Some(children) = node.get("children").and_then(Value::as_array) {
        for child in children {
            render_run_node(child, depth + 1);
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

    fn diagnose(args: &[&str]) -> DiagnoseArgs {
        let cli = Cli::try_parse_from(
            ["sandhi", "diagnose"]
                .into_iter()
                .chain(args.iter().copied()),
        )
        .unwrap();
        let Command::Diagnose(args) = cli.command else {
            panic!("expected diagnose");
        };
        args
    }

    #[test]
    fn diagnostic_selectors_map_to_tagged_post_bodies_not_urls() {
        for kind in ["request", "session", "run"] {
            let flag = format!("--{kind}");
            let args = diagnose(&[&flag, "id / ? secret", "--limit", "500"]);
            let request = diagnostic_request("http://localhost:8787/", &args).unwrap();
            assert_eq!(request.method, "POST");
            assert_eq!(
                request.path,
                "http://localhost:8787/admin/usage/diagnostics"
            );
            assert_eq!(
                serde_json::from_str::<Value>(request.body.as_deref().unwrap()).unwrap(),
                json!({"selector":{"kind":kind,"value":"id / ? secret"},"limit":500})
            );
        }
        assert_eq!(diagnose(&["--request", "id"]).query().unwrap().limit, 100);
    }

    #[test]
    fn diagnostic_selector_group_requires_exactly_one_even_with_a_limit() {
        for args in [
            vec![],
            vec!["--limit", "100"],
            vec!["--request", "a", "--session", "b"],
            vec!["--session", "a", "--run", "b"],
            vec!["--request", "a", "--run", "b"],
            vec!["--request", "a", "--request", "b"],
            vec!["--request", "a", "--limit", "100", "--limit", "200"],
            vec!["--request", "a", "--limit", "not-a-number"],
            vec!["--request", "a", "--limit", "-1"],
            vec!["--request", "a", "--unknown", "value"],
        ] {
            assert!(Cli::try_parse_from(["sandhi", "diagnose"].into_iter().chain(args)).is_err());
        }
    }

    #[test]
    fn diagnostic_validation_uses_utf8_bytes_and_store_limits() {
        for value in [
            "".to_owned(),
            " \t".into(),
            "a\nb".into(),
            "x".repeat(257),
            "é".repeat(129),
        ] {
            let args = diagnose(&["--request", &value]);
            assert_eq!(
                diagnostic_request("http://unused", &args).unwrap_err(),
                "invalid diagnostic selector"
            );
        }
        for value in ["x".repeat(256), "é".repeat(128)] {
            assert!(diagnose(&["--request", &value]).query().is_ok());
        }
        let largest = usize::MAX.to_string();
        for limit in ["0", "501", largest.as_str()] {
            assert!(diagnose(&["--run", "id", "--limit", limit])
                .query()
                .is_err());
        }
        let invalid = DiagnoseArgs {
            request: None,
            session: None,
            run: None,
            limit: 100,
        };
        assert!(invalid.query().is_err());
    }

    #[test]
    fn diagnostic_reads_accept_exact_byte_limit_and_stop_after_overflow_probe() {
        let mut exact = b"{}".to_vec();
        exact.resize(MAX_RESPONSE_BYTES, b' ');
        assert_eq!(
            read_diagnostic_response(exact.as_slice()).unwrap(),
            json!({})
        );

        // An unbounded peer must not cause an unbounded read or reach JSON parsing.
        let mut unbounded = std::io::repeat(b' ');
        assert_eq!(
            read_diagnostic_response(&mut unbounded).unwrap_err(),
            "diagnostic response exceeds byte limit"
        );
        let mut cursor = std::io::Cursor::new(vec![b' '; MAX_RESPONSE_BYTES * 2]);
        assert!(read_diagnostic_response(&mut cursor).is_err());
        assert_eq!(cursor.position(), (MAX_RESPONSE_BYTES + 1) as u64);
    }

    #[test]
    fn diagnostic_parse_and_read_errors_never_echo_body_or_io_details() {
        for body in [
            b"BODY_SECRET".as_slice(),
            b"{\"error\":\"BODY_SECRET\"}",
            b"\"BODY_SECRET\"",
            b"[]",
            b"",
            b"\xff",
        ] {
            assert_eq!(
                read_diagnostic_response(body).unwrap_err(),
                "invalid diagnostic response"
            );
        }
        struct Broken;
        impl Read for Broken {
            fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
                Err(std::io::Error::other("IO_SECRET"))
            }
        }
        assert_eq!(
            read_diagnostic_response(Broken).unwrap_err(),
            "could not read diagnostic response"
        );
    }

    #[test]
    fn cache_read_labels_do_not_infer_reporting_from_numeric_zero() {
        let mut row = serde_json::json!({"calls":1,"cache_read_tokens":0});
        assert_eq!(cache_read_cell(&row), "unknown");
        row["cache_read_coverage"] =
            serde_json::json!({"reported":1,"absent":0,"malformed":0,"unsupported":0,"unknown":0});
        assert_eq!(cache_read_cell(&row), "0 (1/1 reported)");
        for (status, label) in [
            ("absent", "not reported"),
            ("malformed", "malformed"),
            ("unsupported", "unsupported"),
            ("unknown", "unknown"),
        ] {
            let mut coverage = serde_json::json!({"reported":0,"absent":0,"malformed":0,"unsupported":0,"unknown":0});
            coverage[status] = 1.into();
            row["cache_read_coverage"] = coverage;
            assert!(cache_read_cell(&row).starts_with(label));
        }
        row["cache_read_tokens"] = 17.into();
        assert!(cache_read_cell(&row).starts_with("unknown"));
    }

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
    fn reference_command_never_serializes_a_secret() {
        let cli =
            Cli::try_parse_from(["sandhi", "keys", "reference", "openai", "default"]).unwrap();
        let req = admin_request(url(), &cli.command);
        assert_eq!(req.path, "http://localhost:8787/admin/keys/reference");
        let body: Value = serde_json::from_str(req.body.as_deref().unwrap()).unwrap();
        assert!(body.get("secret").is_none());
        assert_eq!(body["label"], "default");
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
    fn usage_run_flag_maps_to_the_run_tree_endpoint() {
        let cmd = Command::Usage {
            by: "subject".into(),
            since: None,
            run: Some("run-1".into()),
            format: Format::Table,
        };
        let req = admin_request(url(), &cmd);
        assert_eq!(req.method, "GET");
        assert_eq!(req.path, "http://localhost:8787/admin/usage/run/run-1");
        assert_eq!(req.body, None);

        // Without --run, the aggregate path is unchanged.
        let cmd = Command::Usage {
            by: "run".into(),
            since: None,
            run: None,
            format: Format::Table,
        };
        let req = admin_request(url(), &cmd);
        assert_eq!(req.path, "http://localhost:8787/admin/usage?by=run");
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
            run: None,
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
