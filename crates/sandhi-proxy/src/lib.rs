//! Sandhi reverse-proxy — the **in-path (inline) egress gate** (AnvaiOps ADR-0047 D8).
//!
//! A client points its `base_url` at Sandhi and presents a **virtual key** (never the real
//! upstream key). The gate resolves the key → subject/group + which upstream, budget-checks,
//! normalizes the request through Sandhi's typed runtime, then emits one neutral usage event and
//! reconciles the budget. It is *in-path*, not a redirect: a client cannot bypass the meter.

mod codec;
pub mod ledger;
pub mod operator;

// Re-export the admin API request/response types for the `sandhi` CLI client + the startup
// rehydration helpers used by the `sandhi-proxy` binary.
pub use ledger::{Admission, ProxyLedger};
pub use operator::{
    admin, build_provider_handle, rehydrate_alerts, rehydrate_budgets, rehydrate_live_keys,
};

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use axum::body::{Body, Bytes};
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Json, Response};
use axum::routing::{delete, get, post};
use axum::Router;
use futures_util::StreamExt;
use serde_json::{json, Value};

use time::OffsetDateTime;

use sandhi_core::{
    billable, AlertRegistry, Backend, ChatRequestV1, KeyStore, Policy, RequestMetadataV1,
    Reservation, Sink, UsageCompleteness, UsageEvent, UsageV2, VirtualKey,
};
use sandhi_providers::{ProviderError, ProviderFamily, ProviderHandle, ProviderRuntime};
use sandhi_store::{hash_secret, AlertStore, SqliteStore, VaultStore, VirtualKeyStore};

use codec::{decode_request, encode_response, encode_stream_event, IngressDialect};
pub use operator::BudgetSpec;

/// Conservative output ceiling applied to a **budget-capped** scope when the client omits
/// `max_output_tokens` (ADR-0005 D1). The reservation holds this as an upper bound and the value
/// is set on the upstream request so the provider bounds output — otherwise an unbounded stream
/// overshoots the cap (the 100× soft-cap bug). Unlimited scopes are never modified.
const DEFAULT_OUTPUT_CEILING: u64 = 4096;

/// Shared server state: the virtual-key store, the budget ledger, the usage sink, and the
/// registry of configured upstream providers (each already holding its real credential).
pub struct ProxyState {
    pub keys: KeyStore,
    /// The enforcement ledger (ADR-0005 lease model): durable [`SqliteLedger`](sandhi_store::SqliteLedger)
    /// when `SANDHI_STORE` is set, else volatile in-memory. See [`ProxyLedger`].
    pub ledger: Mutex<ProxyLedger>,
    pub sink: Arc<dyn Sink>,
    /// `upstream_ref` → a persistent typed provider handle (real key baked in). Interior-mutable:
    /// the admin API registers handles here at runtime; the demo path seeds it at startup.
    pub providers: Mutex<HashMap<String, ProviderHandle>>,
    /// The durable store backing the dashboard. When set, `/dashboard` serves usage aggregates;
    /// typically the same object is also used as `sink` so events persist.
    pub store: Option<Arc<SqliteStore>>,

    // --- TD-0003 P1 operator surface ---
    /// Durable provider-credential vault (metadata in SQLite, secret in the active backend).
    pub vault: Option<Arc<VaultStore>>,
    /// Durable virtual-key store (hashes + scope), rehydrates `keys` on startup.
    pub vkeys: Option<Arc<VirtualKeyStore>>,
    /// Builds typed upstream handles from vault-resolved credentials.
    pub runtime: ProviderRuntime,
    /// Admin-API bearer token (distinct from virtual keys). `None` disables the admin API.
    pub admin_token: Option<String>,
    /// Operator-set budgets (scope → spec). The live [`ProxyLedger`] enforces them; this map is the
    /// metadata surface (policy lookup, dashboard, alert thresholds) and is rehydrated from the
    /// durable ledger on startup.
    pub budgets: Mutex<HashMap<String, BudgetSpec>>,
    /// The externally-reachable base URL shared with minted-key callers (e.g.
    /// `http://localhost:8787`).
    pub public_url: String,
    // --- TD-0003 P2 budget depth + alerts ---
    /// Live alert-rule registry + dedup (the evaluation engine). `None` when alerts are off.
    pub alerts: Option<Arc<Mutex<AlertRegistry>>>,
    /// Durable alert-rule store (rules + last_fired_at + ack), backs `/admin/alerts`.
    pub alert_store: Option<Arc<AlertStore>>,
    /// ADR-0004 D4: when `false` (default) and an admin token is configured, the
    /// `/dashboard/api/*` read endpoints require the admin bearer — they serve subject/group
    /// usage aggregates. `SANDHI_DASHBOARD_PUBLIC=1` restores the previous open, masked-only
    /// behavior for trusted single-node deployments. With no admin token configured the
    /// endpoints stay open (there is no credential to present).
    pub dashboard_public: bool,
}

impl ProxyState {
    /// Build a state with the operator surface defaulted off (no vault, no admin token). The
    /// existing demo + request-handling path is unchanged.
    #[must_use]
    pub fn new(
        keys: KeyStore,
        ledger: ProxyLedger,
        sink: Arc<dyn Sink>,
        providers: HashMap<String, ProviderHandle>,
        store: Option<Arc<SqliteStore>>,
    ) -> Self {
        Self {
            keys,
            ledger: Mutex::new(ledger),
            sink,
            providers: Mutex::new(providers),
            store,
            vault: None,
            vkeys: None,
            runtime: ProviderRuntime::new(),
            admin_token: None,
            budgets: Mutex::new(HashMap::new()),
            public_url: "http://localhost:8787".into(),
            alerts: None,
            alert_store: None,
            dashboard_public: false,
        }
    }
}

/// Build the axum app. Ingress paths mirror the provider wire formats (OpenAI Chat Completions,
/// OpenAI Responses, Anthropic Messages); the presented virtual key selects the actual upstream.
/// The `/admin/*` routes are the TD-0003 operator surface (authed by an admin token).
pub fn build_app(state: Arc<ProxyState>) -> Router {
    Router::new()
        .route("/healthz", get(health))
        .route("/catalog/models", get(catalog_models))
        .route("/dashboard", get(dashboard_html))
        .route("/dashboard/api/usage", get(dashboard_api))
        // TD-0003 P4 dashboard read-only endpoints (masked; admin-bearer-gated when an admin
        // token is configured, unless SANDHI_DASHBOARD_PUBLIC=1 — ADR-0004 D4).
        .route("/dashboard/api/keys", get(dashboard_keys))
        .route("/dashboard/api/budgets", get(dashboard_budgets))
        .route("/dashboard/api/alerts", get(dashboard_alerts))
        .route("/v1/chat/completions", post(handle_openai))
        .route("/v1/messages", post(handle_anthropic))
        .route("/v1/responses", post(handle_responses))
        // TD-0003 P1 operator (admin) API.
        .route(
            "/admin/keys",
            post(operator::add_key).get(operator::list_keys),
        )
        .route("/admin/keys/share", post(operator::share_key))
        .route("/admin/keys/virtual", get(operator::list_virtual_keys))
        .route("/admin/keys/:provider/:label", delete(operator::revoke_key))
        .route("/admin/vkeys/:id", delete(operator::revoke_virtual_key))
        .route(
            "/admin/budget",
            post(operator::set_budget).get(operator::list_budgets),
        )
        .route("/admin/budget/usage", get(operator::budget_usage))
        .route("/admin/usage", get(operator::usage))
        // TD-0003 P2 alert rules.
        .route(
            "/admin/alerts",
            post(operator::create_alert).get(operator::list_alerts),
        )
        .route("/admin/alerts/:id/ack", post(operator::ack_alert))
        .route("/admin/alerts/:id", delete(operator::delete_alert))
        .with_state(state)
}

/// Bind and serve until shutdown.
pub async fn serve(state: Arc<ProxyState>, addr: SocketAddr) -> std::io::Result<()> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, build_app(state)).await
}

async fn health() -> &'static str {
    "ok"
}

#[derive(serde::Deserialize)]
struct CatalogQuery {
    provider: Option<String>,
}

/// Public catalog discovery (TD-0004): curated model descriptors for a provider, facts only
/// (no pricing). Unauthed -- stable public facts, like OpenAI/OpenRouter list-models endpoints.
/// Usage: `GET /catalog/models?provider=anthropic`.
async fn catalog_models(Query(query): Query<CatalogQuery>) -> Response {
    let Some(provider) = query.provider else {
        return error(
            StatusCode::BAD_REQUEST,
            "missing 'provider' query parameter",
        );
    };
    match sandhi_providers::provider_descriptor(&provider) {
        Some(descriptor) => Json(descriptor.models).into_response(),
        None => error(
            StatusCode::NOT_FOUND,
            &format!("unknown provider: {provider}"),
        ),
    }
}

/// ADR-0004 D4 dashboard gate: the read endpoints serve subject/group usage aggregates, so
/// when an admin token is configured they require it (same bearer as `/admin/*`) unless the
/// operator explicitly opted back into the open, masked-only model (`dashboard_public`).
/// No admin token configured → open (nothing to present; single-node dev trust).
#[allow(clippy::result_large_err)] // axum::Response is intentionally large; idiomatic shape.
fn require_dashboard_access(state: &ProxyState, headers: &HeaderMap) -> Result<(), Response> {
    if state.dashboard_public || state.admin_token.is_none() {
        return Ok(());
    }
    operator::require_admin(state, headers)
}

/// Usage aggregates for the dashboard (JSON). 404 when no durable store is configured.
async fn dashboard_api(State(state): State<Arc<ProxyState>>, headers: HeaderMap) -> Response {
    if let Err(denied) = require_dashboard_access(&state, &headers) {
        return denied;
    }
    let Some(store) = state.store.clone() else {
        return error(
            StatusCode::NOT_FOUND,
            "dashboard not configured (set SANDHI_STORE)",
        );
    };
    let payload = json!({
        "total": store.grand_total().ok(),
        "by_subject": store.totals_by_subject().unwrap_or_default(),
        "by_group": store.totals_by_group().unwrap_or_default(),
        "by_provider": store.totals_by_provider().unwrap_or_default(),
        "by_model": store.totals_by_model().unwrap_or_default(),
    });
    Json(payload).into_response()
}

// --- TD-0003 P4 dashboard read-only endpoints ----------------------------------
//
// Auth model: these mirror the self-hosted single-node trust of the existing `/dashboard` HTML and
// `/dashboard/api/usage` — they are **unauthed**, and rely on **masked-only** output as the security
// boundary. The operator binds the proxy to a trusted network / localhost and controls access; no
// secret (raw provider key, virtual-key plaintext, or virtual-key hash) is ever serialized here.
// Programmatic/automated access that needs gating uses the admin-token-protected `/admin/*` routes.
// Units are neutral tokens throughout — no dollars / SKU / tier (the measure-vs-price boundary).

/// `GET /dashboard/api/keys` — masked virtual keys + masked vault entries (no secrets, no hashes).
/// 404 when neither the vault nor the virtual-key store is configured.
async fn dashboard_keys(State(state): State<Arc<ProxyState>>, headers: HeaderMap) -> Response {
    if let Err(denied) = require_dashboard_access(&state, &headers) {
        return denied;
    }
    let (vault, vkeys) = (state.vault.clone(), state.vkeys.clone());
    if vault.is_none() && vkeys.is_none() {
        return error(
            StatusCode::NOT_FOUND,
            "keys dashboard not configured (set SANDHI_STORE)",
        );
    }
    let vkey_records = vkeys
        .as_ref()
        .and_then(|s| s.list().ok())
        .unwrap_or_default()
        .iter()
        .map(operator::vkey_record_response)
        .collect::<Vec<_>>();
    let vault_entries = vault
        .as_ref()
        .and_then(|s| s.list().ok())
        .unwrap_or_default()
        .iter()
        .map(operator::vault_entry_response)
        .collect::<Vec<_>>();
    Json(json!({ "virtual_keys": vkey_records, "vault": vault_entries })).into_response()
}

/// `GET /dashboard/api/budgets` — every configured scope with limit / window / policy + live spent
/// (from the budget ledger). Neutral tokens; no pricing.
async fn dashboard_budgets(State(state): State<Arc<ProxyState>>, headers: HeaderMap) -> Response {
    if let Err(denied) = require_dashboard_access(&state, &headers) {
        return denied;
    }
    let ledger = state.ledger.lock().expect("ledger poisoned");
    let scopes: Vec<Value> = state
        .budgets
        .lock()
        .expect("budgets poisoned")
        .values()
        .map(|spec| {
            let spent = ledger.spent(&spec.scope);
            let limit = spec.limit_tokens;
            json!({
                "scope": spec.scope,
                "limit_tokens": limit,
                "spent": spent,
                "remaining": limit.saturating_sub(spent),
                "window": spec.window,
                "policy": spec.policy,
            })
        })
        .collect();
    Json(json!({ "budgets": scopes })).into_response()
}

/// `GET /dashboard/api/alerts` — recent fired alerts (rules whose threshold has tripped) plus all
/// configured rules. 404 when the alert store is not configured.
async fn dashboard_alerts(State(state): State<Arc<ProxyState>>, headers: HeaderMap) -> Response {
    if let Err(denied) = require_dashboard_access(&state, &headers) {
        return denied;
    }
    let Some(store) = state.alert_store.clone() else {
        return error(
            StatusCode::NOT_FOUND,
            "alerts dashboard not configured (set SANDHI_STORE)",
        );
    };
    let rules = store.list().unwrap_or_default();
    let all: Vec<Value> = rules.iter().map(operator::alert_rule_response).collect();
    let fired: Vec<Value> = rules
        .iter()
        .filter(|r| r.last_fired_at.is_some())
        .map(operator::alert_rule_response)
        .collect();
    Json(json!({ "rules": all, "fired": fired })).into_response()
}

/// The self-hosted single-node dashboard (static HTML; fetches `/dashboard/api/usage`).
async fn dashboard_html() -> Response {
    axum::response::Html(DASHBOARD_HTML).into_response()
}

const DASHBOARD_HTML: &str = r####"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1">
<title>Sandhi — operator dashboard</title>
<style>
  :root { color-scheme: light dark; }
  body { font: 14px/1.5 ui-sans-serif, system-ui, sans-serif; margin: 0; padding: 2rem;
         max-width: 1000px; margin-inline: auto; }
  h1 { font-size: 1.4rem; margin: 0 0 .25rem; }
  .sub { color: #6b7280; margin-bottom: 1.5rem; }
  .cards { display: flex; gap: 1rem; flex-wrap: wrap; margin-bottom: 1.5rem; }
  .card { border: 1px solid #8883; border-radius: 10px; padding: 1rem 1.25rem; min-width: 8rem; }
  .card .n { font-size: 1.6rem; font-weight: 700; }
  .card .l { color: #6b7280; font-size: .8rem; text-transform: uppercase; letter-spacing: .04em; }
  h2 { font-size: 1rem; margin: 1.75rem 0 .5rem; border-top: 1px solid #8882; padding-top: 1.25rem; }
  h2:first-of-type { border-top: none; padding-top: 0; }
  table { width: 100%; border-collapse: collapse; }
  th, td { text-align: left; padding: .4rem .5rem; border-bottom: 1px solid #8882; vertical-align: top; }
  th { color: #6b7280; font-weight: 600; font-size: .8rem; }
  td.num, th.num { text-align: right; font-variant-numeric: tabular-nums; }
  .amber { color: #b45309; }
  .muted { color: #6b7280; }
  .badge { display: inline-block; padding: .05rem .4rem; border-radius: 6px; font-size: .72rem;
           border: 1px solid #8883; }
  .badge.active { color: #047857; border-color: #04785755; }
  .badge.revoked { color: #b45309; border-color: #b4530955; }
  .bar { background: #8882; border-radius: 6px; height: 8px; overflow: hidden; min-width: 6rem; }
  .bar > span { display: block; height: 100%; background: #2563eb; }
  .bar.warn > span { background: #b45309; }
  .bar.over > span { background: #b91c1c; }
  .json-link { float: right; font-weight: 400; font-size: .8rem; }
  .fired { background: #b4530910; }
  code { font-size: .85em; }
</style>
</head>
<body>
<h1>Sandhi <span class="amber">— the metering layer for AI agents</span></h1>
<div class="sub">Self-hosted operator dashboard · neutral token units (no pricing) ·
  <a href="/dashboard/api/usage">usage</a> · <a href="/dashboard/api/keys">keys</a> ·
  <a href="/dashboard/api/budgets">budgets</a> · <a href="/dashboard/api/alerts">alerts</a></div>

<div class="cards" id="cards"></div>
<div id="tables"></div>

<h2>Keys <span class="json-link"><a href="/dashboard/api/keys">JSON</a></span></h2>
<div id="keys"></div>

<h2>Budgets <span class="json-link"><a href="/dashboard/api/budgets">JSON</a></span></h2>
<div id="budgets"></div>

<h2>Alerts <span class="json-link"><a href="/dashboard/api/alerts">JSON</a></span></h2>
<div id="alerts"></div>

<script>
const fmt = n => (n ?? 0).toLocaleString();
const esc = s => String(s ?? "").replace(/[&<>"]/g, c =>
  ({ "&":"&amp;", "<":"&lt;", ">":"&gt;", '"':"&quot;" }[c]));
const orDash = s => (s === null || s === undefined || s === "") ? "—" : esc(s);

function tbl(title, rows) {
  const body = rows.map(r => `<tr><td>${esc(r.key)}</td><td class="num">${fmt(r.calls)}</td>`
    + `<td class="num">${fmt(r.tokens_in)}</td><td class="num">${fmt(r.tokens_out)}</td>`
    + `<td class="num">${fmt(r.cache_read_tokens)}</td></tr>`).join("");
  return `<h2>${title}</h2><table><thead><tr><th>key</th><th class="num">calls</th>`
    + `<th class="num">in</th><th class="num">out</th><th class="num">cache read</th></tr></thead>`
    + `<tbody>${body || '<tr><td colspan=5>no data yet</td></tr>'}</tbody></table>`;
}

fetch("/dashboard/api/usage").then(r => r.json()).then(d => {
  const t = d.total || { calls: 0, tokens_in: 0, tokens_out: 0, cache_read_tokens: 0 };
  document.getElementById("cards").innerHTML =
    [["calls", t.calls], ["tokens in", t.tokens_in], ["tokens out", t.tokens_out],
     ["cache read", t.cache_read_tokens]]
    .map(([l, n]) => `<div class="card"><div class="n">${fmt(n)}</div><div class="l">${l}</div></div>`).join("");
  document.getElementById("tables").innerHTML =
    tbl("Attribution — by user (subject)", d.by_subject || [])
    + tbl("Attribution — by team (group)", d.by_group || [])
    + tbl("Attribution — by provider", d.by_provider || [])
    + tbl("Attribution — by model", d.by_model || []);
}).catch(() => { document.getElementById("tables").innerHTML =
  '<p class="muted">usage store not configured (set SANDHI_STORE).</p>'; });

// Keys: masked virtual keys + vault entries. Never a secret.
function keysView(d) {
  const vkeys = (d.virtual_keys || []).map(k => {
    const status = k.revoked_at ? "revoked" : "active";
    return `<tr><td><code>${esc(k.id)}</code></td><td>${orDash(k.subject)}</td><td>${orDash(k.group)}</td>`
      + `<td><code>${esc(k.upstream_ref)}</code></td><td>${(k.models||[]).map(esc).join(", ")||'<span class="muted">any</span>'}</td>`
      + `<td><span class="badge ${status}">${status}</span></td><td>${orDash(k.expires_at)}</td></tr>`;
  }).join("");
  const vault = (d.vault || []).map(e => `<tr><td><code>${esc(e.credential_id)}</code></td>`
    + `<td>${esc(e.scheme)}</td><td>${orDash(e.base_url)}</td>`
    + `<td><span class="badge ${e.status}">${esc(e.status)}</span></td></tr>`).join("");
  return `<h3 class="muted" style="font-size:.8rem;text-transform:uppercase;letter-spacing:.04em;margin-bottom:.25rem">Virtual keys (masked — secrets are never stored)</h3>`
    + `<table><thead><tr><th>id</th><th>subject</th><th>group</th><th>upstream</th><th>models</th><th>status</th><th>expires</th></tr></thead>`
    + `<tbody>${vkeys || '<tr><td colspan=7>no virtual keys</td></tr>'}</tbody></table>`
    + `<h3 class="muted" style="font-size:.8rem;text-transform:uppercase;letter-spacing:.04em;margin:1rem 0 .25rem">Provider credentials (vault metadata)</h3>`
    + `<table><thead><tr><th>credential</th><th>scheme</th><th>base url</th><th>status</th></tr></thead>`
    + `<tbody>${vault || '<tr><td colspan=4>no provider credentials</td></tr>'}</tbody></table>`;
}
fetch("/dashboard/api/keys").then(r => r.ok ? r.json() : null).then(d => {
  document.getElementById("keys").innerHTML = d ? keysView(d) : "";
}).catch(() => {});

// Budgets: spent-vs-limit bar + window + policy. Neutral tokens.
function budgetsView(d) {
  const rows = (d.budgets || []).map(b => {
    const limit = b.limit_tokens || 0, spent = b.spent || 0;
    const pct = limit > 0 ? Math.min(100, Math.round(spent * 100 / limit)) : 0;
    const cls = pct >= 100 ? "over" : (pct >= 80 ? "warn" : "");
    return `<tr><td><code>${esc(b.scope)}</code></td>`
      + `<td>${fmt(spent)} <span class="muted">/ ${fmt(limit)}</span></td>`
      + `<td style="min-width:8rem"><div class="bar ${cls}"><span style="width:${pct}%"></span></div></td>`
      + `<td>${esc(b.window)}</td><td>${esc(b.policy)}</td></tr>`;
  }).join("");
  return `<table><thead><tr><th>scope</th><th class="num">spent / limit (tokens)</th><th>utilization</th><th>window</th><th>policy</th></tr></thead>`
    + `<tbody>${rows || '<tr><td colspan=5">no budgets configured</td></tr>'}</tbody></table>`;
}
fetch("/dashboard/api/budgets").then(r => r.ok ? r.json() : { budgets: [] }).then(d => {
  document.getElementById("budgets").innerHTML = budgetsView(d);
}).catch(() => { document.getElementById("budgets").innerHTML = ""; });

// Alerts: fired first, then all configured rules.
function alertRow(a) {
  const fired = a.last_fired_at ? `<span class="badge active">fired</span> ${esc(a.last_fired_at)}` : '<span class="muted">—</span>';
  return `<tr ${a.last_fired_at ? 'class="fired"' : ''}><td><code>${esc(a.id)}</code></td>`
    + `<td><code>${esc(a.scope)}</code></td><td class="num">${esc(a.threshold_pct)}%</td>`
    + `<td>${esc(a.channel)}</td><td>${fired}</td></tr>`;
}
function alertsView(d) {
  const fired = (d.fired || []).map(alertRow).join("");
  const rules = (d.rules || []).map(alertRow).join("");
  return `<h3 class="muted" style="font-size:.8rem;text-transform:uppercase;letter-spacing:.04em;margin-bottom:.25rem">Recently fired</h3>`
    + `<table><thead><tr><th>id</th><th>scope</th><th class="num">threshold</th><th>channel</th><th>last fired</th></tr></thead>`
    + `<tbody>${fired || '<tr><td colspan=5">none fired</td></tr>'}</tbody></table>`
    + `<h3 class="muted" style="font-size:.8rem;text-transform:uppercase;letter-spacing:.04em;margin:1rem 0 .25rem">All configured rules</h3>`
    + `<table><thead><tr><th>id</th><th>scope</th><th class="num">threshold</th><th>channel</th><th>last fired</th></tr></thead>`
    + `<tbody>${rules || '<tr><td colspan=5">no rules configured</td></tr>'}</tbody></table>`;
}
fetch("/dashboard/api/alerts").then(r => r.ok ? r.json() : null).then(d => {
  document.getElementById("alerts").innerHTML = d ? alertsView(d) : "";
}).catch(() => {});
</script>
</body>
</html>
"####;

async fn handle_openai(
    State(state): State<Arc<ProxyState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    handle(state, headers, body, IngressDialect::OpenAi).await
}

async fn handle_anthropic(
    State(state): State<Arc<ProxyState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    handle(state, headers, body, IngressDialect::Anthropic).await
}

async fn handle_responses(
    State(state): State<Arc<ProxyState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    handle(state, headers, body, IngressDialect::Responses).await
}

async fn handle(
    state: Arc<ProxyState>,
    headers: HeaderMap,
    body: Bytes,
    dialect: IngressDialect,
) -> Response {
    // 1. Virtual key from `Authorization: Bearer vk_…`. Resolve the live key store by exact token
    //    (legacy/demo path, where the id doubles as the token) then by its hash (operator-minted
    //    path, where only the hash is the lookup key — the plaintext is never retained).
    let Some(vk_token) = bearer(&headers) else {
        return error(StatusCode::UNAUTHORIZED, "missing bearer virtual key");
    };
    let vk = match resolve_virtual_key(&state, vk_token) {
        VirtualKeyResolution::Found(vk) => vk,
        VirtualKeyResolution::Expired => {
            return error(StatusCode::UNAUTHORIZED, "virtual key expired");
        }
        VirtualKeyResolution::NotFound => {
            return error(StatusCode::UNAUTHORIZED, "unknown virtual key");
        }
    };

    // 2. The upstream this key is bound to.
    let Some(provider) = state
        .providers
        .lock()
        .expect("providers poisoned")
        .get(&vk.upstream_ref)
        .cloned()
    else {
        return error(
            StatusCode::BAD_GATEWAY,
            "no upstream registered for this key",
        );
    };

    // 3. Decode the public ingress dialect into the one canonical runtime request.
    let Ok(body_json) = serde_json::from_slice::<Value>(&body) else {
        return ingress_error(dialect, StatusCode::BAD_REQUEST, "body is not valid JSON");
    };
    let session = headers
        .get("x-sandhi-session")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let route = match dialect {
        IngressDialect::OpenAi => "/v1/chat/completions",
        IngressDialect::Anthropic => "/v1/messages",
        IngressDialect::Responses => "/v1/responses",
    };
    let metadata = RequestMetadataV1 {
        session_id: session,
        virtual_key_id: Some(vk.id.clone()),
        subject_id: vk.subject_id.clone(),
        group_id: vk.group_id.clone(),
        route: Some(route.into()),
        // ADR-0005 D7 neutral identity: `idempotency-key` for reconcile-once, run/step/parent for
        // the agent cost-tree, W3C `traceparent` for external trace linkage. Optional metadata —
        // never pricing, never inside the cached wire body.
        idempotency_key: header_str(&headers, "idempotency-key"),
        run_id: header_str(&headers, "x-sandhi-run-id"),
        step_id: header_str(&headers, "x-sandhi-step-id"),
        parent_id: header_str(&headers, "x-sandhi-parent-id"),
        trace_context: header_str(&headers, "traceparent"),
    };
    let (mut request, wants_stream) = match decode_request(dialect, body_json, metadata) {
        Ok(decoded) => decoded,
        Err(message) => return ingress_error(dialect, StatusCode::BAD_REQUEST, &message),
    };

    // 4. Model allowlist (TD-0003 P4): if the resolved key carries a non-empty `models[]`, admit
    //    only a model on that list. Empty/absent allowlist = any model (unchanged). Enforced after
    //    vk auth + decode (so the request model is known) and before the budget reservation, so the
    //    ordering is vk auth → allowlist → budget → dispatch (a disallowed model never reserves).
    if !vk.permits_model(&request.model) {
        let allowed = vk.models.as_deref().unwrap_or(&[]);
        return ingress_error(
            dialect,
            StatusCode::FORBIDDEN,
            &format!(
                "model '{}' is not permitted for this virtual key (allowed models: {})",
                request.model,
                allowed.join(", ")
            ),
        );
    }

    // 5. Reserve a **ceiling** — a conservative upper bound (input estimate + the effective output
    //    max), not a lower-bound estimate (ADR-0005 D1). A call whose worst case would breach the
    //    cap is refused *before* dispatch, so a hard cap cannot be overshot. On a budget-capped
    //    scope where the client left the output unbounded, we also set that bound on the upstream
    //    request so the provider caps output — making the reservation enforceable. The measured
    //    `billable()` (cache split included, D4) replaces the reservation after completion.
    let scope = budget_scope(&vk);
    let policy = scope_policy(&state, &scope);
    // A scope is "capped" (for output-bounding) only under a hard `Block` cap: a `Warn` soft cap
    // never rejects, so we do not shrink the client's request. Bounding output makes the ceiling
    // reservation enforceable when the client left `max_output_tokens` unset (ADR-0005 D1).
    let capped = policy == Policy::Block
        && state
            .ledger
            .lock()
            .ok()
            .and_then(|ledger| ledger.limit(&scope))
            .is_some();
    let (ceiling, effective_max) = reservation_ceiling(&request);
    if capped && request.max_output_tokens.is_none() {
        request.max_output_tokens = Some(effective_max);
    }
    let reservation = match reserve_budget(&state, &scope, ceiling, policy) {
        Admission::Leased(reservation) => Some(reservation),
        // Fail-open (Warn on a backend error): admit without a lease; the usage event still emits.
        Admission::Unmetered => None,
        Admission::Denied => {
            return ingress_error(dialect, StatusCode::TOO_MANY_REQUESTS, "budget exhausted");
        }
    };

    let accounting = RequestAccounting::new(
        Arc::clone(&state),
        scope,
        reservation,
        provider.slug().into(),
        &request,
    );

    // Plane selection (ADR-0004 D1 / TD-0006): when the client's ingress dialect and the resolved
    // upstream are the SAME family, forward the client's bytes verbatim (transparent metering) —
    // no `ChatRequestV1` re-encode, so prompt-cache prefixes and provider-specific fields survive,
    // and usage is metered at the source. Cross-family (or a handle with no raw forwarder) falls
    // back to the typed translation path. Enforcement (reserve/settle via `accounting`) wraps both.
    let transparent =
        ingress_family(dialect) == provider.family() && provider.raw_forwarder().is_some();
    match (transparent, wants_stream) {
        (true, true) => transparent_stream_response(provider, body, dialect, accounting).await,
        (true, false) => transparent_complete_response(provider, body, dialect, accounting).await,
        (false, true) => stream_response(provider, request, dialect, accounting).await,
        (false, false) => complete_response(provider, request, dialect, accounting).await,
    }
}

/// Ingress dialect → the upstream family it maps to, for plane selection (TD-0006 Step 2).
fn ingress_family(dialect: IngressDialect) -> ProviderFamily {
    match dialect {
        IngressDialect::OpenAi => ProviderFamily::OpenAiCompat,
        IngressDialect::Anthropic => ProviderFamily::Anthropic,
        IngressDialect::Responses => ProviderFamily::OpenAiResponses,
    }
}

/// The upstream path suffix for a same-family transparent forward — mirrors each typed adapter's
/// endpoint. Only the three ingress families above ever reach the transparent plane.
fn upstream_path(family: ProviderFamily) -> &'static str {
    match family {
        ProviderFamily::OpenAiCompat => "/chat/completions",
        ProviderFamily::OpenAiResponses => "/responses",
        ProviderFamily::Anthropic => "/v1/messages",
        _ => "/",
    }
}

/// Rebuild an axum response from a raw upstream response: status + curated header allowlist + body
/// bytes, forwarded verbatim (the transparent plane never re-serializes the response).
fn raw_response_to_axum(raw: sandhi_providers::raw::RawResponse) -> Response {
    let mut builder = Response::builder().status(raw.status);
    for (name, value) in raw.headers.iter() {
        builder = builder.header(name, value);
    }
    builder
        .body(Body::from(raw.body))
        .unwrap_or_else(|_| error(StatusCode::BAD_GATEWAY, "invalid upstream response"))
}

/// Transparent same-family non-streaming plane: forward the client's bytes verbatim, meter usage
/// at the source, and return the upstream response unchanged (ADR-0004 D1). Enforcement rides on
/// `accounting` exactly as on the typed path.
async fn transparent_complete_response(
    provider: ProviderHandle,
    body: Bytes,
    dialect: IngressDialect,
    mut accounting: RequestAccounting,
) -> Response {
    let Some(forwarder) = provider.raw_forwarder() else {
        accounting.set_outcome("error");
        accounting.finalize();
        return error(
            StatusCode::BAD_GATEWAY,
            "transparent plane requires a raw forwarder",
        );
    };
    match forwarder
        .forward_metered(upstream_path(provider.family()), body)
        .await
    {
        Ok((raw, mut usage)) => {
            usage.completeness = UsageCompleteness::Final;
            usage.outcome.get_or_insert_with(|| "success".into());
            accounting.observe(&usage);
            accounting.set_outcome("success");
            accounting.finalize();
            raw_response_to_axum(raw)
        }
        Err(err) => {
            accounting.set_outcome("error");
            accounting.finalize();
            provider_error(&err, dialect, provider.slug())
        }
    }
}

/// Transparent same-family streaming plane: forward the upstream SSE bytes verbatim while the
/// metered stream accumulates usage at the source; the terminal frame finalizes the reservation. A
/// mid-stream disconnect settles the accrued (byte-approximate) partial via the `Drop` finalizer
/// rather than releasing to zero (ADR-0005 D1), as on the typed streaming path.
async fn transparent_stream_response(
    provider: ProviderHandle,
    body: Bytes,
    dialect: IngressDialect,
    mut accounting: RequestAccounting,
) -> Response {
    let Some(forwarder) = provider.raw_forwarder() else {
        accounting.set_outcome("error");
        accounting.finalize();
        return error(
            StatusCode::BAD_GATEWAY,
            "transparent plane requires a raw forwarder",
        );
    };
    let mut upstream = match forwarder
        .forward_stream_metered(upstream_path(provider.family()), body)
        .await
    {
        Ok(stream) => stream,
        Err(err) => {
            accounting.set_outcome("error");
            accounting.finalize();
            return provider_error(&err, dialect, provider.slug());
        }
    };

    let body_stream = async_stream::stream! {
        let mut seen_usage = false;
        let mut delta_bytes: u64 = 0;
        while let Some(item) = upstream.next().await {
            match item {
                Ok(chunk) => {
                    if let Some(parsed) = chunk.usage {
                        // Terminal frame: the finalized, source-measured usage.
                        let mut usage: UsageV2 = parsed.into();
                        usage.completeness = UsageCompleteness::Final;
                        usage.outcome.get_or_insert_with(|| "success".into());
                        accounting.observe(&usage);
                        seen_usage = true;
                    } else if !chunk.data.is_empty() {
                        // Running byte-approximate Partial so a disconnect settles accrued spend.
                        delta_bytes = delta_bytes.saturating_add(chunk.data.len() as u64);
                        if !seen_usage {
                            accounting.observe(&partial_usage(delta_bytes));
                        }
                    }
                    if !chunk.data.is_empty() {
                        yield Ok::<Bytes, std::io::Error>(chunk.data);
                    }
                }
                Err(_) => {
                    accounting.set_outcome("error");
                    break;
                }
            }
        }
        if accounting.outcome != "error" {
            accounting.set_outcome("success");
        }
        accounting.finalize();
    };

    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "text/event-stream")
        .body(Body::from_stream(body_stream))
        .expect("valid streaming response")
}

/// The enforcement policy configured for a scope (from the operator budgets map). Drives D6
/// fail-open/closed and whether the scope is a hard `Block` cap. Unset → `Block` (the safe default).
fn scope_policy(state: &ProxyState, scope: &str) -> Policy {
    state
        .budgets
        .lock()
        .ok()
        .and_then(|budgets| budgets.get(scope).map(|spec| Policy::parse(&spec.policy)))
        .unwrap_or(Policy::Block)
}

/// Reserve a ceiling lease for one in-flight call (ADR-0005 D1). A poisoned ledger lock is treated
/// as a backend failure and resolved by D6: `Warn` fails open (unmetered admit), `Block` fails
/// closed (deny).
fn reserve_budget(state: &ProxyState, scope: &str, ceiling: u64, policy: Policy) -> Admission {
    match state.ledger.lock() {
        Ok(mut ledger) => ledger.reserve(scope, ceiling, OffsetDateTime::now_utc(), policy),
        Err(_) => match policy {
            Policy::Warn => Admission::Unmetered,
            Policy::Block => Admission::Denied,
        },
    }
}

/// Owns the reservation and guarantees one terminal usage observation even when an HTTP body is
/// abandoned. Counts are always measured; an unavailable observation releases the reservation.
struct RequestAccounting {
    state: Arc<ProxyState>,
    scope: String,
    /// The held lease to settle by id (ADR-0005 D2). `None` when the scope admitted fail-open with
    /// no durable lease (D6) — nothing to settle.
    reservation: Option<Reservation>,
    provider: String,
    model: String,
    metadata: RequestMetadataV1,
    usage: Option<UsageV2>,
    outcome: &'static str,
    finalized: bool,
}

impl RequestAccounting {
    fn new(
        state: Arc<ProxyState>,
        scope: String,
        reservation: Option<Reservation>,
        provider: String,
        request: &ChatRequestV1,
    ) -> Self {
        Self {
            state,
            scope,
            reservation,
            provider,
            model: request.model.clone(),
            metadata: request.metadata.clone(),
            usage: None,
            outcome: "cancelled",
            finalized: false,
        }
    }

    fn observe(&mut self, usage: &UsageV2) {
        self.usage = Some(usage.clone());
    }

    fn set_outcome(&mut self, outcome: &'static str) {
        self.outcome = outcome;
    }

    /// Evaluate threshold alerts against the reconciled spend. Best-effort: any failure (registry
    /// poisoned, store unavailable) is logged and dropped — never propagated to the caller.
    fn fire_alerts(&self, spent: u64, limit: Option<u64>) {
        let Some(registry) = &self.state.alerts else {
            return;
        };
        let fired = match registry.lock() {
            Ok(mut reg) => reg.evaluate(&self.scope, spent, limit),
            Err(_) => return,
        };
        if let Some(store) = &self.state.alert_store {
            for alert in &fired {
                let _ = store.mark_fired(&alert.rule_id);
            }
        }
    }

    fn finalize(&mut self) {
        if self.finalized {
            return;
        }
        self.finalized = true;
        let mut usage = self.usage.take().unwrap_or_default();
        if usage.outcome.is_none() {
            usage.outcome = Some(self.outcome.into());
        }
        let measured = matches!(
            usage.completeness,
            UsageCompleteness::Final | UsageCompleteness::Partial
        );
        // Settle against the single neutral `billable()` (cache split included, ADR-0005 D4) so the
        // ledger and the emitted usage event count the same quantity. An unmeasured (failed /
        // cancelled) call settles `0`, which releases the lease without recording spend.
        let actual = if measured { billable(&usage) } else { 0 };
        // Settle the lease by id (idempotent, ADR-0005 D2), then capture the post-settle spent for
        // the alert subsystem. Alerts evaluate only on a measured call.
        let mut spent_after: Option<u64> = None;
        if let Ok(mut ledger) = self.state.ledger.lock() {
            if let Some(reservation) = &self.reservation {
                ledger.settle(reservation, actual);
            }
            if measured {
                spent_after = Some(ledger.spent(&self.scope));
            }
        }
        // P2: evaluate threshold alerts against the settled spend (best-effort — never breaks the
        // request). The configured limit comes from the budgets metadata map so a `Warn` scope (no
        // hard cap in the in-memory ledger) still has a threshold to measure against.
        if let Some(spent) = spent_after {
            let limit = self
                .state
                .budgets
                .lock()
                .ok()
                .and_then(|budgets| budgets.get(&self.scope).map(|spec| spec.limit_tokens));
            self.fire_alerts(spent, limit);
        }
        self.state.sink.emit(&usage_event(
            &self.provider,
            &self.model,
            &self.metadata,
            &usage,
        ));
    }
}

impl Drop for RequestAccounting {
    fn drop(&mut self) {
        self.finalize();
    }
}

async fn complete_response(
    provider: ProviderHandle,
    request: ChatRequestV1,
    dialect: IngressDialect,
    mut accounting: RequestAccounting,
) -> Response {
    match provider.complete(request).await {
        Ok(mut response) => {
            response.usage.completeness = UsageCompleteness::Final;
            response
                .usage
                .outcome
                .get_or_insert_with(|| "success".into());
            accounting.observe(&response.usage);
            accounting.set_outcome("success");
            accounting.finalize();
            Json(encode_response(dialect, &response)).into_response()
        }
        Err(error) => {
            accounting.set_outcome("error");
            accounting.finalize();
            provider_error(&error, dialect, provider.slug())
        }
    }
}

async fn stream_response(
    provider: ProviderHandle,
    request: ChatRequestV1,
    dialect: IngressDialect,
    mut accounting: RequestAccounting,
) -> Response {
    let mut upstream = match provider.stream(request).await {
        Ok(s) => s,
        Err(error) => {
            accounting.set_outcome("error");
            accounting.finalize();
            return provider_error(&error, dialect, provider.slug());
        }
    };

    let body = async_stream::stream! {
        let mut last_usage: Option<UsageV2> = None;
        let mut delta_out_bytes: u64 = 0;
        while let Some(item) = upstream.next().await {
            match item {
                Ok(event) => {
                    match &event {
                        sandhi_core::ChatStreamEventV1::Usage { usage } => {
                            // Terminal, authoritative usage — replaces any running partial estimate.
                            accounting.observe(usage);
                            last_usage = Some(usage.clone());
                        }
                        sandhi_core::ChatStreamEventV1::TextDelta { delta }
                        | sandhi_core::ChatStreamEventV1::ReasoningDelta { delta }
                        | sandhi_core::ChatStreamEventV1::RefusalDelta { delta }
                        | sandhi_core::ChatStreamEventV1::ToolCallArgumentsDelta { delta, .. } => {
                            delta_out_bytes = delta_out_bytes.saturating_add(delta.len() as u64);
                        }
                        sandhi_core::ChatStreamEventV1::Error { .. } => {
                            accounting.set_outcome("error");
                        }
                        _ => {}
                    }
                    // ADR-0005 D1: hold a running `Partial` estimate from the output deltas until the
                    // terminal usage arrives, so a mid-stream disconnect (which fires the Drop
                    // finalizer, not the code below) settles the accumulated spend instead of
                    // releasing to zero — closing the open-stream / read-a-lot / disconnect
                    // metering-evasion hole. Approximate (bytes/4); the terminal frame overrides it.
                    if last_usage.is_none() {
                        accounting.observe(&partial_usage(delta_out_bytes));
                    }
                    for (event_name, value) in
                        encode_stream_event(dialect, &event, last_usage.as_ref())
                    {
                        yield Ok::<Bytes, std::io::Error>(Bytes::from(sse_frame(event_name, &value)));
                    }
                }
                Err(error) => {
                    accounting.set_outcome("error");
                    let typed = sandhi_core::ChatStreamEventV1::Error {
                        error: error.as_typed(Some(provider.slug())),
                    };
                    for (event_name, value) in
                        encode_stream_event(dialect, &typed, last_usage.as_ref())
                    {
                        yield Ok::<Bytes, std::io::Error>(Bytes::from(sse_frame(event_name, &value)));
                    }
                    break;
                }
            }
        }
        if accounting.outcome != "error" {
            accounting.set_outcome("success");
        }
        accounting.finalize();
        if dialect == IngressDialect::OpenAi {
            yield Ok::<Bytes, std::io::Error>(Bytes::from_static(b"data: [DONE]\n\n"));
        }
    };

    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "text/event-stream")
        .body(Body::from_stream(body))
        .expect("valid streaming response")
}

/// Coarse input-token estimate: bytes of the prompt payload / 4. A known lower-bound approximation
/// (undercounts CJK, overcounts verbose tool schemas); a model-aware/tokenizer estimator is the
/// follow-up (ADR-0005 D1). The *output* side, not this, is the load-bearing part of the ceiling.
fn input_estimate(request: &ChatRequestV1) -> u64 {
    let bytes = serde_json::to_vec(&request.messages)
        .map(|value| value.len() as u64)
        .unwrap_or(0)
        .saturating_add(
            serde_json::to_vec(&request.tools)
                .map(|value| value.len() as u64)
                .unwrap_or(0),
        );
    bytes.saturating_add(3) / 4
}

/// The reservation **ceiling** (ADR-0005 D1): input estimate + the effective output max (the
/// client's `max_output_tokens`, or [`DEFAULT_OUTPUT_CEILING`] when unbounded). Returns the ceiling
/// and the effective max so the caller can bound a capped scope's upstream request. This is a
/// conservative upper bound, not the old `+ 1` lower-bound estimate that let streams overshoot.
fn reservation_ceiling(request: &ChatRequestV1) -> (u64, u64) {
    let effective_max = request.max_output_tokens.unwrap_or(DEFAULT_OUTPUT_CEILING);
    let ceiling = input_estimate(request).saturating_add(effective_max).max(1);
    (ceiling, effective_max)
}

/// A trimmed header value as an owned `String`, or `None` when absent/non-UTF-8/empty.
fn header_str(headers: &HeaderMap, name: &str) -> Option<String> {
    let value = headers.get(name)?.to_str().ok()?.trim();
    (!value.is_empty()).then(|| value.to_string())
}

/// A best-effort `Partial` usage synthesized from accumulated output-delta bytes, used to settle an
/// interrupted stream (ADR-0005 D1) rather than releasing the reservation to zero.
fn partial_usage(delta_out_bytes: u64) -> UsageV2 {
    UsageV2 {
        tokens_out: delta_out_bytes.saturating_add(3) / 4,
        completeness: UsageCompleteness::Partial,
        ..UsageV2::default()
    }
}

fn sse_frame(event: Option<&str>, value: &Value) -> String {
    let mut frame = String::new();
    if let Some(event) = event {
        frame.push_str("event: ");
        frame.push_str(event);
        frame.push('\n');
    }
    frame.push_str("data: ");
    frame.push_str(&serde_json::to_string(value).unwrap_or_else(|_| "{}".into()));
    frame.push_str("\n\n");
    frame
}

fn usage_event(
    provider: &str,
    model: &str,
    metadata: &RequestMetadataV1,
    usage: &UsageV2,
) -> UsageEvent {
    UsageEvent::new(
        usage
            .upstream_request_id
            .clone()
            .unwrap_or_else(next_request_id),
        now_rfc3339(),
        provider,
        model,
        Backend::External,
    )
    .with_attribution(
        metadata.virtual_key_id.clone(),
        metadata.subject_id.clone(),
        metadata.group_id.clone(),
    )
    .with_route(metadata.route.clone())
    .with_session(metadata.session_id.clone())
    .with_identity(
        metadata.idempotency_key.clone(),
        metadata.run_id.clone(),
        metadata.step_id.clone(),
        metadata.parent_id.clone(),
        metadata.trace_context.clone(),
    )
    .with_tokens(usage.tokens_in, usage.tokens_out)
    .with_cache(usage.cache_creation_tokens, usage.cache_read_tokens)
    .with_measurement(
        usage.completeness,
        usage.attempts,
        usage.outcome.clone(),
        usage.upstream_request_id.clone(),
    )
}

fn now_rfc3339() -> String {
    use time::format_description::well_known::Rfc3339;
    time::OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_default()
}

fn next_request_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or(0);
    format!("req_{millis}_{n}")
}

fn budget_scope(vk: &VirtualKey) -> String {
    // An operator-set explicit scope wins; otherwise derive from the group (the default
    // prompt-cache namespace) or fall back to the key itself.
    if let Some(scope) = vk.budget_scope.as_deref() {
        return scope.to_string();
    }
    match &vk.group_id {
        Some(g) => format!("group:{g}"),
        None => format!("vk:{}", vk.id),
    }
}

enum VirtualKeyResolution {
    Found(VirtualKey),
    NotFound,
    Expired,
}

/// Resolve a presented virtual-key token: exact (legacy demo) then by hash (operator-minted).
/// Filters out expired keys.
fn resolve_virtual_key(state: &ProxyState, token: &str) -> VirtualKeyResolution {
    let vk = state
        .keys
        .resolve(token)
        .or_else(|| state.keys.resolve(&hash_secret(token)));
    match vk {
        Some(vk) => {
            if vk.is_expired(&now_rfc3339()) {
                VirtualKeyResolution::Expired
            } else {
                VirtualKeyResolution::Found(vk)
            }
        }
        None => VirtualKeyResolution::NotFound,
    }
}

fn bearer(headers: &HeaderMap) -> Option<&str> {
    headers
        .get("authorization")?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
        .map(str::trim)
}

fn provider_error(e: &ProviderError, dialect: IngressDialect, provider: &str) -> Response {
    let (status, msg) = match e {
        ProviderError::InvalidRequest(_) => (StatusCode::BAD_REQUEST, "invalid provider request"),
        ProviderError::Auth => (StatusCode::BAD_GATEWAY, "upstream auth failed"),
        ProviderError::RateLimited => (StatusCode::TOO_MANY_REQUESTS, "upstream rate limited"),
        ProviderError::Upstream(_) => (StatusCode::BAD_GATEWAY, "upstream error"),
        ProviderError::Transport(_) => (StatusCode::BAD_GATEWAY, "upstream transport error"),
        ProviderError::CircuitOpen => (
            StatusCode::SERVICE_UNAVAILABLE,
            "circuit open (upstream failing)",
        ),
        ProviderError::Timeout(_) => (StatusCode::GATEWAY_TIMEOUT, "upstream timed out"),
        // ProviderError is #[non_exhaustive]; unknown future variants degrade to 502.
        _ => (StatusCode::BAD_GATEWAY, "upstream error"),
    };
    let typed = e.as_typed(Some(provider));
    let body = match dialect {
        IngressDialect::OpenAi | IngressDialect::Responses => json!({"error":typed}),
        IngressDialect::Anthropic => json!({"type":"error","error":typed}),
    };
    let _ = msg;
    (status, Json(body)).into_response()
}

fn ingress_error(dialect: IngressDialect, status: StatusCode, msg: &str) -> Response {
    let typed = json!({
        "code":"invalid_request",
        "message":msg,
        "retryable":false,
        "http_status":status.as_u16(),
    });
    let body = match dialect {
        IngressDialect::OpenAi | IngressDialect::Responses => json!({"error":typed}),
        IngressDialect::Anthropic => json!({"type":"error","error":typed}),
    };
    (status, Json(body)).into_response()
}

fn error(status: StatusCode, msg: &str) -> Response {
    (status, Json(json!({ "error": msg }))).into_response()
}
