//! Freeze non-secret execution selection at the existing tracked-admission boundary.
use crate::{codec::IngressDialect, routing::DispatchPlan, ProxyState};
use sandhi_core::{cache_digest, ChatRequestV1};
use sandhi_providers::ProviderHandle;
use sandhi_store::ledger::evidence::context::{
    AccountingPolicy, BackendContext, CacheContext, Delivery, ExecutionContext, Plane, RouteContext,
};

pub(crate) struct Selection {
    pub upstream_ref: String,
    pub routing: Option<DispatchPlan>,
}
pub(super) struct Surface {
    pub dialect: IngressDialect,
    pub streaming: bool,
    pub transparent: bool,
}
impl Selection {
    pub(super) fn freeze(
        &self,
        state: &ProxyState,
        provider: &ProviderHandle,
        request: &ChatRequestV1,
        surface: Surface,
        upstream_scope: &str,
    ) -> Result<ExecutionContext, &'static str> {
        let binding = provider.backend_binding();
        if let Some(plan) = &self.routing {
            if !binding.is_some_and(|b| {
                b.id == plan.backend_id
                    && b.revision == plan.backend_revision
                    && b.credential_ref == plan.credential_ref
                    && b.credential_generation == plan.credential_generation
                    && b.provider == plan.provider_label
                    && b.endpoint_family == plan.endpoint_family
                    && b.endpoint_profile == plan.endpoint_profile
            }) || request.model != plan.actual_model
                || provider.family().endpoint_family() != plan.endpoint_family
            {
                return Err("execution selection does not match the admitted backend");
            }
        }
        let credential = provider
            .routing_credential_fingerprint()
            .ok_or("execution transport binding unavailable")?;
        let transport_binding_sha256 = cache_digest(&(
            "sandhi-execution-transport-v1",
            &self.upstream_ref,
            provider.family().endpoint_family(),
            &request.model,
            credential,
            binding.map(|b| {
                (
                    &b.id,
                    b.revision,
                    &b.credential_ref,
                    b.credential_generation,
                    &b.endpoint_profile,
                )
            }),
        ))
        .map_err(|_| "execution binding cannot be encoded")?;
        let cache = request
            .compiled_cache
            .as_ref()
            .map(|plan| {
                Ok::<_, &'static str>(CacheContext {
                    plan_sha256: cache_digest(plan).map_err(|_| "cache plan cannot be encoded")?,
                    binding_sha256: plan.binding_sha256.clone(),
                    intent: plan.intent.clone(),
                    capability: plan.capability,
                    native_preserved: plan.native_preserved,
                })
            })
            .transpose()?;
        let value = ExecutionContext {
            version: 1,
            ingress: crate::ingress_family(surface.dialect).endpoint_family(),
            delivery: if surface.streaming {
                Delivery::Streaming
            } else {
                Delivery::Buffered
            },
            plane: if surface.transparent {
                Plane::Transparent
            } else {
                Plane::Translation
            },
            upstream_ref: self.upstream_ref.clone(),
            upstream_budget_scope: upstream_scope.into(),
            requested_model: self.routing.as_ref().map_or_else(
                || request.model.clone(),
                |plan| plan.requested_model.clone(),
            ),
            actual_model: request.model.clone(),
            provider: binding.map_or_else(|| provider.slug().into(), |b| b.provider.clone()),
            endpoint_family: provider.family().endpoint_family(),
            transport_binding_sha256,
            route: self.routing.as_ref().map(|plan| RouteContext {
                id: plan.route_id.clone(),
                revision: plan.route_revision,
                binding_sha256: plan.binding_sha256.clone(),
            }),
            backend: binding.map(|b| BackendContext {
                id: b.id.clone(),
                revision: b.revision,
                credential_ref: b.credential_ref.clone(),
                credential_generation: b.credential_generation,
                endpoint_profile: b.endpoint_profile.clone(),
            }),
            cache,
            accounting_policy: AccountingPolicy {
                version: 1,
                cache_read_weight: sandhi_core::allowance::Weight::new(state.cache_read_weight_bp)
                    .map_err(|_| "invalid admitted accounting policy")?,
            },
            content_policy_revision: state.policy.as_ref().map(|policy| policy.revision()),
        };
        value.validate().map_err(|_| "invalid execution context")?;
        Ok(value)
    }
}
