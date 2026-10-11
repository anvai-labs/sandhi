//! Request preparation after canonical credential/upstream resolution.
//!
//! The context exposes validation capabilities without ledger, vault or lifecycle
//! authority. The resolved provider passes through without transport execution;
//! callers must still reserve and authorize dispatch after preparation succeeds.
use crate::{
    codec::{self, decode_request, IngressDialect},
    deadlines::{BufferedDeadlines, EffectiveDeadline, StreamLimits, StreamingDeadlines},
    dialect_label, header_str, ingress_error, ingress_family,
    metrics::{self, Metrics},
    policy::PolicyGate,
    rate_limited_error,
    ratelimit::{self, RateLimiter},
    streaming::StreamBodyLifetime,
    GeminiRoute, ProxyState,
};
use axum::{
    body::Bytes,
    http::{HeaderMap, StatusCode},
    response::Response,
};
use sandhi_core::{derive_session_id_scoped, ChatRequestV1, RequestMetadataV1, VirtualKey};
use sandhi_providers::{ProviderFamily, ProviderHandle};
use serde_json::Value;
use std::sync::Mutex;

pub(super) struct PreparationContext<'a> {
    buffered_deadlines: Option<&'a BufferedDeadlines>,
    streaming_deadlines: Option<&'a StreamingDeadlines>,
    stream_body_lifetime: Option<StreamBodyLifetime>,
    rate_limiter: &'a RateLimiter,
    metrics: &'a Metrics,
    policy: Option<&'a PolicyGate>,
}
impl<'a> From<&'a ProxyState> for PreparationContext<'a> {
    fn from(state: &'a ProxyState) -> Self {
        Self {
            buffered_deadlines: state.buffered_deadlines.as_ref(),
            streaming_deadlines: state.streaming_deadlines.as_ref(),
            stream_body_lifetime: state.stream_body_lifetime,
            rate_limiter: &state.rate_limiter,
            metrics: &state.metrics,
            policy: state.policy.as_deref(),
        }
    }
}

pub(super) struct ResolvedRoute {
    pub vk: VirtualKey,
    pub provider: ProviderHandle,
    pub rate_identity: String,
    pub policy_identity: sandhi_core::policy::Identity,
}

pub(super) struct PreparedRequest {
    pub vk: VirtualKey,
    pub provider: ProviderHandle,
    pub request: ChatRequestV1,
    pub body: Bytes,
    pub wants_stream: bool,
    pub raw_text_route: bool,
    pub transparent_eligible: bool,
    pub buffered_deadline: Option<EffectiveDeadline>,
    pub streaming_deadline: Option<StreamLimits>,
    pub body_lifetime: Option<StreamBodyLifetime>,
}

// Preserve the already-rendered dialect response, including policy headers.
#[allow(clippy::result_large_err)]
pub(super) async fn prepare(
    context: PreparationContext<'_>,
    route: ResolvedRoute,
    headers: &HeaderMap,
    mut body: Bytes,
    dialect: IngressDialect,
    gemini_route: Option<&GeminiRoute>,
    policy_receipt: &Mutex<Option<String>>,
) -> Result<PreparedRequest, Response> {
    let ResolvedRoute {
        vk,
        provider,
        rate_identity,
        policy_identity,
    } = route;
    // 3. Decode the public ingress dialect into the one canonical runtime request.
    let raw_text_route = matches!(
        dialect,
        IngressDialect::Embeddings | IngressDialect::Completions
    );
    if raw_text_route
        && (provider.family() != ProviderFamily::OpenAiCompat || provider.raw_forwarder().is_none())
    {
        return Err(ingress_error(
            dialect,
            StatusCode::BAD_REQUEST,
            "text endpoints require a compatible raw upstream",
        ));
    }
    let parsed = if raw_text_route {
        sandhi_core::policy::strict_json(&body).map_err(|_| ())
    } else {
        serde_json::from_slice::<Value>(&body).map_err(|_| ())
    };
    let Ok(mut body_json) = parsed else {
        return Err(ingress_error(
            dialect,
            StatusCode::BAD_REQUEST,
            "body is not valid JSON",
        ));
    };
    if dialect == IngressDialect::Completions && body_json.get("max_tokens").is_none() {
        if let Some(object) = body_json.as_object_mut() {
            object.insert("max_tokens".into(), serde_json::json!(16));
            body = Bytes::from(serde_json::to_vec(&body_json).expect("parsed JSON"));
        }
    }
    // ADR-0005 D7 + ADR-0008 D3: session identity is single-sourced in core. An explicit
    // `x-sandhi-session` header wins; otherwise derive from the wire body's standard signals
    // (OpenAI `user`, Anthropic `metadata.user_id`, then a stable hash of the cacheable
    // system+tools prefix). A drop-in SDK client gets a stable per-conversation key — for
    // usage grouping AND for the vendor affinity header — without setting any sandhi-specific
    // header. Self-reported inputs only; never an identity assertion (the vkey binding is).
    let session = derive_session_id_scoped(
        headers
            .get("x-sandhi-session")
            .and_then(|v| v.to_str().ok()),
        &body_json,
        Some(&vk.id),
    );
    let route = match dialect {
        IngressDialect::OpenAi => "/v1/chat/completions",
        IngressDialect::Embeddings => "/v1/embeddings",
        IngressDialect::Completions => "/v1/completions",
        IngressDialect::Anthropic => "/v1/messages",
        IngressDialect::Responses => "/v1/responses",
        IngressDialect::Gemini => "/v1beta/models/:generateContent",
    };
    let metadata = RequestMetadataV1 {
        session_id: session,
        virtual_key_id: Some(vk.id.clone()),
        subject_id: vk.subject_id.clone(),
        group_id: vk.group_id.clone(),
        route: Some(route.into()),
        // ADR-0005 D7 neutral identity: `idempotency-key` for reconcile-once, run/step/parent for
        // the agent cost-tree, W3C `traceparent` for external trace linkage. Optional metadata —
        // never pricing, never inside the cached wire body.
        idempotency_key: header_str(headers, "idempotency-key"),
        run_id: header_str(headers, "x-sandhi-run-id"),
        step_id: header_str(headers, "x-sandhi-step-id"),
        parent_id: header_str(headers, "x-sandhi-parent-id"),
        trace_context: header_str(headers, "traceparent"),
    };
    let (mut request, mut wants_stream) = match decode_request(dialect, body_json, metadata) {
        Ok(decoded) => decoded,
        Err(message) => return Err(ingress_error(dialect, StatusCode::BAD_REQUEST, &message)),
    };

    // Gemini carries the model and the streaming choice in the PATH, so stamp them onto the
    // decoded request before anything downstream reads them — the allowlist check and the
    // reservation both do.
    if let Some(route) = gemini_route {
        request.model = route.model.clone();
        wants_stream = route.stream;
    }

    // D4a refused a cross-family Gemini request because its decode was accounting-grade and
    // re-encoding from it would silently drop tools, inline media and safety settings. D4b
    // replaced that with a faithful codec (the mirror of the adapter's encoder, pinned by a
    // round-trip test), so the refusal is gone: a Gemini client may now resolve to any upstream.

    // 4. Model allowlist (TD-0003 P4): if the resolved key carries a non-empty `models[]`, admit
    //    only a model on that list. Empty/absent allowlist = any model (unchanged). Enforced after
    //    vk auth + decode (so the request model is known) and before the budget reservation, so the
    //    ordering is vk auth → allowlist → budget → dispatch (a disallowed model never reserves).
    if !vk.permits_model(&request.model) {
        let allowed = vk.models.as_deref().unwrap_or(&[]);
        return Err(ingress_error(
            dialect,
            StatusCode::FORBIDDEN,
            &format!(
                "model '{}' is not permitted for this virtual key (allowed models: {})",
                request.model,
                allowed.join(", ")
            ),
        ));
    }

    // 4b. Attribution is key-authoritative (ADR-0004 D4): the usage event always carries the
    //     resolved key's subject/group (step 3 above), and `x-sandhi-subject-id` /
    //     `x-sandhi-group-id` are admitted only as an idempotent echo of that binding. A
    //     mismatch — or any value on a field the key does not bind — is a fail-loud 403:
    //     silently ignoring it would hide data loss from a client that expects the
    //     attribution, and adopting it would let any key holder pollute another
    //     subject/group's aggregates (and, once a group-keyed cache namespace ships per
    //     ADR-0001 §4, cross prompt-cache namespaces). Checked before the rate limit and the
    //     reservation, so a spoof holds no lease and emits no usage.
    let presented_subject = header_str(headers, "x-sandhi-subject-id");
    let presented_group = header_str(headers, "x-sandhi-group-id");
    if !vk.permits_attribution(presented_subject.as_deref(), presented_group.as_deref()) {
        return Err(ingress_error(
            dialect,
            StatusCode::FORBIDDEN,
            "attribution is key-bound: x-sandhi-subject-id / x-sandhi-group-id must match this \
             virtual key's subject/group (or be omitted); re-mint the key to change attribution",
        ));
    }

    let buffered_deadline = if wants_stream {
        None
    } else {
        context
            .buffered_deadlines
            .as_ref()
            .map(|policy| policy.resolve(&vk.upstream_ref, &request.model))
    };
    if buffered_deadline.is_some() && provider.raw_forwarder().is_none() {
        return Err(ingress_error(
            dialect,
            StatusCode::BAD_GATEWAY,
            "configured buffered deadlines require a built-in transport",
        ));
    }

    let streaming_deadline = if wants_stream {
        context
            .streaming_deadlines
            .as_ref()
            .map(|policy| policy.resolve(&vk.upstream_ref, &request.model).0)
    } else {
        None
    };
    let body_lifetime = streaming_deadline
        .map(|policy| policy.body_lifetime())
        .or(context.stream_body_lifetime);
    if wants_stream && body_lifetime.is_some() && provider.raw_forwarder().is_none() {
        return Err(ingress_error(
            dialect,
            StatusCode::BAD_GATEWAY,
            "configured stream body lifetime requires a built-in transport",
        ));
    }

    // Select the nominal plane before rate-limit and policy checks.
    // Plane selection (ADR-0004 D1 / TD-0006): when the client's ingress dialect and the resolved
    // upstream are the SAME family, forward the client's bytes verbatim (transparent metering) —
    // no `ChatRequestV1` re-encode, so prompt-cache prefixes and provider-specific fields survive,
    // and usage is metered at the source. Cross-family (or a handle with no raw forwarder) falls
    // back to the typed translation path.
    //
    // Decided here, before enforcement, so the plane is a metric dimension for every outcome —
    // including the calls that get refused below and never reach a provider.
    let transparent_eligible =
        ingress_family(dialect) == provider.family() && provider.raw_forwarder().is_some();
    // Preliminary plane label for the rate-limit metric (a throttled call never reaches a provider,
    // so its plane is nominal). The handler recomputes the final plane once it knows whether the cap
    // forces the translation plane.
    let plane = if transparent_eligible {
        metrics::Plane::Transparent
    } else {
        metrics::Plane::Translation
    };

    // TD-0012 D5: rate limit AFTER the allowlist and BEFORE the budget reservation. It is the
    // cheap check (an in-memory bucket) and the reservation is the expensive one (a durable
    // write), and — more importantly — a throttled request must consume no lease, record no
    // spend, and emit no usage event. It never reached a provider.
    if let ratelimit::Decision::Limited { retry_after_secs } = context
        .rate_limiter
        .check(&rate_identity, vk.rate_limit_per_min)
    {
        tracing::warn!(
            provider = provider.slug(),
            limit_per_min = vk.rate_limit_per_min,
            retry_after_secs,
            "request rate-limited"
        );
        context.metrics.record_rate_limited(&metrics::Labels {
            provider: provider.slug().into(),
            model: request.model.clone(),
            dialect: dialect_label(dialect),
            plane,
            outcome: "rate_limited",
        });
        return Err(rate_limited_error(dialect, retry_after_secs));
    }

    if let Some(gate) = context.policy {
        let admission = gate
            .check_input(
                body.clone(),
                policy_identity,
                vk.id.clone(),
                vk.upstream_ref.clone(),
                request.model.clone(),
                match dialect {
                    IngressDialect::OpenAi => Some(sandhi_core::policy::InputFormat::Chat),
                    IngressDialect::Embeddings => {
                        Some(sandhi_core::policy::InputFormat::Embeddings)
                    }
                    IngressDialect::Completions => {
                        Some(sandhi_core::policy::InputFormat::Completions)
                    }
                    _ => None,
                },
            )
            .await;
        if let Ok(ref admitted) = admission {
            *policy_receipt.lock().expect("policy receipt poisoned") =
                Some(admitted.receipt.clone());
        }
        match admission {
            Ok(admission)
                if admission.decision.disposition == sandhi_core::policy::Disposition::Forward => {}
            result => {
                let (code, status, receipt) = match result {
                    Ok(a) => {
                        let (code, status) = match a.decision.disposition {
                            sandhi_core::policy::Disposition::Block => {
                                ("policy_blocked", StatusCode::FORBIDDEN)
                            }
                            sandhi_core::policy::Disposition::Quarantine => {
                                ("policy_quarantined", StatusCode::FORBIDDEN)
                            }
                            _ => ("policy_unavailable", StatusCode::SERVICE_UNAVAILABLE),
                        };
                        (code, status, Some(a.receipt))
                    }
                    Err(_) => ("policy_unavailable", StatusCode::SERVICE_UNAVAILABLE, None),
                };
                let mut response =
                    codec::IngressError::policy(status, code, receipt.as_deref()).render(dialect);
                response
                    .headers_mut()
                    .insert("x-sandhi-policy-code", code.parse().expect("static code"));
                response
                    .headers_mut()
                    .insert("cache-control", "no-store".parse().unwrap());
                if let Some(receipt) = receipt {
                    response.headers_mut().insert(
                        "x-sandhi-policy-receipt",
                        receipt.parse().expect("generated receipt"),
                    );
                }
                return Err(response);
            }
        }
    }

    Ok(PreparedRequest {
        vk,
        provider,
        request,
        body,
        wants_stream,
        raw_text_route,
        transparent_eligible,
        buffered_deadline,
        streaming_deadline,
        body_lifetime,
    })
}
