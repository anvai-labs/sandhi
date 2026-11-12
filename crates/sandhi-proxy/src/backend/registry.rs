//! Registry lifecycle. Definitions and secret references have independent owners.
use super::{resolve, BackendDefinition, BackendProfile};
use crate::{
    access::{admit_mutation, err},
    auth::Permission,
    operator::{require_access, vault_failure},
    ProxyState,
};
use axum::{
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use sandhi_providers::{BackendBinding, ProviderFamily, ProviderHandle, ProviderRuntime};
use sandhi_store::{
    BackendRecord, BackendStore, BackendStoreError, VaultEntry, VaultError, VaultStore,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{collections::HashMap, sync::Arc};

pub(crate) fn validate_definition(
    definition: &BackendDefinition,
    entry: &VaultEntry,
) -> Result<String, &'static str> {
    definition.validate()?;
    if entry.status != "active" || entry.credential_id() != definition.credential_ref {
        return Err("backend requires an active exact credential reference");
    }
    resolve(
        &definition.provider,
        entry.scheme,
        Some(definition.endpoint),
    )
    .ok_or("unsupported backend endpoint/profile/credential combination")?;
    let credential_base = entry.base_url.clone().unwrap_or_else(|| {
        if definition.endpoint.endpoint_profile == BackendProfile::ChatGptCodex {
            "https://chatgpt.com/backend-api/codex".into()
        } else {
            sandhi_providers::resolve_openai_compat_provider(&entry.provider)
                .map(|spec| spec.base_url.to_string())
                .unwrap_or_else(|| {
                    ProviderFamily::for_slug(&entry.provider)
                        .default_base_url()
                        .into()
                })
        }
    });
    let target = definition.base_url.as_deref().unwrap_or(&credential_base);
    let parse = |value: &str| {
        let url = reqwest::Url::parse(value).map_err(|_| "invalid backend base URL")?;
        if !matches!(url.scheme(), "http" | "https")
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err("backend base URL must be HTTP(S) without user info, query or fragment");
        }
        Ok(url)
    };
    if parse(target)?.origin() != parse(&credential_base)?.origin() {
        return Err("backend origin must match the credential's declared origin");
    }
    Ok(target.to_owned())
}

fn build_unbound(
    definition: &BackendDefinition,
    entry: &VaultEntry,
    secret: &str,
    runtime: &ProviderRuntime,
) -> Result<ProviderHandle, &'static str> {
    let base = validate_definition(definition, entry)?;
    crate::build_provider_handle_for_endpoint(
        runtime,
        &definition.provider,
        Some(&base),
        secret,
        entry.scheme,
        definition
            .user_agent
            .as_deref()
            .or(entry.user_agent.as_deref()),
        definition.endpoint,
    )
    .ok_or("invalid or expired backend credential/profile configuration")
}

fn bind(handle: ProviderHandle, record: &BackendRecord, entry: &VaultEntry) -> ProviderHandle {
    let definition = &record.definition;
    handle.with_backend_binding(BackendBinding {
        id: definition.id.clone(),
        revision: record.revision,
        provider: definition.provider.clone(),
        credential_ref: definition.credential_ref.clone(),
        credential_generation: entry.generation,
        endpoint_family: definition.endpoint.endpoint_family,
        endpoint_profile: match definition.endpoint.endpoint_profile {
            BackendProfile::Standard => "standard",
            BackendProfile::ChatGptCodex => "chatgpt_codex",
            BackendProfile::AnthropicImplicitCache => "anthropic_implicit_cache",
            BackendProfile::OpenaiTerminalChunkUsage => "openai_terminal_chunk_usage",
        }
        .into(),
        models: definition.models.clone(),
        cache_policy: definition.cache_policy.clone(),
    })
}

/// Reconstruct exact configured routes. Missing/locked/expired credentials stay
/// dormant; metadata read failures are errors, not an empty successful inventory.
pub fn rehydrate_backends(
    vault: &VaultStore,
    store: &BackendStore,
    runtime: &ProviderRuntime,
    providers: &mut HashMap<String, ProviderHandle>,
) -> Result<(), BackendStoreError> {
    providers.retain(|_, provider| provider.backend_binding().is_none());
    let records = store.list()?;
    let entries = vault.list().map_err(|_| BackendStoreError::Corrupt)?;
    let mut secrets = HashMap::new();
    for record in records
        .into_iter()
        .filter(|record| record.status == "active")
    {
        // Host-owned handles keep their authority if they occupy a registry name.
        if providers.contains_key(&record.definition.route_ref()) {
            continue;
        }
        let Some(entry) = entries.iter().find(|entry| {
            entry.status == "active" && entry.credential_id() == record.definition.credential_ref
        }) else {
            continue;
        };
        let secret = secrets
            .entry(entry.credential_id())
            .or_insert_with(|| vault.get(&entry.provider, &entry.label).ok().flatten());
        if let Some(handle) = secret
            .as_deref()
            .and_then(|secret| build_unbound(&record.definition, entry, secret, runtime).ok())
        {
            providers.insert(record.definition.route_ref(), bind(handle, &record, entry));
        }
    }
    Ok(())
}

/// Publish a credential and its dependents under one registry lock. An unreadable
/// backend index disables dependents instead of retaining the old secret.
pub(crate) fn publish_credential(
    state: &ProxyState,
    reference: &str,
    direct: Option<ProviderHandle>,
    entry: Option<&VaultEntry>,
    secret: Option<&str>,
) -> Result<(), BackendStoreError> {
    let rebuilt = (|| {
        let mut handles = Vec::new();
        if let (Some(store), Some(entry), Some(secret)) = (&state.backends, entry, secret) {
            for record in store.list()?.into_iter().filter(|record| {
                record.status == "active" && record.definition.credential_ref == reference
            }) {
                if let Ok(handle) = build_unbound(&record.definition, entry, secret, &state.runtime)
                {
                    handles.push((record.definition.route_ref(), bind(handle, &record, entry)));
                }
            }
        }
        Ok::<_, BackendStoreError>(handles)
    })();
    let mut providers = state.providers.lock().expect("providers poisoned");
    providers.retain(|id, handle| {
        id != reference
            && handle
                .backend_binding()
                .is_none_or(|binding| binding.credential_ref != reference)
    });
    if let Some(handle) = direct {
        providers.insert(reference.into(), handle);
    }
    match rebuilt {
        Ok(handles) => {
            for (route, handle) in handles {
                providers.entry(route).or_insert(handle);
            }
            Ok(())
        }
        Err(error) => Err(error),
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RegisterBackendRequest {
    pub definition: BackendDefinition,
    pub expected_revision: Option<u64>,
}

/// Validate every backend's non-secret intent before config apply writes any
/// credential. Return the revision snapshot used by subsequent conditional puts.
#[allow(clippy::result_large_err)] // Preserve the existing dialect/admin denial and reconciliation fields.
pub(crate) async fn preflight_config(
    state: &Arc<ProxyState>,
    config: &crate::config::SandhiFileConfig,
) -> Result<Vec<BackendRecord>, Response> {
    if config.backends.is_empty() {
        return Ok(Vec::new());
    }
    if config.backends.len() > sandhi_store::backends::MAX_BACKENDS {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "backend registry capacity exceeded",
        ));
    }
    let (Some(store), Some(vault)) = (state.backends.clone(), state.vault.clone()) else {
        return Err(err(
            StatusCode::SERVICE_UNAVAILABLE,
            "backend registry and vault must be configured",
        ));
    };
    let definitions = config.backends.clone();
    let declared = config.providers.clone();
    tokio::task::spawn_blocking(move || {
        let existing = store.list().map_err(store_error)?;
        let entries = vault.list().map_err(|error| vault_failure(error, false))?;
        let mut ids = std::collections::HashSet::new();
        for definition in &definitions {
            definition
                .validate()
                .map_err(|message| err(StatusCode::BAD_REQUEST, message))?;
            if !ids.insert(&definition.id) {
                return Err(err(StatusCode::BAD_REQUEST, "duplicate backend id"));
            }
            let planned: Vec<_> = declared
                .iter()
                .filter(|provider| {
                    format!(
                        "{}:{}",
                        provider.provider,
                        provider.label.as_deref().unwrap_or("default")
                    ) == definition.credential_ref
                })
                .collect();
            if planned.len() > 1 {
                return Err(err(
                    StatusCode::BAD_REQUEST,
                    "duplicate backend credential intent",
                ));
            }
            let planned_entry = planned
                .first()
                .map(|provider| {
                    Ok::<_, Response>(VaultEntry {
                        provider: provider.provider.clone(),
                        label: provider.label.clone().unwrap_or_else(|| "default".into()),
                        scheme: crate::operator::parse_scheme(provider.scheme.as_deref())
                            .map_err(|message| err(StatusCode::BAD_REQUEST, message))?,
                        base_url: provider.base_url.clone(),
                        user_agent: provider.user_agent.clone(),
                        created_at: String::new(),
                        generation: 0, // Preflight only; never a published binding.
                        status: "active".into(),
                    })
                })
                .transpose()?;
            let entry = planned_entry
                .as_ref()
                .or_else(|| {
                    entries.iter().find(|entry| {
                        entry.status == "active"
                            && entry.credential_id() == definition.credential_ref
                    })
                })
                .ok_or_else(|| {
                    err(
                        StatusCode::BAD_REQUEST,
                        "backend requires a declared or active exact credential reference",
                    )
                })?;
            validate_definition(definition, entry)
                .map_err(|message| err(StatusCode::BAD_REQUEST, message))?;
        }
        Ok(existing)
    })
    .await
    .unwrap_or_else(|_| {
        Err(err(
            StatusCode::SERVICE_UNAVAILABLE,
            "backend configuration validation unavailable",
        ))
    })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct BackendRevision {
    pub revision: u64,
}

fn store_error(error: BackendStoreError) -> Response {
    let status = match error {
        BackendStoreError::Invalid(_) => StatusCode::BAD_REQUEST,
        BackendStoreError::Conflict => StatusCode::CONFLICT,
        BackendStoreError::NotFound => StatusCode::NOT_FOUND,
        _ => StatusCode::SERVICE_UNAVAILABLE,
    };
    (status, Json(json!({"error":error.to_string(),"reconcile_before_retry":status == StatusCode::SERVICE_UNAVAILABLE}))).into_response()
}

fn record_response(record: &BackendRecord, state: &ProxyState) -> Value {
    let mut value = serde_json::to_value(record).expect("nonsecret backend metadata");
    let route = record.definition.route_ref();
    value["upstream_ref"] = json!(route);
    value["ready"] = json!(state
        .providers
        .lock()
        .expect("providers poisoned")
        .get(&route)
        .and_then(ProviderHandle::backend_binding)
        .is_some_and(|binding| binding.id == record.definition.id
            && binding.revision == record.revision
            && record.status == "active"));
    value
}

pub(crate) async fn register(
    State(state): State<Arc<ProxyState>>,
    headers: HeaderMap,
    Json(request): Json<RegisterBackendRequest>,
) -> Response {
    if let Err(response) = require_access(&state, &headers, Permission::Admin, true).await {
        return response;
    }
    let operation = match admit_mutation(&state.lifecycle) {
        Ok(operation) => operation,
        Err(response) => return response,
    };
    if let Err(message) = request.definition.validate() {
        return err(StatusCode::BAD_REQUEST, message);
    }
    let (Some(store), Some(vault)) = (state.backends.clone(), state.vault.clone()) else {
        return err(
            StatusCode::SERVICE_UNAVAILABLE,
            "backend registry and vault must be configured",
        );
    };
    let Ok(permit) = state.vault_writer.clone().try_acquire_owned() else {
        return vault_failure(VaultError::Busy, false);
    };
    tokio::task::spawn_blocking(move || {
        let (_operation, _permit) = (operation, permit);
        if state
            .providers
            .lock()
            .expect("providers poisoned")
            .get(&request.definition.route_ref())
            .is_some_and(|handle| handle.backend_binding().is_none())
        {
            return err(StatusCode::CONFLICT, "backend route is owned by the host");
        }
        let entries = match vault.list() {
            Ok(entries) => entries,
            Err(error) => return vault_failure(error, false),
        };
        let Some(entry) = entries.iter().find(|entry| {
            entry.status == "active" && entry.credential_id() == request.definition.credential_ref
        }) else {
            return err(
                StatusCode::BAD_REQUEST,
                "backend requires an active exact credential reference",
            );
        };
        if let Err(message) = validate_definition(&request.definition, entry) {
            return err(StatusCode::BAD_REQUEST, message);
        }
        let secret = match vault.get(&entry.provider, &entry.label) {
            Ok(Some(secret)) => secret,
            Ok(None) => return err(StatusCode::NOT_FOUND, "backend credential unavailable"),
            Err(error) => return vault_failure(error, false),
        };
        let handle = match build_unbound(&request.definition, entry, &secret, &state.runtime) {
            Ok(handle) => handle,
            Err(message) => return err(StatusCode::BAD_REQUEST, message),
        };
        let record = match store.put(&request.definition, request.expected_revision) {
            Ok(record) => record,
            Err(error) => return store_error(error),
        };
        state
            .providers
            .lock()
            .expect("providers poisoned")
            .insert(record.definition.route_ref(), bind(handle, &record, entry));
        (StatusCode::CREATED, Json(record_response(&record, &state))).into_response()
    })
    .await
    .unwrap_or_else(|_| {
        err(
            StatusCode::SERVICE_UNAVAILABLE,
            "backend publication failed; reconcile before retry",
        )
    })
}

pub(crate) async fn list(State(state): State<Arc<ProxyState>>, headers: HeaderMap) -> Response {
    if let Err(response) = require_access(&state, &headers, Permission::Read, false).await {
        return response;
    }
    let Some(store) = state.backends.clone() else {
        return err(
            StatusCode::SERVICE_UNAVAILABLE,
            "backend registry not configured",
        );
    };
    match tokio::task::spawn_blocking(move || store.list()).await {
        Ok(Ok(records)) => Json(json!({"backends":records.iter().map(|record|record_response(record,&state)).collect::<Vec<_>>()})).into_response(),
        Ok(Err(error)) => store_error(error),
        Err(_) => err(StatusCode::SERVICE_UNAVAILABLE, "backend registry unavailable"),
    }
}

pub(crate) async fn revoke(
    State(state): State<Arc<ProxyState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Query(revision): Query<BackendRevision>,
) -> Response {
    if let Err(response) = require_access(&state, &headers, Permission::Admin, true).await {
        return response;
    }
    let operation = match admit_mutation(&state.lifecycle) {
        Ok(operation) => operation,
        Err(response) => return response,
    };
    let Some(store) = state.backends.clone() else {
        return err(
            StatusCode::SERVICE_UNAVAILABLE,
            "backend registry not configured",
        );
    };
    let Ok(permit) = state.vault_writer.clone().try_acquire_owned() else {
        return vault_failure(VaultError::Busy, false);
    };
    tokio::task::spawn_blocking(move || {
        let (_operation, _permit) = (operation, permit);
        let record = match store.revoke(&id, revision.revision) {
            Ok(record) => record,
            Err(error) => return store_error(error),
        };
        let mut providers = state.providers.lock().expect("providers poisoned");
        let route = record.definition.route_ref();
        if providers
            .get(&route)
            .and_then(ProviderHandle::backend_binding)
            .is_some_and(|binding| binding.id == record.definition.id)
        {
            providers.remove(&route);
        }
        drop(providers);
        Json(record_response(&record, &state)).into_response()
    })
    .await
    .unwrap_or_else(|_| {
        err(
            StatusCode::SERVICE_UNAVAILABLE,
            "backend revocation failed; reconcile before retry",
        )
    })
}
