//! One server-owned principal for the deliberately shared, unauthenticated starter.
use crate::{auth::AuthMode, ProxyState};
use axum::http::{HeaderMap, StatusCode};
use sandhi_core::VirtualKey;
use std::collections::HashSet;

pub(crate) const KEY_ID: &str = "starter_shared_v1";
pub(crate) const SUBJECT: &str = "builtin_starter";
pub(crate) const BUDGET_SCOPE: &str = "starter:shared:v1";

pub(crate) fn enabled(state: &ProxyState) -> bool {
    state.oidc.is_none() && state.auth_mode == AuthMode::None
}

async fn route_status(state: &ProxyState, id: &str) -> Result<Option<bool>, StatusCode> {
    let Some(routes) = state.routes.clone() else {
        return Ok(None);
    };
    let permit = state
        .routing_slots
        .clone()
        .try_acquire_owned()
        .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
    let operation = state
        .lifecycle
        .try_operation()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    let id = id.to_owned();
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let _operation = operation;
        routes
            .get(&id)
            .map(|record| record.map(|r| r.status == "active"))
    })
    .await
    .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?
    .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)
}

pub(crate) async fn principal(
    state: &ProxyState,
    headers: &HeaderMap,
) -> Result<VirtualKey, StatusCode> {
    // Credentials are ignored, never interpreted as an upstream/key selector.
    // Attribution keeps the ordinary key-authoritative echo checks downstream.
    for name in [
        "x-sandhi-upstream",
        "x-sandhi-backend",
        "x-sandhi-key",
        "x-sandhi-grant",
    ] {
        if headers.contains_key(name) {
            return Err(StatusCode::FORBIDDEN);
        }
    }
    let reference = if let Some(reference) = &state.starter_upstream {
        let registered = state
            .providers
            .lock()
            .expect("providers poisoned")
            .contains_key(reference);
        if !registered
            && !match reference.strip_prefix("route/") {
                Some(id) => route_status(state, id).await? == Some(true),
                None => false,
            }
        {
            return Err(StatusCode::SERVICE_UNAVAILABLE);
        }
        reference.clone()
    } else {
        match route_status(state, "default").await? {
            Some(true) => return Ok(key(state, "route/default".into())),
            Some(false) => return Err(StatusCode::SERVICE_UNAVAILABLE),
            None => {}
        }
        let providers = state.providers.lock().expect("providers poisoned");
        // A credential's compatibility handle is not a second configured backend
        // when an explicit backend already owns that exact credential reference.
        let covered: HashSet<_> = providers
            .values()
            .filter_map(|p| p.backend_binding())
            .map(|b| b.credential_ref.as_str())
            .collect();
        let mut candidates = providers
            .iter()
            .filter(|(id, p)| p.backend_binding().is_some() || !covered.contains(id.as_str()));
        let Some((reference, _)) = candidates.next() else {
            return Err(StatusCode::SERVICE_UNAVAILABLE);
        };
        if candidates.next().is_some() {
            return Err(StatusCode::SERVICE_UNAVAILABLE);
        }
        reference.clone()
    };
    Ok(key(state, reference))
}

fn key(state: &ProxyState, reference: String) -> VirtualKey {
    VirtualKey {
        id: KEY_ID.into(),
        subject_id: Some(SUBJECT.into()),
        group_id: Some(SUBJECT.into()),
        upstream_ref: reference,
        budget_scope: Some(BUDGET_SCOPE.into()),
        rate_limit_per_min: state.starter_rate_limit_per_min,
        ..Default::default()
    }
}

/// Host is checked against listener/public origins, not against Origin alone:
/// otherwise a DNS-rebinding page could supply matching attacker Origin/Host.
pub(crate) fn control_allowed(state: &ProxyState, headers: &HeaderMap, mutation: bool) -> bool {
    let unique = |name| {
        let values = headers.get_all(name).iter().collect::<Vec<_>>();
        match values.as_slice() {
            [] => Ok(None),
            [value] => value.to_str().map(Some).map_err(|_| ()),
            _ => Err(()),
        }
    };
    let Ok(Some(host)) = unique("host") else {
        return false;
    };
    let Ok(origin) = unique("origin") else {
        return false;
    };
    let Ok(referer) = unique("referer") else {
        return false;
    };
    if mutation
        && headers
            .get("sec-fetch-site")
            .is_some_and(|v| v == "cross-site")
    {
        return false;
    }
    state
        .starter_control_origins
        .iter()
        .chain(std::iter::once(&state.public_url))
        .any(|configured| {
            let Ok(expected) = reqwest::Url::parse(configured) else {
                return false;
            };
            if !matches!(expected.scheme(), "http" | "https") {
                return false;
            }
            let Ok(request) = reqwest::Url::parse(&format!("{}://{host}", expected.scheme()))
            else {
                return false;
            };
            if !request.username().is_empty()
                || request.password().is_some()
                || request.path() != "/"
                || request.query().is_some()
                || request.fragment().is_some()
            {
                return false;
            }
            let wildcard = expected
                .host_str()
                .is_some_and(|h| matches!(h, "0.0.0.0" | "[::]"));
            let literal = request.host_str().is_some_and(|h| {
                h.trim_matches(['[', ']'])
                    .parse::<std::net::IpAddr>()
                    .is_ok()
            });
            if expected.port_or_known_default() != request.port_or_known_default()
                || !(expected.host_str() == request.host_str() || (wildcard && literal))
            {
                return false;
            }
            let actual_origin = request.origin().ascii_serialization();
            if let Some(origin) = origin {
                return origin == actual_origin;
            }
            if mutation {
                if let Some(referer) = referer {
                    return reqwest::Url::parse(referer)
                        .is_ok_and(|u| u.origin().ascii_serialization() == actual_origin);
                }
            }
            true
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{auth::Permission, operator::require_access, ProxyLedger};
    use sandhi_core::{InMemorySink, KeyStore};
    use sandhi_providers::ProviderRuntime;
    use std::{collections::HashMap, sync::Arc};

    fn state() -> ProxyState {
        let runtime = ProviderRuntime::new();
        let provider = runtime.openai_compat(
            "one",
            "http://127.0.0.1:1/v1",
            "synthetic",
            Default::default(),
            None,
            None,
            None,
            None,
        );
        let mut state = ProxyState::new(
            KeyStore::new(),
            ProxyLedger::in_memory(),
            Arc::new(InMemorySink::new()),
            HashMap::from([("one".into(), provider)]),
            None,
        );
        state.auth_mode = AuthMode::None;
        state.public_url = "http://127.0.0.1:8787".into();
        state
    }

    #[test]
    fn explicit_authentication_configuration_never_becomes_implicit_none() {
        assert_eq!(AuthMode::select(None, false), Ok(AuthMode::None));
        assert_eq!(AuthMode::select(None, true), Ok(AuthMode::Oidc));
        assert_eq!(
            AuthMode::select(Some("tokens"), false),
            Ok(AuthMode::Tokens)
        );
        for (value, configured) in [
            (Some("oidc"), false),
            (Some("none"), true),
            (Some(""), false),
            (Some("unknown"), false),
        ] {
            assert!(AuthMode::select(value, configured).is_err());
        }
    }

    #[tokio::test]
    async fn dummy_credentials_and_registered_keys_cannot_choose_a_starter_principal() {
        let state = state();
        state.keys.insert(VirtualKey {
            id: "someone-else".into(),
            subject_id: Some("different-person".into()),
            upstream_ref: "another".into(),
            ..Default::default()
        });
        let first = principal(&state, &HeaderMap::new()).await.unwrap();
        let headers = HeaderMap::from_iter([(
            "authorization".parse().unwrap(),
            "Bearer someone-else".parse().unwrap(),
        )]);
        assert_eq!(principal(&state, &headers).await.unwrap(), first);
        assert_eq!(first.id, KEY_ID);
        assert_eq!(first.budget_scope.as_deref(), Some(BUDGET_SCOPE));
        assert_eq!(first.subject_id.as_deref(), Some(SUBJECT));
    }

    #[tokio::test]
    async fn ambiguous_upstreams_require_server_selection() {
        let mut state = state();
        let provider = state.providers.lock().unwrap()["one"].clone();
        state
            .providers
            .lock()
            .unwrap()
            .insert("two".into(), provider);
        assert_eq!(
            principal(&state, &HeaderMap::new()).await.unwrap_err(),
            StatusCode::SERVICE_UNAVAILABLE
        );
        state.starter_upstream = Some("two".into());
        assert_eq!(
            principal(&state, &HeaderMap::new())
                .await
                .unwrap()
                .upstream_ref,
            "two"
        );
        state.starter_upstream = Some("missing".into());
        assert_eq!(
            principal(&state, &HeaderMap::new()).await.unwrap_err(),
            StatusCode::SERVICE_UNAVAILABLE
        );
    }

    #[tokio::test]
    async fn revoked_default_route_never_falls_through_to_a_lone_provider() {
        let mut state = state();
        let routes = Arc::new(sandhi_store::routes::RouteStore::in_memory().unwrap());
        let definition = serde_json::from_value(serde_json::json!({"id":"default","models":[{"id":"logical","targets":[{"backend":"one","model":"physical"}]}]})).unwrap();
        let route = routes.put(&definition, None).unwrap();
        state.routes = Some(routes.clone());
        assert_eq!(
            principal(&state, &HeaderMap::new())
                .await
                .unwrap()
                .upstream_ref,
            "route/default"
        );
        routes.revoke("default", route.revision).unwrap();
        assert_eq!(
            principal(&state, &HeaderMap::new()).await.unwrap_err(),
            StatusCode::SERVICE_UNAVAILABLE
        );
    }

    #[tokio::test]
    async fn missing_oidc_authority_and_empty_admin_configuration_are_fail_closed() {
        let mut state = state();
        state.auth_mode = AuthMode::Oidc;
        state.admin_token = Some("valid-admin".into());
        let headers = HeaderMap::from_iter([(
            "authorization".parse().unwrap(),
            "Bearer valid-admin".parse().unwrap(),
        )]);
        assert_eq!(
            require_access(&state, &headers, Permission::Admin, true)
                .await
                .unwrap_err()
                .status(),
            StatusCode::SERVICE_UNAVAILABLE
        );
        state.auth_mode = AuthMode::None;
        state.admin_token = Some(String::new());
        let mut headers = HeaderMap::new();
        headers.insert("host", "127.0.0.1:8787".parse().unwrap());
        headers.insert("authorization", "Bearer ".parse().unwrap());
        assert_eq!(
            require_access(&state, &headers, Permission::Admin, true)
                .await
                .unwrap_err()
                .status(),
            StatusCode::SERVICE_UNAVAILABLE
        );
    }

    #[test]
    fn noauth_control_plane_never_trusts_a_matching_attacker_host_and_origin() {
        let state = state();
        let mut headers = HeaderMap::new();
        headers.insert("host", "127.0.0.1:8787".parse().unwrap());
        assert!(control_allowed(&state, &headers, true));
        headers.insert("origin", "http://127.0.0.1:8787".parse().unwrap());
        assert!(control_allowed(&state, &headers, true));
        headers.insert("origin", "null".parse().unwrap());
        assert!(!control_allowed(&state, &headers, true));
        headers.insert("host", "attacker.invalid".parse().unwrap());
        headers.insert("origin", "http://attacker.invalid".parse().unwrap());
        assert!(!control_allowed(&state, &headers, true));
    }
}
