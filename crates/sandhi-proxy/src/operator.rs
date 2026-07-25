//! TD-0003 operator surface — the admin REST API (P1 + P2).
//!
//! Routes (authed by an **admin token** distinct from virtual keys):
//! - `POST /admin/keys` / `GET /admin/keys` / `DELETE /admin/keys/{provider}/{label}` — provider
//!   credential vault (metadata in SQLite, secret in the active [`Vault`] backend).
//! - `POST /admin/keys/share` / `GET /admin/keys/virtual` / `DELETE /admin/vkeys/{id}` — mint /
//!   list / revoke scoped virtual keys (secret printed once, hash stored).
//! - `POST /admin/budget` / `GET /admin/budget` / `GET /admin/budget/usage?scope=` — neutral-token
//!   budgets (P2: window + policy + optional alert thresholds) over the [`BudgetLedger`].
//! - `GET /admin/alerts` / `POST /admin/alerts` / `POST /admin/alerts/{id}/ack` /
//!   `DELETE /admin/alerts/{id}` — threshold alert rules (P2).
//! - `GET /admin/usage?by=…&since=…` — attribution (wraps [`SqliteStore`] aggregates).
//!
//! Measure-vs-price boundary: budgets + alerts are neutral tokens / percentages; no dollars /
//! SKU / tier anywhere.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Json, Response};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use sandhi_core::{
    Alert, AlertChannel, AlertRegistry, KeyStore, Policy, VirtualKey, WebhookSender, Window,
};
use sandhi_providers::{
    AnthropicAuthScheme, GeminiAuthScheme, ProviderFamily, ProviderHandle, ProviderRuntime,
};
use sandhi_store::{
    AlertRuleRecord, AlertStore, CreateAlertRequest, CredentialScheme, VaultEntry, VaultError,
    VirtualKeyRecord, VirtualKeyStore,
};

use crate::{ProxyLedger, ProxyState};

/// Public request/response types — shared with the `sandhi` CLI client.
pub mod admin {
    use super::Deserialize;

    /// `POST /admin/keys` — add a provider credential to the vault.
    #[derive(Debug, Clone, Deserialize)]
    pub struct AddKeyRequest {
        pub provider: String,
        pub label: Option<String>,
        pub scheme: Option<String>,
        pub base_url: Option<String>,
        pub secret: String,
    }

    /// `POST /admin/keys/share` — mint a scoped virtual key.
    #[derive(Debug, Clone, Deserialize)]
    pub struct ShareKeyRequest {
        pub upstream: String,
        pub subject: Option<String>,
        pub group: Option<String>,
        pub models: Option<Vec<String>>,
        pub budget_scope: Option<String>,
        pub expires_at: Option<String>,
        pub rate_limit_per_min: Option<u32>,
    }

    /// `POST /admin/budget` — set a neutral-token budget on a scope.
    #[derive(Debug, Clone, Deserialize)]
    pub struct SetBudgetRequest {
        pub scope: String,
        pub limit_tokens: u64,
        /// `daily` / `monthly` / `total` (P2). Defaults to `total`.
        pub window: Option<String>,
        /// `block` / `warn` (P2). Defaults to `block`.
        pub policy: Option<String>,
        /// Optional threshold percentages (0–100) that create alert rules for this scope (P2).
        /// Each value creates a `log`-channel rule; a webhook URL may be configured separately via
        /// `/admin/alerts`.
        pub alert_thresholds: Option<Vec<u8>>,
    }

    /// `POST /admin/alerts` — create a threshold alert rule (P2).
    #[derive(Debug, Clone, Deserialize)]
    pub struct CreateAlertRequest {
        pub scope: String,
        pub threshold_pct: u8,
        /// `log` (default) or `webhook:<url>`.
        #[allow(clippy::option_option)]
        pub channel: Option<Option<String>>,
        /// Convenience: when set, builds a `webhook:<this_url>` channel.
        pub webhook_url: Option<String>,
    }

    /// CLI-side: which dimension to aggregate usage by.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum UsageDimension {
        Subject,
        Group,
        Provider,
        Model,
        Key,
        Session,
    }

    impl UsageDimension {
        pub fn parse(s: &str) -> Option<Self> {
            Some(match s {
                "subject" | "user" => Self::Subject,
                "group" => Self::Group,
                "provider" => Self::Provider,
                "model" => Self::Model,
                "key" | "virtual_key" => Self::Key,
                "session" => Self::Session,
                _ => return None,
            })
        }

        pub fn as_query(&self) -> &'static str {
            match self {
                Self::Subject => "subject",
                Self::Group => "group",
                Self::Provider => "provider",
                Self::Model => "model",
                Self::Key => "key",
                Self::Session => "session",
            }
        }
    }
}

/// A budget spec as recorded by the operator (neutral tokens).
#[derive(Debug, Clone, Serialize)]
pub struct BudgetSpec {
    pub scope: String,
    pub limit_tokens: u64,
    pub window: String,
    pub policy: String,
}

/// Admin auth. Returns `Ok(())` when the presented bearer matches the configured admin token.
/// `403` when no admin token is configured; `401` when missing/wrong.
#[allow(clippy::result_large_err)] // axum::Response is intentionally large; this is the idiomatic shape.
pub(crate) fn require_admin(state: &ProxyState, headers: &HeaderMap) -> Result<(), Response> {
    let Some(expected) = state.admin_token.as_deref() else {
        return Err(err(
            StatusCode::FORBIDDEN,
            "admin API not configured (set SANDHI_ADMIN_TOKEN)",
        ));
    };
    let presented = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .map(str::trim);
    match presented {
        Some(t) if constant_time_eq(t.as_bytes(), expected.as_bytes()) => Ok(()),
        _ => Err(err(StatusCode::UNAUTHORIZED, "invalid admin token")),
    }
}

/// Constant-time byte comparison for the admin token (ADR-0004 D4): the accumulator visits
/// every byte regardless of where the first mismatch is, so response timing does not leak a
/// prefix-match oracle. Length is compared by folding it into the accumulator (token length
/// is not a secret, but this keeps the shape branch-free).
pub(crate) fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    let mut acc = u8::from(a.len() != b.len());
    let max = a.len().max(b.len());
    for i in 0..max {
        // Out-of-range indexes fold in a constant; both slices are always walked to `max`.
        acc |= a.get(i).copied().unwrap_or(0) ^ b.get(i).copied().unwrap_or(0);
    }
    acc == 0
}

fn err(status: StatusCode, msg: &str) -> Response {
    (status, Json(json!({ "error": msg }))).into_response()
}

/// Build a typed upstream handle from a resolved credential. The family is inferred from the
/// provider slug; `base_url` defaults per family when unset. Public so the `sandhi-proxy` binary
/// can rehydrate handles from the vault on startup.
pub fn build_provider_handle(
    runtime: &ProviderRuntime,
    provider: &str,
    base_url: Option<&str>,
    secret: &str,
    scheme: CredentialScheme,
) -> Option<ProviderHandle> {
    let family = ProviderFamily::for_slug(provider);
    let base = base_url
        .map(str::to_string)
        .unwrap_or_else(|| default_base_url(provider, family));
    match family {
        ProviderFamily::Anthropic => {
            let auth = match scheme {
                CredentialScheme::Bearer => AnthropicAuthScheme::Bearer,
                _ => AnthropicAuthScheme::ApiKey,
            };
            Some(runtime.anthropic(base, secret, auth, None, None, None))
        }
        ProviderFamily::Gemini => {
            Some(runtime.gemini(base, secret, GeminiAuthScheme::ApiKey, None, None, None))
        }
        ProviderFamily::Cohere => Some(runtime.cohere(base, secret, None, None, None)),
        ProviderFamily::Ollama => Some(runtime.ollama(base, secret, None, None, None)),
        ProviderFamily::OpenAiCompat => Some(runtime.openai_compat(
            provider,
            base,
            secret,
            Default::default(),
            None,
            None,
            None,
        )),
        ProviderFamily::OpenAiResponses => Some(runtime.openai_responses(
            provider,
            base,
            secret,
            Default::default(),
            None,
            None,
            None,
        )),
    }
}

fn default_base_url(provider: &str, family: ProviderFamily) -> String {
    if family == ProviderFamily::OpenAiCompat {
        if let Some(spec) = sandhi_providers::resolve_openai_compat_provider(provider) {
            return spec.base_url.to_string();
        }
    }
    match family {
        ProviderFamily::Anthropic => "https://api.anthropic.com".into(),
        ProviderFamily::OpenAiCompat | ProviderFamily::OpenAiResponses => {
            "https://api.openai.com/v1".into()
        }
        ProviderFamily::Gemini => "https://generativelanguage.googleapis.com".into(),
        ProviderFamily::Cohere => "https://api.cohere.ai".into(),
        ProviderFamily::Ollama => "http://localhost:11434".into(),
    }
}

fn parse_scheme(s: Option<&str>) -> CredentialScheme {
    match s.map(str::to_ascii_lowercase).as_deref() {
        Some("bearer") => CredentialScheme::Bearer,
        Some("oauth") => CredentialScheme::Oauth,
        _ => CredentialScheme::ApiKey,
    }
}

pub(crate) fn vault_entry_response(e: &VaultEntry) -> Value {
    json!({
        "provider": e.provider,
        "label": e.label,
        "scheme": e.scheme,
        "base_url": e.base_url,
        "created_at": e.created_at,
        "status": e.status,
        "credential_id": e.credential_id(),
        // NB: the raw secret lives only in the Vault backend — never serialized here.
    })
}

pub(crate) fn vkey_record_response(r: &VirtualKeyRecord) -> Value {
    json!({
        "id": r.id,
        "subject": r.subject_id,
        "group": r.group_id,
        "upstream_ref": r.upstream_ref,
        "models": r.model_list(),
        "budget_scope": r.budget_scope,
        "expires_at": r.expires_at,
        "rate_limit_per_min": r.rate_limit_per_min,
        "created_at": r.created_at,
        "revoked_at": r.revoked_at,
        "status": if r.revoked_at.is_some() { "revoked" } else { "active" },
        // NB: secret_hash is intentionally NOT exposed over the API (nor the plaintext secret).
    })
}

// --- Handlers ----------------------------------------------------------------

/// `POST /admin/keys` — add a provider credential to the vault + register an upstream handle.
pub(crate) async fn add_key(
    State(state): State<Arc<ProxyState>>,
    headers: HeaderMap,
    Json(req): Json<admin::AddKeyRequest>,
) -> Response {
    if let Err(r) = require_admin(&state, &headers) {
        return r;
    }
    let Some(vault) = state.vault.clone() else {
        return err(
            StatusCode::SERVICE_UNAVAILABLE,
            "vault not configured (set SANDHI_STORE)",
        );
    };
    let label = req.label.as_deref().unwrap_or("default");
    let scheme = parse_scheme(req.scheme.as_deref());
    match vault.set(
        &req.provider,
        label,
        scheme,
        req.base_url.as_deref(),
        &req.secret,
    ) {
        Ok(cred_id) => {
            // Build + cache the upstream handle so the request path resolves it immediately.
            if let Some(handle) = build_provider_handle(
                &state.runtime,
                &req.provider,
                req.base_url.as_deref(),
                &req.secret,
                scheme,
            ) {
                state
                    .providers
                    .lock()
                    .expect("providers poisoned")
                    .insert(cred_id.clone(), handle);
            }
            let entry = vault
                .list()
                .unwrap_or_default()
                .into_iter()
                .find(|e| e.credential_id() == cred_id);
            let payload = match entry {
                Some(ref e) => vault_entry_response(e),
                None => json!({ "credential_id": cred_id }),
            };
            (StatusCode::CREATED, Json(payload)).into_response()
        }
        Err(VaultError::NotSupported(msg)) => err(StatusCode::NOT_IMPLEMENTED, &msg),
        Err(VaultError::Backend(msg)) => err(StatusCode::INTERNAL_SERVER_ERROR, &msg),
    }
}

/// `GET /admin/keys` — masked provider-credential metadata.
pub(crate) async fn list_keys(
    State(state): State<Arc<ProxyState>>,
    headers: HeaderMap,
) -> Response {
    if let Err(r) = require_admin(&state, &headers) {
        return r;
    }
    let Some(vault) = state.vault.clone() else {
        return err(StatusCode::SERVICE_UNAVAILABLE, "vault not configured");
    };
    let entries = vault.list().unwrap_or_default();
    Json(json!({ "keys": entries.iter().map(vault_entry_response).collect::<Vec<_>>() }))
        .into_response()
}

/// `DELETE /admin/keys/{provider}/{label}` — revoke a provider credential.
pub(crate) async fn revoke_key(
    State(state): State<Arc<ProxyState>>,
    headers: HeaderMap,
    Path((provider, label)): Path<(String, String)>,
) -> Response {
    if let Err(r) = require_admin(&state, &headers) {
        return r;
    }
    let Some(vault) = state.vault.clone() else {
        return err(StatusCode::SERVICE_UNAVAILABLE, "vault not configured");
    };
    match vault.revoke(&provider, &label) {
        Ok(revoked) => {
            if revoked {
                state
                    .providers
                    .lock()
                    .expect("providers poisoned")
                    .remove(&format!("{provider}:{label}"));
            }
            Json(json!({ "revoked": revoked })).into_response()
        }
        Err(VaultError::NotSupported(msg)) => err(StatusCode::NOT_IMPLEMENTED, &msg),
        Err(VaultError::Backend(msg)) => err(StatusCode::INTERNAL_SERVER_ERROR, &msg),
    }
}

/// `POST /admin/keys/share` — mint a scoped virtual key (secret printed once).
pub(crate) async fn share_key(
    State(state): State<Arc<ProxyState>>,
    headers: HeaderMap,
    Json(req): Json<admin::ShareKeyRequest>,
) -> Response {
    if let Err(r) = require_admin(&state, &headers) {
        return r;
    }
    // Require a configured durable vkey store.
    let Some(vkeys) = state.vkeys.clone() else {
        return err(
            StatusCode::SERVICE_UNAVAILABLE,
            "virtual-key store not configured (set SANDHI_STORE)",
        );
    };
    // The upstream credential must resolve (provider handle registered or vault entry active).
    let upstream = req.upstream.clone();
    if !state
        .providers
        .lock()
        .expect("providers poisoned")
        .contains_key(&upstream)
        && state
            .vault
            .as_ref()
            .and_then(|v| v.list().ok())
            .map(|list| {
                !list
                    .iter()
                    .any(|e| e.status == "active" && e.credential_id() == upstream)
            })
            .unwrap_or(true)
    {
        return err(
            StatusCode::BAD_REQUEST,
            &format!("unknown upstream '{upstream}': add it to the vault first (POST /admin/keys)"),
        );
    }

    let mint_req = sandhi_store::MintRequest {
        subject_id: req.subject.clone(),
        group_id: req.group.clone(),
        upstream_ref: upstream.clone(),
        models: req.models.clone().unwrap_or_default(),
        budget_scope: req.budget_scope.clone(),
        expires_at: req.expires_at.clone(),
        rate_limit_per_min: req.rate_limit_per_min,
    };
    let minted = match vkeys.mint(mint_req) {
        Ok(m) => m,
        Err(e) => {
            return err(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("mint failed: {e}"),
            )
        }
    };

    // Rehydrate the live key store keyed by the hash, so the request path resolves the presented
    // secret without the plaintext ever being retained as a lookup key.
    let live_key = VirtualKey {
        id: minted.record.id.clone(),
        subject_id: minted.record.subject_id.clone(),
        group_id: minted.record.group_id.clone(),
        upstream_ref: minted.record.upstream_ref.clone(),
        models: Some(minted.record.model_list()),
        budget_scope: minted.record.budget_scope.clone(),
        expires_at: minted.record.expires_at.clone(),
        rate_limit_per_min: minted.record.rate_limit_per_min,
    };
    state
        .keys
        .insert_keyed(minted.record.secret_hash.clone(), live_key);

    let endpoint = format!("{}/v1/chat/completions", state.public_url);
    Json(json!({
        "virtual_key": minted.secret,   // printed exactly once
        "id": minted.record.id,
        "upstream_ref": minted.record.upstream_ref,
        "endpoint": endpoint,
        "auth": "Authorization: Bearer <virtual_key>",
    }))
    .into_response()
}

/// `GET /admin/keys/virtual` — masked virtual-key listing.
pub(crate) async fn list_virtual_keys(
    State(state): State<Arc<ProxyState>>,
    headers: HeaderMap,
) -> Response {
    if let Err(r) = require_admin(&state, &headers) {
        return r;
    }
    let Some(vkeys) = state.vkeys.clone() else {
        return err(
            StatusCode::SERVICE_UNAVAILABLE,
            "virtual-key store not configured",
        );
    };
    let records = vkeys.list().unwrap_or_default();
    Json(json!({ "virtual_keys": records.iter().map(vkey_record_response).collect::<Vec<_>>() }))
        .into_response()
}

/// `DELETE /admin/vkeys/{id}` — revoke a virtual key by public id.
pub(crate) async fn revoke_virtual_key(
    State(state): State<Arc<ProxyState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Err(r) = require_admin(&state, &headers) {
        return r;
    }
    let Some(vkeys) = state.vkeys.clone() else {
        return err(
            StatusCode::SERVICE_UNAVAILABLE,
            "virtual-key store not configured",
        );
    };
    // Revoke in the durable store + drop the live entry (by hash, if present).
    let record = vkeys.find_by_id(&id).ok().flatten();
    let revoked = vkeys.revoke(&id).unwrap_or(false);
    if let Some(rec) = record {
        state.keys.revoke(&rec.secret_hash);
    }
    Json(json!({ "revoked": revoked })).into_response()
}

/// `POST /admin/budget` — set a neutral-token budget on a scope (P2: window + policy + alerts).
pub(crate) async fn set_budget(
    State(state): State<Arc<ProxyState>>,
    headers: HeaderMap,
    Json(req): Json<admin::SetBudgetRequest>,
) -> Response {
    if let Err(r) = require_admin(&state, &headers) {
        return r;
    }
    let spec = BudgetSpec {
        scope: req.scope.clone(),
        limit_tokens: req.limit_tokens,
        window: req.window.unwrap_or_else(|| "total".into()),
        policy: req.policy.unwrap_or_else(|| "block".into()),
    };
    apply_budget(&state.ledger, &state.budgets, &spec);

    // P2: optional threshold percentages create alert rules for this scope.
    let mut created_alerts: Vec<Value> = Vec::new();
    if let Some(thresholds) = &req.alert_thresholds {
        for &pct in thresholds {
            if let Some(alert) = create_alert_for_scope(&state, &req.scope, pct, AlertChannel::Log)
            {
                created_alerts.push(alert_rule_response(&alert));
            }
        }
    }

    Json(json!({
        "scope": spec.scope,
        "limit_tokens": spec.limit_tokens,
        "window": spec.window,
        "policy": spec.policy,
        "alerts_created": created_alerts,
    }))
    .into_response()
}

/// `GET /admin/budget` — list all configured budget scopes.
pub(crate) async fn list_budgets(
    State(state): State<Arc<ProxyState>>,
    headers: HeaderMap,
) -> Response {
    if let Err(r) = require_admin(&state, &headers) {
        return r;
    }
    let specs: Vec<BudgetSpec> = state
        .budgets
        .lock()
        .expect("budgets poisoned")
        .values()
        .cloned()
        .collect();
    Json(json!({ "budgets": specs })).into_response()
}

/// `GET /admin/budget/usage?scope=…` — spent-vs-limit for a scope.
pub(crate) async fn budget_usage(
    State(state): State<Arc<ProxyState>>,
    headers: HeaderMap,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    if let Err(r) = require_admin(&state, &headers) {
        return r;
    }
    let Some(scope) = params.get("scope") else {
        return err(StatusCode::BAD_REQUEST, "missing ?scope=");
    };
    let spent = state.ledger.lock().expect("ledger poisoned").spent(scope);
    let spec = state
        .budgets
        .lock()
        .expect("budgets poisoned")
        .get(scope)
        .cloned();
    let limit = spec.as_ref().map(|s| s.limit_tokens);
    Json(json!({
        "scope": scope,
        "limit_tokens": limit,
        "spent": spent,
        "remaining": limit.map(|l| l.saturating_sub(spent)),
        "policy": spec.as_ref().map(|s| s.policy.clone()),
        "window": spec.as_ref().map(|s| s.window.clone()),
    }))
    .into_response()
}

/// `GET /admin/usage?by=…&since=…` — attribution aggregates (wraps the durable store).
pub(crate) async fn usage(
    State(state): State<Arc<ProxyState>>,
    headers: HeaderMap,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    if let Err(r) = require_admin(&state, &headers) {
        return r;
    }
    let Some(store) = state.store.clone() else {
        return err(
            StatusCode::SERVICE_UNAVAILABLE,
            "usage store not configured (set SANDHI_STORE)",
        );
    };
    let by = params.get("by").map(String::as_str).unwrap_or("subject");
    let since = params.get("since").cloned();

    let total = store.grand_total().ok();
    let buckets = dimension_buckets(&store, by, since.as_deref());
    Json(json!({
        "dimension": by,
        "since": since,
        "total": total,
        "buckets": buckets.unwrap_or_default(),
    }))
    .into_response()
}

fn dimension_buckets(
    store: &Arc<sandhi_store::SqliteStore>,
    by: &str,
    since: Option<&str>,
) -> Option<Vec<Value>> {
    let validate = |dim: &str| -> bool {
        matches!(
            dim,
            "subject" | "user" | "group" | "provider" | "model" | "key" | "virtual_key" | "session"
        )
    };
    if !validate(by) {
        return None;
    }
    let buckets: Vec<sandhi_store::Bucket> = if let Some(since) = since {
        store.totals_since(by, since).ok()??
    } else {
        match by {
            "subject" | "user" => store.totals_by_subject().ok()?,
            "group" => store.totals_by_group().ok()?,
            "provider" => store.totals_by_provider().ok()?,
            "model" => store.totals_by_model().ok()?,
            "key" | "virtual_key" => store.totals_by_virtual_key().ok()?,
            "session" => store.totals_by_session().ok()?,
            _ => return None,
        }
    };
    Some(
        buckets
            .into_iter()
            .map(|b| serde_json::to_value(b).unwrap_or_else(|_| json!({})))
            .collect(),
    )
}

fn apply_budget(
    ledger: &Mutex<ProxyLedger>,
    budgets: &Mutex<HashMap<String, BudgetSpec>>,
    spec: &BudgetSpec,
) {
    // Carry the cap + window + policy into the live lease ledger (ADR-0005). A `Warn` scope stays a
    // soft cap (the ledger admits over it and tracks spend for alerts); `Block` hard-enforces.
    let window = Window::parse(&spec.window);
    let policy = Policy::parse(&spec.policy);
    ledger.lock().expect("ledger poisoned").set_budget(
        &spec.scope,
        Some(spec.limit_tokens),
        window,
        policy,
    );
    budgets
        .lock()
        .expect("budgets poisoned")
        .insert(spec.scope.clone(), spec.clone());
}

pub(crate) fn alert_rule_response(rec: &AlertRuleRecord) -> Value {
    json!({
        "id": rec.id,
        "scope": rec.scope,
        "threshold_pct": rec.threshold_pct,
        "channel": rec.channel,
        "created_at": rec.created_at,
        "last_fired_at": rec.last_fired_at,
        "acked_at": rec.acked_at,
    })
}

/// Persist a new alert rule + mirror it into the live registry. Returns the record on success.
fn create_alert_for_scope(
    state: &ProxyState,
    scope: &str,
    threshold_pct: u8,
    channel: AlertChannel,
) -> Option<AlertRuleRecord> {
    let store = state.alert_store.clone()?;
    let record = store
        .create(CreateAlertRequest {
            scope: scope.into(),
            threshold_pct,
            channel: channel.clone(),
        })
        .ok()?;
    if let Some(registry) = &state.alerts {
        if let Ok(mut reg) = registry.lock() {
            reg.add_rule(record.to_rule());
        }
    }
    Some(record)
}

// --- Alerts (P2) ------------------------------------------------------------

/// `GET /admin/alerts?scope=…` — list alert rules (optionally filtered by scope).
pub(crate) async fn list_alerts(
    State(state): State<Arc<ProxyState>>,
    headers: HeaderMap,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    if let Err(r) = require_admin(&state, &headers) {
        return r;
    }
    let Some(store) = state.alert_store.clone() else {
        return err(
            StatusCode::SERVICE_UNAVAILABLE,
            "alert store not configured (set SANDHI_STORE)",
        );
    };
    let records = match params.get("scope") {
        Some(scope) => store.list_by_scope(scope).unwrap_or_default(),
        None => store.list().unwrap_or_default(),
    };
    Json(json!({ "alerts": records.iter().map(alert_rule_response).collect::<Vec<_>>() }))
        .into_response()
}

/// `POST /admin/alerts` — create a threshold alert rule.
pub(crate) async fn create_alert(
    State(state): State<Arc<ProxyState>>,
    headers: HeaderMap,
    Json(req): Json<admin::CreateAlertRequest>,
) -> Response {
    if let Err(r) = require_admin(&state, &headers) {
        return r;
    }
    let channel = if let Some(url) = req.webhook_url.as_deref() {
        AlertChannel::Webhook {
            url: url.to_string(),
        }
    } else {
        req.channel
            .flatten()
            .as_deref()
            .map(AlertChannel::parse)
            .unwrap_or(AlertChannel::Log)
    };
    match create_alert_for_scope(&state, &req.scope, req.threshold_pct, channel) {
        Some(rec) => (StatusCode::CREATED, Json(alert_rule_response(&rec))).into_response(),
        None => err(
            StatusCode::SERVICE_UNAVAILABLE,
            "alert store not configured (set SANDHI_STORE)",
        ),
    }
}

/// `POST /admin/alerts/{id}/ack` — acknowledge a fired alert.
pub(crate) async fn ack_alert(
    State(state): State<Arc<ProxyState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Err(r) = require_admin(&state, &headers) {
        return r;
    }
    let Some(store) = state.alert_store.clone() else {
        return err(
            StatusCode::SERVICE_UNAVAILABLE,
            "alert store not configured",
        );
    };
    match store.ack(&id) {
        Ok(acked) => Json(json!({ "acked": acked })).into_response(),
        Err(e) => err(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("ack failed: {e}"),
        ),
    }
}

/// `DELETE /admin/alerts/{id}` — delete an alert rule.
pub(crate) async fn delete_alert(
    State(state): State<Arc<ProxyState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Err(r) = require_admin(&state, &headers) {
        return r;
    }
    let Some(store) = state.alert_store.clone() else {
        return err(
            StatusCode::SERVICE_UNAVAILABLE,
            "alert store not configured",
        );
    };
    let deleted = store.delete(&id).unwrap_or(false);
    if deleted {
        if let Some(registry) = &state.alerts {
            if let Ok(mut reg) = registry.lock() {
                reg.remove_rule(&id);
            }
        }
    }
    Json(json!({ "deleted": deleted })).into_response()
}

/// A best-effort, non-blocking webhook transport: spawns the POST onto the tokio runtime so a slow
/// or failing endpoint can never block a request. Falls back to `None` when no runtime is active
/// (the registry then degrades webhook rules to log-only).
pub(crate) struct TokioWebhookSender {
    client: reqwest::Client,
    handle: tokio::runtime::Handle,
}

impl TokioWebhookSender {
    /// Capture the current tokio runtime handle. `None` when called outside a runtime.
    pub(crate) fn new() -> Option<Self> {
        let handle = tokio::runtime::Handle::try_current().ok()?;
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .ok()?;
        Some(Self { client, handle })
    }
}

impl WebhookSender for TokioWebhookSender {
    fn send(&self, url: &str, alert: &Alert) {
        // A webhook failure must never break the request (TD-0003 P2, best-effort contract).
        let url = match url.parse::<reqwest::Url>() {
            Ok(u) => u,
            Err(e) => {
                eprintln!("sandhi-proxy: bad webhook url {url}: {e}");
                return;
            }
        };
        let client = self.client.clone();
        let payload = serde_json::to_value(alert).unwrap_or_else(|_| json!({}));
        self.handle.spawn(async move {
            match client.post(url).json(&payload).send().await {
                Ok(r) if !r.status().is_success() => {
                    eprintln!("sandhi-proxy: webhook returned {}", r.status());
                }
                Err(e) => eprintln!("sandhi-proxy: webhook failed: {e}"),
                _ => {}
            }
        });
    }
}

/// Rehydrate the live [`KeyStore`] from a durable [`VirtualKeyStore`] (called on startup).
pub fn rehydrate_live_keys(keys: &KeyStore, vkeys: &VirtualKeyStore) {
    let Ok(records) = vkeys.list() else {
        return;
    };
    for rec in records.into_iter().filter(|r| r.revoked_at.is_none()) {
        let vk = VirtualKey {
            id: rec.id.clone(),
            subject_id: rec.subject_id.clone(),
            group_id: rec.group_id.clone(),
            upstream_ref: rec.upstream_ref.clone(),
            models: Some(rec.model_list()),
            budget_scope: rec.budget_scope.clone(),
            expires_at: rec.expires_at.clone(),
            rate_limit_per_min: rec.rate_limit_per_min,
        };
        keys.insert_keyed(rec.secret_hash, vk);
    }
}

/// Rehydrate the operator's in-memory budget metadata from the durable ledger (called on startup).
/// The durable ledger already carries the enforced caps + spend across a restart (ADR-0005 D3); this
/// recovers the [`BudgetSpec`] map the policy lookup, dashboard, and alert thresholds read. A no-op
/// for the volatile in-memory ledger (it has nothing persisted).
pub fn rehydrate_budgets(ledger: &ProxyLedger, budgets: &Mutex<HashMap<String, BudgetSpec>>) {
    let mut map = budgets.lock().expect("budgets poisoned");
    for row in ledger.budgets() {
        let spec = BudgetSpec {
            scope: row.scope.clone(),
            limit_tokens: row.limit.unwrap_or(0),
            window: row.window.as_str().to_string(),
            policy: row.policy.as_str().to_string(),
        };
        map.insert(row.scope, spec);
    }
}

/// Build the live [`AlertRegistry`] from a durable [`AlertStore`] (called on startup). Loads every
/// persisted rule + its `last_fired_at` so dedup survives restarts, and injects the tokio-backed
/// webhook transport when a runtime is active.
pub fn rehydrate_alerts(store: &AlertStore) -> AlertRegistry {
    use sandhi_core::{NoopWebhookSender, DEFAULT_COOLDOWN_SECS};
    let sender: Box<dyn WebhookSender> = match TokioWebhookSender::new() {
        Some(s) => Box::new(s),
        None => Box::new(NoopWebhookSender),
    };
    let mut registry = AlertRegistry::new(DEFAULT_COOLDOWN_SECS, sender);
    if let Ok(records) = store.list() {
        for rec in records {
            registry.set_last_fired_at(&rec.scope, &rec.id, rec.last_fired_at.as_deref());
            registry.add_rule(rec.to_rule());
        }
    }
    registry
}

#[cfg(test)]
mod ct_tests {
    use super::constant_time_eq;

    #[test]
    fn constant_time_eq_matches_equality_semantics() {
        assert!(constant_time_eq(b"admin-secret", b"admin-secret"));
        assert!(constant_time_eq(b"", b""));
        // Same length, differing at the first / middle / last byte.
        assert!(!constant_time_eq(b"admin-secret", b"bdmin-secret"));
        assert!(!constant_time_eq(b"admin-secret", b"admin+secret"));
        assert!(!constant_time_eq(b"admin-secret", b"admin-secreT"));
        // Length mismatches, including prefix relationships.
        assert!(!constant_time_eq(b"admin", b"admin-secret"));
        assert!(!constant_time_eq(b"admin-secret", b"admin"));
        assert!(!constant_time_eq(b"", b"x"));
    }
}
