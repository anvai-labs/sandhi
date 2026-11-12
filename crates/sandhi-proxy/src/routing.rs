//! Logical model authority, deterministic selection and durable compatible affinity.
//! No inference is attempted here. A selected handle is immutable for the admission.
use crate::{access::err, auth::Permission, operator::require_access, ProxyState};
use axum::{
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use sandhi_core::{cache_digest, ChatMessageV1, ChatRequestV1, NativeInputFeatureV1, VirtualKey};
use sandhi_providers::ProviderHandle;
use sandhi_store::routes::{Affinity, Error, Pin, RouteDefinition, RouteRecord, Selection, Target};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::sync::Arc;

#[derive(Clone, Serialize)]
pub(crate) struct DispatchPlan {
    pub route_id: String,
    pub route_revision: u64,
    pub requested_model: String,
    pub backend_id: String,
    pub provider_label: String,
    pub backend_revision: u64,
    pub actual_model: String,
    pub credential_generation: u64,
    pub credential_ref: String,
    pub endpoint_family: sandhi_core::EndpointFamilyV1,
    pub endpoint_profile: String,
    pub binding_sha256: String,
}

pub(crate) struct Selected {
    pub provider: ProviderHandle,
    pub plan: DispatchPlan,
    record: RouteRecord,
    pin: Option<Pin>,
}

fn failure(error: Error) -> Response {
    let status = match error {
        Error::Invalid(_) => StatusCode::BAD_REQUEST,
        Error::Conflict => StatusCode::CONFLICT,
        Error::NotFound => StatusCode::NOT_FOUND,
        _ => StatusCode::SERVICE_UNAVAILABLE,
    };
    err(status, &error.to_string())
}
fn now() -> i64 {
    time::OffsetDateTime::now_utc().unix_timestamp()
}

fn namespace(
    vk: &VirtualKey,
    alias: &str,
    request: &ChatRequestV1,
) -> Result<String, &'static str> {
    let session = request.metadata.session_id.as_deref();
    if session.is_some_and(|value| value.len() > 512) {
        return Err("routing session hint exceeds 512 bytes");
    }
    // Ordinary full-history agents without a hint have a stable first materialized
    // user turn. A continuation-only request needs an explicit/native stable hint.
    let first_user = request
        .messages
        .iter()
        .find(|message| matches!(message, ChatMessageV1::User { .. }));
    cache_digest(&(
        "sandhi-route-session-v1",
        &vk.id,
        &vk.subject_id,
        &vk.group_id,
        &vk.upstream_ref,
        alias.to_ascii_lowercase(),
        session,
        if session.is_none() { first_user } else { None },
    ))
    .map_err(|_| "routing scope cannot be encoded")
}

fn fingerprint(provider: &ProviderHandle, model: &str) -> Result<String, &'static str> {
    let binding = provider
        .backend_binding()
        .ok_or("route target is not an explicit backend")?;
    cache_digest(&(
        "sandhi-route-binding-v1",
        &binding.id,
        binding.revision,
        &binding.provider,
        &binding.credential_ref,
        (!provider.has_account_fenced_credential()).then_some(binding.credential_generation),
        binding.endpoint_family,
        &binding.endpoint_profile,
        model,
        &binding.cache_policy,
        provider
            .routing_credential_fingerprint()
            .ok_or("route requires a qualified credential binding")?,
    ))
    .map_err(|_| "routing binding cannot be encoded")
}

fn available(provider: &ProviderHandle, target: &Target) -> bool {
    provider.backend_binding().is_some_and(|binding| {
        binding.id == target.backend
            && binding.revision > 0
            && binding.credential_generation > 0
            && binding.endpoint_family == provider.family().endpoint_family()
            && binding.models.iter().any(|model| model.id == target.model)
    }) && provider.credential_is_current()
        && provider.routing_credential_fingerprint().is_some()
        && (provider.family() != sandhi_providers::ProviderFamily::Gemini
            || sandhi_core::model_id_is_path_safe(&target.model))
}

#[derive(Clone, Copy)]
pub(crate) enum TrackedDelivery {
    Buffered,
    Streaming,
}

pub(crate) async fn select(
    state: &ProxyState,
    vk: &VirtualKey,
    request: &mut ChatRequestV1,
    tracked: Option<TrackedDelivery>,
) -> Result<Selected, &'static str> {
    let store = state
        .routes
        .clone()
        .ok_or("logical route registry is not configured")?;
    let id = vk
        .upstream_ref
        .strip_prefix("route/")
        .ok_or("invalid logical route reference")?
        .to_owned();
    let requested = request.model.clone();
    let scope = namespace(vk, &requested, request)?;
    let clock = now();
    let permit = state
        .routing_slots
        .clone()
        .try_acquire_owned()
        .map_err(|_| "routing storage is busy")?;
    let scope_for_read = scope.clone();
    let (record, pin) = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let record = store
            .get(&id)?
            .filter(|record| record.status == "active")
            .ok_or(Error::NotFound)?;
        let pin = store.pin(&scope_for_read, &id, clock)?;
        Ok::<_, Error>((record, pin))
    })
    .await
    .map_err(|_| "routing storage is unavailable")?
    .map_err(|_| "logical route is unavailable")?;
    let model = record
        .definition
        .models
        .iter()
        .find(|model| model.id.eq_ignore_ascii_case(&requested))
        .ok_or("model is not declared for this logical route")?;
    let stateful = request.native_input.as_ref().is_some_and(|input| {
        input.nonportable_features.iter().any(|feature| {
            matches!(
                feature,
                NativeInputFeatureV1::OpaqueState | NativeInputFeatureV1::ResourceReference
            )
        })
    });
    if stateful && (record.definition.affinity == Affinity::None || pin.is_none()) {
        return Err("provider-bound state requires an established compatible routing session");
    }
    let handles = {
        let providers = state
            .providers
            .lock()
            .map_err(|_| "backend registry is unavailable")?;
        model
            .targets
            .iter()
            .filter_map(|target| {
                providers
                    .get(&format!("backend/{}", target.backend))
                    .cloned()
                    .map(|provider| (target.clone(), provider))
            })
            .collect::<Vec<_>>()
    };
    let mut eligible = Vec::new();
    for (target, provider) in handles {
        if !available(&provider, &target) {
            continue;
        }
        if request
            .native_input_for(provider.family().endpoint_family())
            .is_err()
        {
            continue;
        }
        request.model = target.model.clone();
        // Invoke the actual codec/profile, including compiled cache controls. A
        // declared family alone does not prove support for every request option.
        let compatible = match tracked {
            Some(TrackedDelivery::Buffered) => provider.preflight_owned_buffered(request).is_ok(),
            Some(TrackedDelivery::Streaming) => {
                state.streaming_deadlines.is_some()
                    && provider.supports_streaming_deadlines()
                    && provider.preflight_owned_streaming(request).is_ok()
            }
            None => provider.preflight_request(request).is_ok(),
        };
        request.model = requested.clone();
        if !compatible {
            continue;
        }
        let binding_hash = fingerprint(&provider, &target.model)?;
        eligible.push((target, provider, binding_hash));
    }
    let chosen = if let Some(pin) = pin
        .as_ref()
        .filter(|_| record.definition.affinity == Affinity::Session)
    {
        eligible.into_iter().find(|(target,_,hash)| *target==pin.target && *hash==pin.fingerprint)
            .ok_or("routing session binding changed or is unavailable; start a new materialized session")?
    } else {
        if record.definition.selection == Selection::Rendezvous {
            // Stable per session (or per input for explicitly stateless routes), independent
            // of candidate ordering. This is deterministic selection, not a replay policy.
            let seed = if record.definition.affinity == Affinity::Session {
                scope.clone()
            } else {
                request
                    .cache_input_sha256()
                    .map_err(|_| "routing input cannot be encoded")?
            };
            eligible.sort_by_cached_key(|(target, _, _)| {
                cache_digest(&(&seed, &target.backend, &target.model)).unwrap_or_default()
            });
            eligible.reverse();
        }
        eligible
            .into_iter()
            .next()
            .ok_or("no compatible backend is available for this logical model")?
    };
    let (target, provider, binding_sha256) = chosen;
    let binding = provider
        .backend_binding()
        .ok_or("route target binding is unavailable")?;
    let plan = DispatchPlan {
        route_id: record.definition.id.clone(),
        route_revision: record.revision,
        requested_model: requested,
        backend_id: binding.id.clone(),
        provider_label: binding.provider.clone(),
        backend_revision: binding.revision,
        actual_model: target.model.clone(),
        credential_generation: binding.credential_generation,
        credential_ref: binding.credential_ref.clone(),
        endpoint_family: binding.endpoint_family,
        endpoint_profile: binding.endpoint_profile.clone(),
        binding_sha256: binding_sha256.clone(),
    };
    let pin = if record.definition.affinity == Affinity::Session {
        Some(Pin {
            namespace: scope,
            target,
            fingerprint: binding_sha256,
            expires_at: pin.map_or(
                clock + i64::from(record.definition.affinity_ttl_seconds),
                |pin| pin.expires_at,
            ),
        })
    } else {
        None
    };
    Ok(Selected {
        provider,
        plan,
        record,
        pin,
    })
}

impl Selected {
    pub async fn confirm(&self, state: &ProxyState) -> Result<(), &'static str> {
        let store = state
            .routes
            .clone()
            .ok_or("logical route registry is not configured")?;
        let (record, pin) = (self.record.clone(), self.pin.clone());
        let permit = state
            .routing_slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| "routing storage is busy")?;
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            store.confirm(&record, pin.as_ref(), now())
        })
        .await
        .map_err(|_| "routing admission is unavailable")?
        .map_err(|_| "routing admission changed or is unavailable; reconcile before retry")
    }
}

pub(crate) fn validate_targets(
    state: &ProxyState,
    definition: &RouteDefinition,
) -> Result<(), &'static str> {
    definition
        .validate()
        .map_err(|_| "invalid logical route definition")?;
    let providers = state
        .providers
        .lock()
        .map_err(|_| "backend registry is unavailable")?;
    if providers.contains_key(&definition.route_ref()) {
        return Err("logical route cannot shadow a host-owned upstream");
    }
    for target in definition.models.iter().flat_map(|model| &model.targets) {
        let provider = providers
            .get(&format!("backend/{}", target.backend))
            .ok_or("route requires registered explicit backends")?;
        let binding = provider
            .backend_binding()
            .ok_or("route requires registered explicit backends")?;
        if binding.id != target.backend
            || !binding.models.iter().any(|model| model.id == target.model)
        {
            return Err("route target model must match an exact declared backend model");
        }
    }
    Ok(())
}

pub(crate) async fn discovery(
    state: &ProxyState,
    vk: &VirtualKey,
) -> Result<Vec<String>, &'static str> {
    let store = state
        .routes
        .clone()
        .ok_or("logical route registry is not configured")?;
    let id = vk
        .upstream_ref
        .strip_prefix("route/")
        .ok_or("invalid logical route")?
        .to_owned();
    let permit = state
        .routing_slots
        .clone()
        .try_acquire_owned()
        .map_err(|_| "routing storage is busy")?;
    let record = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        store.get(&id)
    })
    .await
    .map_err(|_| "routing storage is unavailable")?
    .map_err(|_| "routing storage is unavailable")?
    .filter(|record| record.status == "active")
    .ok_or("logical route is unavailable")?;
    let providers = state
        .providers
        .lock()
        .map_err(|_| "backend registry is unavailable")?;
    Ok(record
        .definition
        .models
        .iter()
        .filter(|model| vk.permits_model(&model.id))
        .filter(|model| {
            model.targets.iter().any(|target| {
                providers
                    .get(&format!("backend/{}", target.backend))
                    .is_some_and(|provider| available(provider, target))
            })
        })
        .map(|model| model.id.clone())
        .collect())
}

pub(crate) async fn active(state: &ProxyState, reference: &str) -> bool {
    let (Some(store), Some(id)) = (state.routes.clone(), reference.strip_prefix("route/")) else {
        return false;
    };
    let id = id.to_owned();
    let Ok(permit) = state.routing_slots.clone().try_acquire_owned() else {
        return false;
    };
    matches!(tokio::task::spawn_blocking(move ||{ let _permit=permit; store.get(&id) }).await,
        Ok(Ok(Some(record))) if record.status=="active")
}

#[allow(clippy::result_large_err)] // Preserve the existing rendered admin/dialect denial.
pub(crate) async fn preflight_config(
    state: &Arc<ProxyState>,
    config: &crate::config::SandhiFileConfig,
) -> Result<Vec<RouteRecord>, Response> {
    if config.routes.is_empty() {
        return Ok(Vec::new());
    }
    let (Some(store), Some(backends)) = (state.routes.clone(), state.backends.clone()) else {
        return Err(err(
            StatusCode::SERVICE_UNAVAILABLE,
            "route and backend registries must be configured",
        ));
    };
    let definitions = config.routes.clone();
    let planned = config.backends.clone();
    let occupied = state
        .providers
        .lock()
        .expect("providers poisoned")
        .keys()
        .cloned()
        .collect::<std::collections::HashSet<_>>();
    let permit = state
        .routing_slots
        .clone()
        .try_acquire_owned()
        .map_err(|_| err(StatusCode::SERVICE_UNAVAILABLE, "route registry busy"))?;
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let existing = store.list().map_err(failure)?;
        let mut available = backends
            .list()
            .map_err(|_| {
                err(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "backend registry unavailable",
                )
            })?
            .into_iter()
            .filter(|record| record.status == "active")
            .map(|record| record.definition)
            .collect::<Vec<_>>();
        for definition in planned {
            available.retain(|old| old.id != definition.id);
            available.push(definition);
        }
        let mut ids = std::collections::HashSet::new();
        for definition in &definitions {
            definition.validate().map_err(failure)?;
            if occupied.contains(&definition.route_ref()) {
                return Err(err(
                    StatusCode::BAD_REQUEST,
                    "logical route cannot shadow a host-owned upstream",
                ));
            }
            if !ids.insert(&definition.id) {
                return Err(err(StatusCode::BAD_REQUEST, "duplicate logical route id"));
            }
            for target in definition.models.iter().flat_map(|model| &model.targets) {
                if !available.iter().any(|backend| {
                    backend.id == target.backend
                        && backend.models.iter().any(|model| model.id == target.model)
                }) {
                    return Err(err(
                        StatusCode::BAD_REQUEST,
                        "route requires a declared exact backend/model target",
                    ));
                }
            }
        }
        if existing.len()
            + definitions
                .iter()
                .filter(|definition| {
                    !existing
                        .iter()
                        .any(|old| old.definition.id == definition.id)
                })
                .count()
            > sandhi_store::routes::MAX_ROUTES
        {
            return Err(err(
                StatusCode::BAD_REQUEST,
                "logical route capacity exceeded",
            ));
        }
        Ok(existing)
    })
    .await
    .unwrap_or_else(|_| {
        Err(err(
            StatusCode::SERVICE_UNAVAILABLE,
            "route preflight unavailable",
        ))
    })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Register {
    pub definition: RouteDefinition,
    pub expected_revision: Option<u64>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Revision {
    pub revision: u64,
}

pub(crate) async fn register(
    State(state): State<Arc<ProxyState>>,
    headers: HeaderMap,
    Json(request): Json<Register>,
) -> Response {
    if let Err(response) = require_access(&state, &headers, Permission::Admin, true).await {
        return response;
    }
    let operation = match crate::access::admit_mutation(&state.lifecycle) {
        Ok(value) => value,
        Err(response) => return response,
    };
    let Some(store) = state.routes.clone() else {
        return err(
            StatusCode::SERVICE_UNAVAILABLE,
            "route registry not configured",
        );
    };
    let Ok(permit) = state.vault_writer.clone().try_acquire_owned() else {
        return err(StatusCode::SERVICE_UNAVAILABLE, "route registry busy");
    };
    tokio::task::spawn_blocking(move || {
        let (_operation, _permit) = (operation, permit);
        if let Err(message) = validate_targets(&state, &request.definition) {
            return err(StatusCode::BAD_REQUEST, message);
        }
        match store.put(&request.definition, request.expected_revision) {
            Ok(record) => (StatusCode::CREATED, Json(record)).into_response(),
            Err(error) => failure(error),
        }
    })
    .await
    .unwrap_or_else(|_| {
        err(
            StatusCode::SERVICE_UNAVAILABLE,
            "route registration unavailable; reconcile before retry",
        )
    })
}
pub(crate) async fn list(State(state): State<Arc<ProxyState>>, headers: HeaderMap) -> Response {
    if let Err(response) = require_access(&state, &headers, Permission::Read, false).await {
        return response;
    }
    let Some(store) = state.routes.clone() else {
        return err(
            StatusCode::SERVICE_UNAVAILABLE,
            "route registry not configured",
        );
    };
    let Ok(permit) = state.routing_slots.clone().try_acquire_owned() else {
        return err(StatusCode::SERVICE_UNAVAILABLE, "route registry busy");
    };
    match tokio::task::spawn_blocking(move || {
        let _permit = permit;
        store.list()
    })
    .await
    {
        Ok(Ok(records)) => Json(json!({"routes":records})).into_response(),
        Ok(Err(error)) => failure(error),
        Err(_) => err(
            StatusCode::SERVICE_UNAVAILABLE,
            "route registry unavailable",
        ),
    }
}
pub(crate) async fn revoke(
    State(state): State<Arc<ProxyState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Query(rev): Query<Revision>,
) -> Response {
    if let Err(response) = require_access(&state, &headers, Permission::Admin, true).await {
        return response;
    }
    let operation = match crate::access::admit_mutation(&state.lifecycle) {
        Ok(value) => value,
        Err(response) => return response,
    };
    let Some(store) = state.routes.clone() else {
        return err(
            StatusCode::SERVICE_UNAVAILABLE,
            "route registry not configured",
        );
    };
    let Ok(permit) = state.vault_writer.clone().try_acquire_owned() else {
        return err(StatusCode::SERVICE_UNAVAILABLE, "route registry busy");
    };
    tokio::task::spawn_blocking(move || {
        let (_operation, _permit) = (operation, permit);
        match store.revoke(&id, rev.revision) {
            Ok(record) => Json(record).into_response(),
            Err(error) => failure(error),
        }
    })
    .await
    .unwrap_or_else(|_| {
        err(
            StatusCode::SERVICE_UNAVAILABLE,
            "route revocation unavailable; reconcile before retry",
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http;
    use sandhi_providers::{BackendBinding, ProviderRuntime};

    fn lease(account: &str, token: &str, generation: u64) -> ProviderHandle {
        let mut headers = http::HeaderMap::new();
        headers.insert(
            "chatgpt-account-id",
            http::HeaderValue::from_str(account).unwrap(),
        );
        ProviderRuntime::new()
            .chatgpt_responses(
                "openai",
                "https://chatgpt.com/backend-api/codex",
                token,
                headers,
                Some(0),
                None,
                None,
                None,
            )
            .with_credential_expiry(u64::MAX)
            .with_backend_binding(BackendBinding {
                id: "codex".into(),
                revision: 1,
                provider: "openai".into(),
                credential_ref: "openai:lease".into(),
                credential_generation: generation,
                endpoint_family: sandhi_core::EndpointFamilyV1::OpenaiResponses,
                endpoint_profile: "chatgpt_codex".into(),
                models: vec![serde_json::from_value(json!({"id":"agent-model"})).unwrap()],
                cache_policy: None,
            })
    }

    #[test]
    fn constrained_same_account_renewal_keeps_affinity_but_account_switch_does_not() {
        let original = lease("account-a", "access-first", 1);
        let renewed = lease("account-a", "access-renewed", 2);
        let switched = lease("account-b", "access-other-account", 2);
        assert!(original.has_account_fenced_credential());
        assert_eq!(
            fingerprint(&original, "agent-model").unwrap(),
            fingerprint(&renewed, "agent-model").unwrap()
        );
        assert_ne!(
            fingerprint(&original, "agent-model").unwrap(),
            fingerprint(&switched, "agent-model").unwrap()
        );
        assert!(!renewed.with_credential_expiry(1).credential_is_current());
        // A metadata label cannot confer the actual constrained transport capability.
        let generic = ProviderRuntime::new()
            .openai_responses(
                "openai",
                "https://models.example/v1",
                "api-key",
                http::HeaderMap::new(),
                None,
                None,
                None,
                None,
            )
            .with_backend_binding(original.backend_binding().unwrap().clone());
        assert!(!generic.has_account_fenced_credential());
        assert_ne!(
            fingerprint(&original, "agent-model").unwrap(),
            fingerprint(&generic, "agent-model").unwrap()
        );
    }
}
