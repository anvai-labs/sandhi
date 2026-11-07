//! `GET /auth/profile` — per-user assembled gateway-profile downloads (Phase 5).
//!
//! One admin declares per-group defaults centrally (the declarative `profiles:` config section,
//! [`crate::config::ProfileEntry`] + `POST /admin/config/apply`); victor users download their
//! assembled profile with a **profile token** — a distinct credential class (`skp_…`, minted via
//! `POST /admin/profile-tokens`), never a virtual key and never the admin token.
//!
//! Secret-secrecy contract: there are no public profile links and nothing here can be probed
//! without a bearer token. The response carries the minted `vk_…` virtual key **exactly once**
//! (shown-once, never persisted plaintext — the store keeps only the hash) and never echoes the
//! presented `skp_…` token or any other token material.
//!
//! Issuer-agnostic identity seam: in today's tokens auth mode the bearer is an `skp_` profile
//! token resolved against the durable `profile_tokens` table. In OIDC mode this same route will
//! instead accept the caller's subject token at the marked seam below — the resolution feeds an
//! identical `(subject, group)` pair, so the response shape does not change.

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Json, Response};
use serde_json::json;
use std::sync::Arc;

use sandhi_core::VirtualKey;

use crate::access::err;
use crate::ProxyState;

pub(crate) async fn profile(State(state): State<Arc<ProxyState>>, headers: HeaderMap) -> Response {
    let presented = match bearer_token(&headers) {
        Some(t) => t,
        None => return err(StatusCode::UNAUTHORIZED, "missing profile token"),
    };
    let Some(store) = state.profiles.clone() else {
        return err(
            StatusCode::SERVICE_UNAVAILABLE,
            "profile store not configured (set SANDHI_STORE)",
        );
    };
    // --- identity seam (issuer-agnostic) -------------------------------------------------------
    // Tokens mode: SHA-256 the presented `skp_` token and look it up (active rows only).
    // OIDC mode (later): validate the presented subject token against the issuer here and
    // derive the same (subject, group); nothing downstream changes.
    let Some((subject, group)) = store.lookup_active_profile_token(&presented).ok().flatten()
    else {
        return err(StatusCode::UNAUTHORIZED, "invalid profile token");
    };
    let Some(record) = store.profile_for_group(&group).ok().flatten() else {
        return err(
            StatusCode::NOT_FOUND,
            &format!("no profile configured for group '{group}'"),
        );
    };
    let defaults = json!({
        "timeout_seconds": record.timeout_seconds,
        "max_tokens": record.max_tokens,
    });
    let mut payload = json!({
        "mode": record.mode,
        "endpoint": profile_endpoint(&state),
        "group": record.group,
        "defaults": defaults,
        "models": record.models,
    });

    // Gateway mode needs a routable credential; local mode is defaults-only by definition.
    if record.mode == "gateway" {
        match mint_profile_virtual_key(&state, &subject, &group, &record) {
            Ok(secret) => payload["credentials"] = json!({ "virtual_key": secret }),
            Err(response) => return response,
        }
    }
    Json(payload).into_response()
}

/// Mint a fresh virtual key for `(subject, group)` bound to the profile's upstream, through the
/// same store mint + live-key rehydration path `POST /admin/keys/share` uses. The plaintext
/// secret leaves this function exactly once (into the response body) and is never persisted.
#[allow(clippy::result_large_err)] // axum::Response is intentionally large; idiomatic shape.
fn mint_profile_virtual_key(
    state: &ProxyState,
    subject: &str,
    group: &str,
    record: &sandhi_store::ProfileRecord,
) -> Result<String, Response> {
    let Some(vkeys) = state.vkeys.clone() else {
        return Err(err(
            StatusCode::SERVICE_UNAVAILABLE,
            "virtual-key store not configured (set SANDHI_STORE)",
        ));
    };
    let Some(upstream) = record.upstream_ref.as_deref().filter(|u| !u.is_empty()) else {
        return Err(err(
            StatusCode::SERVICE_UNAVAILABLE,
            &format!(
                "profile for group '{}' has no upstream configured; cannot mint a virtual key",
                record.group
            ),
        ));
    };
    // Same resolvability bar as `share_key`: the upstream credential must be a registered
    // provider handle or an active vault entry, or the minted key could not route a request.
    let upstream_resolvable = state
        .providers
        .lock()
        .expect("providers poisoned")
        .contains_key(upstream)
        || state
            .vault
            .as_ref()
            .and_then(|v| v.list().ok())
            .map(|list| {
                list.iter()
                    .any(|e| e.status == "active" && e.credential_id() == upstream)
            })
            .unwrap_or(false);
    if !upstream_resolvable {
        return Err(err(
            StatusCode::SERVICE_UNAVAILABLE,
            &format!("profile upstream '{upstream}' is not available on this gateway"),
        ));
    }
    let minted = vkeys
        .mint(sandhi_store::MintRequest {
            subject_id: Some(subject.to_string()),
            group_id: Some(group.to_string()),
            upstream_ref: upstream.to_string(),
            models: record.models.clone(),
            budget_scope: None,
            expires_at: None,
            rate_limit_per_min: None,
        })
        .map_err(|e| {
            err(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("mint failed: {e}"),
            )
        })?;
    // Rehydrate the live key store keyed by the hash (the plaintext is never a lookup key),
    // exactly as the admin mint path does.
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
    Ok(minted.secret)
}

/// The public endpoint a profile consumer should point clients at: the deployment topology's
/// `public_name` + `tls_port` when `SANDHI_TOPOLOGY_FILE` provides both, else the configured
/// `SANDHI_PUBLIC_URL`.
fn profile_endpoint(state: &ProxyState) -> String {
    if let Some(topology) = &state.topology {
        if let (Some(public_name), Some(tls_port)) = (
            &topology.deployment.public_name,
            topology.deployment.tls_port,
        ) {
            return format!("https://{public_name}:{tls_port}");
        }
    }
    state.public_url.clone()
}

fn bearer_token(headers: &HeaderMap) -> Option<String> {
    headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(String::from)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderMap;
    use sandhi_core::{InMemorySink, KeyStore};
    use sandhi_store::ProfileStore;
    use serde_json::Value;
    use std::collections::HashMap;

    fn state() -> ProxyState {
        let mut state = ProxyState::new(
            KeyStore::new(),
            crate::ProxyLedger::in_memory(),
            Arc::new(InMemorySink::new()),
            HashMap::new(),
            None,
        );
        state.profiles = Some(Arc::new(ProfileStore::in_memory().unwrap()));
        state.vkeys = Some(Arc::new(
            sandhi_store::VirtualKeyStore::in_memory().unwrap(),
        ));
        state.vault = Some(Arc::new(sandhi_store::VaultStore::in_memory().unwrap()));
        state
    }

    /// Apply a gateway profile into the store and mint a profile token for it.
    fn seed(state: &ProxyState, group: &str, mode: &str) -> String {
        let store = state.profiles.as_ref().unwrap();
        store
            .upsert_profile(&sandhi_store::ProfileRecord {
                group: group.into(),
                mode: mode.into(),
                upstream_ref: Some("ollama:default".into()),
                models: vec!["glm".into()],
                timeout_seconds: Some(120),
                max_tokens: Some(4096),
                updated_at: "2026-01-01T00:00:00Z".into(),
            })
            .unwrap();
        store
            .mint_profile_token("alice", group)
            .expect("mint profile token")
    }

    fn bearer(value: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert("authorization", format!("Bearer {value}").parse().unwrap());
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

    fn register_upstream(state: &ProxyState) {
        state
            .vault
            .as_ref()
            .unwrap()
            .set(
                "ollama",
                "default",
                sandhi_store::CredentialScheme::ApiKey,
                None,
                None,
                "synthetic-provider",
            )
            .unwrap();
    }

    #[tokio::test]
    async fn happy_path_assembles_profile_with_fresh_virtual_key() {
        let state = state();
        register_upstream(&state);
        let state = Arc::new(state);
        let token = seed(&state, "platform", "gateway");

        let response = profile(State(state.clone()), bearer(&token)).await;
        assert_eq!(response.status(), StatusCode::OK);
        let payload = body(response).await;
        assert_eq!(payload["mode"], "gateway");
        assert_eq!(payload["group"], "platform");
        assert_eq!(payload["endpoint"], state.public_url);
        assert_eq!(payload["defaults"]["timeout_seconds"], 120);
        assert_eq!(payload["defaults"]["max_tokens"], 4096);
        assert_eq!(payload["models"], serde_json::json!(["glm"]));

        // The minted virtual key is shown exactly once and actually resolves.
        let serialized = payload.to_string();
        let vk = payload["credentials"]["virtual_key"].as_str().unwrap();
        assert!(vk.starts_with("vk_"));
        assert_eq!(
            serialized.matches(vk).count(),
            1,
            "the virtual key appears exactly once"
        );
        assert!(
            state
                .vkeys
                .as_ref()
                .unwrap()
                .find_by_secret(vk)
                .unwrap()
                .is_some(),
            "the minted key resolves against the durable store"
        );

        // Secret-secrecy contract: NO `skp_` token material in the response body at all.
        assert!(
            !serialized.contains("skp_"),
            "the profile token must never be echoed: {serialized}"
        );
        assert!(
            !serialized.contains("synthetic-provider"),
            "the upstream secret must never leak: {serialized}"
        );
    }

    #[tokio::test]
    async fn revoked_token_is_unauthorized() {
        let state = state();
        register_upstream(&state);
        let state = Arc::new(state);
        let token = seed(&state, "platform", "gateway");
        assert_eq!(
            state
                .profiles
                .as_ref()
                .unwrap()
                .revoke_profile_tokens("alice", "platform")
                .unwrap(),
            1
        );
        let response = profile(State(state), bearer(&token)).await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn unknown_or_malformed_token_is_unauthorized() {
        let state = Arc::new(state());
        for presented in ["skp_unknown", "vk_someone_else_key", ""] {
            let headers = if presented.is_empty() {
                HeaderMap::new()
            } else {
                bearer(presented)
            };
            let response = profile(State(state.clone()), headers).await;
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{presented}");
        }
    }

    #[tokio::test]
    async fn missing_profile_for_group_is_not_found() {
        let state = Arc::new(state());
        // Token minted for a group that has no applied profile.
        let token = state
            .profiles
            .as_ref()
            .unwrap()
            .mint_profile_token("alice", "orphan")
            .unwrap();
        let response = profile(State(state), bearer(&token)).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let payload = body(response).await;
        assert!(
            payload["error"]
                .as_str()
                .unwrap()
                .contains("no profile configured for group"),
            "shaped 404: {payload}"
        );
    }

    #[tokio::test]
    async fn absent_profile_store_is_unavailable() {
        let mut state = state();
        state.profiles = None;
        let response = profile(State(Arc::new(state)), bearer("skp_x")).await;
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn non_admin_cannot_mint_a_profile_token() {
        // No admin token configured → the admin API denies mutations outright (403, never 401,
        // so a probe cannot distinguish "wrong token" from "no gate").
        let mut state = state();
        state.admin_token = None;
        let state = Arc::new(state);
        let response = crate::operator::mint_profile_token(
            State(state),
            HeaderMap::new(),
            Json(serde_json::from_value(json!({"subject": "alice", "group": "platform"})).unwrap()),
        )
        .await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn admin_mint_downloads_then_revoke_blocks() {
        let mut state = state();
        register_upstream(&state);
        state.admin_token = Some("synthetic-admin".into());
        let state = Arc::new(state);
        // The applied profile the minted token's group resolves against.
        state
            .profiles
            .as_ref()
            .unwrap()
            .upsert_profile(&sandhi_store::ProfileRecord {
                group: "platform".into(),
                mode: "gateway".into(),
                upstream_ref: Some("ollama:default".into()),
                models: vec!["glm".into()],
                timeout_seconds: Some(120),
                max_tokens: Some(4096),
                updated_at: "2026-01-01T00:00:00Z".into(),
            })
            .unwrap();
        let mut admin = HeaderMap::new();
        admin.insert("authorization", "Bearer synthetic-admin".parse().unwrap());

        // Admin mints a profile token (shown once) ...
        let minted = crate::operator::mint_profile_token(
            State(state.clone()),
            admin.clone(),
            Json(serde_json::from_value(json!({"subject": "alice", "group": "platform"})).unwrap()),
        )
        .await;
        assert_eq!(minted.status(), StatusCode::OK);
        let payload = body(minted).await;
        let token = payload["token"].as_str().expect("token in mint response");
        assert!(token.starts_with("skp_"), "profile token shape: {token}");
        assert_eq!(payload["subject"], "alice");
        assert_eq!(payload["group"], "platform");

        // ... the token works on /auth/profile ...
        let response = profile(State(state.clone()), bearer(token)).await;
        assert_eq!(response.status(), StatusCode::OK);

        // ... the admin revokes by (subject, group) ...
        let revoked = crate::operator::revoke_profile_tokens(
            State(state.clone()),
            admin,
            Json(serde_json::from_value(json!({"subject": "alice", "group": "platform"})).unwrap()),
        )
        .await;
        assert_eq!(revoked.status(), StatusCode::OK);
        assert_eq!(body(revoked).await["revoked"], 1);

        // ... and the same token stops resolving.
        let response = profile(State(state), bearer(token)).await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn local_mode_profile_carries_no_credentials() {
        let state = Arc::new(state());
        let token = seed(&state, "platform", "local");
        let response = profile(State(state), bearer(&token)).await;
        assert_eq!(response.status(), StatusCode::OK);
        let payload = body(response).await;
        assert_eq!(payload["mode"], "local");
        assert!(
            payload.get("credentials").is_none(),
            "local mode never mints a key: {payload}"
        );
    }

    #[tokio::test]
    async fn gateway_profile_without_resolvable_upstream_is_unavailable() {
        // Profile declares an upstream nobody registered: minting would produce an unroutable
        // key, so the download fails closed instead.
        let state = Arc::new(state());
        let token = seed(&state, "platform", "gateway");
        let response = profile(State(state), bearer(&token)).await;
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert!(
            body(response).await["error"]
                .as_str()
                .unwrap()
                .contains("not available"),
            "shaped 503"
        );
    }

    #[tokio::test]
    async fn topology_endpoint_preferences() {
        let mut state = state();
        let token = seed(&state, "platform", "local");
        state.topology = Some(Arc::new(crate::topology::TopologyFile {
            deployment: crate::topology::DeploymentTopology {
                public_name: Some("dataserver3.singh.local".into()),
                tls_port: Some(18788),
                ..Default::default()
            },
            ..Default::default()
        }));
        let state = Arc::new(state);
        let payload = body(profile(State(state.clone()), bearer(&token)).await).await;
        assert_eq!(payload["endpoint"], "https://dataserver3.singh.local:18788");

        // Missing tls_port → fall back to the configured public URL.
        let mut state = ProxyState::new(
            KeyStore::new(),
            crate::ProxyLedger::in_memory(),
            Arc::new(InMemorySink::new()),
            HashMap::new(),
            None,
        );
        state.profiles = Some(Arc::new(ProfileStore::in_memory().unwrap()));
        state
            .profiles
            .as_ref()
            .unwrap()
            .upsert_profile(&sandhi_store::ProfileRecord {
                group: "platform".into(),
                mode: "local".into(),
                upstream_ref: None,
                models: Vec::new(),
                timeout_seconds: None,
                max_tokens: None,
                updated_at: "2026-01-01T00:00:00Z".into(),
            })
            .unwrap();
        let token = state
            .profiles
            .as_ref()
            .unwrap()
            .mint_profile_token("alice", "platform")
            .unwrap();
        state.topology = Some(Arc::new(crate::topology::TopologyFile {
            deployment: crate::topology::DeploymentTopology {
                public_name: Some("dataserver3.singh.local".into()),
                ..Default::default()
            },
            ..Default::default()
        }));
        let payload = body(profile(State(Arc::new(state)), bearer(&token)).await).await;
        assert_eq!(payload["endpoint"], "http://localhost:8787");
    }
}
