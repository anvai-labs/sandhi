//! TD-0003 operator surface — the admin REST API (P1 + P2).
//!
//! Routes (authed by an **admin token** distinct from virtual keys):
//! - `POST /admin/keys` / `GET /admin/keys` / `DELETE /admin/keys/{provider}/{label}` — provider
//!   credential vault (metadata in SQLite, secret in the active [`Vault`] backend).
//! - `POST /admin/keys/share` / `GET /admin/keys/virtual` / `DELETE /admin/vkeys/{id}` — mint /
//!   list / revoke scoped virtual keys (secret printed once, hash stored).
//! - `POST /admin/profile-tokens` / `POST /admin/profile-tokens/revoke` — mint / revoke the
//!   `skp_…` profile-download tokens that authenticate `GET /auth/profile` (shown once, hash
//!   stored — gateway-profiles Phase 5).
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
    AlertRuleRecord, AlertStore, CreateAlertRequest, CredentialScheme, SentinelPassExactBinding,
    SentinelPassGrantAcl, VaultEntry, VaultError, VaultReferencePlan, VirtualKeyRecord,
    VirtualKeyStore,
};

use crate::access::{admit_mutation, constant_time_eq, err};
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
        pub user_agent: Option<String>,
        pub secret: String,
    }

    /// Register an existing exact backend reference using only a read grant. Never accepts secrets.
    #[derive(Debug, Clone, Deserialize)]
    #[serde(deny_unknown_fields)]
    pub struct RegisterReferenceRequest {
        pub provider: String,
        pub label: Option<String>,
        pub scheme: Option<String>,
        pub base_url: Option<String>,
        /// Per-endpoint User-Agent: rides the transport's static headers to the
        /// upstream. Agent-allowlisting plans (e.g. Aliyun coding) reject an
        /// empty/default-less identity; set the UA the endpoint expects.
        pub user_agent: Option<String>,
        /// Exact SentinelPass SP-1 entry id. Must be supplied as a pair with
        /// `sentinelpass_token_env`; the service token itself is never accepted.
        pub sentinelpass_entry_id: Option<i64>,
        /// Environment variable holding the exact-entry service-grant token.
        pub sentinelpass_token_env: Option<String>,
        /// Non-secret policy class copied from the SentinelPass grant report.
        /// Protocol 0.20 does not expose a read-only grant inspection, so this
        /// declaration is projection metadata rather than daemon-verified state.
        pub sentinelpass_grant_acl: Option<SentinelPassGrantAclRequest>,
    }

    /// Operator-declared ACL class for an exact SentinelPass service grant.
    #[derive(Debug, Clone, Deserialize)]
    #[serde(deny_unknown_fields)]
    pub struct SentinelPassGrantAclRequest {
        pub exe_pinned: bool,
        pub fingerprint_enrolled: bool,
        /// Unix seconds; omit when the grant has no expiry.
        pub expires_at: Option<i64>,
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

    /// `POST /admin/profile-tokens` — mint a profile-download token (skp_…) for a subject.
    #[derive(Debug, Clone, Deserialize)]
    pub struct ProfileTokenRequest {
        pub subject: String,
        pub group: String,
        /// Accepted for forward compatibility only: v1 profile tokens have NO server-side
        /// expiry — retire one with `POST /admin/profile-tokens/revoke`. Persisting an expiry
        /// would also change the `profile_tokens` schema; that lands with the OIDC seam.
        #[serde(default)]
        pub ttl_seconds: Option<u64>,
    }

    /// `POST /admin/profile-tokens/revoke` — revoke every active token for (subject, group).
    #[derive(Debug, Clone, Deserialize)]
    pub struct RevokeProfileTokensRequest {
        pub subject: String,
        pub group: String,
    }

    /// `POST /admin/budget` — set a neutral-token budget on a scope.
    #[derive(Debug, Clone, Deserialize)]
    #[serde(deny_unknown_fields)]
    pub struct SetBudgetRequest {
        pub scope: String,
        /// Canonical per-window hierarchy. Keys are one or more of `hourly`, `daily`, `weekly`,
        /// `monthly` and `total`; each value carries that window's cap and policy.
        pub limits: std::collections::BTreeMap<String, BudgetLimitRequest>,
        /// Optional threshold percentages (0–100) that create alert rules for this scope (P2).
        /// Each value creates a `log`-channel rule; a webhook URL may be configured separately via
        /// `/admin/alerts`.
        pub alert_thresholds: Option<Vec<u8>>,
    }

    #[derive(Debug, Clone, Deserialize, serde::Serialize, PartialEq, Eq)]
    #[serde(deny_unknown_fields)]
    pub struct BudgetLimitRequest {
        pub limit_tokens: u64,
        /// `block` / `warn`.
        pub policy: String,
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
        Run,
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
                "run" => Self::Run,
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
                Self::Run => "run",
            }
        }
    }
}

/// A budget spec as recorded by the operator (neutral tokens).
#[derive(Debug, Clone, Serialize)]
pub struct BudgetSpec {
    pub scope: String,
    pub windows: Vec<BudgetWindowSpec>,
}

#[derive(Debug, Clone, Serialize)]
pub struct BudgetWindowSpec {
    pub limit_tokens: u64,
    pub window: String,
    pub policy: String,
}

/// Admin auth. Returns `Ok(())` when the presented bearer matches the configured admin token.
/// `403` when no admin token is configured; `401` when missing/wrong.
#[allow(clippy::result_large_err)] // axum::Response is intentionally large; this is the idiomatic shape.
/// `GET /admin/topology` — the optional deployment-topology projection
/// (`SANDHI_TOPOLOGY_FILE`, crates/sandhi-proxy/src/topology.rs). Secret-free
/// by construction; 404 when the feature is off, Read-gated like every other
/// operator read.
pub(crate) async fn topology(State(state): State<Arc<ProxyState>>, headers: HeaderMap) -> Response {
    if let Err(r) = require_access(&state, &headers, crate::auth::Permission::Read, false).await {
        return r;
    }
    let Some(file) = state.topology.clone() else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error": {"code": "topology_unconfigured",
                "message": "topology not configured (set SANDHI_TOPOLOGY_FILE)"}})),
        )
            .into_response();
    };
    Json(crate::topology::topology_response(&file)).into_response()
}

#[allow(clippy::result_large_err)] // axum::Response is intentionally large; this is the idiomatic shape.
/// Protected policy metadata, never prompt text, patterns, selectors or matched values.
pub(crate) async fn policy_status(
    State(state): State<Arc<ProxyState>>,
    headers: HeaderMap,
) -> Response {
    if let Err(r) = require_access(&state, &headers, crate::auth::Permission::Read, false).await {
        return r;
    }
    let Some(policy) = state.policy.clone() else {
        return Json(json!({"enabled":false})).into_response();
    };
    match tokio::task::spawn_blocking(move || policy.status()).await {
        Ok(Ok(status)) => Json(status).into_response(),
        _ => err(StatusCode::SERVICE_UNAVAILABLE, "policy status unavailable"),
    }
}

/// `GET /admin/version` — capability detail beyond the versions (TD-0021 P2, D5/R2).
///
/// The ungated `/version` answers "which contract am I talking to"; this answers
/// "which optional features are on" — operator information, so it sits behind the
/// same admin gate as the rest of `/admin`. The booleans mirror what the proxy was
/// BUILT with and CONFIGURED with at startup; a consumer degrades deliberately
/// instead of by trial and error (the HTTP form of TD-0008's feature detection).
pub(crate) async fn version_capabilities(
    State(state): State<Arc<ProxyState>>,
    headers: HeaderMap,
) -> Response {
    if let Err(r) = require_access(&state, &headers, crate::auth::Permission::Read, false).await {
        return r;
    }
    // The owner is installed only after validating the supplied single file-backed
    // ledger. A usage store alone proves neither execution ownership nor topology.
    // This is a configured contract fact, not a live database-health probe.
    let execution_accounting = state.buffered_accounting.get().is_some();
    let body = serde_json::json!({
        "wire_contract_version": sandhi_core::UsageEvent::SCHEMA_VERSION,
        "chat_contract_version": sandhi_core::CHAT_SCHEMA_VERSION_V1,
        "chat_contract_minor": sandhi_core::CHAT_CONTRACT_MINOR,
        "dialects": ["openai", "anthropic", "responses", "gemini"],
        "capabilities": {
            // The two-plane proxy is structural (ADR-0004 D1), not optional.
            "transparent_plane": true,
            "durable_ledger": execution_accounting,
            "execution_accounting": if execution_accounting {
                "paired_durable_v1"
            } else {
                "unavailable"
            },
            "alerts": state.alerts.is_some(),
            "admin_api": state.oidc.is_some() || state.admin_token.is_some() || crate::starter::enabled(&state),
            // TD-0012: per-vkey token bucket, in-memory (N replicas multiply it).
            "rate_limits": true,
            // Compiled only with the otel-otlp feature AND initialized at startup.
            "otel_export": state.otel.is_some(),
            "vault": state.vault.as_ref().map(|v| json!({
                "backend": v.backend_name(), "operations": v.capabilities(),
                "grant_status": "not_checked", "credential_generations": false,
                "reference_registration": v.capabilities().read && v.backend_name() != "sentinelpass",
            })),
        },
    });
    Json(body).into_response()
}

/// OIDC takes precedence over all compatibility token/public settings.
#[allow(clippy::result_large_err)] // Same ready-to-return axum denial as the existing authorization gates.
pub(crate) async fn require_access(
    state: &ProxyState,
    headers: &HeaderMap,
    permission: crate::auth::Permission,
    mutation: bool,
) -> Result<(), Response> {
    if let Some(oidc) = &state.oidc {
        return oidc.authorize(headers, permission, mutation).await;
    }
    if state.auth_mode == crate::auth::AuthMode::Oidc {
        return Err(err(
            StatusCode::SERVICE_UNAVAILABLE,
            "OIDC authority unavailable",
        ));
    }
    if crate::starter::enabled(state) && !crate::starter::control_allowed(state, headers, mutation)
    {
        return Err(err(
            StatusCode::FORBIDDEN,
            "starter control request requires a trusted Host and same origin",
        ));
    }
    if crate::starter::enabled(state) && state.admin_token.is_none() {
        return Ok(());
    }
    require_admin(state, headers)
}

// The Err is a ready-to-return HTTP Response (status + shaped body) — it is handed
// straight to the caller and never held; boxing it would add an allocation to every
// gated handler for no gain.
#[allow(clippy::result_large_err)]
pub(crate) fn require_admin(state: &ProxyState, headers: &HeaderMap) -> Result<(), Response> {
    let Some(expected) = state.admin_token.as_deref() else {
        return Err(err(
            StatusCode::FORBIDDEN,
            "admin API not configured (set SANDHI_ADMIN_TOKEN)",
        ));
    };
    if expected.is_empty()
        || !expected.is_ascii()
        || expected
            .chars()
            .any(|c| c.is_whitespace() || c.is_control())
    {
        return Err(err(
            StatusCode::SERVICE_UNAVAILABLE,
            "invalid admin token configuration",
        ));
    }
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

/// Preserve only our handler's canonical, secret-free reconciliation fields in a batch result.
async fn config_component_failure(mut failure: Value, response: Response) -> Value {
    failure["status"] = json!(response.status().as_u16());
    if let Ok(bytes) = axum::body::to_bytes(response.into_body(), 4096).await {
        if let Ok(details) = serde_json::from_slice::<Value>(&bytes) {
            for field in [
                "code",
                "reconcile_before_retry",
                "metadata_committed",
                "credential_id",
            ] {
                if let Some(value) = details.get(field) {
                    failure[field] = value.clone();
                }
            }
        }
    }
    failure
}

#[derive(Debug)]
enum AlertApplyError {
    Draining,
    Store(String),
}

impl AlertApplyError {
    fn failure(&self, scope: &str, threshold_pct: u8) -> Value {
        let mut failure = json!({"component": "alert", "scope": scope,
            "threshold_pct": threshold_pct, "error": "alert apply failed"});
        if matches!(self, Self::Draining) {
            failure["code"] = json!("gateway_draining");
            failure["status"] = json!(503);
        }
        failure
    }
}

impl From<String> for AlertApplyError {
    fn from(message: String) -> Self {
        Self::Store(message)
    }
}

impl std::fmt::Display for AlertApplyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Draining => f.write_str("gateway_draining"),
            Self::Store(message) => f.write_str(message),
        }
    }
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
    user_agent: Option<&str>,
) -> Option<ProviderHandle> {
    build_provider_handle_with_endpoint(
        runtime, provider, base_url, secret, scheme, user_agent, None,
    )
}

/// Build an explicitly bound endpoint without provider-name protocol inference.
/// This is a runtime factory seam; it does not register credentials or authorize routes.
pub fn build_provider_handle_for_endpoint(
    runtime: &ProviderRuntime,
    provider: &str,
    base_url: Option<&str>,
    secret: &str,
    scheme: CredentialScheme,
    user_agent: Option<&str>,
    endpoint: crate::backend::BackendEndpoint,
) -> Option<ProviderHandle> {
    build_provider_handle_with_endpoint(
        runtime,
        provider,
        base_url,
        secret,
        scheme,
        user_agent,
        Some(endpoint),
    )
}

fn build_provider_handle_with_endpoint(
    runtime: &ProviderRuntime,
    provider: &str,
    base_url: Option<&str>,
    secret: &str,
    scheme: CredentialScheme,
    user_agent: Option<&str>,
    endpoint: Option<crate::backend::BackendEndpoint>,
) -> Option<ProviderHandle> {
    let endpoint = crate::backend::resolve(provider, scheme, endpoint)?;
    // A per-endpoint UA rides the static header set; unset keeps the shared
    // client default (`sandhi/<version>`).
    let mut static_headers = HeaderMap::new();
    if let Some(ua) = user_agent.map(str::trim).filter(|ua| !ua.is_empty()) {
        match axum::http::HeaderValue::from_str(ua) {
            Ok(value) => {
                static_headers.insert(axum::http::header::USER_AGENT, value);
            }
            Err(_) => {
                tracing::warn!(%ua, "configured user_agent is not a valid header value; keeping the client default");
            }
        }
    }
    if endpoint.profile == crate::backend::BackendProfile::ChatGptCodex {
        // This is an expiring access-token lease, never an API key or refresh token.
        // The login owner refreshes its grant; the gateway owns dispatch and expiry.
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct SubscriptionLease {
            access_token: String,
            account_id: String,
            expires_at: u64,
        }
        const CODEX_BASE: &str = "https://chatgpt.com/backend-api/codex";
        if base_url.is_some_and(|base| base.trim_end_matches('/') != CODEX_BASE) {
            return None;
        }
        let lease: SubscriptionLease = serde_json::from_str(secret).ok()?;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()?
            .as_secs();
        if lease.access_token.trim().is_empty()
            || lease.account_id.trim().is_empty()
            || now.saturating_add(30) >= lease.expires_at
        {
            return None;
        }
        let mut headers = HeaderMap::new();
        headers.insert("chatgpt-account-id", lease.account_id.parse().ok()?);
        // Validate the bearer before constructing a transport. Never include it in errors.
        let _: axum::http::HeaderValue = format!("Bearer {}", lease.access_token).parse().ok()?;
        headers.insert("originator", "sandhi".parse().ok()?);
        return Some(
            runtime
                .chatgpt_responses(
                    provider,
                    CODEX_BASE,
                    lease.access_token,
                    headers,
                    Some(0),
                    Some(120.0),
                    None,
                    Some(90.0),
                )
                .with_credential_expiry(lease.expires_at),
        );
    }
    let family = endpoint.family;
    let base = base_url
        .map(str::to_string)
        .unwrap_or_else(|| default_base_url(provider, family));
    let handle = match family {
        ProviderFamily::Anthropic => {
            let auth = match scheme {
                CredentialScheme::Bearer => AnthropicAuthScheme::Bearer,
                _ => AnthropicAuthScheme::ApiKey,
            };
            Some(runtime.anthropic_with_usage_profile(
                base,
                secret,
                auth,
                static_headers,
                None,
                None,
                None,
                None,
                if endpoint.profile == crate::backend::BackendProfile::AnthropicImplicitCache {
                    sandhi_core::usage::AnthropicUsageProfile::ImplicitCache
                } else {
                    sandhi_core::usage::AnthropicUsageProfile::SeparateCacheWrites
                },
            ))
        }
        ProviderFamily::Gemini => Some(runtime.gemini(
            base,
            secret,
            match scheme {
                CredentialScheme::Bearer => GeminiAuthScheme::Bearer,
                _ => GeminiAuthScheme::ApiKey,
            },
            static_headers.clone(),
            None,
            None,
            None,
            None,
        )),
        ProviderFamily::Cohere => {
            Some(runtime.cohere(base, secret, static_headers, None, None, None, None))
        }
        ProviderFamily::Ollama => {
            Some(runtime.ollama(base, secret, static_headers.clone(), None, None, None, None))
        }
        ProviderFamily::OpenAiCompat => Some(runtime.openai_compat_with_stream_usage_profile(
            provider,
            base,
            secret,
            static_headers.clone(),
            None,
            None,
            None,
            None,
            endpoint.profile.openai_stream_usage_profile(),
        )),
        ProviderFamily::OpenAiResponses => Some(runtime.openai_responses(
            provider,
            base,
            secret,
            static_headers,
            None,
            None,
            None,
            None,
        )),
    }?;
    handle.with_provider_identity(provider).ok()
}

fn default_base_url(provider: &str, family: ProviderFamily) -> String {
    if family == ProviderFamily::OpenAiCompat {
        if let Some(spec) = sandhi_providers::resolve_openai_compat_provider(provider) {
            return spec.base_url.to_string();
        }
    }
    family.default_base_url().to_string()
}

pub(crate) fn parse_scheme(s: Option<&str>) -> Result<CredentialScheme, &'static str> {
    match s.map(str::to_ascii_lowercase).as_deref() {
        None | Some("api_key" | "api-key") => Ok(CredentialScheme::ApiKey),
        Some("bearer") => Ok(CredentialScheme::Bearer),
        Some("oauth") => Ok(CredentialScheme::Oauth),
        _ => Err("unknown credential scheme"),
    }
}

pub(crate) fn vault_entry_response(
    e: &VaultEntry,
    reference: Option<&VaultReferencePlan>,
) -> Result<Value, VaultError> {
    let acl = vault_acl_response(e, reference)?;
    Ok(json!({
        "provider": e.provider,
        "label": e.label,
        "scheme": e.scheme,
        "base_url": e.base_url,
        "created_at": e.created_at,
        "generation": e.generation,
        "status": e.status,
        "credential_id": e.credential_id(),
        "sentinelpass_entry_id": e.sentinelpass_entry_id,
        "sentinelpass_token_env": e.sentinelpass_token_env,
        "acl": acl,
        "vault": reference,
        // NB: the raw secret lives only in the Vault backend — never serialized here.
    }))
}

pub(crate) fn inventory_reference_plan(
    vault: &sandhi_store::VaultStore,
    entry: &VaultEntry,
) -> Result<VaultReferencePlan, VaultError> {
    match vault.reference_plan(&entry.provider, &entry.label) {
        Ok(plan) => Ok(plan),
        Err(VaultError::InvalidReference) => Ok(VaultReferencePlan {
            backend: vault.backend_name(),
            resolution: "invalid_reference",
            searched_references: vec![entry.credential_id()],
        }),
        Err(error) => Err(error),
    }
}

fn vault_acl_response(
    e: &VaultEntry,
    reference: Option<&VaultReferencePlan>,
) -> Result<Value, VaultError> {
    let declared_acl = e.grant_acl()?;
    let resolution = reference.map(|reference| reference.resolution);
    if resolution != Some("exact_entry") {
        return Ok(json!({
            "class": if resolution == Some("legacy_domain") {
                "legacy_domain"
            } else {
                "backend_native"
            },
            "exe_pinned": false,
            "fingerprint_enrolled": false,
            "expires_at": Value::Null,
            "source": "active_backend",
        }));
    }
    let Some(acl) = declared_acl else {
        return Ok(json!({
            "class": "unknown_service_grant",
            "exe_pinned": Value::Null,
            "fingerprint_enrolled": Value::Null,
            "expires_at": Value::Null,
            "source": "not_declared",
        }));
    };
    let class = match (acl.exe_pinned, acl.fingerprint_enrolled) {
        (true, true) => "exe_and_fingerprint",
        (true, false) => "exe_pinned",
        (false, true) => "fingerprint_enrolled",
        (false, false) => "token_only",
    };
    let now = time::OffsetDateTime::now_utc().unix_timestamp();
    Ok(json!({
        "class": class,
        "exe_pinned": acl.exe_pinned,
        "fingerprint_enrolled": acl.fingerprint_enrolled,
        "expires_at": acl.expires_at,
        "expiry_state": match acl.expires_at {
            None => "non_expiring",
            Some(expires_at) if expires_at <= now => "expired",
            Some(_) => "active",
        },
        "source": "registration_declaration",
    }))
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

/// Only canonical error text crosses the admin boundary; broker errors may contain secret material.
pub(crate) fn vault_failure(error: VaultError, write: bool) -> Response {
    let reconcile = write && matches!(error, VaultError::Timeout | VaultError::Backend(_));
    let (status, code, message) = match error {
        VaultError::Busy => (StatusCode::SERVICE_UNAVAILABLE, "vault_busy", "Credential operation busy; not admitted"),
        VaultError::Timeout => (StatusCode::GATEWAY_TIMEOUT, "vault_timeout", "Broker deadline exceeded"),
        VaultError::Locked => (StatusCode::LOCKED, "vault_locked", "Unlock the broker outside Sandhi and retry explicitly"),
        VaultError::Denied => (StatusCode::FORBIDDEN, "vault_denied", "Broker rejected operation; check the exact client/domain/field and read/write grant"),
        VaultError::Configuration => (StatusCode::SERVICE_UNAVAILABLE, "vault_configuration", "Check selected backend, IPC feature, daemon token and client token; no automatic fallback"),
        VaultError::InvalidReference => (StatusCode::BAD_REQUEST, "invalid_reference", "Provider and label require 1–128 lowercase ASCII letters, digits, dots, underscores or hyphens"),
        VaultError::StrictLegacyDenied => (StatusCode::CONFLICT, "vault_legacy_denied", "Strict SentinelPass profile refused the legacy domain grant; provision an exact-entry service grant"),
        VaultError::NotSupported(_) => (StatusCode::NOT_IMPLEMENTED, "vault_unsupported", "Operation unsupported by this backend; provision in the broker and use a supported read-only reference path"),
        VaultError::Backend(_) => (StatusCode::SERVICE_UNAVAILABLE, "vault_unavailable", "Credential operation failed; backend details are redacted"),
    };
    (
        status,
        Json(json!({ "error": message, "code": code,
        "reconcile_before_retry": reconcile })),
    )
        .into_response()
}

/// POST /admin/keys writes a secret; POST /admin/keys/reference only resolves an existing reference.
pub(crate) async fn add_key(
    State(state): State<Arc<ProxyState>>,
    headers: HeaderMap,
    Json(req): Json<admin::AddKeyRequest>,
) -> Response {
    if let Err(r) = require_access(&state, &headers, crate::auth::Permission::Admin, true).await {
        return r;
    }
    register_credential(
        state,
        admin::RegisterReferenceRequest {
            provider: req.provider,
            label: req.label,
            scheme: req.scheme,
            base_url: req.base_url,
            user_agent: req.user_agent,
            sentinelpass_entry_id: None,
            sentinelpass_token_env: None,
            sentinelpass_grant_acl: None,
        },
        Some(req.secret),
    )
    .await
}

pub(crate) async fn register_reference(
    State(state): State<Arc<ProxyState>>,
    headers: HeaderMap,
    Json(req): Json<admin::RegisterReferenceRequest>,
) -> Response {
    if let Err(r) = require_access(&state, &headers, crate::auth::Permission::Admin, true).await {
        return r;
    }
    register_credential(state, req, None).await
}

async fn register_credential(
    state: Arc<ProxyState>,
    req: admin::RegisterReferenceRequest,
    supplied: Option<String>,
) -> Response {
    let operation = match admit_mutation(&state.lifecycle) {
        Ok(operation) => operation,
        Err(response) => return response,
    };
    let Some(vault) = state.vault.clone() else {
        return err(
            StatusCode::SERVICE_UNAVAILABLE,
            "vault not configured (set SANDHI_STORE)",
        );
    };
    let label = req.label.as_deref().unwrap_or("default");
    if let Err(e) = sandhi_store::vault::validate_reference(&req.provider, label) {
        return vault_failure(e, false);
    }
    let scheme = match parse_scheme(req.scheme.as_deref()) {
        Ok(scheme) => scheme,
        Err(message) => return err(StatusCode::BAD_REQUEST, message),
    };
    if vault.backend_name() == "unavailable" {
        return vault_failure(VaultError::Configuration, false);
    }
    if supplied.is_some() && !vault.capabilities().write {
        return vault_failure(
            VaultError::NotSupported("write capability absent".into()),
            false,
        );
    }
    // The legacy CLI is explicitly unbounded; do not admit interactive reference operations on it.
    if supplied.is_none() && vault.backend_name() == "sentinelpass" {
        return vault_failure(
            VaultError::NotSupported("bounded native IPC required".into()),
            false,
        );
    }
    let Ok(permit) = state.vault_writer.clone().try_acquire_owned() else {
        return vault_failure(VaultError::Busy, false);
    };
    let write = supplied.is_some();
    // The permit and publication live inside the blocking task: cancellation cannot admit a
    // conflicting writer while a detached operation is still committing.
    tokio::task::spawn_blocking(move || {
        let _operation = operation;
        let _permit = permit;
        let label = req.label.as_deref().unwrap_or("default");
        let exact = match (
            req.sentinelpass_entry_id,
            req.sentinelpass_token_env.as_deref(),
        ) {
            (Some(entry_id), Some(token_env)) => {
                match SentinelPassExactBinding::new(entry_id, token_env) {
                    Ok(binding) => Some(binding),
                    Err(_) => return err(
                        StatusCode::BAD_REQUEST,
                        "sentinelpass exact-entry binding requires a positive entry id and valid token environment name",
                    ),
                }
            }
            (None, None) => None,
            (Some(_), None) | (None, Some(_)) => {
                return err(
                    StatusCode::BAD_REQUEST,
                    "sentinelpass_entry_id and sentinelpass_token_env must be supplied together",
                );
            }
        };
        let grant_acl = req.sentinelpass_grant_acl.map(|acl| SentinelPassGrantAcl {
            exe_pinned: acl.exe_pinned,
            fingerprint_enrolled: acl.fingerprint_enrolled,
            expires_at: acl.expires_at,
        });
        if exact.is_none() && grant_acl.is_some() {
            return err(
                StatusCode::BAD_REQUEST,
                "sentinelpass_grant_acl requires an exact-entry binding",
            );
        }
        let reference_plan = if let Some(binding) = &exact {
            match vault.reference_plan_for_exact(&req.provider, label, binding) {
                Ok(plan) => plan,
                Err(e) => return vault_failure(e, false),
            }
        } else {
            match vault.reference_plan(&req.provider, label) {
                Ok(plan) => plan,
                Err(e) => return vault_failure(e, false),
            }
        };
        let secret = match supplied {
            Some(secret) => secret,
            None => match if let Some(binding) = &exact {
                vault.get_exact(&req.provider, label, binding)
            } else {
                vault.get(&req.provider, label)
            } {
                Ok(Some(secret)) => secret,
                Ok(None) => return (StatusCode::NOT_FOUND, Json(json!({
                    "error": "No credential at the authorized reference", "code": "vault_missing",
                    "vault": reference_plan
                }))).into_response(),
                Err(e) => return vault_failure(e, false),
            },
        };
        let handle = build_provider_handle(
            &state.runtime,
            &req.provider,
            req.base_url.as_deref(),
            &secret,
            scheme,
            req.user_agent.as_deref(),
        );
        if handle.is_none() {
            return err(
                StatusCode::BAD_REQUEST,
                "invalid or expired provider credential configuration",
            );
        }
        let committed = if write {
            vault.set(
                &req.provider,
                label,
                scheme,
                req.base_url.as_deref(),
                req.user_agent.as_deref(),
                &secret,
            )
        } else {
            vault.register_metadata(
                &req.provider,
                label,
                scheme,
                req.base_url.as_deref(),
                req.user_agent.as_deref(),
                exact,
                grant_acl,
            )
        };
        let cred_id = match committed {
            Ok(id) => id,
            Err(e) => return vault_failure(e, write),
        };
        let entry = match vault.metadata_entry(&req.provider, label) {
            Ok(Some(entry)) => entry,
            result => {
                // The mutation already committed. Disable runtime dependents if
                // the canonical generation cannot be read; never publish a guessed one.
                let _ = crate::backend::publish_credential(&state, &cred_id, None, None, None);
                return match result {
                    Err(error) => vault_failure(error, write),
                    _ => err(
                        StatusCode::SERVICE_UNAVAILABLE,
                        "credential committed; metadata unavailable; reconcile before retry",
                    ),
                };
            }
        };
        if crate::backend::publish_credential(&state, &cred_id, handle, Some(&entry), Some(&secret))
            .is_err()
        {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(
                    json!({"error":"credential committed; dependent backend refresh failed",
                "metadata_committed":true,"credential_id":cred_id,"reconcile_before_retry":true}),
                ),
            )
                .into_response();
        }
        // Failure to read after commit must not turn a persisted mutation into a success-shaped
        // empty inventory. Preserve its identity so an operator can reconcile without rewriting.
        match vault.list() {
            Ok(entries) => {
                let entry = entries.iter().find(|e| e.credential_id() == cred_id);
                let body = match entry {
                    Some(entry) => match vault_entry_response(entry, Some(&reference_plan)) {
                        Ok(body) => body,
                        Err(error) => return vault_failure(error, false),
                    },
                    None => json!({ "credential_id": cred_id }),
                };
                (
                    StatusCode::CREATED,
                    Json(body),
                )
                    .into_response()
            }
            Err(_) => (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({
                    "error": "Credential committed; metadata refresh failed",
                    "code": "credential_committed_refresh_failed", "credential_id": cred_id,
                    "metadata_committed": true, "reconcile_before_retry": true
                })),
            )
                .into_response(),
        }
    })
    .await
    .unwrap_or_else(|_| vault_failure(VaultError::Backend("worker failed".into()), write))
}

/// GET /admin/keys reads only masked metadata.
pub(crate) async fn list_keys(
    State(state): State<Arc<ProxyState>>,
    headers: HeaderMap,
) -> Response {
    if let Err(r) = require_access(&state, &headers, crate::auth::Permission::Read, false).await {
        return r;
    }
    let Some(vault) = state.vault.clone() else {
        return err(StatusCode::SERVICE_UNAVAILABLE, "vault not configured");
    };
    match tokio::task::spawn_blocking(move || {
        let entries = vault.list()?;
        let backend = vault.backend_name();
        let keys = entries
            .iter()
            .map(|entry| {
                let reference = inventory_reference_plan(&vault, entry)?;
                vault_entry_response(entry, Some(&reference))
            })
            .collect::<Result<Vec<_>, VaultError>>()?;
        Ok::<Json<Value>, VaultError>(Json(json!({ "backend": backend, "keys": keys })))
    })
    .await
    {
        Ok(Ok(response)) => response.into_response(),
        _ => vault_failure(VaultError::Backend("inventory unavailable".into()), false),
    }
}

/// DELETE revokes local dispatch metadata. It never claims broker-grant or provider-key revocation.
pub(crate) async fn revoke_key(
    State(state): State<Arc<ProxyState>>,
    headers: HeaderMap,
    Path((provider, label)): Path<(String, String)>,
) -> Response {
    if let Err(r) = require_access(&state, &headers, crate::auth::Permission::Admin, true).await {
        return r;
    }
    let operation = match admit_mutation(&state.lifecycle) {
        Ok(operation) => operation,
        Err(response) => return response,
    };
    let Some(vault) = state.vault.clone() else {
        return err(StatusCode::SERVICE_UNAVAILABLE, "vault not configured");
    };
    let Ok(permit) = state.vault_writer.clone().try_acquire_owned() else {
        return vault_failure(VaultError::Busy, false);
    };
    tokio::task::spawn_blocking(move || {
        let _operation = operation;
        let _permit = permit;
        match vault.revoke_with_status(&provider, &label) {
            Ok((revoked, deletion)) => {
                let _ = crate::backend::publish_credential(
                    &state,
                    &format!("{provider}:{label}"),
                    None,
                    None,
                    None,
                );
                Json(json!({ "revoked": revoked, "secret_deletion": deletion,
                    "broker_grant_revoked": false, "provider_key_revoked": false }))
                .into_response()
            }
            Err(e) => vault_failure(e, true),
        }
    })
    .await
    .unwrap_or_else(|_| vault_failure(VaultError::Backend("worker failed".into()), true))
}

/// `POST /admin/keys/share` — mint a scoped virtual key (secret printed once).
pub(crate) async fn share_key(
    State(state): State<Arc<ProxyState>>,
    headers: HeaderMap,
    Json(req): Json<admin::ShareKeyRequest>,
) -> Response {
    if let Err(r) = require_access(&state, &headers, crate::auth::Permission::Admin, true).await {
        return r;
    }
    let _operation = match admit_mutation(&state.lifecycle) {
        Ok(operation) => operation,
        Err(response) => return response,
    };
    // Require a configured durable vkey store.
    let Some(vkeys) = state.vkeys.clone() else {
        return err(
            StatusCode::SERVICE_UNAVAILABLE,
            "virtual-key store not configured (set SANDHI_STORE)",
        );
    };
    // ADR-0015: `upstream:` is the gateway's own back-leg scope prefix. A virtual key pointing
    // its budget scope there would hold TWO leases on the same scope per call and settle the
    // same actual into it twice — exhausting the cap at half its configured limit. Rejected at
    // minting; the declarative vkeys section never carries a budget scope at all.
    if let Some(scope) = req
        .budget_scope
        .as_deref()
        .map(str::trim)
        .filter(|s| s.starts_with("upstream:"))
    {
        return err(
            StatusCode::BAD_REQUEST,
            &format!(
                "budget_scope '{scope}' uses the reserved 'upstream:' prefix: back-leg per-provider caps are gateway-managed, not per-key (ADR-0015)"
            ),
        );
    }
    // The upstream credential must resolve (provider handle registered or vault entry active).
    let upstream = req.upstream.clone();
    let registered = state
        .providers
        .lock()
        .expect("providers poisoned")
        .contains_key(&upstream);
    let logical = crate::routing::active(&state, &upstream).await;
    if !registered
        && !logical
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
        model_budget_scope: None,
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
        model_budget_scope: minted.record.model_budget_scope.clone(),
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
    if let Err(r) = require_access(&state, &headers, crate::auth::Permission::Read, false).await {
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
    if let Err(r) = require_access(&state, &headers, crate::auth::Permission::Admin, true).await {
        return r;
    }
    let _operation = match admit_mutation(&state.lifecycle) {
        Ok(operation) => operation,
        Err(response) => return response,
    };
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

/// `POST /admin/profile-tokens` — mint a profile-download token (`skp_…`) for
/// `(subject, group)`. The plaintext token is shown **once** in the response; the store keeps
/// only its SHA-256 hash (same scheme as virtual keys), so it can never be recovered later.
/// A holder of this token can download the group's assembled profile and — in gateway mode —
/// receive a fresh virtual key, so minting is an Admin mutation like key sharing.
pub(crate) async fn mint_profile_token(
    State(state): State<Arc<ProxyState>>,
    headers: HeaderMap,
    Json(req): Json<admin::ProfileTokenRequest>,
) -> Response {
    if let Err(r) = require_access(&state, &headers, crate::auth::Permission::Admin, true).await {
        return r;
    }
    let _operation = match admit_mutation(&state.lifecycle) {
        Ok(operation) => operation,
        Err(response) => return response,
    };
    let Some(store) = state.profiles.clone() else {
        return err(
            StatusCode::SERVICE_UNAVAILABLE,
            "profile store not configured (set SANDHI_STORE)",
        );
    };
    if req.subject.trim().is_empty() || req.group.trim().is_empty() {
        return err(
            StatusCode::BAD_REQUEST,
            "profile tokens require a nonempty subject and group",
        );
    }
    let token = match store.mint_profile_token(req.subject.trim(), req.group.trim()) {
        Ok(t) => t,
        Err(e) => {
            return err(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("profile-token mint failed: {e}"),
            )
        }
    };
    Json(json!({
        "token": token,   // shown exactly once
        "subject": req.subject,
        "group": req.group,
        "auth": "Authorization: Bearer <profile_token> on GET /auth/profile",
    }))
    .into_response()
}

/// `POST /admin/profile-tokens/revoke` — revoke every active token for `(subject, group)`
/// (offboarding / rotation). Revoked tokens stop resolving on `GET /auth/profile` immediately.
pub(crate) async fn revoke_profile_tokens(
    State(state): State<Arc<ProxyState>>,
    headers: HeaderMap,
    Json(req): Json<admin::RevokeProfileTokensRequest>,
) -> Response {
    if let Err(r) = require_access(&state, &headers, crate::auth::Permission::Admin, true).await {
        return r;
    }
    let _operation = match admit_mutation(&state.lifecycle) {
        Ok(operation) => operation,
        Err(response) => return response,
    };
    let Some(store) = state.profiles.clone() else {
        return err(
            StatusCode::SERVICE_UNAVAILABLE,
            "profile store not configured (set SANDHI_STORE)",
        );
    };
    let revoked = match store.revoke_profile_tokens(&req.subject, &req.group) {
        Ok(n) => n,
        Err(e) => {
            return err(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("profile-token revoke failed: {e}"),
            )
        }
    };
    // Lifecycle note: revocation stops FUTURE profile downloads only — virtual keys already
    // minted by those downloads stay active and must be revoked via the /admin/vkeys surface
    // (profiles.rs module docs carry the full two-step offboarding contract).
    Json(json!({
        "revoked": revoked,
        "subject": req.subject,
        "group": req.group,
        "note": "revocation stops future profile downloads; already-minted virtual keys must be revoked separately via /admin/vkeys",
    }))
    .into_response()
}

/// `POST /admin/budget` — set a neutral-token budget on a scope (P2: window + policy + alerts).
pub(crate) async fn set_budget(
    State(state): State<Arc<ProxyState>>,
    headers: HeaderMap,
    Json(req): Json<admin::SetBudgetRequest>,
) -> Response {
    set_budget_impl(State(state), headers, Json(req), true).await
}

/// Direct operator requests replace the scope's complete hierarchy. Declarative config apply uses
/// the same implementation with additive semantics so omission cannot revoke a live budget.
async fn set_budget_impl(
    State(state): State<Arc<ProxyState>>,
    headers: HeaderMap,
    Json(req): Json<admin::SetBudgetRequest>,
    replace: bool,
) -> Response {
    if let Err(r) = require_access(&state, &headers, crate::auth::Permission::Operate, true).await {
        return r;
    }
    let _operation = match admit_mutation(&state.lifecycle) {
        Ok(operation) => operation,
        Err(response) => return response,
    };
    if let Err(message) = validate_budget(&req) {
        return err(StatusCode::BAD_REQUEST, message);
    }
    if req.alert_thresholds.as_ref().is_some_and(|v| !v.is_empty()) && state.alert_store.is_none() {
        return err(
            StatusCode::SERVICE_UNAVAILABLE,
            "alert store not configured; budget unchanged",
        );
    }
    let windows =
        requested_budget_windows(&req).expect("set_budget validates its request before commit");
    if let Err(error) =
        apply_budget_hierarchy(&state.ledger, &state.budgets, &req.scope, &windows, replace)
    {
        tracing::error!(%error, "budget hierarchy commit failed");
        return err(
            StatusCode::SERVICE_UNAVAILABLE,
            "budget commit failed; budget unchanged",
        );
    }

    // P2: optional threshold percentages create alert rules for this scope.
    let mut created_alerts: Vec<Value> = Vec::new();
    let mut failed_alerts: Vec<Value> = Vec::new();
    if let Some(thresholds) = &req.alert_thresholds {
        for &pct in thresholds {
            match create_alert_for_scope(&state, &req.scope, pct, AlertChannel::Log) {
                Ok(alert) => created_alerts.push(alert_rule_response(&alert)),
                Err(error) => {
                    tracing::error!(%error, "inline alert commit failed");
                    failed_alerts
                        .push(json!({"threshold_pct": pct, "error": "alert commit failed"}));
                }
            }
        }
    }

    let complete = failed_alerts.is_empty();
    let mut payload = json!({
        "ok": complete,
        "budget_applied": true,
        "scope": req.scope.clone(),
        "limits": req.limits.clone(),
        "alerts_created": created_alerts,
        "alerts_failed": failed_alerts,
    });
    if !complete {
        payload["error"] = json!(
            "budget committed, but some alerts failed; inspect partial results before retrying"
        );
    }
    (
        if complete {
            StatusCode::OK
        } else {
            StatusCode::SERVICE_UNAVAILABLE
        },
        Json(payload),
    )
        .into_response()
}

/// `GET /admin/budget` — list all configured budget scopes.
pub(crate) async fn list_budgets(
    State(state): State<Arc<ProxyState>>,
    headers: HeaderMap,
) -> Response {
    if let Err(r) = require_access(&state, &headers, crate::auth::Permission::Read, false).await {
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
    if let Err(r) = require_access(&state, &headers, crate::auth::Permission::Read, false).await {
        return r;
    }
    let Some(scope) = params.get("scope") else {
        return err(StatusCode::BAD_REQUEST, "missing ?scope=");
    };
    let spec = state
        .budgets
        .lock()
        .expect("budgets poisoned")
        .get(scope)
        .cloned();
    let windows = spec
        .as_ref()
        .map(|spec| {
            spec.windows
                .iter()
                .map(|window| {
                    let spent = state
                        .ledger
                        .lock()
                        .expect("ledger poisoned")
                        .try_spent_for_window(scope, Window::parse(&window.window))
                        .unwrap_or(0);
                    json!({
                        "limit_tokens": window.limit_tokens,
                        "spent": spent,
                        "remaining": window.limit_tokens.saturating_sub(spent),
                        "window": window.window,
                        "policy": window.policy,
                    })
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    Json(json!({
        "scope": scope,
        "windows": windows,
    }))
    .into_response()
}

/// `GET /admin/usage?by=…&since=…` — attribution aggregates (wraps the durable store).
/// A parsed, validated, bounded usage query window (ADR dashboard windowing).
/// Bounds are canonical UTC RFC-3339 strings, `from` inclusive and `to`
/// exclusive — matching the lexicographic comparison over the `occurred_at`
/// index. Every windowed query is date-bounded; the hard maximum exists so a
/// dashboard request can never become an unbounded scan.
#[derive(Debug)]
pub(crate) struct UsageWindow {
    pub from: String,
    pub to: String,
    pub source: &'static str,
    pub advisory: Option<String>,
}

/// Default lookback when no explicit window is supplied.
const USAGE_WINDOW_DEFAULT_DAYS: i64 = 30;
/// Hard maximum: beyond one year the dashboard advises direct database queries
/// instead of widening an HTTP aggregate.
const USAGE_WINDOW_MAX_DAYS: i64 = 366;

pub(crate) fn parse_usage_window(
    from: Option<&str>,
    to: Option<&str>,
    now: time::OffsetDateTime,
) -> Result<UsageWindow, (StatusCode, String)> {
    use time::format_description::well_known::Rfc3339;
    use time::Duration;
    let parse_bound = |raw: &str, is_from: bool| -> Result<time::OffsetDateTime, String> {
        if let Ok(ts) = time::OffsetDateTime::parse(raw, &Rfc3339) {
            return Ok(ts);
        }
        // Date-only (YYYY-MM-DD), interpreted as UTC midnight. A `to` date
        // advances one day so the exclusive bound covers that whole day.
        let parts = raw.split('-').collect::<Vec<_>>();
        let day = (|| {
            if parts.len() != 3 {
                return Err(());
            }
            let year = parts[0].parse::<i32>().map_err(|_| ())?;
            let month =
                time::Month::try_from(parts[1].parse::<u8>().map_err(|_| ())?).map_err(|_| ())?;
            let day = parts[2].parse::<u8>().map_err(|_| ())?;
            time::Date::from_calendar_date(year, month, day).map_err(|_| ())
        })()
        .map_err(|_| format!("window bound {raw:?} must be RFC-3339 or a YYYY-MM-DD date"))?;
        // next_day() (Option) instead of +1 day: 9999-12-31 has no successor
        // and must 400, not panic the handler.
        let midnight = day.with_time(time::Time::MIDNIGHT).assume_utc();
        Ok(if is_from {
            midnight
        } else {
            day.next_day()
                .ok_or_else(|| format!("window bound {raw:?} exceeds the supported date range"))?
                .with_time(time::Time::MIDNIGHT)
                .assume_utc()
        })
    };
    let to = match to {
        Some(raw) => parse_bound(raw, false).map_err(|m| (StatusCode::BAD_REQUEST, m))?,
        None => now,
    };
    let (mut from, source, mut advisory) = match from {
        Some(raw) => (
            parse_bound(raw, true).map_err(|m| (StatusCode::BAD_REQUEST, m))?,
            "explicit",
            None,
        ),
        None => (
            to - Duration::days(USAGE_WINDOW_DEFAULT_DAYS),
            "default",
            None,
        ),
    };
    if from >= to {
        return Err((
            StatusCode::BAD_REQUEST,
            "usage window 'from' must precede 'to'".into(),
        ));
    }
    if to - from > Duration::days(USAGE_WINDOW_MAX_DAYS) {
        from = to - Duration::days(USAGE_WINDOW_MAX_DAYS);
        advisory = Some(format!(
            "requested range exceeded the {USAGE_WINDOW_MAX_DAYS}-day maximum; \
             showing the most recent {USAGE_WINDOW_MAX_DAYS} days of the requested \
             window — use direct database queries for older history"
        ));
    }
    // Canonical UTC: bounds compare lexicographically against stored
    // occurred_at values, which are always written in UTC "…Z" form. An
    // offset-bearing input would otherwise exclude in-window rows silently.
    let format_bound = |value: time::OffsetDateTime| -> Result<String, (StatusCode, String)> {
        value.to_utc().format(&Rfc3339).map_err(|error| {
            (
                StatusCode::BAD_REQUEST,
                format!("window bound out of representable range: {error}"),
            )
        })
    };
    Ok(UsageWindow {
        from: format_bound(from)?,
        to: format_bound(to)?,
        source,
        advisory,
    })
}

pub(crate) async fn usage(
    State(state): State<Arc<ProxyState>>,
    headers: HeaderMap,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    if let Err(r) = require_access(&state, &headers, crate::auth::Permission::Read, false).await {
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
    // `from`/`to` bound the window (default: last 30 days, hard max one year).
    // The legacy `since` parameter keeps its exact contract — a pure lower
    // bound with no upper cap and an unbounded total — so existing callers
    // never change semantics, even for future-dated bounds.
    enum Bounds<'a> {
        LegacySince(&'a str),
        Window(UsageWindow),
    }
    let bounds = if params.contains_key("from") || params.contains_key("to") {
        match parse_usage_window(
            params.get("from").map(String::as_str),
            params.get("to").map(String::as_str),
            time::OffsetDateTime::now_utc(),
        ) {
            Ok(window) => Bounds::Window(window),
            Err((status, message)) => return err(status, &message),
        }
    } else {
        match since.as_deref() {
            Some(lower) => Bounds::LegacySince(lower),
            None => match parse_usage_window(None, None, time::OffsetDateTime::now_utc()) {
                Ok(window) => Bounds::Window(window),
                Err((status, message)) => return err(status, &message),
            },
        }
    };
    match bounds {
        Bounds::LegacySince(lower) => {
            let total = store.grand_total().ok();
            let buckets = dimension_buckets_between(&store, by, Some(lower), None);
            Json(json!({
                "dimension": by,
                "since": since,
                "total": total,
                "buckets": buckets.unwrap_or_default(),
            }))
            .into_response()
        }
        Bounds::Window(window) => {
            let total = store.grand_total_between(&window.from, &window.to).ok();
            let buckets =
                dimension_buckets_between(&store, by, Some(&window.from), Some(&window.to));
            Json(json!({
                "dimension": by,
                "since": since,
                "window": {"from": window.from, "to": window.to, "source": window.source},
                "advisory": window.advisory,
                "total": total,
                "buckets": buckets.unwrap_or_default(),
            }))
            .into_response()
        }
    }
}

/// Run admitted work off the async executor. The blocking closure owns admission, so dropping
/// its HTTP waiter cannot release the slot while SQLite still runs (ADR-0011).
async fn admitted_diagnostic<T: Send + 'static>(
    permit: tokio::sync::OwnedSemaphorePermit,
    operation: crate::lifecycle::OperationGuard,
    work: impl FnOnce() -> T + Send + 'static,
) -> Result<T, tokio::task::JoinError> {
    tokio::task::spawn_blocking(move || {
        let _operation = operation;
        let _permit = permit;
        work()
    })
    .await
}

/// Read-only POST keeps correlation IDs out of URL/access-log query strings. Only persisted,
/// bounded fields are returned: no raw body capture or inferred historical measurements.
pub(crate) async fn usage_diagnostics(
    State(state): State<Arc<ProxyState>>,
    request: axum::extract::Request,
) -> Response {
    use sandhi_store::diagnostics::{DiagnosticQuery, MAX_REQUEST_BYTES};
    if let Err(response) = require_access(
        &state,
        request.headers(),
        crate::auth::Permission::Diagnostics,
        true,
    )
    .await
    {
        return response;
    }
    let Some(store) = state.store.clone() else {
        return err(
            StatusCode::SERVICE_UNAVAILABLE,
            "usage store not configured",
        );
    };
    let Some(operation) = state.lifecycle.try_operation() else {
        return err(
            StatusCode::SERVICE_UNAVAILABLE,
            "gateway draining; diagnostic not admitted",
        );
    };
    let permit = match state.diagnostics_reader.clone().try_acquire_owned() {
        Ok(permit) => permit,
        Err(_) => {
            return err(
                StatusCode::SERVICE_UNAVAILABLE,
                "diagnostic operation already in progress",
            )
        }
    };
    // Include decoding in admission and bound body-read time as well as size. This timeout
    // does not apply to the later SQL work, whose permit remains inside the blocking task.
    let body = match tokio::time::timeout(
        Duration::from_secs(5),
        axum::body::to_bytes(request.into_body(), MAX_REQUEST_BYTES),
    )
    .await
    {
        Ok(Ok(body)) => body,
        Ok(Err(_)) => {
            return err(
                StatusCode::PAYLOAD_TOO_LARGE,
                "diagnostic request unreadable or too large",
            )
        }
        Err(_) => {
            return err(
                StatusCode::REQUEST_TIMEOUT,
                "diagnostic request body timed out",
            )
        }
    };
    let query: DiagnosticQuery = match serde_json::from_slice(&body) {
        Ok(query) => query,
        Err(_) => return err(StatusCode::BAD_REQUEST, "invalid diagnostic query"),
    };
    if query.validate().is_err() {
        return err(StatusCode::BAD_REQUEST, "invalid diagnostic query");
    }
    match admitted_diagnostic(permit, operation, move || store.diagnostics(&query)).await {
        Ok(Ok(response)) => Json(response).into_response(),
        // Never include SQL, selector values, paths, or panic details in the response.
        _ => err(StatusCode::INTERNAL_SERVER_ERROR, "diagnostic query failed"),
    }
}

/// `GET /admin/usage/run/{run_id}` — the ADR-0005 D7 agent cost tree for one run: per-step
/// spend assembled by `parent_id`, with subtree-inclusive rollups. 404 when no usage event
/// carries the run id (including events recorded before the identity columns existed — that
/// data was never persisted and cannot be backfilled).
pub(crate) async fn usage_run(
    State(state): State<Arc<ProxyState>>,
    headers: HeaderMap,
    Path(run_id): Path<String>,
) -> Response {
    if let Err(r) = require_access(&state, &headers, crate::auth::Permission::Read, false).await {
        return r;
    }
    let Some(store) = state.store.clone() else {
        return err(
            StatusCode::SERVICE_UNAVAILABLE,
            "usage store not configured (set SANDHI_STORE)",
        );
    };
    match store.run_cost_tree(&run_id) {
        Ok(Some(tree)) => Json(json!({ "run": tree })).into_response(),
        Ok(None) => err(
            StatusCode::NOT_FOUND,
            "unknown run id (no usage event carries it)",
        ),
        Err(e) => err(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("run query failed: {e}"),
        ),
    }
}

fn dimension_buckets_between(
    store: &Arc<sandhi_store::SqliteStore>,
    by: &str,
    from: Option<&str>,
    to: Option<&str>,
) -> Option<Vec<Value>> {
    let validate = |dim: &str| -> bool {
        matches!(
            dim,
            "subject"
                | "user"
                | "group"
                | "provider"
                | "model"
                | "key"
                | "virtual_key"
                | "session"
                | "run"
        )
    };
    if !validate(by) {
        return None;
    }
    let col = match by {
        "subject" | "user" => "subject_id",
        "group" => "group_id",
        "provider" => "provider",
        "model" => "model",
        "key" | "virtual_key" => "virtual_key_id",
        "session" => "session_id",
        "run" => "run_id",
        _ => return None,
    };
    let buckets: Vec<sandhi_store::Bucket> = if from.is_some() || to.is_some() {
        store.totals_grouped_between(col, from, to).ok()?
    } else {
        match by {
            "subject" | "user" => store.totals_by_subject().ok()?,
            "group" => store.totals_by_group().ok()?,
            "provider" => store.totals_by_provider().ok()?,
            "model" => store.totals_by_model().ok()?,
            "key" | "virtual_key" => store.totals_by_virtual_key().ok()?,
            "session" => store.totals_by_session().ok()?,
            "run" => store.totals_by_run().ok()?,
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

fn validate_budget(req: &admin::SetBudgetRequest) -> Result<(), &'static str> {
    if req.scope.trim().is_empty() {
        return Err("budget scope must not be empty");
    }
    if req
        .limits
        .values()
        .any(|limit| limit.limit_tokens > i64::MAX as u64)
    {
        return Err("budget limit exceeds the supported signed 64-bit range");
    }
    if req.limits.is_empty() {
        return Err("limits must contain at least one window");
    }
    if req.limits.keys().any(|key| {
        !matches!(
            key.as_str(),
            "hourly" | "daily" | "weekly" | "monthly" | "total"
        )
    }) {
        return Err("limits keys must be hourly, daily, weekly, monthly or total");
    }
    if req
        .limits
        .values()
        .any(|limit| !matches!(limit.policy.as_str(), "block" | "warn"))
    {
        return Err("each budget policy must be block or warn");
    }
    if req.alert_thresholds.iter().flatten().any(|pct| *pct > 100) {
        return Err("alert thresholds must be between 0 and 100");
    }
    Ok(())
}

fn requested_budget_windows(
    req: &admin::SetBudgetRequest,
) -> Result<Vec<BudgetWindowSpec>, &'static str> {
    validate_budget(req)?;
    Ok(req
        .limits
        .iter()
        .map(|(window, limit_tokens)| BudgetWindowSpec {
            limit_tokens: limit_tokens.limit_tokens,
            window: window.clone(),
            policy: limit_tokens.policy.clone(),
        })
        .collect())
}

fn apply_budget_hierarchy(
    ledger: &Mutex<ProxyLedger>,
    budgets: &Mutex<HashMap<String, BudgetSpec>>,
    scope: &str,
    requested: &[BudgetWindowSpec],
    replace: bool,
) -> Result<(), String> {
    // Carry every cap + window + policy into the live lease ledger atomically (ADR-0046). A
    // `Warn` window stays soft; a `Block` window hard-enforces.
    // Hold both locks through commit and publication: concurrent writers cannot publish
    // metadata in the opposite order from durable commits. Match dashboard lock ordering.
    let mut ledger = ledger.lock().map_err(|e| e.to_string())?;
    let mut budgets = budgets.lock().map_err(|e| e.to_string())?;
    let durable_limits = requested
        .iter()
        .map(|spec| {
            (
                Some(spec.limit_tokens),
                Window::parse(&spec.window),
                Policy::parse(&spec.policy),
            )
        })
        .collect::<Vec<_>>();
    ledger.set_budget_hierarchy(scope, &durable_limits, replace)?;
    let spec = budgets.get_mut(scope);
    if let Some(spec) = spec {
        if replace {
            spec.windows.clear();
        }
        for window_spec in requested {
            if let Some(existing) = spec
                .windows
                .iter_mut()
                .find(|existing| existing.window == window_spec.window)
            {
                *existing = window_spec.clone();
            } else {
                spec.windows.push(window_spec.clone());
            }
        }
    } else {
        budgets.insert(
            scope.to_owned(),
            BudgetSpec {
                scope: scope.to_owned(),
                windows: requested.to_vec(),
            },
        );
    }
    budgets
        .get_mut(scope)
        .expect("budget spec was just inserted or updated")
        .windows
        .sort_by_key(|spec| {
            sandhi_core::Window::parse(&spec.window)
                .duration()
                .map_or(i64::MAX, |d| d.whole_seconds())
        });
    Ok(())
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
) -> Result<AlertRuleRecord, String> {
    let store = state
        .alert_store
        .as_ref()
        .ok_or("alert store not configured")?;
    let mut registry = state
        .alerts
        .as_ref()
        .map(|r| r.lock())
        .transpose()
        .map_err(|e| e.to_string())?;
    let record = store
        .create(CreateAlertRequest {
            scope: scope.into(),
            threshold_pct,
            channel: channel.clone(),
        })
        .map_err(|e| e.to_string())?;
    if let Some(registry) = registry.as_mut() {
        registry.add_rule(record.to_rule());
    }
    Ok(record)
}

// --- Alerts (P2) ------------------------------------------------------------

/// `GET /admin/alerts?scope=…` — list alert rules (optionally filtered by scope).
pub(crate) async fn list_alerts(
    State(state): State<Arc<ProxyState>>,
    headers: HeaderMap,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    if let Err(r) = require_access(&state, &headers, crate::auth::Permission::Read, false).await {
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
    if let Err(r) = require_access(&state, &headers, crate::auth::Permission::Operate, true).await {
        return r;
    }
    let _operation = match admit_mutation(&state.lifecycle) {
        Ok(operation) => operation,
        Err(response) => return response,
    };
    if req.scope.trim().is_empty() || req.threshold_pct > 100 {
        return err(
            StatusCode::BAD_REQUEST,
            "alerts require a nonempty scope and threshold between 0 and 100",
        );
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
        Ok(rec) => (StatusCode::CREATED, Json(alert_rule_response(&rec))).into_response(),
        Err(error) => {
            tracing::error!(%error, "alert commit failed");
            err(
                StatusCode::SERVICE_UNAVAILABLE,
                "alert commit failed or store not configured",
            )
        }
    }
}

/// `POST /admin/alerts/{id}/ack` — acknowledge a fired alert.
pub(crate) async fn ack_alert(
    State(state): State<Arc<ProxyState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Err(r) = require_access(&state, &headers, crate::auth::Permission::Operate, true).await {
        return r;
    }
    let _operation = match admit_mutation(&state.lifecycle) {
        Ok(operation) => operation,
        Err(response) => return response,
    };
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
    if let Err(r) = require_access(&state, &headers, crate::auth::Permission::Operate, true).await {
        return r;
    }
    let _operation = match admit_mutation(&state.lifecycle) {
        Ok(operation) => operation,
        Err(response) => return response,
    };
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
            // Never follow redirects: the alert payload would be re-POSTed to
            // whatever host the 30x Location names (best-effort delivery means
            // a redirect failure is dropped, not retried elsewhere).
            .redirect(reqwest::redirect::Policy::none())
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
            model_budget_scope: rec.model_budget_scope.clone(),
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
    map.clear();
    let mut grouped: std::collections::BTreeMap<String, Vec<BudgetWindowSpec>> =
        std::collections::BTreeMap::new();
    for row in ledger.budgets() {
        grouped
            .entry(row.scope.clone())
            .or_default()
            .push(BudgetWindowSpec {
                limit_tokens: row.limit.unwrap_or(0),
                window: row.window.as_str().to_string(),
                policy: row.policy.as_str().to_string(),
            });
    }
    for (scope, windows) in grouped {
        map.insert(scope.clone(), BudgetSpec { scope, windows });
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

/// RFC-3339 UTC timestamp for durable `updated_at` columns (same convention the stores use).
fn now_rfc3339() -> String {
    use time::format_description::well_known::Rfc3339;
    time::OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_default()
}

/// Read + parse the config file at `state.config_path`. Shared by the preview and apply routes
/// so both ever see exactly the same parsed shape.
#[allow(clippy::result_large_err)] // axum::Response is intentionally large; idiomatic shape.
fn read_config_file(state: &ProxyState) -> Result<crate::config::SandhiFileConfig, Response> {
    let Some(path) = &state.config_path else {
        return Err(err(
            StatusCode::NOT_FOUND,
            "no config file configured (set SANDHI_CONFIG)",
        ));
    };
    let text = std::fs::read_to_string(path).map_err(|e| {
        err(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("reading {}: {e}", path.display()),
        )
    })?;
    serde_json::from_str(&text).map_err(|e| {
        err(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("parsing {}: {e}", path.display()),
        )
    })
}

/// `GET /admin/config` — parse the declared config and report what `POST /admin/config/apply`
/// would do, without doing it. Never returns secrets (the file never has them; this doesn't
/// resolve `secret_env`/`webhook_env` at all — only whether each entry would be new or already
/// satisfied).
pub(crate) async fn config_preview(
    State(state): State<Arc<ProxyState>>,
    headers: HeaderMap,
) -> Response {
    if let Err(r) = require_access(&state, &headers, crate::auth::Permission::Admin, false).await {
        return r;
    }
    let cfg = match read_config_file(&state) {
        Ok(c) => c,
        Err(r) => return r,
    };
    let existing_backends = match crate::backend::preflight_config(&state, &cfg).await {
        Ok(records) => records,
        Err(response) => return response,
    };
    let existing_routes = match crate::routing::preflight_config(&state, &cfg).await {
        Ok(records) => records,
        Err(response) => return response,
    };

    let existing_vault: Vec<Value> = state
        .vault
        .as_ref()
        .and_then(|v| v.list().ok())
        .unwrap_or_default()
        .iter()
        .map(|e| Value::String(e.credential_id()))
        .collect();
    let existing_budgets: std::collections::HashSet<String> = state
        .budgets
        .lock()
        .expect("budgets poisoned")
        .keys()
        .cloned()
        .collect();
    let existing_alerts = state
        .alert_store
        .as_ref()
        .and_then(|s| s.list().ok())
        .unwrap_or_default();
    let existing_vkeys = state
        .vkeys
        .as_ref()
        .and_then(|s| s.list().ok())
        .unwrap_or_default();
    let existing_profiles = state
        .profiles
        .as_ref()
        .and_then(|s| s.list_profiles().ok())
        .unwrap_or_default();

    let providers_plan: Vec<Value> = cfg
        .providers
        .iter()
        .map(|p| {
            let id = format!("{}:{}", p.provider, p.label.as_deref().unwrap_or("default"));
            let action = if existing_vault.contains(&Value::String(id.clone())) {
                "update"
            } else {
                "create"
            };
            json!({ "credential_id": id, "base_url": p.base_url, "action": action })
        })
        .collect();
    let backends_plan:Vec<Value> = cfg.backends.iter().map(|definition| {
        let existing = existing_backends.iter().find(|record| record.definition.id == definition.id);
        let action = match existing {
            None => "create",
            Some(record) if record.status == "active" && record.definition == *definition => "satisfied",
            Some(_) => "update",
        };
        json!({"id":definition.id,"credential_ref":definition.credential_ref,"endpoint":definition.endpoint,
            "revision":existing.map(|record|record.revision),"action":action})
    }).collect();
    let budgets_plan: Vec<Value> = cfg
        .budgets
        .iter()
        .map(|b| {
            let action = if existing_budgets.contains(&b.scope) {
                "update"
            } else {
                "create"
            };
            json!({ "scope": b.scope, "limits": b.limits, "action": action })
        })
        .collect();
    let alerts_plan: Vec<Value> = cfg
        .alerts
        .iter()
        .map(|a| {
            // Preview can't know the resolved channel without reading env — compare against both
            // possible shapes is overkill for a preview; report "pending" and let apply resolve it.
            let action = if existing_alerts
                .iter()
                .any(|r| r.scope == a.scope && r.threshold_pct == a.threshold_pct)
            {
                "likely satisfied"
            } else {
                "create"
            };
            json!({ "scope": a.scope, "threshold_pct": a.threshold_pct, "action": action })
        })
        .collect();
    let vkeys_plan: Vec<Value> = cfg
        .vkeys
        .iter()
        .map(|v| {
            let action = if crate::config::vkey_already_applied(&existing_vkeys, v) { "already satisfied" } else { "mint" };
            json!({ "upstream": v.upstream, "subject": v.subject, "group": v.group, "action": action })
        })
        .collect();
    // Profiles are a single upsert-able row per group, so the verb distinguishes all three
    // states (create / satisfied / update) — see `config::profile_matches`.
    let profiles_plan: Vec<Value> = cfg
        .profiles
        .iter()
        .map(|p| {
            let existing = existing_profiles.iter().find(|r| r.group == p.group);
            let action = match existing {
                None => "create",
                Some(row) if crate::config::profile_matches(row, p) => "satisfied",
                Some(_) => "update",
            };
            json!({ "group": p.group, "mode": p.effective_mode(), "action": action })
        })
        .collect();
    let mut payload = json!({
        "path": state.config_path.as_ref().map(|p| p.display().to_string()),
        "providers": providers_plan,
        "backends": backends_plan,
        "routes": cfg.routes.iter().map(|definition| {
            let existing=existing_routes.iter().find(|record|record.definition.id==definition.id);
            json!({"id":definition.id,"upstream_ref":definition.route_ref(),"revision":existing.map(|record|record.revision),
                "action":match existing {None=>"create",Some(record) if record.status=="active" && record.definition==*definition=>"satisfied",Some(_)=>"update"}})
        }).collect::<Vec<_>>(),
        "budgets": budgets_plan,
        "alerts": alerts_plan,
        "vkeys": vkeys_plan,
        "profiles": profiles_plan,
        // Listener TLS is a startup-only section (see `config` module docs): desired-state
        // apply cannot activate it, so it is reported as `restart required` — never silently
        // ignored. Live replacement is TD-0017 P2.
        "tls": if cfg.tls.is_some() { "restart required" } else { "not configured" },
    });
    add_deadline_status(&mut payload, &state, &cfg);
    Json(payload).into_response()
}

fn add_deadline_status(
    payload: &mut Value,
    state: &ProxyState,
    cfg: &crate::config::SandhiFileConfig,
) {
    if state.streaming_policy().is_some() || cfg.streaming_deadlines.is_some() {
        payload["streaming_deadlines"] = json!({
            "activation": "applies on apply",
            "active": state.streaming_policy().map(|policy|policy.report()),
            "desired": cfg.streaming_deadlines.as_ref().map(|policy|policy.report()),
        });
    }
    if state.buffered_deadlines.is_some() || cfg.buffered_deadlines.is_some() {
        payload["buffered_deadlines"] = json!({
            "activation": "restart required",
            "active": state.buffered_deadlines.as_ref().map(|policy| policy.report()),
            "desired": cfg.buffered_deadlines.as_ref().map(|policy| policy.report()),
        });
    }
}

/// `POST /admin/config/apply` — additive-only: creates/updates everything declared, never
/// deletes or revokes anything missing from the file (see `config` module docs for why).
/// Secrets are resolved from THIS PROCESS's environment via `secret_env`/`webhook_env` — never
/// read from the file, which never carries them.
pub(crate) async fn config_apply(
    State(state): State<Arc<ProxyState>>,
    headers: HeaderMap,
) -> Response {
    if let Err(r) = require_access(&state, &headers, crate::auth::Permission::Admin, true).await {
        return r;
    }
    let _operation = match admit_mutation(&state.lifecycle) {
        Ok(operation) => operation,
        Err(response) => return response,
    };
    let cfg = match read_config_file(&state) {
        Ok(c) => c,
        Err(r) => return r,
    };
    let existing_backends = match crate::backend::preflight_config(&state, &cfg).await {
        Ok(records) => records,
        Err(response) => return response,
    };
    let existing_routes = match crate::routing::preflight_config(&state, &cfg).await {
        Ok(records) => records,
        Err(response) => return response,
    };
    let getenv = |k: &str| std::env::var(k).ok();

    // Reject malformed budget/alert intent before touching any backend, including the vault.
    for b in &cfg.budgets {
        let req = admin::SetBudgetRequest {
            scope: b.scope.clone(),
            limits: b.limits.clone(),
            alert_thresholds: b.alert_thresholds.clone(),
        };
        if let Err(message) = validate_budget(&req) {
            return err(StatusCode::BAD_REQUEST, message);
        }
    }
    if cfg
        .alerts
        .iter()
        .any(|a| a.scope.trim().is_empty() || a.threshold_pct > 100)
    {
        return err(
            StatusCode::BAD_REQUEST,
            "alerts require a nonempty scope and threshold between 0 and 100",
        );
    }
    if cfg
        .profiles
        .iter()
        .any(|p| p.group.trim().is_empty() || !matches!(p.effective_mode(), "gateway" | "local"))
    {
        return err(
            StatusCode::BAD_REQUEST,
            "profiles require a nonempty group and a mode of 'gateway' or 'local'",
        );
    }
    let mut failures = Vec::new();
    let mut providers_applied = Vec::new();
    let mut failed_provider_refs = std::collections::HashSet::new();
    let mut failed_backend_refs = std::collections::HashSet::new();
    for p in &cfg.providers {
        let secret = crate::config::resolve_secret(&p.secret_env, getenv);
        let req = admin::AddKeyRequest {
            provider: p.provider.clone(),
            label: p.label.clone(),
            scheme: p.scheme.clone(),
            base_url: p.base_url.clone(),
            user_agent: p.user_agent.clone(),
            secret,
        };
        let resp = add_key(State(state.clone()), headers.clone(), Json(req)).await;
        if resp.status().is_success() {
            providers_applied.push(json!({ "provider": p.provider, "label": p.label }));
        } else {
            failed_provider_refs.insert(format!(
                "{}:{}",
                p.provider,
                p.label.as_deref().unwrap_or("default")
            ));
            failures.push(
                config_component_failure(
                    json!({"component": "provider", "provider": p.provider,
                "label": p.label, "error": "provider apply failed"}),
                    resp,
                )
                .await,
            );
        }
    }

    let mut backends_applied = Vec::new();
    for definition in &cfg.backends {
        if failed_provider_refs.contains(&definition.credential_ref) {
            failed_backend_refs.insert(definition.route_ref());
            failures.push(json!({"component":"backend","id":definition.id,"error":"credential dependency failed; backend not applied"}));
            continue;
        }
        let expected_revision = existing_backends
            .iter()
            .find(|record| record.definition.id == definition.id)
            .map(|record| record.revision);
        let response = crate::backend::register(
            State(state.clone()),
            headers.clone(),
            Json(crate::backend::RegisterBackendRequest {
                definition: definition.clone(),
                expected_revision,
            }),
        )
        .await;
        if response.status().is_success() {
            backends_applied
                .push(json!({"id":definition.id,"upstream_ref":definition.route_ref()}));
        } else {
            failed_backend_refs.insert(definition.route_ref());
            failures.push(
                config_component_failure(
                    json!({"component":"backend","id":definition.id,
                "error":"backend apply failed"}),
                    response,
                )
                .await,
            );
        }
    }

    let mut routes_applied = Vec::new();
    for definition in &cfg.routes {
        if definition
            .models
            .iter()
            .flat_map(|model| &model.targets)
            .any(|target| failed_backend_refs.contains(&format!("backend/{}", target.backend)))
        {
            failed_backend_refs.insert(definition.route_ref());
            failures.push(json!({"component":"route","id":definition.id,"error":"backend dependency failed; route not applied"}));
            continue;
        }
        let response = crate::routing::register(
            State(state.clone()),
            headers.clone(),
            Json(crate::routing::Register {
                definition: definition.clone(),
                expected_revision: existing_routes
                    .iter()
                    .find(|record| record.definition.id == definition.id)
                    .map(|record| record.revision),
            }),
        )
        .await;
        if response.status().is_success() {
            routes_applied.push(json!({"id":definition.id,"upstream_ref":definition.route_ref()}));
        } else {
            failed_backend_refs.insert(definition.route_ref());
            failures.push(
                config_component_failure(
                    json!({"component":"route","id":definition.id,"error":"route apply failed"}),
                    response,
                )
                .await,
            );
        }
    }
    let mut alerts_created = Vec::new();
    let mut alerts_skipped = Vec::new();

    // Streaming deadlines hot-apply (ADR-0047): validated against the provider
    // registry as of THIS apply — so a file adding a provider and a deadline
    // reference together succeeds. Applied after providers/backends/routes so
    // the references they register are visible here. Refusal leaves the active
    // policy untouched; other sections still apply (no rollback). The apply
    // response's deadline status (active vs desired) is the convergence receipt.
    if let Some(desired) = &cfg.streaming_deadlines {
        match desired.validate_endpoints(state.registered_reference()) {
            Ok(()) => state.set_streaming_deadlines(Some(desired.clone())),
            Err(message) => failures.push(json!({
                "component":"streaming_deadlines",
                "error": format!("{message}; streaming policy not activated"),
            })),
        }
    }

    let mut budgets_applied = Vec::new();
    for b in &cfg.budgets {
        let req = admin::SetBudgetRequest {
            scope: b.scope.clone(),
            limits: b.limits.clone(),
            alert_thresholds: None, // handled below, so it shares the same dedup + reporting as cfg.alerts
        };
        let resp = set_budget_impl(State(state.clone()), headers.clone(), Json(req), false).await;
        if resp.status().is_success() {
            budgets_applied.push(json!({ "scope": b.scope, "limits": b.limits }));
        } else {
            failures.push(
                config_component_failure(
                    json!({"component": "budget", "scope": b.scope,
                "error": "budget apply failed"}),
                    resp,
                )
                .await,
            );
            // Dependent alerts must not be installed for a budget that failed to commit.
            continue;
        }
        for &pct in b.alert_thresholds.iter().flatten() {
            match apply_one_alert(&state, &b.scope, pct, &None, getenv) {
                Ok(Some(rec)) => alerts_created.push(alert_rule_response(&rec)),
                Ok(None) => alerts_skipped.push(json!({ "scope": b.scope, "threshold_pct": pct })),
                Err(error) => {
                    tracing::error!(%error, "config alert apply failed");
                    failures.push(error.failure(&b.scope, pct));
                }
            }
        }
    }

    for a in &cfg.alerts {
        match apply_one_alert(&state, &a.scope, a.threshold_pct, &a.webhook_env, getenv) {
            Ok(Some(rec)) => alerts_created.push(alert_rule_response(&rec)),
            Ok(None) => {
                alerts_skipped.push(json!({ "scope": a.scope, "threshold_pct": a.threshold_pct }))
            }
            Err(error) => {
                tracing::error!(%error, "config alert apply failed");
                failures.push(error.failure(&a.scope, a.threshold_pct));
            }
        }
    }

    let mut vkeys_minted = Vec::new();
    let mut vkeys_skipped = Vec::new();
    for v in &cfg.vkeys {
        if failed_backend_refs.contains(&v.upstream) {
            failures.push(json!({"component":"vkey","upstream":v.upstream,"error":"backend dependency failed; no key minted"}));
            continue;
        }
        let existing = match state.vkeys.as_ref().map(|s| s.list()).transpose() {
            Ok(Some(records)) => records,
            result => {
                if let Err(error) = result {
                    tracing::error!(%error, "config vkey lookup failed");
                }
                failures.push(json!({"component": "vkey", "upstream": v.upstream,
                    "error": "virtual key inventory unavailable; no key minted"}));
                continue;
            }
        };
        if crate::config::vkey_already_applied(&existing, v) {
            vkeys_skipped
                .push(json!({ "upstream": v.upstream, "subject": v.subject, "group": v.group }));
            continue;
        }
        let req = admin::ShareKeyRequest {
            upstream: v.upstream.clone(),
            subject: v.subject.clone(),
            group: v.group.clone(),
            models: v.models.clone(),
            budget_scope: None,
            expires_at: None,
            rate_limit_per_min: v.rate_limit_per_min,
        };
        let resp = share_key(State(state.clone()), headers.clone(), Json(req)).await;
        if resp.status().is_success() {
            let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
                .await
                .unwrap_or_default();
            if let Ok(val) = serde_json::from_slice::<Value>(&bytes) {
                // Shown once, same convention as a direct `keys share` call — the caller (dashboard
                // or CLI) is responsible for capturing it; Sandhi never stores the plaintext.
                vkeys_minted.push(val);
            } else {
                failures.push(json!({"component": "vkey", "upstream": v.upstream,
                    "error": "key minted but response unavailable; inspect inventory before retrying"}));
            }
        } else {
            failures.push(
                config_component_failure(
                    json!({"component": "vkey", "upstream": v.upstream,
                "error": "virtual key apply failed"}),
                    resp,
                )
                .await,
            );
        }
    }

    // Profiles apply as direct durable-table upserts (additive-only): no secrets are involved,
    // so unlike vkeys there is no mint step to delegate to an internal handler.
    let mut profiles_applied = Vec::new();
    let mut profiles_skipped = Vec::new();
    for p in &cfg.profiles {
        if p.effective_mode() == "gateway"
            && p.upstream
                .as_ref()
                .is_some_and(|upstream| failed_backend_refs.contains(upstream))
        {
            failures.push(json!({"component":"profile","group":p.group,
                "error":"backend dependency failed; profile not applied"}));
            continue;
        }
        let Some(store) = state.profiles.clone() else {
            failures.push(json!({"component": "profile", "group": p.group,
                "error": "profile store not configured (set SANDHI_STORE)"}));
            continue;
        };
        let record = sandhi_store::ProfileRecord {
            group: p.group.clone(),
            mode: p.effective_mode().to_string(),
            upstream_ref: p.upstream.clone(),
            models: p.models.clone(),
            timeout_seconds: p.defaults.timeout_seconds,
            max_tokens: p.defaults.max_tokens,
            updated_at: now_rfc3339(),
        };
        match store.profile_for_group(&p.group) {
            Ok(Some(ref existing)) if crate::config::profile_matches(existing, p) => {
                profiles_skipped.push(json!({ "group": p.group }));
                continue;
            }
            Ok(_) => {}
            Err(error) => {
                tracing::error!(%error, "config profile lookup failed");
                failures.push(json!({"component": "profile", "group": p.group,
                    "error": "profile lookup unavailable; not applied"}));
                continue;
            }
        }
        match store.upsert_profile(&record) {
            Ok(()) => profiles_applied.push(json!({ "group": p.group, "mode": record.mode })),
            Err(error) => {
                tracing::error!(%error, "config profile apply failed");
                failures.push(json!({"component": "profile", "group": p.group,
                    "error": "profile commit failed"}));
            }
        }
    }

    let complete = failures.is_empty();
    let draining = failures
        .iter()
        .any(|failure| failure["code"] == "gateway_draining");
    let mut payload = json!({
        "ok": complete,
        "failures": failures,
        "providers": { "applied": providers_applied },
        "backends": { "applied": backends_applied },
        "routes": { "applied": routes_applied },
        "budgets": { "applied": budgets_applied },
        "alerts": { "created": alerts_created, "skipped": alerts_skipped },
        "vkeys": { "minted": vkeys_minted, "skipped": vkeys_skipped },
        "profiles": { "applied": profiles_applied, "skipped": profiles_skipped },
        // Same acknowledgement as preview: the `tls` section parsed but was NOT applied —
        // listener TLS activates only at process bootstrap (TD-0017 P1), never mid-flight.
        "tls": if cfg.tls.is_some() { "restart required" } else { "not configured" },
    });
    add_deadline_status(&mut payload, &state, &cfg);
    if !complete {
        payload["error"] = json!("config apply incomplete; committed items were not rolled back; inspect results before retrying");
    }
    if draining {
        payload["code"] = json!("gateway_draining");
    }
    let mut response = (
        if complete {
            StatusCode::OK
        } else {
            StatusCode::SERVICE_UNAVAILABLE
        },
        Json(payload),
    )
        .into_response();
    if draining {
        response
            .headers_mut()
            .insert("retry-after", "1".parse().unwrap());
    }
    response
}

/// Shared by both the top-level `alerts[]` config entries and a budget's inline
/// `alert_thresholds`, so both paths get the same dedup (`POST /admin/alerts` has no upsert
/// semantics — see `config::alert_already_applied`).
fn apply_one_alert(
    state: &ProxyState,
    scope: &str,
    threshold_pct: u8,
    webhook_env: &Option<String>,
    getenv: impl Fn(&str) -> Option<String> + Copy,
) -> Result<Option<AlertRuleRecord>, AlertApplyError> {
    // Config orchestration does not confer authority on components that reach cutoff later.
    let _operation = state
        .lifecycle
        .try_operation()
        .ok_or(AlertApplyError::Draining)?;
    let channel_str = crate::config::resolve_channel(webhook_env, getenv);
    let existing = state
        .alert_store
        .as_ref()
        .ok_or_else(|| "alert store not configured".to_string())?
        .list()
        .map_err(|e| e.to_string())?;
    let entry = crate::config::AlertEntry {
        scope: scope.to_string(),
        threshold_pct,
        webhook_env: webhook_env.clone(),
    };
    if crate::config::alert_already_applied(&existing, &entry, &channel_str) {
        return Ok(None);
    }
    let channel = channel_str
        .strip_prefix("webhook:")
        .map(|url| AlertChannel::Webhook {
            url: url.to_string(),
        })
        .unwrap_or(AlertChannel::Log);
    create_alert_for_scope(state, scope, threshold_pct, channel)
        .map(Some)
        .map_err(Into::into)
}

#[cfg(test)]
mod mutation_lifecycle_tests {
    use super::*;
    use sandhi_core::InMemorySink;
    use sandhi_store::vault::{Vault, VaultCapabilities};

    fn state() -> ProxyState {
        let mut state = ProxyState::new(
            KeyStore::new(),
            ProxyLedger::in_memory(),
            Arc::new(InMemorySink::new()),
            HashMap::new(),
            None,
        );
        state.admin_token = Some("synthetic-admin".into());
        state.vault = Some(Arc::new(sandhi_store::VaultStore::in_memory().unwrap()));
        state.vkeys = Some(Arc::new(VirtualKeyStore::in_memory().unwrap()));
        state.alert_store = Some(Arc::new(AlertStore::in_memory().unwrap()));
        state
    }

    fn headers() -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert("authorization", "Bearer synthetic-admin".parse().unwrap());
        headers
    }

    async fn body(response: Response) -> Value {
        serde_json::from_slice(
            &axum::body::to_bytes(response.into_body(), 16384)
                .await
                .unwrap(),
        )
        .unwrap()
    }

    #[tokio::test]
    async fn every_mutating_handler_rejects_after_cutoff_before_touching_stores() {
        let state = Arc::new(state());
        let vault = state.vault.as_ref().unwrap();
        vault
            .set(
                "openai",
                "default",
                CredentialScheme::ApiKey,
                None,
                None,
                "synthetic-provider",
            )
            .unwrap();
        let rule = state
            .alert_store
            .as_ref()
            .unwrap()
            .create(CreateAlertRequest {
                scope: "group:test".into(),
                threshold_pct: 50,
                channel: AlertChannel::Log,
            })
            .unwrap();
        let before_vault = vault.list().unwrap();
        let before_alerts = state.alert_store.as_ref().unwrap().list().unwrap();
        let minted = state
            .vkeys
            .as_ref()
            .unwrap()
            .mint(sandhi_store::MintRequest {
                upstream_ref: "openai:default".into(),
                subject_id: None,
                group_id: None,
                models: Vec::new(),
                budget_scope: None,
                model_budget_scope: None,
                expires_at: None,
                rate_limit_per_min: None,
            })
            .unwrap();
        let before_vkeys = state.vkeys.as_ref().unwrap().list().unwrap();
        state.lifecycle.begin_quiesce(Duration::from_secs(1));
        for operation in 0..12 {
            // Invoke handlers directly: an outer router rejection must not hide a missing gate.
            let response = match operation {
                0 => {
                    add_key(
                        State(state.clone()),
                        headers(),
                        Json(
                            serde_json::from_value(json!({
                                "provider": "openai", "secret": "replacement-secret"
                            }))
                            .unwrap(),
                        ),
                    )
                    .await
                }
                1 => {
                    register_reference(
                        State(state.clone()),
                        headers(),
                        Json(
                            serde_json::from_value(json!({
                                "provider": "openai"
                            }))
                            .unwrap(),
                        ),
                    )
                    .await
                }
                2 => {
                    revoke_key(
                        State(state.clone()),
                        headers(),
                        Path(("openai".into(), "default".into())),
                    )
                    .await
                }
                3 => {
                    share_key(
                        State(state.clone()),
                        headers(),
                        Json(
                            serde_json::from_value(json!({
                                "upstream": "openai:default"
                            }))
                            .unwrap(),
                        ),
                    )
                    .await
                }
                4 => {
                    revoke_virtual_key(
                        State(state.clone()),
                        headers(),
                        Path(minted.record.id.clone()),
                    )
                    .await
                }
                5 => {
                    set_budget(
                        State(state.clone()),
                        headers(),
                        Json(
                            serde_json::from_value(json!({
                                "scope": "group:test",
                                "limits": {"total": {"limit_tokens": 10, "policy": "block"}}
                            }))
                            .unwrap(),
                        ),
                    )
                    .await
                }
                6 => {
                    create_alert(
                        State(state.clone()),
                        headers(),
                        Json(
                            serde_json::from_value(json!({
                                "scope": "group:test", "threshold_pct": 90
                            }))
                            .unwrap(),
                        ),
                    )
                    .await
                }
                7 => ack_alert(State(state.clone()), headers(), Path(rule.id.clone())).await,
                8 => delete_alert(State(state.clone()), headers(), Path(rule.id.clone())).await,
                9 => config_apply(State(state.clone()), headers()).await,
                // Gateway-profiles token minting/revoke are Admin mutations like key sharing.
                10 => {
                    mint_profile_token(
                        State(state.clone()),
                        headers(),
                        Json(
                            serde_json::from_value(json!({
                                "subject": "alice", "group": "platform"
                            }))
                            .unwrap(),
                        ),
                    )
                    .await
                }
                _ => {
                    revoke_profile_tokens(
                        State(state.clone()),
                        headers(),
                        Json(
                            serde_json::from_value(json!({
                                "subject": "alice", "group": "platform"
                            }))
                            .unwrap(),
                        ),
                    )
                    .await
                }
            };
            assert_eq!(
                response.status(),
                StatusCode::SERVICE_UNAVAILABLE,
                "operation {operation}"
            );
            assert_eq!(response.headers()["retry-after"], "1");
            assert_eq!(body(response).await["code"], "gateway_draining");
        }
        assert_eq!(vault.list().unwrap(), before_vault);
        assert_eq!(
            vault.get("openai", "default").unwrap().as_deref(),
            Some("synthetic-provider")
        );
        assert_eq!(
            state.alert_store.as_ref().unwrap().list().unwrap(),
            before_alerts
        );
        assert_eq!(state.vkeys.as_ref().unwrap().list().unwrap(), before_vkeys);
        assert!(state.budgets.lock().unwrap().is_empty());
        assert_eq!(state.lifecycle.active_operations(), 0);
        // Existing authentication remains authoritative, even while draining.
        assert_eq!(
            revoke_key(
                State(state),
                HeaderMap::new(),
                Path(("openai".into(), "default".into()))
            )
            .await
            .status(),
            StatusCode::UNAUTHORIZED
        );
    }

    struct ControlledVault {
        entered: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
        release: Arc<(Mutex<bool>, std::sync::Condvar)>,
        cutoff: Option<Arc<crate::lifecycle::Lifecycle>>,
    }

    impl Vault for ControlledVault {
        fn name(&self) -> &'static str {
            "synthetic-controlled"
        }
        fn capabilities(&self) -> VaultCapabilities {
            VaultCapabilities {
                read: true,
                write: true,
                delete: false,
                bounded_io: true,
            }
        }
        fn get_secret(&self, _: &str, _: &str) -> Result<Option<String>, VaultError> {
            Ok(Some("synthetic-provider".into()))
        }
        fn set_secret(&self, _: &str, _: &str, _: &str) -> Result<(), VaultError> {
            if let Some(cutoff) = &self.cutoff {
                cutoff.begin_quiesce(Duration::from_secs(1));
            }
            if let Some(entered) = self.entered.lock().unwrap().take() {
                let _ = entered.send(());
            }
            let (released, wake) = &*self.release;
            let (_released, result) = wake
                .wait_timeout_while(
                    released.lock().unwrap(),
                    Duration::from_secs(5),
                    |released| !*released,
                )
                .unwrap();
            if result.timed_out() {
                return Err(VaultError::Timeout);
            }
            Ok(())
        }
        fn delete_secret(&self, _: &str, _: &str) -> Result<bool, VaultError> {
            Ok(false)
        }
    }

    #[tokio::test]
    async fn cancelled_credential_caller_does_not_release_the_worker_operation_guard() {
        let mut state = state();
        let (entered, started) = tokio::sync::oneshot::channel();
        let release = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
        state.vault = Some(Arc::new(
            sandhi_store::VaultStore::with_backend(
                ":memory:",
                Box::new(ControlledVault {
                    entered: Mutex::new(Some(entered)),
                    release: release.clone(),
                    cutoff: None,
                }),
            )
            .unwrap(),
        ));
        let state = Arc::new(state);
        let caller_state = state.clone();
        let caller = tokio::spawn(async move {
            add_key(
                State(caller_state),
                headers(),
                Json(
                    serde_json::from_value(json!({
                        "provider": "openai", "secret": "synthetic-provider"
                    }))
                    .unwrap(),
                ),
            )
            .await
        });
        tokio::time::timeout(Duration::from_secs(2), started)
            .await
            .unwrap()
            .unwrap();
        state.lifecycle.begin_quiesce(Duration::from_secs(1));
        caller.abort();
        assert!(caller.await.unwrap_err().is_cancelled());
        assert_eq!(state.lifecycle.active_operations(), 1);
        assert_eq!(state.vault_writer.available_permits(), 0);
        *release.0.lock().unwrap() = true;
        release.1.notify_all();
        tokio::time::timeout(Duration::from_secs(2), async {
            while state.lifecycle.active_operations() != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(state.vault.as_ref().unwrap().list().unwrap().len(), 1);
        assert!(state
            .providers
            .lock()
            .unwrap()
            .contains_key("openai:default"));
        assert_eq!(state.vault_writer.available_permits(), 1);
    }

    #[tokio::test]
    async fn config_cutoff_preserves_committed_provider_and_rejects_remaining_components() {
        let mut state = state();
        state.vault = Some(Arc::new(
            sandhi_store::VaultStore::with_backend(
                ":memory:",
                Box::new(ControlledVault {
                    entered: Mutex::new(None),
                    release: Arc::new((Mutex::new(true), std::sync::Condvar::new())),
                    cutoff: Some(state.lifecycle.clone()),
                }),
            )
            .unwrap(),
        ));
        let path = std::env::temp_dir().join(format!(
            "sandhi-cutoff-{}-{}.json",
            std::process::id(),
            time::OffsetDateTime::now_utc().unix_timestamp_nanos()
        ));
        std::fs::write(
            &path,
            json!({
                "providers": [{"provider": "openai"}],
                "budgets": [{
                    "scope": "group:test",
                    "limits": {"total": {"limit_tokens": 10, "policy": "block"}},
                    "alert_thresholds": [50]
                }],
                "alerts": [{"scope": "group:test", "threshold_pct": 90}],
                "vkeys": [{"upstream": "openai:default"}]
            })
            .to_string(),
        )
        .unwrap();
        state.config_path = Some(path.clone());
        let state = Arc::new(state);
        let response = config_apply(State(state.clone()), headers()).await;
        std::fs::remove_file(path).unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(response.headers()["retry-after"], "1");
        let result = body(response).await;
        assert_eq!(result["ok"], false);
        assert_eq!(result["code"], "gateway_draining");
        assert_eq!(result["providers"]["applied"].as_array().unwrap().len(), 1);
        let failures = result["failures"].as_array().unwrap();
        assert_eq!(failures.len(), 3);
        assert!(failures
            .iter()
            .all(|failure| failure["code"] == "gateway_draining"));
        assert!(state.budgets.lock().unwrap().is_empty());
        assert!(state
            .alert_store
            .as_ref()
            .unwrap()
            .list()
            .unwrap()
            .is_empty());
        assert!(state.vkeys.as_ref().unwrap().list().unwrap().is_empty());
        assert_eq!(state.lifecycle.active_operations(), 0);
    }
}

#[cfg(test)]
mod diagnostic_admission_tests {
    use super::*;

    #[tokio::test]
    async fn cancelled_waiter_does_not_release_running_blocking_work() {
        let slots = Arc::new(tokio::sync::Semaphore::new(1));
        let lifecycle = Arc::new(crate::lifecycle::Lifecycle::new());
        let operation = lifecycle.try_operation().unwrap();
        let permit = slots.clone().try_acquire_owned().unwrap();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let waiter = tokio::spawn(admitted_diagnostic(permit, operation, move || {
            let _ = started_tx.send(());
            let _ = release_rx.recv();
        }));
        started_rx.await.unwrap();
        waiter.abort();
        assert!(waiter.await.unwrap_err().is_cancelled());
        assert_eq!(slots.available_permits(), 0);
        assert!(slots.clone().try_acquire_owned().is_err());
        lifecycle.begin_quiesce(Duration::from_secs(2));
        assert_eq!(lifecycle.active_operations(), 1);
        assert!(lifecycle.try_operation().is_none());
        assert!(
            tokio::time::timeout(Duration::from_millis(10), lifecycle.wait_idle())
                .await
                .is_err()
        );
        release_tx.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(2), lifecycle.wait_idle())
            .await
            .unwrap();
        assert_eq!(lifecycle.active_operations(), 0);
        let _permit = tokio::time::timeout(Duration::from_secs(2), slots.acquire())
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn failed_blocking_work_releases_admission() {
        let slots = Arc::new(tokio::sync::Semaphore::new(1));
        let lifecycle = Arc::new(crate::lifecycle::Lifecycle::new());
        let operation = lifecycle.try_operation().unwrap();
        let permit = slots.clone().try_acquire_owned().unwrap();
        assert!(
            admitted_diagnostic(permit, operation, || panic!("fixed test failure"))
                .await
                .is_err()
        );
        assert_eq!(slots.available_permits(), 1);
        assert_eq!(lifecycle.active_operations(), 0);
    }
}

#[cfg(test)]
mod vault_acl_projection_tests {
    use super::*;

    fn entry(acl: Option<SentinelPassGrantAcl>) -> VaultEntry {
        VaultEntry {
            provider: "openai".into(),
            label: "default".into(),
            scheme: CredentialScheme::ApiKey,
            base_url: None,
            user_agent: None,
            sentinelpass_entry_id: acl.map(|_| 42),
            sentinelpass_token_env: acl.map(|_| "SANDHI_TEST_SERVICE_TOKEN".into()),
            sentinelpass_grant_acl: acl,
            created_at: String::new(),
            generation: 1,
            status: "active".into(),
        }
    }

    fn plan(resolution: &'static str) -> VaultReferencePlan {
        VaultReferencePlan {
            backend: "sentinelpass-ipc",
            resolution,
            searched_references: Vec::new(),
        }
    }

    #[test]
    fn inventory_projects_declared_exact_grant_classes() {
        let acl = vault_acl_response(
            &entry(Some(SentinelPassGrantAcl {
                exe_pinned: true,
                fingerprint_enrolled: true,
                expires_at: Some(i64::MAX),
            })),
            Some(&plan("exact_entry")),
        )
        .unwrap();
        assert_eq!(acl["class"], "exe_and_fingerprint");
        assert_eq!(acl["exe_pinned"], true);
        assert_eq!(acl["fingerprint_enrolled"], true);
        assert_eq!(acl["expiry_state"], "active");
        assert_eq!(acl["source"], "registration_declaration");

        assert_eq!(
            vault_acl_response(&entry(None), Some(&plan("exact_entry"))).unwrap()["class"],
            "unknown_service_grant"
        );
        assert_eq!(
            vault_acl_response(&entry(None), Some(&plan("legacy_domain"))).unwrap()["class"],
            "legacy_domain"
        );
    }
}

#[cfg(test)]
mod usage_window_tests {
    use super::*;

    fn parse<'a>(
        from: Option<&'a str>,
        to: Option<&'a str>,
    ) -> Result<UsageWindow, (StatusCode, String)> {
        parse_usage_window(from, to, time::OffsetDateTime::now_utc())
    }

    #[test]
    fn default_window_is_last_thirty_days_ending_now() {
        let now = time::OffsetDateTime::now_utc();
        let window = parse_usage_window(None, None, now).unwrap();
        assert_eq!(window.source, "default");
        assert!(window.advisory.is_none());
        let to =
            time::OffsetDateTime::parse(&window.to, &time::format_description::well_known::Rfc3339)
                .unwrap();
        let from = time::OffsetDateTime::parse(
            &window.from,
            &time::format_description::well_known::Rfc3339,
        )
        .unwrap();
        assert_eq!(to, now.to_utc());
        assert_eq!((to - from).whole_days(), 30);
    }

    #[test]
    fn offset_bearing_inputs_canonicalize_to_utc() {
        // +05:30 offset: semantically 2026-01-01T18:30:00Z — stored rows are
        // UTC "…Z", so the persisted bound must be the canonical UTC form,
        // not the offset-bearing literal.
        let window = parse(Some("2026-01-02T00:00:00+05:30"), None).unwrap();
        assert_eq!(window.from, "2026-01-01T18:30:00Z");
    }

    #[test]
    fn date_only_to_covers_its_day_and_date_max_is_rejected_not_a_panic() {
        let window = parse(Some("2026-07-01"), Some("2026-07-02")).unwrap();
        assert_eq!(window.from, "2026-07-01T00:00:00Z");
        assert_eq!(window.to, "2026-07-03T00:00:00Z");
        assert_eq!(window.source, "explicit");
        // 9999-12-31 has no successor day: a 400, never a handler panic.
        let result = parse(None, Some("9999-12-31"));
        assert_eq!(result.unwrap_err().0, StatusCode::BAD_REQUEST);
    }

    #[test]
    fn inverted_bounds_and_year_zero_are_refused() {
        assert_eq!(
            parse(Some("2026-09-01"), Some("2026-08-01")).unwrap_err().0,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            parse(Some("2026-01-02T00:00:00Z"), Some("2026-01-02T00:00:00Z"))
                .unwrap_err()
                .0,
            StatusCode::BAD_REQUEST
        );
        // Year 0 with the 30-day default would fall outside the RFC-3339
        // representable range: refused, never an empty-string bound.
        let result = parse(None, Some("0000-01-01"));
        assert_eq!(result.unwrap_err().0, StatusCode::BAD_REQUEST);
        assert!(parse(Some("not-a-date"), None).is_err());
    }

    #[test]
    fn oversize_range_clamps_from_and_sets_advisory() {
        let window = parse(Some("2020-01-01"), Some("2026-08-20")).unwrap();
        assert!(window.advisory.unwrap().contains("direct database"));
        let to =
            time::OffsetDateTime::parse(&window.to, &time::format_description::well_known::Rfc3339)
                .unwrap();
        let from = time::OffsetDateTime::parse(
            &window.from,
            &time::format_description::well_known::Rfc3339,
        )
        .unwrap();
        assert_eq!((to - from).whole_days(), 366);
    }
}
