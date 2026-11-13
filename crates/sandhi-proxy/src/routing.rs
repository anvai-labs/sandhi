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
    established: bool,
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
    Counting,
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
            Some(TrackedDelivery::Counting) => true,
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
    let established = pin.is_some();
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
        established,
    })
}

impl Selected {
    pub(crate) fn require_stateful_session(&self) -> Result<(), &'static str> {
        if self.record.definition.affinity != Affinity::Session || !self.established {
            return Err("provider-bound state requires an established compatible routing session");
        }
        Ok(())
    }
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

    #[tokio::test]
    async fn legacy_sql_pin_and_cache_binding_survive_namespace_startup_migration() {
        use sandhi_core::ChatMessageV1;
        use sandhi_store::{
            routes::{RouteDefinition, RouteStore},
            BackendDefinition, BackendStore, CredentialScheme, VaultStore,
        };
        for (family, old_label) in [
            (
                sandhi_core::EndpointFamilyV1::OpenaiChatCompletions,
                "fixture",
            ),
            (
                sandhi_core::EndpointFamilyV1::AnthropicMessages,
                "anthropic",
            ),
            (sandhi_core::EndpointFamilyV1::OpenaiResponses, "fixture"),
            (
                sandhi_core::EndpointFamilyV1::GeminiGenerateContent,
                "gemini",
            ),
        ] {
            let directory = tempfile::tempdir().unwrap();
            let db = directory.path().join("legacy.db");
            let vault = VaultStore::in_memory().unwrap();
            vault
                .set(
                    "fixture",
                    "default",
                    CredentialScheme::ApiKey,
                    Some("http://127.0.0.1:1"),
                    None,
                    "synthetic",
                )
                .unwrap();
            let mut definition:BackendDefinition=serde_json::from_value(json!({"id":"primary","provider":"fixture","credential_ref":"fixture:default","endpoint":{"endpoint_family":family},"models":[{"id":"physical"}]})).unwrap();
            if matches!(
                family,
                sandhi_core::EndpointFamilyV1::OpenaiChatCompletions
                    | sandhi_core::EndpointFamilyV1::OpenaiResponses
            ) {
                definition.cache_policy=Some(serde_json::from_value(json!({"namespace":"scoped_key","models":{"physical":{"profile":if family==sandhi_core::EndpointFamilyV1::OpenaiChatCompletions {"openai_legacy"}else{"openai_explicit"},"lifetimes":if family==sandhi_core::EndpointFamilyV1::OpenaiChatCompletions {json!(["in_memory","24h"])}else{json!(["30m","12h"])},"max_breakpoints":if family==sandhi_core::EndpointFamilyV1::OpenaiResponses {json!(4)}else{serde_json::Value::Null}}}})).unwrap());
            }
            let legacy = rusqlite::Connection::open(&db).unwrap();
            legacy.execute_batch("CREATE TABLE backends(id TEXT PRIMARY KEY,definition_json TEXT NOT NULL,revision INTEGER NOT NULL,status TEXT NOT NULL,created_at TEXT NOT NULL,updated_at TEXT NOT NULL);").unwrap();
            legacy
                .execute(
                    "INSERT INTO backends VALUES('primary',?1,1,'active','old','old')",
                    [serde_json::to_string(&definition).unwrap()],
                )
                .unwrap();
            let old_handle = crate::build_provider_handle_for_endpoint(
                &ProviderRuntime::new(),
                "fixture",
                Some("http://127.0.0.1:1"),
                "synthetic",
                CredentialScheme::ApiKey,
                None,
                definition.endpoint,
            )
            .unwrap()
            .with_backend_binding(BackendBinding {
                id: "primary".into(),
                revision: 1,
                provider: "fixture".into(),
                credential_ref: "fixture:default".into(),
                credential_generation: 1,
                endpoint_family: family,
                endpoint_profile: "standard".into(),
                models: definition.models.clone(),
                cache_policy: definition.cache_policy.clone(),
            });
            let b = old_handle.backend_binding().unwrap();
            // Exact pre-migration v1 formulas, independent of the migrated implementation.
            let legacy_fingerprint = cache_digest(&(
                "sandhi-route-binding-v1",
                &b.id,
                b.revision,
                &b.provider,
                &b.credential_ref,
                Some(b.credential_generation),
                b.endpoint_family,
                &b.endpoint_profile,
                "physical",
                &b.cache_policy,
                old_handle.routing_credential_fingerprint().unwrap(),
            ))
            .unwrap();
            let legacy_cache_binding = cache_digest(&(
                family,
                old_label,
                old_handle.routing_credential_fingerprint(),
                Some((
                    &b.id,
                    b.revision,
                    &b.credential_ref,
                    b.credential_generation,
                    &b.endpoint_profile,
                    &b.cache_policy,
                )),
            ))
            .unwrap();
            let vk = VirtualKey {
                id: "legacy-key".into(),
                upstream_ref: "route/coding".into(),
                models: Some(vec!["agent".into()]),
                ..Default::default()
            };
            let request:ChatRequestV1=serde_json::from_value(json!({"model":"agent","max_output_tokens":16,"messages":[{"role":"user","content":"hello"}],"cache_intent":{"mode":"preserve_native"},"metadata":{"virtual_key_id":"legacy-key","session_id":"legacy-session"}})).unwrap();
            let scope = cache_digest(&(
                "sandhi-route-session-v1",
                &vk.id,
                &vk.subject_id,
                &vk.group_id,
                &vk.upstream_ref,
                "agent",
                Some("legacy-session"),
                Option::<&ChatMessageV1>::None,
            ))
            .unwrap();
            let routes = RouteStore::open(db.to_str().unwrap()).unwrap();
            let route:RouteDefinition=serde_json::from_value(json!({"id":"coding","models":[{"id":"agent","targets":[{"backend":"primary","model":"physical"}]}]})).unwrap();
            routes.put(&route, None).unwrap();
            let expires = now() + 3600;
            legacy
                .execute(
                    "INSERT INTO route_affinity VALUES(?1,'coding','primary','physical',?2,?3)",
                    rusqlite::params![scope, legacy_fingerprint, expires],
                )
                .unwrap();
            drop(legacy);
            drop(routes);
            let store = BackendStore::open(db.to_str().unwrap()).unwrap();
            let record = store.get("primary").unwrap().unwrap();
            assert_eq!(
                record.upstream_budget_scope,
                format!("upstream:{old_label}")
            );
            let mut providers = std::collections::HashMap::new();
            crate::backend::rehydrate_backends(
                &vault,
                &store,
                &ProviderRuntime::new(),
                &mut providers,
            )
            .unwrap();
            let migrated = &providers["backend/primary"];
            assert_eq!(
                fingerprint(migrated, "physical").unwrap(),
                legacy_fingerprint
            );
            let mut cache_request = request.clone();
            cache_request.model = "physical".into();
            let plan = migrated
                .compile_cache_plan(&cache_request)
                .unwrap()
                .unwrap();
            assert_eq!(plan.binding_sha256, legacy_cache_binding);
            if definition.cache_policy.is_some() {
                let expected_key = format!(
                    "sandhi:{}",
                    cache_digest(&(
                        "cache-namespace-v2",
                        old_handle.routing_credential_fingerprint(),
                        "legacy-key",
                        &cache_request.metadata.subject_id,
                        &cache_request.metadata.group_id,
                        &b.id,
                        b.revision,
                        &b.credential_ref,
                        b.credential_generation,
                        "physical",
                        &b.endpoint_profile,
                        &b.cache_policy,
                        "legacy-session"
                    ))
                    .unwrap()
                );
                assert_eq!(plan.scoped_key, Some(expected_key));
            }

            let mut state = ProxyState::new(
                sandhi_core::KeyStore::new(),
                crate::ProxyLedger::in_memory(),
                std::sync::Arc::new(sandhi_core::InMemorySink::new()),
                providers,
                None,
            );
            state.routes = Some(std::sync::Arc::new(
                RouteStore::open(db.to_str().unwrap()).unwrap(),
            ));
            let mut continuation = request.clone();
            let selected = select(&state, &vk, &mut continuation, None).await.unwrap();
            assert_eq!(selected.plan.backend_id, "primary");
            assert_eq!(
                state
                    .routes
                    .as_ref()
                    .unwrap()
                    .pin(&scope, "coding", now())
                    .unwrap()
                    .unwrap()
                    .fingerprint,
                legacy_fingerprint
            );
            let mut changed = definition.clone();
            changed.provider = "new-label".into();
            store.put(&changed, Some(1)).unwrap();
            crate::backend::rehydrate_backends(
                &vault,
                &store,
                &ProviderRuntime::new(),
                &mut state.providers.lock().unwrap(),
            )
            .unwrap();
            let mut continuation = request;
            assert!(select(&state, &vk, &mut continuation, None).await.is_err());
            // Store binding remains immutable even though genuine definition revisions fence old pins.
            assert_eq!(
                store.get("primary").unwrap().unwrap().upstream_budget_scope,
                format!("upstream:{old_label}")
            );
        }
    }

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
