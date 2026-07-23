//! Sandhi reverse-proxy — the **in-path (inline) egress gate** (AnvaiOps ADR-0047 D8).
//!
//! A client points its `base_url` at Sandhi and presents a **virtual key** (never the real
//! upstream key). The gate resolves the key → subject/group + which upstream, budget-checks,
//! normalizes the request through Sandhi's typed runtime, then emits one neutral usage event and
//! reconciles the budget. It is *in-path*, not a redirect: a client cannot bypass the meter.

mod codec;
pub mod operator;

// Re-export the admin API request/response types for the `sandhi` CLI client + the startup
// rehydration helpers used by the `sandhi-proxy` binary.
pub use operator::{admin, build_provider_handle, rehydrate_alerts, rehydrate_live_keys};

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Json, Response};
use axum::routing::{delete, get, post};
use axum::Router;
use futures_util::StreamExt;
use serde_json::{json, Value};

use sandhi_core::{
    AlertRegistry, Backend, BudgetLedger, ChatRequestV1, KeyStore, RequestMetadataV1, Sink,
    UsageCompleteness, UsageEvent, UsageV2, VirtualKey,
};
use sandhi_providers::{ProviderError, ProviderHandle, ProviderRuntime};
use sandhi_store::{hash_secret, AlertStore, SqliteStore, VaultStore, VirtualKeyStore};

use codec::{decode_request, encode_response, encode_stream_event, IngressDialect};
pub use operator::BudgetSpec;

/// Shared server state: the virtual-key store, the budget ledger, the usage sink, and the
/// registry of configured upstream providers (each already holding its real credential).
pub struct ProxyState {
    pub keys: KeyStore,
    pub ledger: Mutex<BudgetLedger>,
    pub sink: Arc<dyn Sink>,
    /// `upstream_ref` → a persistent typed provider handle (real key baked in). Interior-mutable:
    /// the admin API registers handles here at runtime; the demo path seeds it at startup.
    pub providers: Mutex<HashMap<String, ProviderHandle>>,
    /// The durable store backing the dashboard. When set, `/dashboard` serves usage aggregates;
    /// typically the same object is also used as `sink` so events persist.
    pub store: Option<Arc<SqliteStore>>,

    // --- TD-0003 P1 operator surface ---
    /// Durable provider-credential vault (metadata in SQLite, secret in the active backend).
    pub vault: Option<Arc<VaultStore>>,
    /// Durable virtual-key store (hashes + scope), rehydrates `keys` on startup.
    pub vkeys: Option<Arc<VirtualKeyStore>>,
    /// Builds typed upstream handles from vault-resolved credentials.
    pub runtime: ProviderRuntime,
    /// Admin-API bearer token (distinct from virtual keys). `None` disables the admin API.
    pub admin_token: Option<String>,
    /// Operator-set budgets (scope → spec). The live [`BudgetLedger`] enforces them.
    pub budgets: Mutex<HashMap<String, BudgetSpec>>,
    /// The externally-reachable base URL shared with minted-key callers (e.g.
    /// `http://localhost:8787`).
    pub public_url: String,
    // --- TD-0003 P2 budget depth + alerts ---
    /// Live alert-rule registry + dedup (the evaluation engine). `None` when alerts are off.
    pub alerts: Option<Arc<Mutex<AlertRegistry>>>,
    /// Durable alert-rule store (rules + last_fired_at + ack), backs `/admin/alerts`.
    pub alert_store: Option<Arc<AlertStore>>,
}

impl ProxyState {
    /// Build a state with the operator surface defaulted off (no vault, no admin token). The
    /// existing demo + request-handling path is unchanged.
    #[must_use]
    pub fn new(
        keys: KeyStore,
        ledger: BudgetLedger,
        sink: Arc<dyn Sink>,
        providers: HashMap<String, ProviderHandle>,
        store: Option<Arc<SqliteStore>>,
    ) -> Self {
        Self {
            keys,
            ledger: Mutex::new(ledger),
            sink,
            providers: Mutex::new(providers),
            store,
            vault: None,
            vkeys: None,
            runtime: ProviderRuntime::new(),
            admin_token: None,
            budgets: Mutex::new(HashMap::new()),
            public_url: "http://localhost:8787".into(),
            alerts: None,
            alert_store: None,
        }
    }
}

/// Build the axum app. Ingress paths mirror the provider wire formats (OpenAI Chat Completions,
/// OpenAI Responses, Anthropic Messages); the presented virtual key selects the actual upstream.
/// The `/admin/*` routes are the TD-0003 operator surface (authed by an admin token).
pub fn build_app(state: Arc<ProxyState>) -> Router {
    Router::new()
        .route("/healthz", get(health))
        .route("/dashboard", get(dashboard_html))
        .route("/dashboard/api/usage", get(dashboard_api))
        .route("/v1/chat/completions", post(handle_openai))
        .route("/v1/messages", post(handle_anthropic))
        .route("/v1/responses", post(handle_responses))
        // TD-0003 P1 operator (admin) API.
        .route(
            "/admin/keys",
            post(operator::add_key).get(operator::list_keys),
        )
        .route("/admin/keys/share", post(operator::share_key))
        .route("/admin/keys/virtual", get(operator::list_virtual_keys))
        .route("/admin/keys/:provider/:label", delete(operator::revoke_key))
        .route("/admin/vkeys/:id", delete(operator::revoke_virtual_key))
        .route(
            "/admin/budget",
            post(operator::set_budget).get(operator::list_budgets),
        )
        .route("/admin/budget/usage", get(operator::budget_usage))
        .route("/admin/usage", get(operator::usage))
        // TD-0003 P2 alert rules.
        .route(
            "/admin/alerts",
            post(operator::create_alert).get(operator::list_alerts),
        )
        .route("/admin/alerts/:id/ack", post(operator::ack_alert))
        .route("/admin/alerts/:id", delete(operator::delete_alert))
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

/// Usage aggregates for the dashboard (JSON). 404 when no durable store is configured.
async fn dashboard_api(State(state): State<Arc<ProxyState>>) -> Response {
    let Some(store) = state.store.clone() else {
        return error(
            StatusCode::NOT_FOUND,
            "dashboard not configured (set SANDHI_STORE)",
        );
    };
    let payload = json!({
        "total": store.grand_total().ok(),
        "by_subject": store.totals_by_subject().unwrap_or_default(),
        "by_group": store.totals_by_group().unwrap_or_default(),
        "by_provider": store.totals_by_provider().unwrap_or_default(),
    });
    Json(payload).into_response()
}

/// The self-hosted single-node dashboard (static HTML; fetches `/dashboard/api/usage`).
async fn dashboard_html() -> Response {
    axum::response::Html(DASHBOARD_HTML).into_response()
}

const DASHBOARD_HTML: &str = r####"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1">
<title>Sandhi — usage</title>
<style>
  :root { color-scheme: light dark; }
  body { font: 14px/1.5 ui-sans-serif, system-ui, sans-serif; margin: 0; padding: 2rem;
         max-width: 900px; margin-inline: auto; }
  h1 { font-size: 1.4rem; margin: 0 0 .25rem; }
  .sub { color: #6b7280; margin-bottom: 1.5rem; }
  .cards { display: flex; gap: 1rem; flex-wrap: wrap; margin-bottom: 1.5rem; }
  .card { border: 1px solid #8883; border-radius: 10px; padding: 1rem 1.25rem; min-width: 8rem; }
  .card .n { font-size: 1.6rem; font-weight: 700; }
  .card .l { color: #6b7280; font-size: .8rem; text-transform: uppercase; letter-spacing: .04em; }
  h2 { font-size: 1rem; margin: 1.5rem 0 .5rem; }
  table { width: 100%; border-collapse: collapse; }
  th, td { text-align: left; padding: .4rem .5rem; border-bottom: 1px solid #8882; }
  th { color: #6b7280; font-weight: 600; font-size: .8rem; }
  td.num, th.num { text-align: right; font-variant-numeric: tabular-nums; }
  .amber { color: #b45309; }
</style>
</head>
<body>
<h1>Sandhi <span class="amber">— the metering layer for AI agents</span></h1>
<div class="sub">Self-hosted usage dashboard · neutral units (no pricing) · <a href="/dashboard/api/usage">JSON</a></div>
<div class="cards" id="cards"></div>
<div id="tables"></div>
<script>
const fmt = n => (n ?? 0).toLocaleString();
function tbl(title, rows) {
  const body = rows.map(r => `<tr><td>${r.key}</td><td class="num">${fmt(r.calls)}</td>`
    + `<td class="num">${fmt(r.tokens_in)}</td><td class="num">${fmt(r.tokens_out)}</td>`
    + `<td class="num">${fmt(r.cache_read_tokens)}</td></tr>`).join("");
  return `<h2>${title}</h2><table><thead><tr><th>key</th><th class="num">calls</th>`
    + `<th class="num">in</th><th class="num">out</th><th class="num">cache read</th></tr></thead>`
    + `<tbody>${body || '<tr><td colspan=5>no data yet</td></tr>'}</tbody></table>`;
}
fetch("/dashboard/api/usage").then(r => r.json()).then(d => {
  const t = d.total || { calls: 0, tokens_in: 0, tokens_out: 0, cache_read_tokens: 0 };
  document.getElementById("cards").innerHTML =
    [["calls", t.calls], ["tokens in", t.tokens_in], ["tokens out", t.tokens_out],
     ["cache read", t.cache_read_tokens]]
    .map(([l, n]) => `<div class="card"><div class="n">${fmt(n)}</div><div class="l">${l}</div></div>`).join("");
  document.getElementById("tables").innerHTML =
    tbl("By user (subject)", d.by_subject || []) + tbl("By team (group)", d.by_group || [])
    + tbl("By provider", d.by_provider || []);
}).catch(e => { document.getElementById("tables").textContent = "failed to load: " + e; });
</script>
</body>
</html>
"####;

async fn handle_openai(
    State(state): State<Arc<ProxyState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    handle(state, headers, body, IngressDialect::OpenAi).await
}

async fn handle_anthropic(
    State(state): State<Arc<ProxyState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    handle(state, headers, body, IngressDialect::Anthropic).await
}

async fn handle_responses(
    State(state): State<Arc<ProxyState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    handle(state, headers, body, IngressDialect::Responses).await
}

async fn handle(
    state: Arc<ProxyState>,
    headers: HeaderMap,
    body: Bytes,
    dialect: IngressDialect,
) -> Response {
    // 1. Virtual key from `Authorization: Bearer vk_…`. Resolve the live key store by exact token
    //    (legacy/demo path, where the id doubles as the token) then by its hash (operator-minted
    //    path, where only the hash is the lookup key — the plaintext is never retained).
    let Some(vk_token) = bearer(&headers) else {
        return error(StatusCode::UNAUTHORIZED, "missing bearer virtual key");
    };
    let vk = match resolve_virtual_key(&state, vk_token) {
        VirtualKeyResolution::Found(vk) => vk,
        VirtualKeyResolution::Expired => {
            return error(StatusCode::UNAUTHORIZED, "virtual key expired");
        }
        VirtualKeyResolution::NotFound => {
            return error(StatusCode::UNAUTHORIZED, "unknown virtual key");
        }
    };

    // 2. The upstream this key is bound to.
    let Some(provider) = state
        .providers
        .lock()
        .expect("providers poisoned")
        .get(&vk.upstream_ref)
        .cloned()
    else {
        return error(
            StatusCode::BAD_GATEWAY,
            "no upstream registered for this key",
        );
    };

    // 3. Decode the public ingress dialect into the one canonical runtime request.
    let Ok(body_json) = serde_json::from_slice::<Value>(&body) else {
        return ingress_error(dialect, StatusCode::BAD_REQUEST, "body is not valid JSON");
    };
    let session = headers
        .get("x-sandhi-session")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let route = match dialect {
        IngressDialect::OpenAi => "/v1/chat/completions",
        IngressDialect::Anthropic => "/v1/messages",
        IngressDialect::Responses => "/v1/responses",
    };
    let metadata = RequestMetadataV1 {
        session_id: session,
        virtual_key_id: Some(vk.id.clone()),
        subject_id: vk.subject_id.clone(),
        group_id: vk.group_id.clone(),
        route: Some(route.into()),
    };
    let (request, wants_stream) = match decode_request(dialect, body_json, metadata) {
        Ok(decoded) => decoded,
        Err(message) => return ingress_error(dialect, StatusCode::BAD_REQUEST, &message),
    };

    // 4. Atomically reserve the request's conservative token estimate. The measured UsageV2
    //    replaces this reservation after completion; failed/unmeasured calls release it.
    let scope = budget_scope(&vk);
    let reserved = estimate_reservation(&request);
    match reserve_budget(&state, &scope, reserved) {
        Ok(()) => {}
        Err(StatusCode::TOO_MANY_REQUESTS) => {
            return ingress_error(dialect, StatusCode::TOO_MANY_REQUESTS, "budget exhausted")
        }
        Err(status) => {
            return ingress_error(dialect, status, "budget ledger unavailable");
        }
    }

    let accounting = RequestAccounting::new(
        Arc::clone(&state),
        scope,
        reserved,
        provider.slug().into(),
        &request,
    );

    if wants_stream {
        stream_response(provider, request, dialect, accounting).await
    } else {
        complete_response(provider, request, dialect, accounting).await
    }
}

fn reserve_budget(state: &ProxyState, scope: &str, reserved: u64) -> Result<(), StatusCode> {
    let mut ledger = state
        .ledger
        .lock()
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    ledger
        .reserve(scope, reserved)
        .map_err(|_| StatusCode::TOO_MANY_REQUESTS)
}

/// Owns the reservation and guarantees one terminal usage observation even when an HTTP body is
/// abandoned. Counts are always measured; an unavailable observation releases the reservation.
struct RequestAccounting {
    state: Arc<ProxyState>,
    scope: String,
    reserved: u64,
    provider: String,
    model: String,
    metadata: RequestMetadataV1,
    usage: Option<UsageV2>,
    outcome: &'static str,
    finalized: bool,
}

impl RequestAccounting {
    fn new(
        state: Arc<ProxyState>,
        scope: String,
        reserved: u64,
        provider: String,
        request: &ChatRequestV1,
    ) -> Self {
        Self {
            state,
            scope,
            reserved,
            provider,
            model: request.model.clone(),
            metadata: request.metadata.clone(),
            usage: None,
            outcome: "cancelled",
            finalized: false,
        }
    }

    fn observe(&mut self, usage: &UsageV2) {
        self.usage = Some(usage.clone());
    }

    fn set_outcome(&mut self, outcome: &'static str) {
        self.outcome = outcome;
    }

    /// Evaluate threshold alerts against the reconciled spend. Best-effort: any failure (registry
    /// poisoned, store unavailable) is logged and dropped — never propagated to the caller.
    fn fire_alerts(&self, spent: u64, limit: Option<u64>) {
        let Some(registry) = &self.state.alerts else {
            return;
        };
        let fired = match registry.lock() {
            Ok(mut reg) => reg.evaluate(&self.scope, spent, limit),
            Err(_) => return,
        };
        if let Some(store) = &self.state.alert_store {
            for alert in &fired {
                let _ = store.mark_fired(&alert.rule_id);
            }
        }
    }

    fn finalize(&mut self) {
        if self.finalized {
            return;
        }
        self.finalized = true;
        let mut usage = self.usage.take().unwrap_or_default();
        if usage.outcome.is_none() {
            usage.outcome = Some(self.outcome.into());
        }
        let measured = matches!(
            usage.completeness,
            UsageCompleteness::Final | UsageCompleteness::Partial
        );
        let actual = if measured {
            usage.tokens_in.saturating_add(usage.tokens_out)
        } else {
            0
        };
        // Reconcile the reservation against the measured terminal usage, and capture the
        // post-reconcile spent + limit so the alert subsystem can evaluate thresholds.
        let mut alert_input: Option<(u64, Option<u64>)> = None;
        if let Ok(mut ledger) = self.state.ledger.lock() {
            if measured {
                ledger.reconcile(&self.scope, self.reserved, actual);
                let spent = ledger.spent(&self.scope);
                let limit = ledger.limit_of(&self.scope);
                alert_input = Some((spent, limit));
            } else {
                ledger.release(&self.scope, self.reserved);
            }
        }
        // P2: evaluate threshold alerts against the reconciled spend (best-effort — never breaks
        // the request). Persists last_fired_at for restart-surviving dedup.
        if let Some((spent, limit)) = alert_input {
            self.fire_alerts(spent, limit);
        }
        self.state.sink.emit(&usage_event(
            &self.provider,
            &self.model,
            &self.metadata,
            &usage,
        ));
    }
}

impl Drop for RequestAccounting {
    fn drop(&mut self) {
        self.finalize();
    }
}

async fn complete_response(
    provider: ProviderHandle,
    request: ChatRequestV1,
    dialect: IngressDialect,
    mut accounting: RequestAccounting,
) -> Response {
    match provider.complete(request).await {
        Ok(mut response) => {
            response.usage.completeness = UsageCompleteness::Final;
            response
                .usage
                .outcome
                .get_or_insert_with(|| "success".into());
            accounting.observe(&response.usage);
            accounting.set_outcome("success");
            accounting.finalize();
            Json(encode_response(dialect, &response)).into_response()
        }
        Err(error) => {
            accounting.set_outcome("error");
            accounting.finalize();
            provider_error(&error, dialect, provider.slug())
        }
    }
}

async fn stream_response(
    provider: ProviderHandle,
    request: ChatRequestV1,
    dialect: IngressDialect,
    mut accounting: RequestAccounting,
) -> Response {
    let mut upstream = match provider.stream(request).await {
        Ok(s) => s,
        Err(error) => {
            accounting.set_outcome("error");
            accounting.finalize();
            return provider_error(&error, dialect, provider.slug());
        }
    };

    let body = async_stream::stream! {
        let mut last_usage: Option<UsageV2> = None;
        while let Some(item) = upstream.next().await {
            match item {
                Ok(event) => {
                    if let sandhi_core::ChatStreamEventV1::Usage { usage } = &event {
                        accounting.observe(usage);
                        last_usage = Some(usage.clone());
                    }
                    if matches!(event, sandhi_core::ChatStreamEventV1::Error { .. }) {
                        accounting.set_outcome("error");
                    }
                    for (event_name, value) in
                        encode_stream_event(dialect, &event, last_usage.as_ref())
                    {
                        yield Ok::<Bytes, std::io::Error>(Bytes::from(sse_frame(event_name, &value)));
                    }
                }
                Err(error) => {
                    accounting.set_outcome("error");
                    let typed = sandhi_core::ChatStreamEventV1::Error {
                        error: error.as_typed(Some(provider.slug())),
                    };
                    for (event_name, value) in
                        encode_stream_event(dialect, &typed, last_usage.as_ref())
                    {
                        yield Ok::<Bytes, std::io::Error>(Bytes::from(sse_frame(event_name, &value)));
                    }
                    break;
                }
            }
        }
        if accounting.outcome != "error" {
            accounting.set_outcome("success");
        }
        accounting.finalize();
        if dialect == IngressDialect::OpenAi {
            yield Ok::<Bytes, std::io::Error>(Bytes::from_static(b"data: [DONE]\n\n"));
        }
    };

    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "text/event-stream")
        .body(Body::from_stream(body))
        .expect("valid streaming response")
}

fn estimate_reservation(request: &ChatRequestV1) -> u64 {
    let bytes = serde_json::to_vec(&request.messages)
        .map(|value| value.len() as u64)
        .unwrap_or(0)
        .saturating_add(
            serde_json::to_vec(&request.tools)
                .map(|value| value.len() as u64)
                .unwrap_or(0),
        );
    let estimated_input = bytes.saturating_add(3) / 4;
    estimated_input
        .saturating_add(request.max_output_tokens.unwrap_or(1))
        .max(1)
}

fn sse_frame(event: Option<&str>, value: &Value) -> String {
    let mut frame = String::new();
    if let Some(event) = event {
        frame.push_str("event: ");
        frame.push_str(event);
        frame.push('\n');
    }
    frame.push_str("data: ");
    frame.push_str(&serde_json::to_string(value).unwrap_or_else(|_| "{}".into()));
    frame.push_str("\n\n");
    frame
}

fn usage_event(
    provider: &str,
    model: &str,
    metadata: &RequestMetadataV1,
    usage: &UsageV2,
) -> UsageEvent {
    UsageEvent::new(
        usage
            .upstream_request_id
            .clone()
            .unwrap_or_else(next_request_id),
        now_rfc3339(),
        provider,
        model,
        Backend::External,
    )
    .with_attribution(
        metadata.virtual_key_id.clone(),
        metadata.subject_id.clone(),
        metadata.group_id.clone(),
    )
    .with_route(metadata.route.clone())
    .with_session(metadata.session_id.clone())
    .with_tokens(usage.tokens_in, usage.tokens_out)
    .with_cache(usage.cache_creation_tokens, usage.cache_read_tokens)
    .with_measurement(
        usage.completeness,
        usage.attempts,
        usage.outcome.clone(),
        usage.upstream_request_id.clone(),
    )
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
        .map(|duration| duration.as_millis())
        .unwrap_or(0);
    format!("req_{millis}_{n}")
}

fn budget_scope(vk: &VirtualKey) -> String {
    // An operator-set explicit scope wins; otherwise derive from the group (the default
    // prompt-cache namespace) or fall back to the key itself.
    if let Some(scope) = vk.budget_scope.as_deref() {
        return scope.to_string();
    }
    match &vk.group_id {
        Some(g) => format!("group:{g}"),
        None => format!("vk:{}", vk.id),
    }
}

enum VirtualKeyResolution {
    Found(VirtualKey),
    NotFound,
    Expired,
}

/// Resolve a presented virtual-key token: exact (legacy demo) then by hash (operator-minted).
/// Filters out expired keys.
fn resolve_virtual_key(state: &ProxyState, token: &str) -> VirtualKeyResolution {
    let vk = state
        .keys
        .resolve(token)
        .or_else(|| state.keys.resolve(&hash_secret(token)));
    match vk {
        Some(vk) => {
            if vk.is_expired(&now_rfc3339()) {
                VirtualKeyResolution::Expired
            } else {
                VirtualKeyResolution::Found(vk)
            }
        }
        None => VirtualKeyResolution::NotFound,
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

fn provider_error(e: &ProviderError, dialect: IngressDialect, provider: &str) -> Response {
    let (status, msg) = match e {
        ProviderError::InvalidRequest(_) => (StatusCode::BAD_REQUEST, "invalid provider request"),
        ProviderError::Auth => (StatusCode::BAD_GATEWAY, "upstream auth failed"),
        ProviderError::RateLimited => (StatusCode::TOO_MANY_REQUESTS, "upstream rate limited"),
        ProviderError::Upstream(_) => (StatusCode::BAD_GATEWAY, "upstream error"),
        ProviderError::Transport(_) => (StatusCode::BAD_GATEWAY, "upstream transport error"),
        ProviderError::CircuitOpen => (
            StatusCode::SERVICE_UNAVAILABLE,
            "circuit open (upstream failing)",
        ),
        ProviderError::Timeout(_) => (StatusCode::GATEWAY_TIMEOUT, "upstream timed out"),
        // ProviderError is #[non_exhaustive]; unknown future variants degrade to 502.
        _ => (StatusCode::BAD_GATEWAY, "upstream error"),
    };
    let typed = e.as_typed(Some(provider));
    let body = match dialect {
        IngressDialect::OpenAi | IngressDialect::Responses => json!({"error":typed}),
        IngressDialect::Anthropic => json!({"type":"error","error":typed}),
    };
    let _ = msg;
    (status, Json(body)).into_response()
}

fn ingress_error(dialect: IngressDialect, status: StatusCode, msg: &str) -> Response {
    let typed = json!({
        "code":"invalid_request",
        "message":msg,
        "retryable":false,
        "http_status":status.as_u16(),
    });
    let body = match dialect {
        IngressDialect::OpenAi | IngressDialect::Responses => json!({"error":typed}),
        IngressDialect::Anthropic => json!({"type":"error","error":typed}),
    };
    (status, Json(body)).into_response()
}

fn error(status: StatusCode, msg: &str) -> Response {
    (status, Json(json!({ "error": msg }))).into_response()
}
