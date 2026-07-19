//! Sandhi reverse-proxy — the **in-path (inline) egress gate** (AnvaiOps ADR-0047 D8).
//!
//! A client points its `base_url` at Sandhi and presents a **virtual key** (never the real
//! upstream key). The gate resolves the key → subject/group + which upstream, budget-checks,
//! forwards to the provider (holding the real key server-side), streams the response back
//! **verbatim** (O(1) pass-through, ADR-0047 D9), then emits one neutral usage event and records
//! the budget. It is *in-path*, not a redirect: a client cannot bypass the meter.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Json, Response};
use axum::routing::{get, post};
use axum::Router;
use futures_util::StreamExt;
use serde_json::{json, Value};

use sandhi_core::{Backend, BudgetLedger, KeyStore, Sink, UsageEvent, VirtualKey};
use sandhi_providers::{ParsedUsage, Provider, ProviderError, ProviderRequest};

/// Shared server state: the virtual-key store, the budget ledger, the usage sink, and the
/// registry of configured upstream providers (each already holding its real credential).
pub struct ProxyState {
    pub keys: KeyStore,
    pub ledger: Mutex<BudgetLedger>,
    pub sink: Arc<dyn Sink>,
    /// `upstream_ref` → a ready provider (real key baked in).
    pub providers: HashMap<String, Arc<dyn Provider>>,
}

/// Build the axum app. Ingress paths mirror the provider wire formats (OpenAI + Anthropic);
/// the presented virtual key selects the actual upstream.
pub fn build_app(state: Arc<ProxyState>) -> Router {
    Router::new()
        .route("/healthz", get(health))
        .route("/v1/chat/completions", post(handle))
        .route("/v1/messages", post(handle))
        .with_state(state)
}

/// Bind and serve until shutdown.
pub async fn serve(state: Arc<ProxyState>, addr: SocketAddr) -> std::io::Result<()> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, build_app(state)).await
}

async fn health() -> &'static str {
    "ok"
}

async fn handle(State(state): State<Arc<ProxyState>>, headers: HeaderMap, body: Bytes) -> Response {
    // 1. Virtual key from `Authorization: Bearer vk_…`.
    let Some(vk_id) = bearer(&headers) else {
        return error(StatusCode::UNAUTHORIZED, "missing bearer virtual key");
    };
    let Some(vk) = state.keys.resolve(vk_id).cloned() else {
        return error(StatusCode::UNAUTHORIZED, "unknown virtual key");
    };

    // 2. The upstream this key is bound to.
    let Some(provider) = state.providers.get(&vk.upstream_ref).cloned() else {
        return error(
            StatusCode::BAD_GATEWAY,
            "no upstream registered for this key",
        );
    };

    // 3. Parse the request body.
    let Ok(body_json) = serde_json::from_slice::<Value>(&body) else {
        return error(StatusCode::BAD_REQUEST, "body is not valid JSON");
    };
    let model = body_json
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_string();
    let wants_stream = body_json
        .get("stream")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let session = headers
        .get("x-sandhi-session")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);

    // 4. Budget pre-check. We don't know exact tokens yet, so probe with 1: this blocks a
    //    scope that is already at or over its cap (spent + 1 > limit ⇔ spent >= limit). The
    //    real usage is recorded after the call.
    let scope = budget_scope(&vk);
    if let Ok(ledger) = state.ledger.lock() {
        if ledger.check(&scope, 1).is_err() {
            return error(StatusCode::TOO_MANY_REQUESTS, "budget exhausted");
        }
    }

    let req = ProviderRequest::new(model.clone(), body_json).with_session(session.clone());

    if wants_stream {
        stream_response(state, provider, vk, model, session, scope, req).await
    } else {
        complete_response(&state, provider.as_ref(), &vk, &model, session, &scope, req).await
    }
}

async fn complete_response(
    state: &Arc<ProxyState>,
    provider: &dyn Provider,
    vk: &VirtualKey,
    model: &str,
    session: Option<String>,
    scope: &str,
    req: ProviderRequest,
) -> Response {
    match provider.complete(req).await {
        Ok(resp) => {
            let event = build_event(vk, provider.slug(), model, session, resp.usage);
            emit_and_record(state, scope, &event);
            let status = StatusCode::from_u16(resp.status).unwrap_or(StatusCode::OK);
            (status, Json(resp.body)).into_response()
        }
        Err(e) => provider_error(&e),
    }
}

async fn stream_response(
    state: Arc<ProxyState>,
    provider: Arc<dyn Provider>,
    vk: VirtualKey,
    model: String,
    session: Option<String>,
    scope: String,
    req: ProviderRequest,
) -> Response {
    let mut upstream = match provider.stream(req).await {
        Ok(s) => s,
        Err(e) => return provider_error(&e),
    };
    let slug = provider.slug().to_string();

    let body = async_stream::stream! {
        let mut final_usage = ParsedUsage::default();
        while let Some(item) = upstream.next().await {
            match item {
                Ok(chunk) => {
                    if let Some(u) = chunk.usage {
                        final_usage = u;
                    }
                    if !chunk.data.is_empty() {
                        yield Ok::<Bytes, std::io::Error>(chunk.data);
                    }
                }
                // Upstream stream error: stop forwarding; whatever usage we saw is still metered.
                Err(_) => break,
            }
        }
        // Best-effort emit + record once the stream completes (ADR-0047 D7 — off the hot path).
        let event = build_event(&vk, &slug, &model, session, final_usage);
        emit_and_record(&state, &scope, &event);
    };

    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "text/event-stream")
        .body(Body::from_stream(body))
        .expect("valid streaming response")
}

fn emit_and_record(state: &Arc<ProxyState>, scope: &str, event: &UsageEvent) {
    state.sink.emit(event);
    if let Ok(mut ledger) = state.ledger.lock() {
        ledger.record(scope, event.billable_tokens());
    }
}

fn build_event(
    vk: &VirtualKey,
    provider: &str,
    model: &str,
    session: Option<String>,
    usage: ParsedUsage,
) -> UsageEvent {
    let base = UsageEvent::new(
        next_request_id(),
        now_rfc3339(),
        provider,
        model,
        Backend::External,
    )
    .with_attribution(
        Some(vk.id.clone()),
        vk.subject_id.clone(),
        vk.group_id.clone(),
    )
    .with_session(session);
    usage.apply(base)
}

fn budget_scope(vk: &VirtualKey) -> String {
    match &vk.group_id {
        Some(g) => format!("group:{g}"),
        None => format!("vk:{}", vk.id),
    }
}

fn bearer(headers: &HeaderMap) -> Option<&str> {
    headers
        .get("authorization")?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
        .map(str::trim)
}

fn provider_error(e: &ProviderError) -> Response {
    let (status, msg) = match e {
        ProviderError::Auth => (StatusCode::BAD_GATEWAY, "upstream auth failed"),
        ProviderError::RateLimited => (StatusCode::TOO_MANY_REQUESTS, "upstream rate limited"),
        ProviderError::Upstream(_) => (StatusCode::BAD_GATEWAY, "upstream error"),
        ProviderError::Transport(_) => (StatusCode::BAD_GATEWAY, "upstream transport error"),
        ProviderError::CircuitOpen => (
            StatusCode::SERVICE_UNAVAILABLE,
            "circuit open (upstream failing)",
        ),
    };
    error(status, msg)
}

fn error(status: StatusCode, msg: &str) -> Response {
    (status, Json(json!({ "error": msg }))).into_response()
}

fn now_rfc3339() -> String {
    use time::format_description::well_known::Rfc3339;
    time::OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_default()
}

fn next_request_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    format!("req_{millis}_{n}")
}
