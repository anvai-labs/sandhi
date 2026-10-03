//! TD-0031: non-interactive client-credentials grant.
//!
//! A registered client exchanges its long-lived client credential for a
//! SHORT-LIVED opaque virtual key through the same mint path as
//! `POST /admin/keys/share` — TTL, model allowlist, budget scope, and rate
//! limit all come from the registry entry and are enforced by the ordinary
//! downstream pipeline. Design constraints (deliberate):
//!
//! - **No JWT issuance.** Tokens are gateway-opaque vkey secrets, the same
//!   CSPRNG + hash-only pattern as every other vkey. Minting JWTs would add
//!   a new RSA/JWT use site and trip the dependency advisory guard
//!   (docs/security/oidc-rsa-advisory.md). True OIDC token issuance remains
//!   the IdP's job (TD-0029 §service-accounts); this endpoint is the
//!   homelab/tokens-mode counterpart.
//! - **Registry is static config** (`SANDHI_CLIENT_CREDENTIALS_FILE`,
//!   owner-only JSON, loaded at startup, fail-closed). Rotation = edit +
//!   restart, the same operator contract as the rest of the static config.
//! - **No enumeration**: unknown client and bad secret return the identical
//!   401 body; attempts are counted per KNOWN client id only (unknown ids
//!   learn nothing and cost nothing).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::json;
use sha2::{Digest, Sha256};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

use crate::operator::{admit_mutation, constant_time_eq, err};
use crate::{ProxyState, VirtualKey};
use sandhi_store::MintRequest;

const DEFAULT_TTL_SECS: u64 = 900;
const MAX_TTL_CEILING_SECS: u64 = 3600;
const MAX_ATTEMPTS_PER_WINDOW: u32 = 10;
const ATTEMPT_WINDOW_SECS: u64 = 60;

/// One registered client. `secret_hash` is the lowercase hex SHA-256 of the
/// client secret — the plaintext is never stored server-side.
#[derive(Debug, Clone, Deserialize, serde::Serialize)]
pub struct ClientCredentialEntry {
    pub client_id: String,
    pub secret_hash: String,
    pub subject_id: String,
    #[serde(default)]
    pub group_id: Option<String>,
    pub upstream_ref: String,
    #[serde(default)]
    pub models: Vec<String>,
    #[serde(default)]
    pub budget_scope: Option<String>,
    #[serde(default)]
    pub rate_limit_per_min: Option<u32>,
    #[serde(default = "default_max_ttl")]
    pub max_ttl_seconds: u64,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

fn default_max_ttl() -> u64 {
    DEFAULT_TTL_SECS
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Deserialize)]
pub struct ClientTokenRequest {
    pub client_id: String,
    pub client_secret: String,
    #[serde(default)]
    pub ttl_seconds: Option<u64>,
}

/// Fixed-window per-client attempt limiter. The map only ever holds ids from
/// the (operator-bounded) registry, so memory is capped by configuration.
#[derive(Default)]
struct AttemptLimiter {
    window_start: Option<Instant>,
    counts: HashMap<String, u32>,
}

impl AttemptLimiter {
    fn allow(&mut self, client_id: &str) -> bool {
        let now = Instant::now();
        match self.window_start {
            Some(start) if now.duration_since(start) < Duration::from_secs(ATTEMPT_WINDOW_SECS) => {
            }
            _ => {
                self.window_start = Some(now);
                self.counts.clear();
            }
        }
        let count = self.counts.entry(client_id.to_string()).or_insert(0);
        if *count >= MAX_ATTEMPTS_PER_WINDOW {
            return false;
        }
        *count += 1;
        true
    }

    /// A successful exchange proves the credential — clear the count so a
    /// legitimate high-volume client can never lock itself out.
    fn reset(&mut self, client_id: &str) {
        self.counts.remove(client_id);
    }
}

pub struct ClientCredentialRegistry {
    clients: HashMap<String, ClientCredentialEntry>,
    attempts: Mutex<AttemptLimiter>,
}

impl ClientCredentialRegistry {
    /// Load + validate the registry file. Any malformed entry is fatal to the
    /// caller (startup exits): a partially-trusted client list must never run.
    pub fn load(path: &std::path::Path) -> Result<Self, String> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(path)
                .map_err(|e| format!("client-credentials file unreadable: {e}"))?
                .permissions()
                .mode();
            if mode & 0o077 != 0 {
                return Err("client-credentials file must be owner-only (chmod 600)".into());
            }
        }
        let raw = std::fs::read_to_string(path)
            .map_err(|e| format!("client-credentials file unreadable: {e}"))?;
        let entries: Vec<ClientCredentialEntry> = serde_json::from_str(&raw)
            .map_err(|e| format!("client-credentials file malformed: {e}"))?;
        let mut clients = HashMap::new();
        for mut entry in entries {
            entry.secret_hash = entry.secret_hash.to_lowercase();
            if entry.models.is_empty() || entry.models.iter().any(|m| m.trim().is_empty()) {
                // Allowlist-by-default: an empty model list (or blank entries,
                // which model_list() strips) would mint tokens valid for EVERY
                // model on the upstream.
                return Err(format!(
                    "client '{}' must list at least one non-blank model",
                    entry.client_id
                ));
            }
            if entry.client_id.is_empty() {
                return Err("client-credentials entry has an empty client_id".into());
            }
            if entry.secret_hash.len() != 64
                || !entry.secret_hash.bytes().all(|b| b.is_ascii_hexdigit())
            {
                return Err(format!(
                    "client '{}' secret_hash must be 64 hex chars (sha256)",
                    entry.client_id
                ));
            }
            if entry.upstream_ref.is_empty() {
                return Err(format!(
                    "client '{}' has an empty upstream_ref",
                    entry.client_id
                ));
            }
            if entry.max_ttl_seconds == 0 || entry.max_ttl_seconds > MAX_TTL_CEILING_SECS {
                return Err(format!(
                    "client '{}' max_ttl_seconds must be 1..={MAX_TTL_CEILING_SECS}",
                    entry.client_id
                ));
            }
            if clients.insert(entry.client_id.clone(), entry).is_some() {
                return Err("duplicate client_id in client-credentials file".into());
            }
        }
        Ok(Self {
            clients,
            attempts: Mutex::new(AttemptLimiter::default()),
        })
    }

    fn lookup(&self, client_id: &str) -> Option<&ClientCredentialEntry> {
        self.clients.get(client_id)
    }

    /// True when the attempt may proceed. Counts known clients only.
    fn admit_attempt(&self, client_id: &str) -> bool {
        if self.clients.contains_key(client_id) {
            {
                self.attempts
                    .lock()
                    .expect("attempt limiter poisoned")
                    .allow(client_id)
            }
        } else {
            false
        }
    }

    fn secret_matches(entry: &ClientCredentialEntry, presented: &str) -> bool {
        let digest = Sha256::digest(presented.as_bytes());
        let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
        constant_time_eq(hex.as_bytes(), entry.secret_hash.as_bytes())
    }
}

fn invalid_client() -> Response {
    // Identical body for unknown client and bad secret: no enumeration.
    err(StatusCode::UNAUTHORIZED, "invalid client credentials")
}

/// `POST /auth/token` — exchange client credentials for a short-lived virtual key.
///
/// The response shape follows the OAuth 2.0 client-credentials grant
/// (`access_token` / `token_type` / `expires_in` / `scope`); the token itself
/// is a normal gateway vkey usable on every model route.
pub(crate) async fn client_token(
    State(state): State<Arc<ProxyState>>,
    Json(req): Json<ClientTokenRequest>,
) -> Response {
    let Some(registry) = state.client_credentials.clone() else {
        return err(
            StatusCode::NOT_FOUND,
            "client-credentials grant not configured",
        );
    };
    let Some(entry) = registry.lookup(&req.client_id).cloned() else {
        // Unknown ids must be indistinguishable from bad secrets at EVERY
        // stage: an over-limit known id also returns this exact 401 (silent
        // throttle), so response shape never reveals registration status.
        return invalid_client();
    };
    if !registry.admit_attempt(&req.client_id) {
        // Silent throttle: same 401 body as any other failure. Revealing the
        // limit would make the limiter a client-id registration oracle.
        return invalid_client();
    }
    if !entry.enabled || !ClientCredentialRegistry::secret_matches(&entry, &req.client_secret) {
        return invalid_client();
    }

    if req.ttl_seconds == Some(0) {
        return err(StatusCode::BAD_REQUEST, "ttl_seconds must be at least 1");
    }
    let _operation = match admit_mutation(&state) {
        Ok(operation) => operation,
        Err(response) => return response,
    };
    let Some(vkeys) = state.vkeys.clone() else {
        return err(
            StatusCode::SERVICE_UNAVAILABLE,
            "virtual-key store not configured (set SANDHI_STORE)",
        );
    };
    // The upstream credential must resolve, exactly as /admin/keys/share requires.
    let upstream = entry.upstream_ref.clone();
    let upstream_known = state
        .providers
        .lock()
        .expect("providers poisoned")
        .contains_key(&upstream)
        || state
            .vault
            .as_ref()
            .and_then(|v| v.list().ok())
            .map(|list| {
                list.iter()
                    .any(|e| e.status == "active" && e.credential_id() == upstream)
            })
            .unwrap_or(false);
    if !upstream_known {
        return err(
            StatusCode::BAD_REQUEST,
            &format!("registered upstream '{upstream}' is not configured on this gateway"),
        );
    }

    let ttl = req
        .ttl_seconds
        .unwrap_or(DEFAULT_TTL_SECS)
        .min(entry.max_ttl_seconds)
        .min(MAX_TTL_CEILING_SECS);
    let expires_at = OffsetDateTime::now_utc()
        .saturating_add(time::Duration::seconds(ttl as i64))
        .format(&Rfc3339)
        .ok()
        .ok_or_else(|| {
            err(
                StatusCode::INTERNAL_SERVER_ERROR,
                "token expiry encoding failed",
            )
        });
    let expires_at = match expires_at {
        Ok(value) if !value.is_empty() => value,
        _ => {
            return err(
                StatusCode::INTERNAL_SERVER_ERROR,
                "token expiry encoding failed",
            )
        }
    };

    let mint_req = MintRequest {
        subject_id: Some(entry.subject_id.clone()),
        group_id: entry.group_id.clone(),
        upstream_ref: upstream.clone(),
        models: entry.models.clone(),
        budget_scope: entry.budget_scope.clone(),
        expires_at: Some(expires_at),
        rate_limit_per_min: entry.rate_limit_per_min,
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
    registry
        .attempts
        .lock()
        .expect("attempt limiter poisoned")
        .reset(&req.client_id);

    let scope = entry.models.join(" ");
    Json(json!({
        "access_token": minted.secret,
        "token_type": "bearer",
        "expires_in": ttl,
        "scope": scope,
    }))
    .into_response()
}
