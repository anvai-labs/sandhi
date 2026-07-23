//! TD-0003 P1 operator surface — the admin REST API.
//!
//! Routes (authed by an **admin token** distinct from virtual keys):
//! - `POST /admin/keys` / `GET /admin/keys` / `DELETE /admin/keys/{provider}/{label}` — provider
//!   credential vault (metadata in SQLite, secret in the active [`Vault`] backend).
//! - `POST /admin/keys/share` / `GET /admin/keys/virtual` / `DELETE /admin/vkeys/{id}` — mint /
//!   list / revoke scoped virtual keys (secret printed once, hash stored).
//! - `POST /admin/budget` / `GET /admin/budget` / `GET /admin/budget/usage?scope=` — neutral-token
//!   budgets over the existing [`BudgetLedger`].
//! - `GET /admin/usage?by=…&since=…` — attribution (wraps [`SqliteStore`] aggregates).
//!
//! Measure-vs-price boundary: budgets are neutral tokens; no dollars/SKU/tier anywhere.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Json, Response};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use sandhi_core::{Budget, BudgetLedger, KeyStore, VirtualKey};
use sandhi_providers::{
    AnthropicAuthScheme, GeminiAuthScheme, ProviderFamily, ProviderHandle, ProviderRuntime,
};
use sandhi_store::{CredentialScheme, VaultEntry, VaultError, VirtualKeyRecord, VirtualKeyStore};

use crate::ProxyState;

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
        /// `daily` / `monthly` / `total` (window refinement is P2; P1 stores + enforces `total`).
        pub window: Option<String>,
        /// `block` / `warn` (warn policy is P2; P1 enforces block).
        pub policy: Option<String>,
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
        Some(t) if t == expected => Ok(()),
        _ => Err(err(StatusCode::UNAUTHORIZED, "invalid admin token")),
    }
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

fn vault_entry_response(e: &VaultEntry) -> Value {
    json!({
        "provider": e.provider,
        "label": e.label,
        "scheme": e.scheme,
        "base_url": e.base_url,
        "created_at": e.created_at,
        "status": e.status,
        "credential_id": e.credential_id(),
    })
}

fn vkey_record_response(r: &VirtualKeyRecord) -> Value {
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
        // NB: secret_hash is intentionally NOT exposed over the API.
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

/// `POST /admin/budget` — set a neutral-token budget on a scope.
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
    Json(json!({
        "scope": spec.scope,
        "limit_tokens": spec.limit_tokens,
        "window": spec.window,
        "policy": spec.policy,
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
    ledger: &Mutex<BudgetLedger>,
    budgets: &Mutex<HashMap<String, BudgetSpec>>,
    spec: &BudgetSpec,
) {
    // P1 enforces the `block` policy over the existing ledger (window + warn are P2).
    ledger
        .lock()
        .expect("ledger poisoned")
        .set_limit(spec.scope.clone(), Budget::tokens(spec.limit_tokens));
    budgets
        .lock()
        .expect("budgets poisoned")
        .insert(spec.scope.clone(), spec.clone());
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
