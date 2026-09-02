//! Sandhi reverse-proxy — the **in-path (inline) egress gate** (AnvaiOps ADR-0047 D8).
//!
//! A client points its `base_url` at Sandhi and presents a **virtual key** (never the real
//! upstream key). The gate resolves the key → subject/group + which upstream, budget-checks,
//! normalizes the request through Sandhi's typed runtime, then emits one neutral usage event and
//! reconciles the budget. It is *in-path*, not a redirect: a client cannot bypass the meter.

mod codec;
pub mod config;
pub mod ledger;
pub mod metrics;
pub mod operator;
pub mod persistence;
pub mod ratelimit;

/// First-party OTel/OTLP export of `gen_ai.*` spans + metrics (Scope 5, TD-0011 P3). Feature-gated
/// (`otel-otlp`, default off); provides no-op stubs when the feature is off so call sites compile
/// identically either way.
pub mod otel;

// Re-export the admin API request/response types for the `sandhi` CLI client + the startup
// rehydration helpers used by the `sandhi-proxy` binary.
pub use ledger::{reclaim_sweep_at, Admission, ProxyLedger};
pub use operator::{
    admin, build_provider_handle, rehydrate_alerts, rehydrate_budgets, rehydrate_live_keys,
};
pub use persistence::BufferedAlertStore;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use axum::body::{Body, Bytes};
use axum::extract::rejection::BytesRejection;
use axum::extract::{DefaultBodyLimit, Extension, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Json, Response};
use axum::routing::{delete, get, post};
use axum::Router;
use futures_util::StreamExt;
use serde_json::{json, Value};
use tower::{Layer, Service as TowerService};

use time::OffsetDateTime;

use sandhi_core::{
    billable, derive_session_id_scoped, AlertRegistry, Backend, ChatRequestV1, FinishReasonV1,
    KeyStore, ParsedUsage, Policy, RequestMetadataV1, Reservation, Sink, UsageBasis,
    UsageCompleteness, UsageEvent, UsageV2, VirtualKey,
};
use sandhi_providers::{ProviderError, ProviderFamily, ProviderHandle, ProviderRuntime};
use sandhi_store::{hash_secret, AlertStore, SqliteStore, VaultStore, VirtualKeyStore};

use codec::{decode_request, encode_response, encode_stream_event, IngressDialect};
pub use operator::BudgetSpec;

/// Conservative output ceiling applied to a **budget-capped** scope when the client omits
/// `max_output_tokens` (ADR-0005 D1). The reservation holds this as an upper bound and the value
/// is set on the upstream request so the provider bounds output — otherwise an unbounded stream
/// overshoots the cap (the 100× soft-cap bug). Unlimited scopes are never modified.
const DEFAULT_OUTPUT_CEILING: u64 = 4096;

/// Axum's historical `Bytes` extractor default, now made an explicit Sandhi policy. AI requests
/// can legitimately be much larger (notably inline media), so operators may raise this through
/// `SANDHI_MAX_REQUEST_BODY_BYTES`; keeping the defensive default avoids silently multiplying
/// buffered memory by concurrent requests.
pub const DEFAULT_MAX_REQUEST_BODY_BYTES: usize = 2 * 1024 * 1024;

/// Maximum AI calls admitted far enough to buffer and inspect their request bodies at once.
/// Together with [`DEFAULT_MAX_REQUEST_BODY_BYTES`], this bounds application-owned request-body
/// memory to roughly 256 MiB before JSON decoding and other per-request state.
///
/// 64 was calibrated when a slot was held only for the handler future — which for a stream
/// meant *first byte*. Since TD-0014 P2 the slot is held for the whole response body, so the
/// same number now bounds simultaneously open streams (upstream connection + lease + task
/// each). Doubling the default keeps real SSE traffic flowing while still bounding memory.
pub const DEFAULT_MAX_IN_FLIGHT_AI_REQUESTS: usize = 128;

/// Maximum time the server gives active responses to finish after shutdown begins.
pub const DEFAULT_SHUTDOWN_GRACE: Duration = Duration::from_secs(30);

/// Maximum concurrent TCP connections (TD-0014 P3). 1024 is comfortably above any sane AI
/// admission limit while keeping worst-case per-connection buffers bounded (see
/// `CONNECTION_READ_BUF_BYTES`).
pub const DEFAULT_MAX_CONNECTIONS: usize = 1024;

/// Maximum concurrent TCP connections from one peer IP (TD-0014 P3, G19); **0 (the default)
/// disables the cap**. Opt-in on purpose (TD-0014 pressure-test 3): keyed on the peer at accept
/// time, a proxy-fronted or NAT'd deployment shares one IP, so a default-on cap would shed the
/// fronting proxy's connections. Enable it when directly exposed, or after configuring
/// `SANDHI_TRUSTED_PROXIES` for the per-request client resolution.
pub const DEFAULT_MAX_CONNECTIONS_PER_IP: usize = 0;

/// Seconds a connection may spend transmitting its request head (TD-0014 P3, G03). Matches
/// hyper's current default — but hyper documents it as "do not depend on that", so Sandhi now
/// sets and guarantees it.
pub const DEFAULT_HEADER_READ_TIMEOUT_SECS: u64 = 30;

/// Per-connection HTTP read-buffer ceiling. hyper's default allows multi-megabyte growth per
/// connection; 256 KiB is ample for header-heavy AI requests because bodies stream.
const CONNECTION_READ_BUF_BYTES: usize = 256 * 1024;

/// Pause after a failed accept (TD-0014 P3): accept errors under overload fail
/// instantly while the listener stays readable, so without a pause the loop
/// busy-spins exactly when it should be shedding.
const ACCEPT_ERROR_BACKOFF: Duration = Duration::from_millis(100);

/// Validated TLS material for the inbound listener (TD-0017 P1).
///
/// Sandhi configures rustls; it does not implement any cryptographic primitive.
/// Construction parses the complete certificate chain and private key together,
/// and rustls rejects an invalid key or one that does not match the leaf
/// certificate before the listener starts accepting traffic.
#[derive(Clone)]
pub struct TlsConfig {
    acceptor: tokio_rustls::TlsAcceptor,
}

impl TlsConfig {
    /// Load and validate a PEM certificate chain and matching private key.
    pub fn from_pem_files(
        cert_path: impl AsRef<std::path::Path>,
        key_path: impl AsRef<std::path::Path>,
    ) -> std::io::Result<Self> {
        let cert_path = cert_path.as_ref();
        let key_path = key_path.as_ref();
        let cert_pem = std::fs::read(cert_path).map_err(|error| {
            std::io::Error::new(
                error.kind(),
                format!("reading TLS certificate {}: {error}", cert_path.display()),
            )
        })?;
        let key_pem = std::fs::read(key_path).map_err(|error| {
            std::io::Error::new(
                error.kind(),
                format!("reading TLS private key {}: {error}", key_path.display()),
            )
        })?;
        Self::from_pem(&cert_pem, &key_pem)
    }

    /// Parse in-memory PEM material. Public so embedders do not need temporary
    /// files; the standalone binary uses [`Self::from_pem_files`].
    pub fn from_pem(cert_pem: &[u8], key_pem: &[u8]) -> std::io::Result<Self> {
        use rustls::pki_types::{pem::PemObject, CertificateDer, PrivateKeyDer};

        let certs = CertificateDer::pem_slice_iter(cert_pem)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!("parsing TLS certificate chain: {error}"),
                )
            })?;
        if certs.is_empty() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "TLS certificate file contains no CERTIFICATE entries",
            ));
        }
        let key = PrivateKeyDer::from_pem_slice(key_pem).map_err(|error| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("parsing TLS private key: {error}"),
            )
        })?;
        // reqwest's feature graph can enable both rustls providers. Select one
        // locally instead of mutating process-global provider state (an
        // embedding application may already have made its own choice).
        let server = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .map_err(|error| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("selecting safe TLS protocol versions: {error}"),
            )
        })?
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|error| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("validating TLS certificate and private key: {error}"),
            )
        })?;
        Ok(Self {
            acceptor: tokio_rustls::TlsAcceptor::from(Arc::new(server)),
        })
    }
}

/// Warning emitted when bearer credentials would be accepted on a non-loopback
/// plaintext listener. Loopback development remains quiet; TLS makes the
/// warning unnecessary.
#[must_use]
pub fn plaintext_bind_warning(addr: SocketAddr, tls_enabled: bool) -> Option<&'static str> {
    (!tls_enabled && !addr.ip().is_loopback()).then_some(
        "WARNING: accepting bearer credentials over plaintext on a non-loopback listener; \
         configure tls.cert/tls.key in SANDHI_CONFIG or terminate TLS at a trusted proxy",
    )
}

/// Shared server state: the virtual-key store, the budget ledger, the usage sink, and the
/// registry of configured upstream providers (each already holding its real credential).
pub struct ProxyState {
    pub keys: KeyStore,
    /// The enforcement ledger (ADR-0005 lease model): durable [`SqliteLedger`](sandhi_store::SqliteLedger)
    /// when `SANDHI_STORE` is set, else volatile in-memory. See [`ProxyLedger`].
    pub ledger: Mutex<ProxyLedger>,
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
    /// Operator-set budgets (scope → spec). The live [`ProxyLedger`] enforces them; this map is the
    /// metadata surface (policy lookup, dashboard, alert thresholds) and is rehydrated from the
    /// durable ledger on startup.
    pub budgets: Mutex<HashMap<String, BudgetSpec>>,
    /// The externally-reachable base URL shared with minted-key callers (e.g.
    /// `http://localhost:8787`).
    pub public_url: String,
    // --- TD-0003 P2 budget depth + alerts ---
    /// Live alert-rule registry + dedup (the evaluation engine). `None` when alerts are off.
    pub alerts: Option<Arc<Mutex<AlertRegistry>>>,
    /// Durable alert-rule store (rules + last_fired_at + ack), backs `/admin/alerts`.
    pub alert_store: Option<Arc<AlertStore>>,
    /// Bounded background mirror for alert `last_fired_at` updates. The live registry remains the
    /// request-time dedup authority; only its SQLite persistence leaves the async task.
    pub alert_writer: Option<Arc<BufferedAlertStore>>,
    /// ADR-0004 D4: when `false` (default) and an admin token is configured, the
    /// `/dashboard/api/*` read endpoints require the admin bearer — they serve subject/group
    /// usage aggregates. `SANDHI_DASHBOARD_PUBLIC=1` restores the previous open, masked-only
    /// behavior for trusted single-node deployments. With no admin token configured the
    /// endpoints stay open (there is no credential to present).
    pub dashboard_public: bool,
    /// TD-0008 D: when `false` (default), client-facing provider errors are REDACTED to
    /// code + http_status + request_id + a canonical short message — upstream bodies and
    /// transport internals can echo prompt fragments or infra detail, which must not leak
    /// to a different tenant's client. `SANDHI_ERROR_DETAIL=full` opts single-tenant /
    /// self-hosted deployments into the full ProviderErrorV1 (bounded upstream body in
    /// `details.upstream_body`). Server-side logs always carry the full error either way.
    pub error_detail_full: bool,
    /// Maximum buffered request body accepted by proxy/admin handlers. Sandhi must inspect the
    /// complete JSON envelope for auth/model/budget/translation decisions, so this is an explicit
    /// memory boundary rather than an accidental framework default.
    pub max_request_body_bytes: usize,
    /// Concurrent AI requests allowed through the admission boundary. The concurrency layer wraps
    /// the routes outside body extraction, so queued calls do not each allocate a full body.
    pub max_in_flight_ai_requests: usize,
    // --- TD-0014 P3: connection-level limits (pre-authentication blast-radius controls) ---
    /// Maximum concurrent TCP connections served. At the cap, new accepts are closed without a
    /// response — the AI admission path is where dialect-shaped shedding happens; this is the
    /// cheaper, earlier backpressure.
    pub max_connections: usize,
    /// Maximum concurrent TCP connections from a single peer IP; **0 disables the cap**.
    /// Counted at accept time, before headers — so behind a trusted proxy every connection
    /// shares the proxy IP and this should be 0 (let the proxy enforce per-client limits).
    pub max_connections_per_ip: usize,
    /// Seconds a connection may spend sending its request head before timeout. Explicitly set
    /// (hyper's default is documented as "do not depend on that") — this is the slowloris bound.
    pub header_read_timeout_secs: u64,
    /// CIDR ranges whose `X-Forwarded-For` header is believed. Empty = trust no one (default).
    pub trusted_proxies: Vec<ipnet::IpNet>,
    /// TD-0011 P2 metric registry, served at `/metrics` (gated like the dashboard).
    pub metrics: Arc<metrics::Metrics>,
    /// Scope 5 (TD-0011 P3): OTLP export of `gen_ai.*` spans + metrics, when the `otel-otlp`
    /// feature is compiled in **and** `SANDHI_OTEL_EXPORT=otlp` is set. `None` otherwise — the
    /// default prerequisite-free `/metrics` path is unaffected. Built by `otel::init` in `main`.
    pub otel: Option<Arc<otel::OtelRecorder>>,
    /// TD-0012 per-virtual-key request rate limiting. In-memory: with N replicas the effective
    /// limit is N × the configured value (D2).
    pub rate_limiter: Arc<ratelimit::RateLimiter>,
    /// Declarative desired-state config file (`SANDHI_CONFIG`) backing `/admin/config` +
    /// `/admin/config/apply` — providers/budgets/alerts/vkeys as committable JSON, secrets
    /// referenced by env-var name rather than inlined. `None` disables both routes (404).
    pub config_path: Option<std::path::PathBuf>,
}

impl ProxyState {
    /// Build a state with the operator surface defaulted off (no vault, no admin token). The
    /// existing demo + request-handling path is unchanged.
    #[must_use]
    pub fn new(
        keys: KeyStore,
        ledger: ProxyLedger,
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
            alert_writer: None,
            dashboard_public: false,
            error_detail_full: false,
            max_request_body_bytes: DEFAULT_MAX_REQUEST_BODY_BYTES,
            max_in_flight_ai_requests: DEFAULT_MAX_IN_FLIGHT_AI_REQUESTS,
            max_connections: DEFAULT_MAX_CONNECTIONS,
            max_connections_per_ip: DEFAULT_MAX_CONNECTIONS_PER_IP,
            header_read_timeout_secs: DEFAULT_HEADER_READ_TIMEOUT_SECS,
            trusted_proxies: Vec::new(),
            metrics: Arc::new(metrics::Metrics::new()),
            rate_limiter: Arc::new(ratelimit::RateLimiter::new()),
            otel: None,
            config_path: None,
        }
    }
}

/// Build the axum app. Ingress paths mirror the provider wire formats (OpenAI Chat Completions,
/// OpenAI Responses, Anthropic Messages); the presented virtual key selects the actual upstream.
/// The `/admin/*` routes are the TD-0003 operator surface (authed by an admin token).
/// An admission slot for one in-flight AI call.
///
/// Unlike `tower`'s `ConcurrencyLimit` — whose permit is a private field of its response future,
/// released when the handler future resolves — this permit is an ordinary value the handlers can
/// MOVE into a streaming response body (TD-0014 P2). That is the whole point: for SSE the handler
/// future resolves at first byte, so a future-held permit bounds buffering but not the resource
/// that actually dominates — simultaneously open streams, each holding an upstream connection, a
/// lease, a task, and decoder buffers.
#[derive(Debug)]
pub(crate) struct AdmissionPermit {
    _permit: OwnedSemaphorePermit,
}

/// Bounds concurrent AI calls, admitting in `call` so queued requests hold only transport-level
/// buffers. The acquired permit rides in the request extensions; the handlers move it into the
/// response body on the streaming paths and drop it at handler end on the unary paths.
#[derive(Clone)]
pub(crate) struct AdmissionLayer {
    permits: Arc<Semaphore>,
}

impl AdmissionLayer {
    pub(crate) fn new(max_in_flight: usize) -> Self {
        Self {
            permits: Arc::new(Semaphore::new(max_in_flight)),
        }
    }
}

impl<S> Layer<S> for AdmissionLayer {
    type Service = AdmissionService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        AdmissionService {
            inner,
            permits: self.permits.clone(),
        }
    }
}

#[derive(Clone)]
pub(crate) struct AdmissionService<S> {
    inner: S,
    permits: Arc<Semaphore>,
}

impl<S> TowerService<axum::http::Request<Body>> for AdmissionService<S>
where
    S: TowerService<axum::http::Request<Body>> + Clone + Send + 'static,
    S::Future: Send + 'static,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future =
        Pin<Box<dyn std::future::Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, mut request: axum::http::Request<Body>) -> Self::Future {
        let permits = self.permits.clone();
        let mut inner = self.inner.clone();
        Box::pin(async move {
            let permit = permits
                .acquire_owned()
                .await
                .expect("admission semaphore must not be closed");
            request
                .extensions_mut()
                .insert(Arc::new(AdmissionPermit { _permit: permit }));
            inner.call(request).await
        })
    }
}

/// Connection-level admission policy, resolved once at startup from `ProxyState` (TD-0014 P3).
#[derive(Clone)]
pub(crate) struct ConnectionPolicy {
    pub max_connections: usize,
    /// 0 disables the per-IP cap. Behind a trusted proxy every connection
    /// shares the proxy's IP at accept time (headers are not parsed yet), so
    /// deployments behind one should disable this and let the proxy do it.
    /// Trusted-proxy CIDRs are NOT here: `X-Forwarded-For` is per-request and
    /// is resolved by the `resolve_client_ip` middleware from `ProxyState`.
    pub max_per_ip: usize,
    pub header_read_timeout: Duration,
    pub metrics: Arc<crate::metrics::Metrics>,
}

impl Default for ConnectionPolicy {
    fn default() -> Self {
        Self {
            max_connections: DEFAULT_MAX_CONNECTIONS,
            max_per_ip: DEFAULT_MAX_CONNECTIONS_PER_IP,
            header_read_timeout: Duration::from_secs(DEFAULT_HEADER_READ_TIMEOUT_SECS),
            metrics: Arc::new(crate::metrics::Metrics::new()),
        }
    }
}

impl ConnectionPolicy {
    pub(crate) fn from_state(state: &ProxyState) -> Self {
        Self {
            max_connections: state.max_connections.max(1),
            max_per_ip: state.max_connections_per_ip,
            header_read_timeout: Duration::from_secs(state.header_read_timeout_secs.max(1)),
            metrics: Arc::clone(&state.metrics),
        }
    }
}

/// `Some(client_ip)` only when `peer` is a configured trusted proxy; the
/// `X-Forwarded-For` header is otherwise attacker-controlled and never believed.
/// With several hops, the FIRST address is the original client; callers behind
/// exactly one trusted proxy is the supported shape.
pub(crate) fn resolve_forwarded_for(
    peer: std::net::IpAddr,
    forwarded_for: Option<&str>,
    trusted_proxies: &[ipnet::IpNet],
) -> Option<std::net::IpAddr> {
    let trusted = |ip: std::net::IpAddr| trusted_proxies.iter().any(|net| net.contains(&ip));
    if !trusted(peer) {
        return None;
    }
    forwarded_for
        .and_then(|header| header.split(',').next())
        .map(str::trim)
        .and_then(|first| first.parse().ok())
        .filter(|client| !trusted(*client))
}

/// Parse `SANDHI_TRUSTED_PROXIES` — comma-separated CIDRs; empty string → empty allowlist.
/// A malformed entry is a startup panic (consistent with `SANDHI_BIND`): a typo that silently
/// narrows the trust boundary is worse than refusing to start.
pub fn parse_trusted_proxies(spec: &str) -> Vec<ipnet::IpNet> {
    spec.split(',')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .map(|entry| {
            entry.parse().unwrap_or_else(|_| {
                panic!(
                    "SANDHI_TRUSTED_PROXIES entry {entry:?} is not a valid CIDR (e.g. 10.0.0.0/8)"
                )
            })
        })
        .collect()
}

/// Per-connection peer identity riding request extensions (TD-0014 P3, G19).
/// Deliberately minimal: no protocol metadata beyond what admission needs, and
/// it never reaches `RequestMetadataV1`, usage events, or metric labels
/// (TD-0011 D2 — an IP is unbounded cardinality and personal data).
/// Drop guard releasing one per-IP connection slot when its connection task
/// ends. Deadlock note: created only while holding the map lock is NOT the
/// case — the increment happens in the accept loop, the guard drops in the
/// task, and each takes the lock briefly.
struct PerIpSlot {
    per_ip: Arc<std::sync::Mutex<HashMap<std::net::IpAddr, usize>>>,
    ip: std::net::IpAddr,
}

impl Drop for PerIpSlot {
    fn drop(&mut self) {
        if let Ok(mut map) = self.per_ip.lock() {
            if let Some(count) = map.get_mut(&self.ip) {
                *count -= 1;
                if *count == 0 {
                    map.remove(&self.ip);
                }
            }
        }
    }
}

/// The connection's peer address, inserted per connection at accept time
/// (TD-0014 P3, G19). Deliberately minimal: it never reaches
/// `RequestMetadataV1`, usage events, or metric labels (TD-0011 D2 — an IP is
/// unbounded cardinality and personal data).
#[derive(Debug, Clone)]
pub(crate) struct PeerCtx {
    #[allow(dead_code)]
    pub peer: std::net::IpAddr,
}

/// The believed client IP for one REQUEST, inserted by the `resolve_client_ip`
/// middleware after the trusted-proxy check. Distinct from [`PeerCtx`] because
/// `X-Forwarded-For` is per-request: the accept loop cannot see headers, which
/// is exactly how an earlier draft shipped dead wiring (adversarial review,
/// finding 1).
#[derive(Debug, Clone)]
pub(crate) struct ClientAddr {
    #[allow(dead_code)]
    pub client: std::net::IpAddr,
}

/// Per-request trusted-proxy resolution: believe `X-Forwarded-For` only when
/// the connection's peer is an allowlisted proxy. Middleware, not accept-loop
/// logic, because headers do not exist at accept time.
pub(crate) async fn resolve_client_ip(
    axum::extract::State(state): axum::extract::State<std::sync::Arc<ProxyState>>,
    mut request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let peer = request.extensions().get::<PeerCtx>().map(|peer| peer.peer);
    let client = peer
        .and_then(|peer| {
            let forwarded = request
                .headers()
                .get("x-forwarded-for")
                .and_then(|value| value.to_str().ok());
            resolve_forwarded_for(peer, forwarded, &state.trusted_proxies)
                .map(|ip: std::net::IpAddr| ip.to_canonical())
        })
        .or(peer);
    if let Some(client) = client {
        request.extensions_mut().insert(ClientAddr { client });
    }
    next.run(request).await
}

/// The AI ingress routes with their connection-scoped layers: admission and
/// per-request trusted-proxy resolution. A separate function so the layering
/// itself is testable against the exact production wiring (the middleware was
/// once absent from this chain while its pure logic stayed green — the wiring
/// test exists because of that).
fn ingress_routes(state: &Arc<ProxyState>) -> axum::Router<Arc<ProxyState>> {
    let ai_routes = Router::new()
        .route("/v1/chat/completions", post(handle_openai))
        .route("/v1/messages", post(handle_anthropic))
        .route("/v1/responses", post(handle_responses))
        // Gemini's path carries the model AND the method, colon-separated
        // (`/v1beta/models/gemini-2.5-flash:generateContent`), so it matches as ONE segment and
        // is split below — axum has no pattern for a colon-suffixed verb.
        .route("/v1beta/models/:model_method", post(handle_gemini));
    // Test-only observability probe: lets the wiring test confirm THIS
    // function's middleware chain actually inserts ClientAddr.
    #[cfg(test)]
    let ai_routes = ai_routes.route(
        "/__client_probe",
        axum::routing::get(|Extension(client): Extension<ClientAddr>| async move {
            client.client.to_string()
        }),
    );
    ai_routes
        // Admission wraps extraction: waiting requests retain only transport-level buffers rather
        // than each allocating `SANDHI_MAX_REQUEST_BODY_BYTES` in application memory.
        .layer(AdmissionLayer::new(state.max_in_flight_ai_requests.max(1)))
        // TD-0014 P3 (G19): per-request trusted-proxy resolution —
        // X-Forwarded-For believed only from SANDHI_TRUSTED_PROXIES peers.
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            resolve_client_ip,
        ))
}

pub fn build_app(state: Arc<ProxyState>) -> Router {
    let max_request_body_bytes = state.max_request_body_bytes;
    let ai_routes = ingress_routes(&state);
    // TD-0021 P2 (D4/R3): every response — success AND error, including ingress errors —
    // carries the chat contract version, so a consumer hitting a mismatch knows which
    // contract it is talking to at the moment it most needs to (R3: a mismatch presents
    // as an error). One middleware layer is the single source; no builder can forget it.
    Router::new()
        .route("/healthz", get(health))
        // TD-0021 P2 (D4/R2): the HTTP-path contract handshake. Ungated — versions
        // and wired dialects are public facts; the capability detail (D5) lives
        // behind the admin gate at /admin/version.
        .route("/version", get(version))
        .route("/catalog/models", get(catalog_models))
        .route("/dashboard", get(dashboard_html))
        .route("/dashboard/api/usage", get(dashboard_api))
        // TD-0003 P4 dashboard read-only endpoints (masked; admin-bearer-gated when an admin
        // token is configured, unless SANDHI_DASHBOARD_PUBLIC=1 — ADR-0004 D4).
        .route("/dashboard/api/keys", get(dashboard_keys))
        .route("/dashboard/api/budgets", get(dashboard_budgets))
        .route("/dashboard/api/alerts", get(dashboard_alerts))
        // TD-0011 D5: /metrics reveals traffic shape and model mix, so it reuses the dashboard's
        // gate rather than inventing a second policy.
        .route("/metrics", get(metrics_endpoint))
        // TD-0010 D3 discovery. `/v1/models` is shared by the OpenAI and Anthropic SDKs, which
        // is resolvable because they authenticate differently: an `x-api-key` request is an
        // Anthropic client and gets Anthropic's envelope.
        .route("/v1/models", get(list_models))
        .route("/v1beta/models", get(list_models_gemini))
        // TD-0003 P1 operator (admin) API.
        // TD-0021 P2 (D5/R2): capability detail — which optional features are on —
        // is operator information, gated like the rest of /admin.
        .route("/admin/version", get(operator::version_capabilities))
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
        // ADR-0005 D7: the agent cost tree for one run (per-step rollups by parent_id).
        .route("/admin/usage/run/:run_id", get(operator::usage_run))
        // TD-0003 P2 alert rules.
        .route(
            "/admin/alerts",
            post(operator::create_alert).get(operator::list_alerts),
        )
        .route("/admin/alerts/:id/ack", post(operator::ack_alert))
        .route("/admin/alerts/:id", delete(operator::delete_alert))
        // Declarative desired-state config (SANDHI_CONFIG) — additive-only apply, see config.rs.
        .route("/admin/config", get(operator::config_preview))
        .route("/admin/config/apply", post(operator::config_apply))
        .merge(ai_routes)
        .layer(DefaultBodyLimit::max(max_request_body_bytes))
        .layer(axum::middleware::from_fn(contract_version_header))
        .with_state(state)
}

/// Bind and serve until shutdown.
///
/// Delegates to [`serve_with_shutdown_timeout`] with a never-resolving
/// shutdown, so embedders on this path get the SAME connection-level
/// protections as the graceful path (an earlier form served via bare
/// `axum::serve` and silently skipped every P3 defence — review finding 6).
pub async fn serve(state: Arc<ProxyState>, addr: SocketAddr) -> std::io::Result<()> {
    serve_with_shutdown_timeout(state, addr, std::future::pending(), DEFAULT_SHUTDOWN_GRACE).await
}

/// Bind and serve until `shutdown` resolves, then stop accepting connections and drain every
/// in-flight request/response stream before returning.
///
/// The standalone binary uses this for SIGINT/SIGTERM handling. Keeping the signal source as a
/// caller-provided future also makes the lifecycle usable by embedders without imposing a process
/// signal policy on them.
pub async fn serve_with_shutdown<F>(
    state: Arc<ProxyState>,
    addr: SocketAddr,
    shutdown: F,
) -> std::io::Result<()>
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    serve_with_shutdown_timeout(state, addr, shutdown, DEFAULT_SHUTDOWN_GRACE).await
}

/// Bind and serve with an explicit maximum graceful-drain period.
///
/// Once `shutdown` resolves, new connections are refused. Active calls may finish and settle
/// until `grace` expires; after that the server future is dropped, which cancels remaining
/// response streams and runs their accounting finalizers as `Partial`/cancelled.
pub async fn serve_with_shutdown_timeout<F>(
    state: Arc<ProxyState>,
    addr: SocketAddr,
    shutdown: F,
    grace: Duration,
) -> std::io::Result<()>
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    let listener = tokio::net::TcpListener::bind(addr).await?;
    serve_listener_with_shutdown(state, listener, shutdown, grace, None).await
}

/// TLS counterpart to [`serve_with_shutdown_timeout`]. The HTTP application,
/// admission controls, slowloris timeout, and drain semantics are identical;
/// only the accepted byte stream is wrapped in a validated rustls session.
pub async fn serve_with_tls_shutdown_timeout<F>(
    state: Arc<ProxyState>,
    addr: SocketAddr,
    shutdown: F,
    grace: Duration,
    tls: TlsConfig,
) -> std::io::Result<()>
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    let listener = tokio::net::TcpListener::bind(addr).await?;
    serve_listener_with_shutdown(state, listener, shutdown, grace, Some(tls.acceptor)).await
}

async fn serve_listener_with_shutdown<F>(
    state: Arc<ProxyState>,
    listener: tokio::net::TcpListener,
    shutdown: F,
    grace: Duration,
    tls: Option<tokio_rustls::TlsAcceptor>,
) -> std::io::Result<()>
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    let policy = ConnectionPolicy::from_state(&state);
    serve_router_listener_with_shutdown(build_app(state), listener, shutdown, grace, policy, tls)
        .await
}

/// Serve until `shutdown` resolves, with TD-0014 P3 connection-level defence:
/// a hard cap on concurrent connections and per-peer connections, a guaranteed
/// header-read timeout, a bounded per-connection read buffer, and a drain that
/// actually closes hung connections at the grace deadline (axum's own serve
/// spawns connection tasks detached, so its grace deadline alone cannot).
async fn serve_router_listener_with_shutdown<F>(
    app: Router,
    listener: tokio::net::TcpListener,
    shutdown: F,
    grace: Duration,
    policy: ConnectionPolicy,
    tls: Option<tokio_rustls::TlsAcceptor>,
) -> std::io::Result<()>
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    use std::collections::HashMap;

    let slots = Arc::new(Semaphore::new(policy.max_connections));
    let per_ip: Arc<std::sync::Mutex<HashMap<std::net::IpAddr, usize>>> = Default::default();
    let graceful = Arc::new(hyper_util::server::graceful::GracefulShutdown::new());
    let mut tasks: tokio::task::JoinSet<()> = tokio::task::JoinSet::new();
    tokio::pin!(shutdown);

    // Accept loop — ends on shutdown signal; connection tasks keep draining
    // through the grace window below.
    loop {
        tokio::select! {
            _ = &mut shutdown => break,
            accepted = listener.accept() => {
                let (stream, peer_addr) = match accepted {
                    Ok(pair) => pair,
                    Err(error) => {
                        // Accept errors under overload (EMFILE) fail instantly
                        // while the listener stays readable: without a pause
                        // this loop hot-spins exactly when it should shed.
                        tokio::time::sleep(ACCEPT_ERROR_BACKOFF).await;
                        tracing::warn!(%error, "accept failed");
                        continue;
                    }
                };
                // Canonicalize once: v4-mapped IPv6 peers must share buckets
                // and allowlist entries with their v4 form.
                let peer = std::net::SocketAddr::new(peer_addr.ip().to_canonical(), peer_addr.port());

                // Shed BEFORE any allocation or service build: over-cap and
                // over-per-IP connections are closed without a response. The
                // AI admission path is where dialect-shaped shedding happens;
                // this is the cheaper, pre-authentication backpressure.
                let Ok(permit) = slots.clone().try_acquire_owned() else {
                    policy.metrics.connection_shed();
                    drop(stream);
                    continue;
                };
                let mut ip_count = per_ip.lock().expect("per-ip connection map poisoned");
                if policy.max_per_ip > 0
                    && ip_count.get(&peer.ip()).copied().unwrap_or(0) >= policy.max_per_ip
                {
                    drop(ip_count);
                    policy.metrics.connection_shed();
                    drop(permit);
                    drop(stream);
                    continue;
                }
                *ip_count.entry(peer.ip()).or_insert(0) += 1;
                drop(ip_count);
                // Built HERE, before the task exists: an aborted never-polled
                // task would never run a guard constructed inside it, leaking
                // the per-IP slot (adversarial review, finding 5).
                let per_ip_slot = PerIpSlot {
                    per_ip: Arc::clone(&per_ip),
                    ip: peer.ip(),
                };

                let watcher = graceful.watcher();
                let metrics = Arc::clone(&policy.metrics);
                let app = app.clone();
                let tls = tls.clone();
                let header_read_timeout = policy.header_read_timeout;
                // Builder, connection, watcher, guards: one task owns all of
                // it, because the connection borrows its builder.
                tasks.spawn(async move {
                    let _permit = permit;
                    let _per_ip_slot = per_ip_slot;
                    let _conn_open = metrics.connection_open_guard();
                    if let Some(acceptor) = tls {
                        // TLS handshakes happen before hyper can arm its request-head
                        // timer. Apply the same deadline here so a silent ClientHello
                        // cannot recreate the pre-first-byte slowloris gap.
                        match tokio::time::timeout(header_read_timeout, acceptor.accept(stream)).await {
                            Ok(Ok(stream)) => {
                                serve_http1_connection(
                                    stream,
                                    app,
                                    peer.ip(),
                                    header_read_timeout,
                                    watcher,
                                )
                                .await;
                            }
                            Ok(Err(error)) => {
                                tracing::debug!(%peer, %error, "TLS handshake rejected");
                            }
                            Err(_) => {
                                tracing::debug!(%peer, "TLS handshake timed out");
                            }
                        }
                    } else {
                        serve_http1_connection(
                            stream,
                            app,
                            peer.ip(),
                            header_read_timeout,
                            watcher,
                        )
                        .await;
                    }
                });
            }
        }
    }

    // Grace window: signal every watched connection, wait until they finish or
    // the deadline passes, then abort the stragglers. This is what makes the
    // doc claim true — hung streams do not outlive `serve_with_shutdown_timeout`.
    tracing::info!("shutdown received; draining connections");
    // `shutdown(self)` consumes; unwrap the Arc (watchers hold only the rx
    // side) and let the signal+wait run on its own task.
    let _shutdown_task = Arc::try_unwrap(graceful)
        .map(|g| tokio::spawn(g.shutdown()))
        .ok();
    let deadline = tokio::time::Instant::now() + grace;
    let mut expired = false;
    while !tasks.is_empty() {
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => { expired = true; break; }
            _ = tasks.join_next() => {}
        }
    }
    if expired {
        tracing::warn!(
            grace_secs = grace.as_secs_f64(),
            "shutdown grace expired; cancelling remaining connections (and their response streams)"
        );
        tasks.abort_all();
    }
    // Await aborted tasks so their guards (permits, gauges, per-IP slots) drop
    // before return.
    while tasks.join_next().await.is_some() {}
    Ok(())
}

/// Serve one already-admitted plaintext or TLS stream through the exact same
/// HTTP/1 path. Keeping the protocol builder below the transport wrapper
/// prevents TLS configuration from forking Sandhi's request semantics.
async fn serve_http1_connection<IO>(
    stream: IO,
    app: Router,
    peer: std::net::IpAddr,
    header_read_timeout: Duration,
    watcher: hyper_util::server::graceful::Watcher,
) where
    IO: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    use hyper::server::conn::http1;
    use hyper_util::rt::TokioTimer;
    use hyper_util::service::TowerToHyperService;

    let service = TowerToHyperService::new(app.layer(axum::Extension(PeerCtx { peer })));
    // hyper's http1 builder directly — NOT hyper-util's auto builder, whose
    // h2c preface sniff buffered reads before the header timer armed, exempting
    // silent connections from the slowloris defence (review finding 3).
    let mut builder = http1::Builder::new();
    builder
        .timer(TokioTimer::new())
        .header_read_timeout(Some(header_read_timeout))
        .max_buf_size(CONNECTION_READ_BUF_BYTES);
    let conn = builder.serve_connection(hyper_util::rt::TokioIo::new(stream), service);
    let _ = watcher.watch(conn).await;
}

#[cfg(test)]
mod server_lifecycle_tests {
    use super::*;
    use sandhi_core::{InMemorySink, KeyStore};

    #[test]
    fn plaintext_warning_is_only_for_exposed_unencrypted_listeners() {
        assert!(plaintext_bind_warning("127.0.0.1:8787".parse().unwrap(), false).is_none());
        assert!(plaintext_bind_warning("[::1]:8787".parse().unwrap(), false).is_none());
        assert!(plaintext_bind_warning("0.0.0.0:8787".parse().unwrap(), true).is_none());
        let warning = plaintext_bind_warning("0.0.0.0:8787".parse().unwrap(), false)
            .expect("non-loopback plaintext must be loud");
        assert!(warning.contains("bearer credentials over plaintext"));
    }

    #[test]
    fn tls_material_is_validated_before_listening() {
        let error = TlsConfig::from_pem(b"not a certificate", b"not a key")
            .err()
            .expect("invalid identity must be rejected");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);

        let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/tls");
        TlsConfig::from_pem_files(
            fixture.join("localhost-cert.pem"),
            fixture.join("localhost-key.pem"),
        )
        .expect("the configured file-loading path accepts a valid identity");

        let mismatch = TlsConfig::from_pem_files(
            fixture.join("localhost-cert.pem"),
            fixture.join("mismatched-key.pem"),
        )
        .err()
        .expect("a valid but unrelated private key must be rejected");
        assert_eq!(mismatch.kind(), std::io::ErrorKind::InvalidInput);
    }

    #[tokio::test]
    async fn shutdown_bounds_a_stalled_tls_handshake_by_the_grace_deadline() {
        use tokio::io::AsyncReadExt;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind test listener");
        let addr = listener.local_addr().expect("test listener address");
        let mut state = ProxyState::new(
            KeyStore::new(),
            ProxyLedger::in_memory(),
            Arc::new(InMemorySink::new()),
            HashMap::new(),
            None,
        );
        // Keep the handshake timeout well beyond the shutdown grace. The
        // server must still return at the grace deadline, not at this timer.
        state.header_read_timeout_secs = 30;

        let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/tls");
        let tls = TlsConfig::from_pem_files(
            fixture.join("localhost-cert.pem"),
            fixture.join("localhost-key.pem"),
        )
        .expect("valid test identity");
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        let server = tokio::spawn(serve_listener_with_shutdown(
            Arc::new(state),
            listener,
            async {
                let _ = shutdown_rx.await;
            },
            Duration::from_millis(50),
            Some(tls.acceptor),
        ));

        let mut stalled = tokio::net::TcpStream::connect(addr)
            .await
            .expect("connect silent TLS peer");
        tokio::time::sleep(Duration::from_millis(25)).await;
        let started = std::time::Instant::now();
        shutdown_tx.send(()).expect("signal shutdown");
        tokio::time::timeout(Duration::from_secs(1), server)
            .await
            .expect("stalled TLS handshake must not outlive the grace deadline")
            .expect("server task joins")
            .expect("server exits cleanly");
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "shutdown waited for the 30-second handshake timeout"
        );

        let mut byte = [0u8; 1];
        let closed = tokio::time::timeout(Duration::from_secs(1), stalled.read(&mut byte))
            .await
            .expect("aborted handshake closes its transport")
            .unwrap_or(0);
        assert_eq!(closed, 0, "stalled handshake transport remained open");
    }

    #[tokio::test]
    async fn resolved_shutdown_stops_the_listener_and_returns() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind test listener");
        let state = Arc::new(ProxyState::new(
            KeyStore::new(),
            ProxyLedger::in_memory(),
            Arc::new(InMemorySink::new()),
            HashMap::new(),
            None,
        ));

        let result = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            serve_listener_with_shutdown(
                state,
                listener,
                async {},
                // This path should finish immediately; the bound is still part of the contract.
                DEFAULT_SHUTDOWN_GRACE,
                None,
            ),
        )
        .await
        .expect("graceful shutdown should not hang");

        result.expect("server shutdown should succeed");
    }

    #[tokio::test]
    async fn grace_expiry_forces_a_stuck_response_to_stop() {
        use axum::extract::State;

        async fn stuck(State(entered): State<Arc<tokio::sync::Notify>>) {
            entered.notify_one();
            std::future::pending::<()>().await;
        }

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind test listener");
        let addr = listener.local_addr().unwrap();
        let entered = Arc::new(tokio::sync::Notify::new());
        let app = Router::new()
            .route("/stuck", get(stuck))
            .with_state(Arc::clone(&entered));
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(serve_router_listener_with_shutdown(
            app,
            listener,
            async move {
                let _ = shutdown_rx.await;
            },
            Duration::from_millis(25),
            ConnectionPolicy::default(),
            None,
        ));
        let client_call = tokio::spawn(async move {
            let _ = reqwest::get(format!("http://{addr}/stuck")).await;
        });
        entered.notified().await;
        shutdown_tx.send(()).unwrap();

        tokio::time::timeout(Duration::from_secs(1), server)
            .await
            .expect("bounded shutdown must return")
            .expect("server task joined")
            .expect("server returned successfully");
        client_call.abort();
    }
}

#[cfg(test)]
mod request_body_limit_tests {
    use super::*;
    use axum::http::Request;
    use sandhi_core::{InMemorySink, KeyStore};
    use tower::ServiceExt;

    fn limited_state(limit: usize) -> Arc<ProxyState> {
        let mut state = ProxyState::new(
            KeyStore::new(),
            ProxyLedger::in_memory(),
            Arc::new(InMemorySink::new()),
            HashMap::new(),
            None,
        );
        state.max_request_body_bytes = limit;
        Arc::new(state)
    }

    #[tokio::test]
    async fn oversized_ai_requests_return_each_sdks_error_shape() {
        let cases = [
            ("/v1/chat/completions", "openai"),
            ("/v1/messages", "anthropic"),
            ("/v1/responses", "responses"),
            ("/v1beta/models/gemini-test:generateContent", "gemini"),
        ];
        for (path, dialect) in cases {
            let response = build_app(limited_state(32))
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri(path)
                        .header("content-type", "application/json")
                        .body(Body::from(vec![b'x'; 33]))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(
                response.status(),
                StatusCode::PAYLOAD_TOO_LARGE,
                "{dialect}"
            );
            let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap();
            let body: Value = serde_json::from_slice(&bytes).unwrap();
            match dialect {
                "anthropic" => {
                    assert_eq!(body["type"], "error");
                    assert_eq!(body["error"]["http_status"], 413);
                }
                "gemini" => {
                    assert_eq!(body["error"]["code"], 413);
                    assert_eq!(body["error"]["status"], "RESOURCE_EXHAUSTED");
                }
                _ => assert_eq!(body["error"]["http_status"], 413),
            }
        }
    }
}

#[cfg(test)]
mod request_admission_tests {
    use super::*;
    use async_trait::async_trait;
    use axum::http::Request;
    use sandhi_core::{ChatResponseV1, ChatStreamEventV1, InMemorySink, KeyStore};
    use sandhi_providers::{ChatEventStream, ChatProvider};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tower::ServiceExt;

    struct StuckProvider {
        calls: AtomicUsize,
        entered: tokio::sync::Notify,
    }

    #[async_trait]
    impl ChatProvider for StuckProvider {
        fn slug(&self) -> &str {
            "stuck"
        }

        async fn complete(
            &self,
            _request: ChatRequestV1,
            _call_headers: axum::http::HeaderMap,
        ) -> Result<ChatResponseV1, ProviderError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.entered.notify_one();
            std::future::pending().await
        }

        async fn stream(
            &self,
            _request: ChatRequestV1,
            _call_headers: axum::http::HeaderMap,
        ) -> Result<ChatEventStream, ProviderError> {
            let stream =
                futures_util::stream::pending::<Result<ChatStreamEventV1, ProviderError>>();
            Ok(Box::pin(stream))
        }
    }

    fn request() -> Request<Body> {
        Request::builder()
            .method("POST")
            .uri("/v1/chat/completions")
            .header("authorization", "Bearer vk-test")
            .header("content-type", "application/json")
            .body(Body::from(
                r#"{"model":"model","messages":[{"role":"user","content":"hi"}]}"#,
            ))
            .unwrap()
    }

    #[tokio::test]
    async fn concurrency_boundary_precedes_body_extraction_and_dispatch() {
        let keys = KeyStore::new();
        keys.insert(VirtualKey {
            id: "vk-test".into(),
            upstream_ref: "stuck".into(),
            ..Default::default()
        });
        let provider = Arc::new(StuckProvider {
            calls: AtomicUsize::new(0),
            entered: tokio::sync::Notify::new(),
        });
        let mut providers = HashMap::new();
        providers.insert("stuck".into(), ProviderHandle::new(provider.clone()));
        let mut state = ProxyState::new(
            keys,
            ProxyLedger::in_memory(),
            Arc::new(InMemorySink::new()),
            providers,
            None,
        );
        state.max_in_flight_ai_requests = 1;
        let app = build_app(Arc::new(state));

        let first = tokio::spawn(app.clone().oneshot(request()));
        provider.entered.notified().await;
        let second = tokio::spawn(app.oneshot(request()));
        tokio::time::sleep(Duration::from_millis(25)).await;

        assert_eq!(
            provider.calls.load(Ordering::SeqCst),
            1,
            "the queued request must not reach decode/dispatch while the permit is held"
        );
        first.abort();
        second.abort();
    }
}

async fn health() -> &'static str {
    "ok"
}

/// Append `x-sandhi-contract-version` to every response (TD-0021 P2, D4/R3).
///
/// A middleware, not per-builder calls: the header must appear on success bodies,
/// upstream-forwarded bytes, streaming bodies, AND dialect-shaped errors — including
/// future error paths that do not exist yet. One layer cannot be forgotten.
async fn contract_version_header(
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let mut response = next.run(request).await;
    response.headers_mut().insert(
        axum::http::HeaderName::from_static("x-sandhi-contract-version"),
        axum::http::HeaderValue::from_static(sandhi_core::CHAT_SCHEMA_VERSION_V1),
    );
    response
}

/// `GET /version` — the ungated HTTP form of the contract handshake (TD-0021 P2, D4).
///
/// What an HTTP consumer needs before its first call, with nothing an unauthenticated
/// caller could use for anything else: the wire (usage-event) and chat contract
/// versions, the additive minor round, and the wired ingress dialects. The capability
/// detail (D5) is gated at `/admin/version` (R2).
async fn version() -> Response {
    let body = json!({
        "wire_contract_version": sandhi_core::UsageEvent::SCHEMA_VERSION,
        "chat_contract_version": sandhi_core::CHAT_SCHEMA_VERSION_V1,
        "chat_contract_minor": sandhi_core::CHAT_CONTRACT_MINOR,
        "dialects": ["openai", "anthropic", "responses", "gemini"],
    });
    Json(body).into_response()
}

#[derive(serde::Deserialize)]
struct CatalogQuery {
    provider: Option<String>,
}

/// Public catalog discovery (TD-0004): curated model descriptors for a provider, facts only
/// (no pricing). Unauthed -- stable public facts, like OpenAI/OpenRouter list-models endpoints.
/// Usage: `GET /catalog/models?provider=anthropic`.
async fn catalog_models(Query(query): Query<CatalogQuery>) -> Response {
    let Some(provider) = query.provider else {
        return error(
            StatusCode::BAD_REQUEST,
            "missing 'provider' query parameter",
        );
    };
    match sandhi_providers::provider_descriptor(&provider) {
        Some(descriptor) => Json(descriptor.models).into_response(),
        None => error(
            StatusCode::NOT_FOUND,
            &format!("unknown provider: {provider}"),
        ),
    }
}

/// ADR-0004 D4 dashboard gate: the read endpoints serve subject/group usage aggregates, so
/// when an admin token is configured they require it (same bearer as `/admin/*`) unless the
/// operator explicitly opted back into the open, masked-only model (`dashboard_public`).
/// No admin token configured → open (nothing to present; single-node dev trust).
#[allow(clippy::result_large_err)] // axum::Response is intentionally large; idiomatic shape.
fn require_dashboard_access(state: &ProxyState, headers: &HeaderMap) -> Result<(), Response> {
    if state.dashboard_public || state.admin_token.is_none() {
        return Ok(());
    }
    operator::require_admin(state, headers)
}

/// Usage aggregates for the dashboard (JSON). 404 when no durable store is configured.
async fn dashboard_api(State(state): State<Arc<ProxyState>>, headers: HeaderMap) -> Response {
    if let Err(denied) = require_dashboard_access(&state, &headers) {
        return denied;
    }
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
        "by_model": store.totals_by_model().unwrap_or_default(),
    });
    Json(payload).into_response()
}

// --- TD-0003 P4 dashboard read-only endpoints ----------------------------------
//
// Auth model: these mirror the self-hosted single-node trust of the existing `/dashboard` HTML and
// `/dashboard/api/usage` — they are **unauthed**, and rely on **masked-only** output as the security
// boundary. The operator binds the proxy to a trusted network / localhost and controls access; no
// secret (raw provider key, virtual-key plaintext, or virtual-key hash) is ever serialized here.
// Programmatic/automated access that needs gating uses the admin-token-protected `/admin/*` routes.
// Units are neutral tokens throughout — no dollars / SKU / tier (the measure-vs-price boundary).

/// `GET /dashboard/api/keys` — masked virtual keys + masked vault entries (no secrets, no hashes).
/// 404 when neither the vault nor the virtual-key store is configured.
async fn dashboard_keys(State(state): State<Arc<ProxyState>>, headers: HeaderMap) -> Response {
    if let Err(denied) = require_dashboard_access(&state, &headers) {
        return denied;
    }
    let (vault, vkeys) = (state.vault.clone(), state.vkeys.clone());
    if vault.is_none() && vkeys.is_none() {
        return error(
            StatusCode::NOT_FOUND,
            "keys dashboard not configured (set SANDHI_STORE)",
        );
    }
    let vkey_records = vkeys
        .as_ref()
        .and_then(|s| s.list().ok())
        .unwrap_or_default()
        .iter()
        .map(operator::vkey_record_response)
        .collect::<Vec<_>>();
    let vault_entries = vault
        .as_ref()
        .and_then(|s| s.list().ok())
        .unwrap_or_default()
        .iter()
        .map(operator::vault_entry_response)
        .collect::<Vec<_>>();
    Json(json!({ "virtual_keys": vkey_records, "vault": vault_entries })).into_response()
}

/// `GET /dashboard/api/budgets` — every configured scope with limit / window / policy + live spent
/// (from the budget ledger). Neutral tokens; no pricing.
async fn dashboard_budgets(State(state): State<Arc<ProxyState>>, headers: HeaderMap) -> Response {
    if let Err(denied) = require_dashboard_access(&state, &headers) {
        return denied;
    }
    let ledger = state.ledger.lock().expect("ledger poisoned");
    let scopes: Vec<Value> = state
        .budgets
        .lock()
        .expect("budgets poisoned")
        .values()
        .map(|spec| {
            let spent = ledger.spent(&spec.scope);
            let limit = spec.limit_tokens;
            json!({
                "scope": spec.scope,
                "limit_tokens": limit,
                "spent": spent,
                "remaining": limit.saturating_sub(spent),
                "window": spec.window,
                "policy": spec.policy,
            })
        })
        .collect();
    Json(json!({ "budgets": scopes })).into_response()
}

/// `GET /dashboard/api/alerts` — recent fired alerts (rules whose threshold has tripped) plus all
/// configured rules. 404 when the alert store is not configured.
async fn dashboard_alerts(State(state): State<Arc<ProxyState>>, headers: HeaderMap) -> Response {
    if let Err(denied) = require_dashboard_access(&state, &headers) {
        return denied;
    }
    let Some(store) = state.alert_store.clone() else {
        return error(
            StatusCode::NOT_FOUND,
            "alerts dashboard not configured (set SANDHI_STORE)",
        );
    };
    let rules = store.list().unwrap_or_default();
    let all: Vec<Value> = rules.iter().map(operator::alert_rule_response).collect();
    let fired: Vec<Value> = rules
        .iter()
        .filter(|r| r.last_fired_at.is_some())
        .map(operator::alert_rule_response)
        .collect();
    Json(json!({ "rules": all, "fired": fired })).into_response()
}

/// The self-hosted single-node dashboard (static HTML; fetches `/dashboard/api/usage`).
async fn dashboard_html() -> Response {
    axum::response::Html(DASHBOARD_HTML).into_response()
}

const DASHBOARD_HTML: &str = r####"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1">
<title>Sandhi — operator dashboard</title>
<style>
  :root {
    color-scheme: light dark;
    --bg: #f7f7f8; --surface: #fff; --border: #e2e2e6; --border-soft: #ececef;
    --text: #16161a; --muted: #6b7280; --accent: #2563eb; --accent-soft: #eff4ff;
    --good: #047857; --good-soft: #ecfdf5; --warn: #b45309; --warn-soft: #fffbeb;
    --bad: #b91c1c; --bad-soft: #fef2f2; --shadow: 0 1px 2px rgb(0 0 0 / 0.04);
  }
  @media (prefers-color-scheme: dark) {
    :root {
      --bg: #0f1115; --surface: #17191f; --border: #2a2d36; --border-soft: #23252c;
      --text: #e7e8ea; --muted: #93949c; --accent: #5b8def; --accent-soft: #182236;
      --good: #34d399; --good-soft: #0d2420; --warn: #f5a524; --warn-soft: #2b2110;
      --bad: #f87171; --bad-soft: #2b1616; --shadow: 0 1px 2px rgb(0 0 0 / 0.4);
    }
  }
  * { box-sizing: border-box; }
  body { font: 14px/1.55 -apple-system, ui-sans-serif, system-ui, sans-serif; margin: 0;
         background: var(--bg); color: var(--text); }
  .wrap { max-width: 1120px; margin-inline: auto; padding: 0 1.5rem 3rem; }
  header { position: sticky; top: 0; z-index: 10; background: var(--bg);
           border-bottom: 1px solid var(--border); padding: 1rem 1.5rem;
           display: flex; align-items: center; gap: 1rem; flex-wrap: wrap; }
  header .brand { display: flex; align-items: baseline; gap: .5rem; margin-right: auto; }
  h1 { font-size: 1.15rem; margin: 0; font-weight: 700; letter-spacing: -.01em; }
  .tagline { color: var(--muted); font-size: .8rem; }
  .token-box { display: flex; align-items: center; gap: .5rem; }
  .token-box input { font: inherit; font-size: .8rem; padding: .4rem .6rem; border-radius: 7px;
    border: 1px solid var(--border); background: var(--surface); color: var(--text); width: 15rem; }
  .dot { width: .5rem; height: .5rem; border-radius: 50%; background: var(--border);
         box-shadow: 0 0 0 3px transparent; transition: background .15s; }
  .dot.on { background: var(--good); }
  h2 { font-size: .95rem; margin: 2.25rem 0 .75rem; font-weight: 700; letter-spacing: -.005em;
       display: flex; align-items: center; gap: .5rem; }
  h2:first-of-type { margin-top: 1.75rem; }
  h2 .hint { font-weight: 400; color: var(--muted); font-size: .78rem; }
  h3 { color: var(--muted); font-size: .74rem; text-transform: uppercase; letter-spacing: .05em;
       margin: 1.1rem 0 .4rem; font-weight: 600; }
  h3:first-child { margin-top: 0; }
  section.panel { background: var(--surface); border: 1px solid var(--border); border-radius: 12px;
                  padding: 1.1rem 1.25rem; box-shadow: var(--shadow); }
  .cards { display: grid; grid-template-columns: repeat(auto-fit, minmax(8.5rem, 1fr));
           gap: .75rem; margin-bottom: .5rem; }
  .card { border: 1px solid var(--border); border-radius: 10px; padding: .9rem 1.1rem;
          background: var(--surface); box-shadow: var(--shadow); }
  .card .n { font-size: 1.5rem; font-weight: 700; font-variant-numeric: tabular-nums; }
  .card .l { color: var(--muted); font-size: .74rem; text-transform: uppercase; letter-spacing: .04em; }
  table { width: 100%; border-collapse: collapse; }
  th, td { text-align: left; padding: .5rem .55rem; border-bottom: 1px solid var(--border-soft);
           vertical-align: middle; }
  th { color: var(--muted); font-weight: 600; font-size: .74rem; text-transform: uppercase;
       letter-spacing: .03em; }
  tbody tr:hover { background: var(--border-soft); }
  td.num, th.num { text-align: right; font-variant-numeric: tabular-nums; }
  .muted { color: var(--muted); }
  .badge { display: inline-block; padding: .1rem .5rem; border-radius: 999px; font-size: .7rem;
           font-weight: 600; border: 1px solid transparent; }
  .badge.active { color: var(--good); background: var(--good-soft); }
  .badge.revoked { color: var(--bad); background: var(--bad-soft); }
  .bar { background: var(--border-soft); border-radius: 6px; height: 7px; overflow: hidden; min-width: 6rem; }
  .bar > span { display: block; height: 100%; background: var(--accent); }
  .bar.warn > span { background: var(--warn); }
  .bar.over > span { background: var(--bad); }
  .fired { background: var(--warn-soft); }
  code { font-size: .82em; background: var(--border-soft); padding: .05rem .35rem; border-radius: 4px; }
  a { color: var(--accent); }
  .btn { font: inherit; font-size: .76rem; font-weight: 600; padding: .3rem .65rem; border-radius: 7px;
         border: 1px solid var(--border); background: var(--surface); color: var(--text); cursor: pointer; }
  .btn:hover:not(:disabled) { border-color: var(--accent); color: var(--accent); }
  .btn:disabled { opacity: .4; cursor: not-allowed; }
  .btn.danger:hover:not(:disabled) { border-color: var(--bad); color: var(--bad); }
  .btn.primary { background: var(--accent); color: #fff; border-color: var(--accent); }
  .btn.primary:hover:not(:disabled) { opacity: .9; }
  details.actions { margin-top: .9rem; border-top: 1px solid var(--border-soft); padding-top: .75rem; }
  details.actions summary { cursor: pointer; font-size: .78rem; font-weight: 600; color: var(--muted); }
  details.actions summary:hover { color: var(--text); }
  .form-row { display: flex; gap: .6rem; flex-wrap: wrap; margin-top: .75rem; align-items: end; }
  .field { display: flex; flex-direction: column; gap: .25rem; }
  .field label { font-size: .72rem; color: var(--muted); }
  .field input, .field select { font: inherit; font-size: .8rem; padding: .35rem .5rem;
    border-radius: 6px; border: 1px solid var(--border); background: var(--bg); color: var(--text); }
  .callout { font-size: .78rem; padding: .5rem .7rem; border-radius: 8px; margin-top: .6rem; }
  .callout.ok { background: var(--good-soft); color: var(--good); }
  .callout.err { background: var(--bad-soft); color: var(--bad); }
  .callout.info { background: var(--accent-soft); color: var(--accent); }
  .toast-wrap { position: fixed; bottom: 1.25rem; right: 1.25rem; display: flex;
                flex-direction: column; gap: .5rem; z-index: 50; }
  .toast { padding: .6rem 1rem; border-radius: 9px; font-size: .82rem; box-shadow: var(--shadow);
           border: 1px solid var(--border); background: var(--surface); animation: slidein .15s ease-out; }
  .toast.ok { border-color: var(--good); color: var(--good); }
  .toast.err { border-color: var(--bad); color: var(--bad); }
  @keyframes slidein { from { transform: translateY(.4rem); opacity: 0; } to { transform: none; opacity: 1; } }
  .tree { list-style: none; margin: 0; padding-left: 0; }
  .tree li { margin: 0; }
  .tree .node { display: flex; align-items: baseline; gap: .6rem; padding: .3rem .4rem;
                border-radius: 6px; border-bottom: 1px solid var(--border-soft); }
  .tree .node:hover { background: var(--border-soft); }
  .tree ul { list-style: none; padding-left: 1.4rem; border-left: 1px dashed var(--border); margin-left: .4rem; }
  .tree .step { font-weight: 600; }
  .tree .stat { color: var(--muted); font-size: .76rem; }
  footer { color: var(--muted); font-size: .76rem; margin-top: 2.5rem; padding-top: 1rem;
           border-top: 1px solid var(--border-soft); }
</style>
</head>
<body>
<header>
  <div class="brand">
    <h1>Sandhi</h1>
    <span class="tagline">the metering layer for AI agents — neutral units, no pricing</span>
  </div>
  <div class="token-box">
    <span class="dot" id="token-dot" title="Admin actions locked until a token is set"></span>
    <input id="admin-token" type="password" placeholder="Admin token — unlocks revoke / ack / add"
           autocomplete="off">
  </div>
</header>
<div class="wrap">

<h2>Overview</h2>
<section class="panel"><div class="cards" id="cards"></div></section>

<div id="tables"></div>

<h2>Declarative config <span class="hint">providers, budgets, alerts &amp; vkeys declared in a committed JSON file</span></h2>
<section class="panel" id="config"></section>

<h2>Virtual keys &amp; credentials <span class="hint">who has access, and where calls actually go</span></h2>
<section class="panel" id="keys"></section>

<h2>Budgets <span class="hint">neutral-token caps per scope</span></h2>
<section class="panel">
  <div id="budgets"></div>
  <details class="actions"><summary>Set a budget</summary>
    <div class="form-row">
      <div class="field"><label>Scope</label><input id="b-scope" placeholder="group:platform"></div>
      <div class="field"><label>Limit (tokens)</label><input id="b-limit" type="number" placeholder="1000000"></div>
      <div class="field"><label>Window</label>
        <select id="b-window"><option value="total">total</option><option value="daily">daily</option>
          <option value="monthly" selected>monthly</option></select></div>
      <div class="field"><label>Policy</label>
        <select id="b-policy"><option value="block" selected>block</option><option value="warn">warn</option></select></div>
      <button class="btn primary" onclick="setBudget()">Set budget</button>
    </div>
    <div id="b-result"></div>
  </details>
</section>

<h2>Alerts <span class="hint">threshold rules on budget scopes</span></h2>
<section class="panel" id="alerts"></section>

<h2>Run cost tree <span class="hint">one agent run's spend, per step, own vs subtree</span></h2>
<section class="panel">
  <div class="form-row" style="margin-top:0">
    <div class="field"><label>Run ID</label><input id="run-id" placeholder="run_01HK..." style="width:16rem"></div>
    <button class="btn primary" onclick="lookupRun()">Look up</button>
  </div>
  <div id="run-tree" style="margin-top:.75rem"></div>
</section>

<footer>
  Read-only data above loads without a token (self-hosted single-node trust; masked secrets only).
  Mutating actions — revoke, acknowledge, mint, add, set — require the admin token and call the
  same <code>/admin/*</code> API the <code>sandhi</code> CLI uses.
</footer>
</div>
<div class="toast-wrap" id="toasts"></div>

<script>
const fmt = n => (n ?? 0).toLocaleString();
const esc = s => String(s ?? "").replace(/[&<>"]/g, c =>
  ({ "&":"&amp;", "<":"&lt;", ">":"&gt;", '"':"&quot;" }[c]));
const orDash = s => (s === null || s === undefined || s === "") ? "—" : esc(s);
// No latency is "—", never "0 ms": a call that never reported a duration is unknown, not fast.
const lat = l => (!l || !l.samples) ? "—"
  : `${fmt(l.p50_ms)} / ${fmt(l.p95_ms)} ms <span class="muted">(n=${fmt(l.samples)})</span>`;

// --- admin token: session-only, unlocks mutating actions -----------------------------------
const tokenEl = document.getElementById("admin-token");
const dotEl = document.getElementById("token-dot");
tokenEl.value = sessionStorage.getItem("sandhi_admin_token") || "";
function refreshTokenState() {
  const on = tokenEl.value.length > 0;
  dotEl.classList.toggle("on", on);
  document.querySelectorAll("[data-needs-token]").forEach(b => b.disabled = !on);
}
tokenEl.addEventListener("input", () => {
  sessionStorage.setItem("sandhi_admin_token", tokenEl.value);
  refreshTokenState();
  loadConfig();
});

function toast(msg, ok) {
  const t = document.createElement("div");
  t.className = "toast " + (ok ? "ok" : "err");
  t.textContent = msg;
  document.getElementById("toasts").appendChild(t);
  setTimeout(() => t.remove(), 4000);
}

// Thin wrapper: attaches the admin bearer, surfaces non-2xx as a toast, returns parsed JSON or null.
async function adminCall(method, path, body) {
  const token = tokenEl.value;
  if (!token) { toast("Enter the admin token first", false); return null; }
  try {
    const resp = await fetch(path, {
      method, headers: { "Authorization": "Bearer " + token, "Content-Type": "application/json" },
      body: body ? JSON.stringify(body) : undefined,
    });
    const data = await resp.json().catch(() => ({}));
    if (!resp.ok) { toast((data.error && data.error.message) || `request failed (${resp.status})`, false); return null; }
    return data;
  } catch (e) { toast("network error: " + e.message, false); return null; }
}

function tbl(title, rows) {
  const body = rows.map(r => `<tr><td>${esc(r.key)}</td><td class="num">${fmt(r.calls)}</td>`
    + `<td class="num">${fmt(r.tokens_in)}</td><td class="num">${fmt(r.tokens_out)}</td>`
    + `<td class="num">${fmt(r.cache_creation_tokens)}</td><td class="num">${fmt(r.cache_read_tokens)}</td>`
    + `<td class="num">${fmt(r.billable_tokens)}</td>`
    + `<td class="num">${lat(r.latency)}</td></tr>`).join("");
  return `<h3>${title}</h3><table><thead><tr><th>key</th><th class="num">calls</th>`
    + `<th class="num">in</th><th class="num">out</th><th class="num">cache write</th>`
    + `<th class="num">cache read</th><th class="num" title="ADR-0005 D4: the quantity budgets `
    + `are enforced on — fresh input + cache split + output (+ unfolded reasoning)">billable`
    + `</th><th class="num" title="p50 / p95 milliseconds over the sampled calls that reported a `
    + `duration — approximate by design; tokens above are exact">latency</th></tr></thead>`
    + `<tbody>${body || '<tr><td colspan=8>no data yet</td></tr>'}</tbody></table>`;
}

function loadUsage() {
  fetch("/dashboard/api/usage").then(r => r.json()).then(d => {
    const t = d.total || { calls: 0, tokens_in: 0, tokens_out: 0, cache_read_tokens: 0, billable_tokens: 0 };
    document.getElementById("cards").innerHTML =
      [["calls", fmt(t.calls)], ["tokens in", fmt(t.tokens_in)], ["tokens out", fmt(t.tokens_out)],
       ["cache read", fmt(t.cache_read_tokens)], ["billable", fmt(t.billable_tokens)],
       ["latency p50/p95", lat(t.latency)]]
      .map(([l, n]) => `<div class="card"><div class="n">${n}</div><div class="l">${l}</div></div>`).join("");
    document.getElementById("tables").innerHTML =
      `<h2>Attribution</h2><section class="panel">`
      + tbl("By user (subject)", d.by_subject || [])
      + tbl("By team (group)", d.by_group || [])
      + tbl("By provider", d.by_provider || [])
      + tbl("By model", d.by_model || [])
      + `</section>`;
  }).catch(() => { document.getElementById("tables").innerHTML =
    '<p class="muted">usage store not configured (set SANDHI_STORE).</p>'; });
}

// Keys: masked virtual keys + vault entries. Never a secret. Revoke/add are admin-gated.
function keysView(d) {
  const vkeys = (d.virtual_keys || []).map(k => {
    const status = k.revoked_at ? "revoked" : "active";
    const disabled = status === "revoked" ? "disabled" : "";
    return `<tr><td><code>${esc(k.id)}</code></td><td>${orDash(k.subject)}</td><td>${orDash(k.group)}</td>`
      + `<td><code>${esc(k.upstream_ref)}</code></td><td>${(k.models||[]).map(esc).join(", ")||'<span class="muted">any</span>'}</td>`
      + `<td><span class="badge ${status}">${status}</span></td><td>${orDash(k.expires_at)}</td>`
      + `<td><button class="btn danger" data-needs-token ${disabled} onclick="revokeVkey('${esc(k.id)}')">Revoke</button></td></tr>`;
  }).join("");
  const vault = (d.vault || []).map(e => `<tr><td><code>${esc(e.credential_id)}</code></td>`
    + `<td>${esc(e.scheme)}</td><td>${orDash(e.base_url)}</td>`
    + `<td><span class="badge ${e.status}">${esc(e.status)}</span></td>`
    + `<td><button class="btn danger" data-needs-token onclick="revokeCred('${esc(e.provider)}','${esc(e.label)}')">Revoke</button></td></tr>`).join("");
  return `<h3>Virtual keys (masked — secrets are never stored)</h3>`
    + `<table><thead><tr><th>id</th><th>subject</th><th>group</th><th>upstream</th><th>models</th><th>status</th><th>expires</th><th></th></tr></thead>`
    + `<tbody>${vkeys || '<tr><td colspan=8>no virtual keys</td></tr>'}</tbody></table>`
    + `<details class="actions"><summary>Mint a virtual key</summary>
        <div class="form-row">
          <div class="field"><label>Upstream</label><input id="v-upstream" placeholder="ollama:default" style="width:9rem"></div>
          <div class="field"><label>Subject</label><input id="v-subject" placeholder="alice"></div>
          <div class="field"><label>Group</label><input id="v-group" placeholder="platform"></div>
          <div class="field"><label>Models (csv)</label><input id="v-models" placeholder="optional"></div>
          <div class="field"><label>Rate/min</label><input id="v-rate" type="number" placeholder="60" style="width:5rem"></div>
          <button class="btn primary" data-needs-token onclick="mintVkey()">Mint</button>
        </div>
        <div id="v-result"></div>
      </details>`
    + `<h3 style="margin-top:1.5rem">Provider credentials (vault metadata)</h3>`
    + `<table><thead><tr><th>credential</th><th>scheme</th><th>base url</th><th>status</th><th></th></tr></thead>`
    + `<tbody>${vault || '<tr><td colspan=5>no provider credentials</td></tr>'}</tbody></table>`
    + `<details class="actions"><summary>Add a credential</summary>
        <div class="form-row">
          <div class="field"><label>Provider</label><input id="c-provider" placeholder="ollama" style="width:7rem"></div>
          <div class="field"><label>Label</label><input id="c-label" placeholder="default" style="width:7rem"></div>
          <div class="field"><label>Base URL</label><input id="c-baseurl" placeholder="http://host:11434" style="width:13rem"></div>
          <div class="field"><label>Secret</label><input id="c-secret" type="password" placeholder="empty is fine for keyless" style="width:12rem"></div>
          <button class="btn primary" data-needs-token onclick="addCredential()">Add</button>
        </div>
        <div id="c-result"></div>
      </details>`;
}
function loadKeys() {
  fetch("/dashboard/api/keys").then(r => r.ok ? r.json() : null).then(d => {
    document.getElementById("keys").innerHTML = d ? keysView(d) : "";
    refreshTokenState();
  }).catch(() => {});
}
async function revokeVkey(id) {
  if (!(await adminCall("DELETE", `/admin/vkeys/${encodeURIComponent(id)}`))) return;
  toast("Virtual key revoked", true); loadKeys();
}
async function revokeCred(provider, label) {
  if (!(await adminCall("DELETE", `/admin/keys/${encodeURIComponent(provider)}/${encodeURIComponent(label)}`))) return;
  toast("Credential revoked", true); loadKeys();
}
async function mintVkey() {
  const models = document.getElementById("v-models").value.trim();
  const rate = document.getElementById("v-rate").value;
  const body = {
    upstream: document.getElementById("v-upstream").value.trim(),
    subject: document.getElementById("v-subject").value.trim() || null,
    group: document.getElementById("v-group").value.trim() || null,
    models: models ? models.split(",").map(s => s.trim()).filter(Boolean) : null,
    rate_limit_per_min: rate ? Number(rate) : null,
  };
  const data = await adminCall("POST", "/admin/keys/share", body);
  if (!data) return;
  document.getElementById("v-result").innerHTML =
    `<div class="callout ok">Minted — copy now, shown once: <code>${esc(data.virtual_key)}</code></div>`;
  loadKeys();
}
async function addCredential() {
  const body = {
    provider: document.getElementById("c-provider").value.trim(),
    label: document.getElementById("c-label").value.trim() || null,
    base_url: document.getElementById("c-baseurl").value.trim() || null,
    secret: document.getElementById("c-secret").value,
  };
  const data = await adminCall("POST", "/admin/keys", body);
  if (!data) return;
  document.getElementById("c-result").innerHTML = `<div class="callout ok">Registered ${esc(data.credential_id || "")}</div>`;
  loadKeys();
}

// Budgets: spent-vs-limit bar + window + policy. Neutral tokens.
function budgetsView(d) {
  const rows = (d.budgets || []).map(b => {
    const limit = b.limit_tokens || 0, spent = b.spent || 0;
    const pct = limit > 0 ? Math.min(100, Math.round(spent * 100 / limit)) : 0;
    const cls = pct >= 100 ? "over" : (pct >= 80 ? "warn" : "");
    return `<tr><td><code>${esc(b.scope)}</code></td>`
      + `<td>${fmt(spent)} <span class="muted">/ ${fmt(limit)}</span></td>`
      + `<td style="min-width:8rem"><div class="bar ${cls}"><span style="width:${pct}%"></span></div></td>`
      + `<td>${esc(b.window)}</td><td>${esc(b.policy)}</td></tr>`;
  }).join("");
  return `<table><thead><tr><th>scope</th><th class="num">spent / limit (tokens)</th><th>utilization</th><th>window</th><th>policy</th></tr></thead>`
    + `<tbody>${rows || '<tr><td colspan=5>no budgets configured</td></tr>'}</tbody></table>`;
}
function loadBudgets() {
  fetch("/dashboard/api/budgets").then(r => r.ok ? r.json() : { budgets: [] }).then(d => {
    document.getElementById("budgets").innerHTML = budgetsView(d);
  }).catch(() => { document.getElementById("budgets").innerHTML =
    '<p class="muted">usage store not configured (set SANDHI_STORE).</p>'; });
}
async function setBudget() {
  const body = {
    scope: document.getElementById("b-scope").value.trim(),
    limit_tokens: Number(document.getElementById("b-limit").value || 0),
    window: document.getElementById("b-window").value,
    policy: document.getElementById("b-policy").value,
  };
  const data = await adminCall("POST", "/admin/budget", body);
  if (!data) return;
  document.getElementById("b-result").innerHTML = `<div class="callout ok">Budget set for ${esc(body.scope)}</div>`;
  loadBudgets();
}

function alertRow(a) {
  const fired = a.last_fired_at
    ? `<span style="color:var(--warn)">${esc(a.last_fired_at)}</span>` : '<span class="muted">never</span>';
  const ackBtn = a.last_fired_at
    ? `<button class="btn" data-needs-token onclick="ackAlert('${esc(a.id)}')">Ack</button>` : "";
  return `<tr ${a.last_fired_at ? 'class="fired"' : ''}><td><code>${esc(a.id)}</code></td>`
    + `<td><code>${esc(a.scope)}</code></td><td class="num">${esc(a.threshold_pct)}%</td>`
    + `<td>${esc(a.channel)}</td><td>${fired}</td><td>${ackBtn}</td></tr>`;
}
function alertsView(d) {
  const fired = (d.fired || []).map(alertRow).join("");
  const rules = (d.rules || []).map(alertRow).join("");
  return `<h3>Recently fired</h3>`
    + `<table><thead><tr><th>id</th><th>scope</th><th class="num">threshold</th><th>channel</th><th>last fired</th><th></th></tr></thead>`
    + `<tbody>${fired || '<tr><td colspan=6>none fired</td></tr>'}</tbody></table>`
    + `<h3 style="margin-top:1.5rem">All configured rules</h3>`
    + `<table><thead><tr><th>id</th><th>scope</th><th class="num">threshold</th><th>channel</th><th>last fired</th><th></th></tr></thead>`
    + `<tbody>${rules || '<tr><td colspan=6>no rules configured</td></tr>'}</tbody></table>`;
}
function loadAlerts() {
  fetch("/dashboard/api/alerts").then(r => r.ok ? r.json() : null).then(d => {
    document.getElementById("alerts").innerHTML = d ? alertsView(d) : "";
    refreshTokenState();
  }).catch(() => {});
}
async function ackAlert(id) {
  if (!(await adminCall("POST", `/admin/alerts/${encodeURIComponent(id)}/ack`))) return;
  toast("Alert acknowledged", true); loadAlerts();
}

// Declarative config: preview needs the admin token just to READ (config_preview is
// admin-gated, unlike the other read-only dashboard panels) since it reflects live vault/vkey
// state, not just masked metadata. Apply is additive-only server-side — see config.rs.
const actionBadge = a => {
  const cls = a === "create" || a === "mint" ? "active" : (a === "update" ? "" : "revoked");
  return `<span class="badge ${cls}" style="${cls ? '' : 'color:var(--muted);background:var(--border-soft)'}">${esc(a)}</span>`;
};
function configPlanTable(title, rows, cols) {
  const body = rows.map(r => `<tr>${cols.map(c => `<td>${orDash(r[c])}</td>`).join("")}<td>${actionBadge(r.action)}</td></tr>`).join("");
  return `<h3>${title}</h3><table><thead><tr>${cols.map(c => `<th>${c}</th>`).join("")}<th>plan</th></tr></thead>`
    + `<tbody>${body || `<tr><td colspan=${cols.length + 1}>none declared</td></tr>`}</tbody></table>`;
}
async function loadConfig() {
  const el = document.getElementById("config");
  if (!tokenEl.value) {
    el.innerHTML = '<p class="muted">Enter the admin token above to preview the declarative config (config_preview reflects live credential/vkey state, so it\'s admin-gated like everything else that isn\'t masked-only).</p>';
    return;
  }
  const data = await adminCall("GET", "/admin/config");
  if (!data) { el.innerHTML = '<p class="muted">no config configured (set SANDHI_CONFIG on the proxy)</p>'; return; }
  el.innerHTML = `<p class="muted" style="margin:0 0 .75rem">${esc(data.path)}</p>`
    + configPlanTable("Providers", data.providers, ["credential_id", "base_url"])
    + configPlanTable("Budgets", data.budgets, ["scope", "limit_tokens"])
    + configPlanTable("Alerts", data.alerts, ["scope", "threshold_pct"])
    + configPlanTable("Virtual keys", data.vkeys, ["upstream", "subject", "group"])
    + `<div class="form-row" style="margin-top:1rem"><button class="btn primary" data-needs-token onclick="applyConfig()">Apply config</button></div>`
    + `<div id="config-result"></div>`;
  refreshTokenState();
}
async function applyConfig() {
  const data = await adminCall("POST", "/admin/config/apply");
  if (!data) return;
  const n = (x) => (x || []).length;
  let summary = `<div class="callout ok">Applied — providers: ${n(data.providers.applied)}, `
    + `budgets: ${n(data.budgets.applied)}, alerts: ${n(data.alerts.created)} created / ${n(data.alerts.skipped)} already satisfied, `
    + `vkeys: ${n(data.vkeys.minted)} minted / ${n(data.vkeys.skipped)} already satisfied</div>`;
  if (n(data.vkeys.minted)) {
    summary += `<div class="callout info">New virtual keys — copy now, shown once:<br>`
      + data.vkeys.minted.map(k => `<code>${esc(k.virtual_key)}</code> (${esc(k.upstream_ref)})`).join("<br>")
      + `</div>`;
  }
  document.getElementById("config-result").innerHTML = summary;
  loadConfig(); loadKeys(); loadBudgets(); loadAlerts();
}

// Run cost tree: recursive own-vs-rollup breakdown for one agentic run. Admin-gated (attribution
// across a whole run can span multiple subjects, so it is treated like any other admin query).
function renderNode(n) {
  const kids = (n.children || []).map(renderNode).join("");
  return `<li><div class="node"><span class="step">${esc(n.step_id)}</span>`
    + `<span class="stat">own ${fmt(n.own && n.own.billable_tokens)} · subtree ${fmt(n.rollup && n.rollup.billable_tokens)} tok</span></div>`
    + (kids ? `<ul>${kids}</ul>` : "") + `</li>`;
}
async function lookupRun() {
  const id = document.getElementById("run-id").value.trim();
  if (!id) return;
  const data = await adminCall("GET", `/admin/usage/run/${encodeURIComponent(id)}`);
  const el = document.getElementById("run-tree");
  if (!data) { el.innerHTML = ""; return; }
  const roots = (data.roots || []).map(renderNode).join("");
  el.innerHTML = `<div class="callout info">Total: ${fmt(data.total && data.total.billable_tokens)} billable tokens across ${fmt(data.total && data.total.calls)} calls</div>`
    + `<ul class="tree" style="margin-top:.6rem">${roots || '<li class="muted">no steps recorded for this run</li>'}</ul>`;
}

refreshTokenState();
loadUsage(); loadKeys(); loadBudgets(); loadAlerts(); loadConfig();
</script>
</body>
</html>
"####;

async fn handle_openai(
    State(state): State<Arc<ProxyState>>,
    permit: Extension<Arc<AdmissionPermit>>,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Response {
    let body = match request_body(body, IngressDialect::OpenAi) {
        Ok(body) => body,
        Err(response) => return *response,
    };
    handle(state, permit.0, headers, body, IngressDialect::OpenAi, None).await
}

async fn handle_anthropic(
    State(state): State<Arc<ProxyState>>,
    permit: Extension<Arc<AdmissionPermit>>,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Response {
    let body = match request_body(body, IngressDialect::Anthropic) {
        Ok(body) => body,
        Err(response) => return *response,
    };
    handle(
        state,
        permit.0,
        headers,
        body,
        IngressDialect::Anthropic,
        None,
    )
    .await
}

async fn handle_responses(
    State(state): State<Arc<ProxyState>>,
    permit: Extension<Arc<AdmissionPermit>>,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Response {
    let body = match request_body(body, IngressDialect::Responses) {
        Ok(body) => body,
        Err(response) => return *response,
    };
    handle(
        state,
        permit.0,
        headers,
        body,
        IngressDialect::Responses,
        None,
    )
    .await
}

/// `GET /metrics` — Prometheus text exposition (TD-0011 P2).
async fn metrics_endpoint(State(state): State<Arc<ProxyState>>, headers: HeaderMap) -> Response {
    if let Err(denied) = require_dashboard_access(&state, &headers) {
        return denied;
    }
    (
        StatusCode::OK,
        [(
            axum::http::header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )],
        state.metrics.render(),
    )
        .into_response()
}

/// `GET /v1/models` — OpenAI and Anthropic discovery (TD-0010 D3).
///
/// The listing is the key's *permitted* models: the upstream catalog intersected with the virtual
/// key's allowlist. A key that may call two models lists two, which makes the allowlist
/// discoverable instead of a surprise 403 at call time, and is honest in a way a static catalog
/// dump is not.
async fn list_models(State(state): State<Arc<ProxyState>>, headers: HeaderMap) -> Response {
    // Same path, two vendors: the credential presentation identifies the client.
    let dialect = if headers.contains_key("x-api-key") {
        IngressDialect::Anthropic
    } else {
        IngressDialect::OpenAi
    };
    let (vk, provider) = match resolve_for_discovery(&state, dialect, &headers) {
        Ok(pair) => pair,
        Err(response) => return response,
    };
    let models = permitted_models(&vk, provider.slug());
    let body = match dialect {
        IngressDialect::Anthropic => json!({
            "data": models.iter().map(|id| json!({
                "type": "model",
                "id": id,
                "display_name": id,
            })).collect::<Vec<_>>(),
            "has_more": false,
        }),
        _ => json!({
            "object": "list",
            "data": models.iter().map(|id| json!({
                "id": id,
                "object": "model",
                "owned_by": provider.slug(),
            })).collect::<Vec<_>>(),
        }),
    };
    Json(body).into_response()
}

/// `GET /v1beta/models` — Gemini discovery (TD-0010 D3).
async fn list_models_gemini(State(state): State<Arc<ProxyState>>, headers: HeaderMap) -> Response {
    let (vk, provider) = match resolve_for_discovery(&state, IngressDialect::Gemini, &headers) {
        Ok(pair) => pair,
        Err(response) => return response,
    };
    let models = permitted_models(&vk, provider.slug());
    Json(json!({
        "models": models.iter().map(|id| json!({
            // Gemini names a model by its resource path, and its SDK strips the prefix back off.
            "name": format!("models/{id}"),
            "displayName": id,
            "supportedGenerationMethods": ["generateContent", "streamGenerateContent"],
        })).collect::<Vec<_>>(),
    }))
    .into_response()
}

/// Shared auth + upstream resolution for the discovery endpoints.
#[allow(clippy::result_large_err)] // axum::Response is intentionally large; idiomatic shape.
fn resolve_for_discovery(
    state: &Arc<ProxyState>,
    dialect: IngressDialect,
    headers: &HeaderMap,
) -> Result<(VirtualKey, ProviderHandle), Response> {
    let Some(token) = dialect.extract_credential(headers) else {
        return Err(ingress_error(
            dialect,
            StatusCode::UNAUTHORIZED,
            &format!(
                "missing virtual key: send it as {}",
                dialect.credential_hint()
            ),
        ));
    };
    let vk = match resolve_virtual_key(state, token) {
        VirtualKeyResolution::Found(vk) => vk,
        VirtualKeyResolution::Expired => {
            return Err(ingress_error(
                dialect,
                StatusCode::UNAUTHORIZED,
                "virtual key expired",
            ));
        }
        VirtualKeyResolution::NotFound => {
            return Err(ingress_error(
                dialect,
                StatusCode::UNAUTHORIZED,
                "unknown virtual key",
            ));
        }
    };
    let Some(provider) = state
        .providers
        .lock()
        .expect("providers poisoned")
        .get(&vk.upstream_ref)
        .cloned()
    else {
        return Err(ingress_error(
            dialect,
            StatusCode::BAD_GATEWAY,
            "no upstream registered for this key",
        ));
    };
    Ok((vk, provider))
}

/// The models this key may actually call: the upstream's catalog filtered by the allowlist.
///
/// With no allowlist the catalog is returned as-is. With an allowlist, entries the catalog does
/// not know about are still listed — the catalog holds transport facts, not an authority on which
/// models exist, so omitting an allowed-but-uncatalogued model would under-report what works.
fn permitted_models(vk: &VirtualKey, slug: &str) -> Vec<String> {
    let catalog: Vec<String> = sandhi_providers::provider_descriptor(slug)
        .map(|d| d.models.into_iter().map(|m| m.id).collect())
        .unwrap_or_default();
    match vk.models.as_deref() {
        None | Some([]) => catalog,
        Some(allowed) => {
            let mut out: Vec<String> = catalog
                .iter()
                .filter(|id| allowed.iter().any(|a| a.eq_ignore_ascii_case(id)))
                .cloned()
                .collect();
            for a in allowed {
                if !out.iter().any(|id| id.eq_ignore_ascii_case(a)) {
                    out.push(a.clone());
                }
            }
            out
        }
    }
}

/// `POST /v1beta/models/{model}:{generateContent|streamGenerateContent}` (TD-0010 D4a).
async fn handle_gemini(
    State(state): State<Arc<ProxyState>>,
    permit: Extension<Arc<AdmissionPermit>>,
    axum::extract::Path(model_method): axum::extract::Path<String>,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Response {
    let body = match request_body(body, IngressDialect::Gemini) {
        Ok(body) => body,
        Err(response) => return *response,
    };
    // Split on the LAST colon: a model id may legitimately contain one (tuned models are
    // `tunedModels/x`), the method never does.
    let Some((model, method)) = model_method.rsplit_once(':') else {
        return ingress_error(
            IngressDialect::Gemini,
            StatusCode::NOT_FOUND,
            "expected /v1beta/models/{model}:generateContent",
        );
    };
    let stream = match method {
        "generateContent" => false,
        "streamGenerateContent" => true,
        // countTokens, embedContent and the rest are not metered surfaces yet; say so in Gemini's
        // own error shape rather than 404-ing with an empty body.
        other => {
            return ingress_error(
                IngressDialect::Gemini,
                StatusCode::NOT_IMPLEMENTED,
                &format!("method '{other}' is not supported by this gateway"),
            );
        }
    };
    if model.is_empty() {
        return ingress_error(
            IngressDialect::Gemini,
            StatusCode::BAD_REQUEST,
            "model is empty",
        );
    }
    let route = GeminiRoute {
        model: model.to_string(),
        stream,
    };
    handle(
        state,
        permit.0,
        headers,
        body,
        IngressDialect::Gemini,
        Some(route),
    )
    .await
}

/// Convert Axum's body-buffering rejection into the caller's native SDK error envelope. A body
/// limit enforced below the handler but rendered as plaintext is observable incompatibility for
/// clients that otherwise see an OpenAI/Anthropic/Gemini-shaped API.
fn request_body(
    body: Result<Bytes, BytesRejection>,
    dialect: IngressDialect,
) -> Result<Bytes, Box<Response>> {
    body.map_err(|rejection| {
        let status = rejection.status();
        let message = if status == StatusCode::PAYLOAD_TOO_LARGE {
            "request body exceeds SANDHI_MAX_REQUEST_BODY_BYTES"
        } else {
            "could not read request body"
        };
        Box::new(ingress_error(dialect, status, message))
    })
}

async fn handle(
    state: Arc<ProxyState>,
    permit: Arc<AdmissionPermit>,
    headers: HeaderMap,
    body: Bytes,
    dialect: IngressDialect,
    gemini_route: Option<GeminiRoute>,
) -> Response {
    // 1. Virtual key, presented the way this dialect's own SDK presents a credential
    //    (TD-0010 D1 — `x-api-key` on `/v1/messages`, `Authorization: Bearer` on the OpenAI
    //    paths). Resolve the live key store by exact token (legacy/demo path, where the id
    //    doubles as the token) then by its hash (operator-minted path, where only the hash is
    //    the lookup key — the plaintext is never retained).
    let Some(vk_token) = dialect.extract_credential(&headers) else {
        // TD-0010 D2 (auth slice): render in the caller's dialect and NAME ITS OWN SCHEME. The
        // flat `{"error":"missing bearer virtual key"}` this used to return was unparseable by
        // two of the three SDKs and told an Anthropic or Gemini client to send a header its
        // vendor never documents. The dialect is already known here — it is a parameter — so
        // there is no ordering problem to solve at these call sites.
        return ingress_error(
            dialect,
            StatusCode::UNAUTHORIZED,
            &format!(
                "missing virtual key: send it as {}",
                dialect.credential_hint()
            ),
        );
    };
    let vk = match resolve_virtual_key(&state, vk_token) {
        VirtualKeyResolution::Found(vk) => vk,
        VirtualKeyResolution::Expired => {
            return ingress_error(dialect, StatusCode::UNAUTHORIZED, "virtual key expired");
        }
        VirtualKeyResolution::NotFound => {
            return ingress_error(dialect, StatusCode::UNAUTHORIZED, "unknown virtual key");
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
        return ingress_error(
            dialect,
            StatusCode::BAD_GATEWAY,
            "no upstream registered for this key",
        );
    };

    // 3. Decode the public ingress dialect into the one canonical runtime request.
    let Ok(body_json) = serde_json::from_slice::<Value>(&body) else {
        return ingress_error(dialect, StatusCode::BAD_REQUEST, "body is not valid JSON");
    };
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
        idempotency_key: header_str(&headers, "idempotency-key"),
        run_id: header_str(&headers, "x-sandhi-run-id"),
        step_id: header_str(&headers, "x-sandhi-step-id"),
        parent_id: header_str(&headers, "x-sandhi-parent-id"),
        trace_context: header_str(&headers, "traceparent"),
    };
    let (mut request, mut wants_stream) = match decode_request(dialect, body_json, metadata) {
        Ok(decoded) => decoded,
        Err(message) => return ingress_error(dialect, StatusCode::BAD_REQUEST, &message),
    };

    // Gemini carries the model and the streaming choice in the PATH, so stamp them onto the
    // decoded request before anything downstream reads them — the allowlist check and the
    // reservation both do.
    if let Some(route) = &gemini_route {
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
        return ingress_error(
            dialect,
            StatusCode::FORBIDDEN,
            &format!(
                "model '{}' is not permitted for this virtual key (allowed models: {})",
                request.model,
                allowed.join(", ")
            ),
        );
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
    let presented_subject = header_str(&headers, "x-sandhi-subject-id");
    let presented_group = header_str(&headers, "x-sandhi-group-id");
    if !vk.permits_attribution(presented_subject.as_deref(), presented_group.as_deref()) {
        return ingress_error(
            dialect,
            StatusCode::FORBIDDEN,
            "attribution is key-bound: x-sandhi-subject-id / x-sandhi-group-id must match this \
             virtual key's subject/group (or be omitted); re-mint the key to change attribution",
        );
    }

    // 5. Reserve a **ceiling** — a conservative upper bound (input estimate + the effective output
    //    max), not a lower-bound estimate (ADR-0005 D1). A call whose worst case would breach the
    //    cap is refused *before* dispatch, so a hard cap cannot be overshot. On a budget-capped
    //    scope where the client left the output unbounded, we also set that bound on the upstream
    //    request so the provider caps output — making the reservation enforceable. The measured
    //    `billable()` (cache split included, D4) replaces the reservation after completion.
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
    // so its plane is nominal). The final plane is recomputed below once we know whether the cap
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
    if let ratelimit::Decision::Limited { retry_after_secs } =
        state.rate_limiter.check(&vk.id, vk.rate_limit_per_min)
    {
        tracing::warn!(
            provider = provider.slug(),
            limit_per_min = vk.rate_limit_per_min,
            retry_after_secs,
            "request rate-limited"
        );
        state.metrics.record_rate_limited(&metrics::Labels {
            provider: provider.slug().into(),
            model: request.model.clone(),
            dialect: dialect_label(dialect),
            plane,
            outcome: "rate_limited",
        });
        return rate_limited_error(dialect, retry_after_secs);
    }

    let scope = budget_scope(&vk);
    let policy = scope_policy(&state, &scope);
    // A scope is "capped" (for output-bounding) only under a hard `Block` cap: a `Warn` soft cap
    // never rejects, so we do not shrink the client's request. Bounding output makes the ceiling
    // reservation enforceable when the client left `max_output_tokens` unset (ADR-0005 D1).
    let (ceiling, effective_max) = reservation_ceiling(&request, body.len());
    // SQLite's transaction remains a synchronous correctness boundary, but it runs on Tokio's
    // blocking pool so its busy timeout never parks an async scheduler worker.
    let (capped, admission) = reserve_budget(&state, &scope, ceiling, policy).await;
    let inject_output_bound = capped && request.max_output_tokens.is_none();
    if inject_output_bound {
        request.max_output_tokens = Some(effective_max);
    }
    // A Block cap is an enforcement boundary. When the client left output unbounded we inject
    // `effective_max` above — but only the translation plane re-encodes `request`, so only it can
    // carry that bound to the upstream (`max_tokens`). The transparent plane forwards the raw body
    // verbatim and would stream unbounded output past the cap (ADR-0005 D1). So a capped,
    // otherwise-same-family call with unbounded output must NOT take the transparent plane.
    let transparent = transparent_eligible && !inject_output_bound;
    let plane = if transparent {
        metrics::Plane::Transparent
    } else {
        metrics::Plane::Translation
    };
    let reservation = match admission {
        Admission::Leased(reservation) => Some(reservation),
        // Fail-open (Warn on a backend error): admit without a lease; the usage event still emits.
        Admission::Unmetered => {
            // ADR-0005 D6. Admitting unmetered is correct under a Warn policy but must never be
            // invisible: without this, a ledger outage looks like normal traffic.
            tracing::warn!(scope = %scope, "admitted WITHOUT a lease (fail-open)");
            state.metrics.record_admitted_unmetered();
            None
        }
        Admission::Denied => {
            // A denial is the one enforcement outcome an operator must be able to see without
            // reading the sink; `scope` is an operator-set budget name, not caller-supplied.
            tracing::warn!(
                scope = %scope,
                ceiling,
                provider = provider.slug(),
                "reservation denied: budget exhausted"
            );
            // Labelled by POLICY, never by scope: a scope may be `vk:<id>`, which is per-key and
            // therefore unbounded (TD-0011 D2).
            state.metrics.record_denied(match policy {
                Policy::Block => "block",
                Policy::Warn => "warn",
            });
            return ingress_error(dialect, StatusCode::TOO_MANY_REQUESTS, "budget exhausted");
        }
    };

    let mut accounting = RequestAccounting::new(
        Arc::clone(&state),
        scope,
        reservation,
        provider.slug().into(),
        &request,
        dialect_label(dialect),
        plane,
    );
    // TD-0021 P4 (D1): the METER records the LOGICAL call once — a repeat of a settled
    // `(vkey, idempotency-key)` inside the window has its duplicate usage event dropped
    // (the original stands). ENFORCEMENT still counts the physical call: the retry really
    // consumed upstream tokens, so its lease settles into spent too. Meter counts logical
    // calls, enforcement counts physical calls — both true, both visible. The client's
    // retry still happens upstream (this is not response caching). Unavailable/expired
    // dedup falls through to counting (D3) — the measurement is never lost to uncertainty.
    if let Some(idem_key) = request.metadata.idempotency_key.clone() {
        accounting.dedup = Some((
            request.metadata.virtual_key_id.clone().unwrap_or_default(),
            idem_key,
        ));
    }
    // TD-0011 D6: which plane served the call is the ADR-0004 adoption signal — how much traffic
    // still re-encodes. Bounded fields only (D2): provider slug and model, never subject/session.
    tracing::debug!(
        provider = provider.slug(),
        model = %request.model,
        plane = if transparent { "transparent" } else { "translation" },
        stream = wants_stream,
        "plane selected"
    );
    let full_error_detail = state.error_detail_full;
    match (transparent, wants_stream) {
        (true, true) => {
            transparent_stream_response(
                provider,
                body,
                request.metadata.session_id.clone(),
                dialect,
                accounting,
                full_error_detail,
                gemini_route,
                permit,
            )
            .await
        }
        (true, false) => {
            transparent_complete_response(
                provider,
                body,
                request.metadata.session_id.clone(),
                dialect,
                accounting,
                full_error_detail,
                gemini_route,
                permit,
            )
            .await
        }
        (false, true) => {
            stream_response(
                provider,
                request,
                dialect,
                accounting,
                full_error_detail,
                permit,
            )
            .await
        }
        (false, false) => {
            complete_response(
                provider,
                request,
                dialect,
                accounting,
                full_error_detail,
                permit,
            )
            .await
        }
    }
}

/// Ingress dialect → a bounded metric label (TD-0011 D2). Four values, fixed by the code.
fn dialect_label(dialect: IngressDialect) -> &'static str {
    match dialect {
        IngressDialect::OpenAi => "openai",
        IngressDialect::Anthropic => "anthropic",
        IngressDialect::Responses => "responses",
        IngressDialect::Gemini => "gemini",
    }
}

/// Ingress dialect → the upstream family it maps to, for plane selection (TD-0006 Step 2).
fn ingress_family(dialect: IngressDialect) -> ProviderFamily {
    match dialect {
        IngressDialect::OpenAi => ProviderFamily::OpenAiCompat,
        IngressDialect::Anthropic => ProviderFamily::Anthropic,
        IngressDialect::Responses => ProviderFamily::OpenAiResponses,
        IngressDialect::Gemini => ProviderFamily::Gemini,
    }
}

/// The upstream path suffix for a same-family transparent forward — mirrors each typed adapter's
/// endpoint. Only the three ingress families above ever reach the transparent plane.
fn upstream_path(family: ProviderFamily, gemini: Option<&GeminiRoute>) -> String {
    match family {
        ProviderFamily::OpenAiCompat => "/chat/completions".to_string(),
        ProviderFamily::OpenAiResponses => "/responses".to_string(),
        ProviderFamily::Anthropic => "/v1/messages".to_string(),
        // Gemini is the first dialect whose upstream path is not a constant: the model and the
        // streaming choice are path segments, so the route the client asked for determines the
        // route we forward to. `?alt=sse` is what makes the stream SSE rather than a chunked
        // JSON array — the framing the adapter's usage sniffer expects.
        ProviderFamily::Gemini => match gemini {
            Some(route) if route.stream => format!(
                "/v1beta/models/{}:streamGenerateContent?alt=sse",
                route.model
            ),
            Some(route) => format!("/v1beta/models/{}:generateContent", route.model),
            None => "/".to_string(),
        },
        _ => "/".to_string(),
    }
}

/// What a Gemini ingress request carries in its PATH rather than its body.
///
/// The other three dialects read the model from a body field and the streaming choice from a
/// `stream` flag; Gemini puts both in the URL (`/v1beta/models/{model}:generateContent`). That
/// difference reaches the allowlist check, the reservation, and the upstream URL, so it is
/// carried explicitly instead of being re-parsed at each site.
#[derive(Debug, Clone)]
struct GeminiRoute {
    model: String,
    stream: bool,
}

/// Rebuild an axum response from a raw upstream response: status + the curated header allowlist +
/// body bytes, forwarded verbatim (the transparent plane never re-serializes the response body).
///
/// The provider layer already filters headers at construction
/// ([`filter_response_headers`][sandhi_providers::raw::filter_response_headers]); this enforces
/// the **same** allowlist at the egress boundary too — defense-in-depth, so a `RawResponse` that
/// one day bypassed that constructor still cannot surface a non-allowlisted header (an upstream
/// `openai-organization`, `server`, or credential header) to a client.
fn raw_response_to_axum(
    raw: sandhi_providers::raw::RawResponse,
    dialect: IngressDialect,
) -> Response {
    let headers = sandhi_providers::raw::filter_response_headers(&raw.headers);
    let mut builder = Response::builder().status(raw.status);
    for (name, value) in headers.iter() {
        builder = builder.header(name, value);
    }
    builder.body(Body::from(raw.body)).unwrap_or_else(|_| {
        ingress_error(
            dialect,
            StatusCode::BAD_GATEWAY,
            "invalid upstream response",
        )
    })
}

/// Transparent same-family non-streaming plane: forward the client's bytes verbatim, meter usage
/// at the source, and return the upstream response unchanged (ADR-0004 D1). Enforcement rides on
/// `accounting` exactly as on the typed path.
#[allow(clippy::too_many_arguments)]
async fn transparent_complete_response(
    provider: ProviderHandle,
    body: Bytes,
    session: Option<String>,
    dialect: IngressDialect,
    mut accounting: RequestAccounting,
    full_error_detail: bool,
    gemini: Option<GeminiRoute>,
    permit: Arc<AdmissionPermit>,
) -> Response {
    // Unary path: the permit lives exactly as long as this handler future, which is the whole
    // call. Held only so the compiler sees it alive to the last await.
    let _permit = permit;
    let Some(forwarder) = provider.raw_forwarder() else {
        accounting.set_outcome("error");
        accounting.finalize();
        return ingress_error(
            dialect,
            StatusCode::BAD_GATEWAY,
            "transparent plane requires a raw forwarder",
        );
    };
    // Only the neutral conversation key crosses this seam (ADR-0008 D3): it maps onto the
    // catalog-declared vendor affinity header when one exists. Attribution (subject/group/
    // virtual key) is key-authoritative metering input consumed by `usage_event`, never
    // forwarded (ADR-0001 §4).
    match forwarder
        .forward_metered(
            &upstream_path(provider.family(), gemini.as_ref()),
            body,
            session.as_deref(),
            Some(accounting.request_id.as_str()),
        )
        .await
    {
        Ok((raw, mut usage)) => {
            usage.completeness = UsageCompleteness::Final;
            usage.outcome.get_or_insert_with(|| "success".into());
            accounting.observe(&usage);
            accounting.set_outcome("success");
            accounting.finalize();
            raw_response_to_axum(raw, dialect)
        }
        Err(err) => {
            accounting.set_outcome("error");
            accounting.finalize();
            provider_error(&err, dialect, provider.slug(), full_error_detail)
        }
    }
}

/// Transparent same-family streaming plane: forward the upstream SSE bytes verbatim while the
/// metered stream accumulates usage at the source; the terminal frame finalizes the reservation. A
/// mid-stream disconnect settles the accrued (byte-approximate) partial via the `Drop` finalizer
/// rather than releasing to zero (ADR-0005 D1), as on the typed streaming path.
#[allow(clippy::too_many_arguments)]
async fn transparent_stream_response(
    provider: ProviderHandle,
    body: Bytes,
    session: Option<String>,
    dialect: IngressDialect,
    mut accounting: RequestAccounting,
    full_error_detail: bool,
    gemini: Option<GeminiRoute>,
    permit: Arc<AdmissionPermit>,
) -> Response {
    let Some(forwarder) = provider.raw_forwarder() else {
        accounting.set_outcome("error");
        accounting.finalize();
        return ingress_error(
            dialect,
            StatusCode::BAD_GATEWAY,
            "transparent plane requires a raw forwarder",
        );
    };
    // Only the neutral conversation key crosses this seam (ADR-0008 D3): it maps onto the
    // catalog-declared vendor affinity header when one exists. Attribution is metering
    // input, never forwarded (ADR-0001 §4).
    let raw = match forwarder
        .forward_stream_metered(
            &upstream_path(provider.family(), gemini.as_ref()),
            body,
            session.as_deref(),
            Some(accounting.request_id.as_str()),
        )
        .await
    {
        Ok(raw) => raw,
        Err(err) => {
            accounting.set_outcome("error");
            accounting.finalize();
            return provider_error(&err, dialect, provider.slug(), full_error_detail);
        }
    };
    let mut upstream = raw.stream;

    let body_stream = async_stream::stream! {
        // TD-0014 P2: the admission slot lives in the BODY, not the handler future. It is
        // released whenever this generator drops — completion, client disconnect, or
        // graceful-drain cancellation — and the gauge tracks exactly that lifetime.
        let _permit = permit;
        let _open = accounting.state.metrics.stream_open_guard();
        let mut seen_usage = false;
        let mut delta_bytes: u64 = 0;
        while let Some(item) = upstream.next().await {
            match item {
                Ok(chunk) => {
                    if let Some(parsed) = chunk.usage {
                        // Terminal frame: the finalized, source-measured usage.
                        let mut usage: UsageV2 = parsed.into();
                        usage.completeness = UsageCompleteness::Final;
                        usage.outcome.get_or_insert_with(|| "success".into());
                        accounting.observe(&usage);
                        seen_usage = true;
                    } else if !chunk.data.is_empty() {
                        // Running Partial so a disconnect settles accrued spend. `usage_running`
                        // carries whatever the family has already announced — for Anthropic that
                        // is input plus the full cache split from `message_start`, which is the
                        // dominant term on a cached prompt and used to be settled as zero
                        // (TD-0013 D4).
                        delta_bytes = delta_bytes.saturating_add(chunk.data.len() as u64);
                        if !seen_usage {
                            accounting.observe(&partial_usage(chunk.usage_running, delta_bytes));
                        }
                    }
                    if !chunk.data.is_empty() {
                        yield Ok::<Bytes, std::io::Error>(chunk.data);
                    }
                }
                Err(_) => {
                    accounting.set_outcome("error");
                    break;
                }
            }
        }
        if accounting.outcome != "error" {
            accounting.set_outcome("success");
        }
        accounting.finalize();
    };

    let headers = sandhi_providers::raw::filter_response_headers(&raw.headers);
    let mut builder = Response::builder().status(raw.status);
    for (name, value) in headers.iter() {
        builder = builder.header(name, value);
    }
    if !headers.contains_key("content-type") {
        builder = builder.header("content-type", "text/event-stream");
    }
    builder
        .body(Body::from_stream(body_stream))
        .expect("valid streaming response")
}

/// The enforcement policy configured for a scope (from the operator budgets map). Drives D6
/// fail-open/closed and whether the scope is a hard `Block` cap. Unset → `Block` (the safe default).
fn scope_policy(state: &ProxyState, scope: &str) -> Policy {
    state
        .budgets
        .lock()
        .ok()
        .and_then(|budgets| budgets.get(scope).map(|spec| Policy::parse(&spec.policy)))
        .unwrap_or(Policy::Block)
}

/// Reserve a ceiling lease for one in-flight call (ADR-0005 D1). A poisoned ledger lock is treated
/// as a backend failure and resolved by D6: `Warn` fails open (unmetered admit), `Block` fails
/// closed (deny).
async fn reserve_budget(
    state: &Arc<ProxyState>,
    scope: &str,
    ceiling: u64,
    policy: Policy,
) -> (bool, Admission) {
    let state = Arc::clone(state);
    let scope = scope.to_string();
    match tokio::task::spawn_blocking(move || match state.ledger.lock() {
        Ok(mut ledger) => {
            let capped = policy == Policy::Block && ledger.limit(&scope).is_some();
            let admission = ledger.reserve(&scope, ceiling, OffsetDateTime::now_utc(), policy);
            (capped, admission)
        }
        Err(_) => (false, ledger_failure_admission(policy)),
    })
    .await
    {
        Ok(result) => result,
        Err(error) => {
            tracing::error!(%error, "budget admission blocking task failed");
            (false, ledger_failure_admission(policy))
        }
    }
}

fn ledger_failure_admission(policy: Policy) -> Admission {
    match policy {
        Policy::Warn => Admission::Unmetered,
        Policy::Block => Admission::Denied,
    }
}

/// Mark a synchronous correctness operation as blocking when running on Tokio's multithreaded
/// runtime. Tests and embedders using a current-thread/no Tokio runtime execute inline instead;
/// `block_in_place` would panic there.
fn blocking_section<T>(operation: impl FnOnce() -> T) -> T {
    let multithreaded = tokio::runtime::Handle::try_current()
        .map(|handle| handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread)
        .unwrap_or(false);
    if multithreaded {
        tokio::task::block_in_place(operation)
    } else {
        operation()
    }
}

/// Owns the reservation and guarantees one terminal usage observation even when an HTTP body is
/// abandoned. Counts are always measured; an unavailable observation releases the reservation.
struct RequestAccounting {
    /// Bounded metric dimensions for this call (TD-0011 D2) — set once at dispatch.
    dialect: &'static str,
    plane: metrics::Plane,
    state: Arc<ProxyState>,
    scope: String,
    /// The held lease to settle by id (ADR-0005 D2). `None` when the scope admitted fail-open with
    /// no durable lease (D6) — nothing to settle.
    reservation: Option<Reservation>,
    provider: String,
    model: String,
    /// TD-0021 P4: when the call carried an `idempotency-key` AND this proxy already
    /// settled that `(vkey, key)` inside the window, the repeat is the SAME logical call
    /// — reusing the original settlement, metered once (D1). `None` otherwise.
    dedup: Option<(String, String)>,
    /// Sandhi's id for THIS call, minted at admission (not lazily at event assembly) so it can
    /// be sent upstream on the vendor's declared correlation header and then become the usage
    /// event's `request_id` — one string correlating the upstream's logs and sandhi's event
    /// (ADR-0008 D6; also the seam TD-0021 G20's reconcile-once dedup wants).
    request_id: String,
    metadata: RequestMetadataV1,
    usage: Option<UsageV2>,
    outcome: &'static str,
    finalized: bool,
    /// Scope 5 OTel recorder (a clone of `state.otel`); `None` when the feature is off or
    /// unconfigured. Held per-call so finalize records without touching `state` again.
    otel: Option<Arc<otel::OtelRecorder>>,
    /// The gen_ai operation span opened at dispatch, closed (with usage attrs) in finalize.
    otel_span: Option<otel::SpanHandle>,
    /// The response finish reason, captured on the typed translation plane (complete/stream) for
    /// `gen_ai.response.finish_reasons`. `None` on the transparent byte paths (no ChatResponseV1).
    finish_reason: Option<FinishReasonV1>,
}

impl RequestAccounting {
    fn new(
        state: Arc<ProxyState>,
        scope: String,
        reservation: Option<Reservation>,
        provider: String,
        request: &ChatRequestV1,
        dialect: &'static str,
        plane: metrics::Plane,
    ) -> Self {
        // Scope 5: open the gen_ai operation span at dispatch if OTel export is on. Request-time
        // attributes only (system + request.model + operation); usage/response attrs are added when
        // the span is closed in finalize. None of the attribute keys come from the request body.
        let (otel, otel_span) = match state.otel.as_ref() {
            Some(recorder) => (
                Some(Arc::clone(recorder)),
                Some(recorder.start_span(&provider, &request.model)),
            ),
            None => (None, None),
        };
        Self {
            state,
            scope,
            reservation,
            provider,
            model: request.model.clone(),
            dedup: None,
            request_id: next_request_id(),
            metadata: request.metadata.clone(),
            usage: None,
            outcome: "cancelled",
            finalized: false,
            dialect,
            plane,
            otel,
            otel_span,
            finish_reason: None,
        }
    }

    /// The bounded label set for this call (TD-0011 D2).
    fn metric_labels(&self) -> metrics::Labels {
        metrics::Labels {
            provider: self.provider.clone(),
            model: self.model.clone(),
            dialect: self.dialect,
            plane: self.plane,
            outcome: self.outcome,
        }
    }

    fn observe(&mut self, usage: &UsageV2) {
        self.usage = Some(usage.clone());
    }

    fn set_outcome(&mut self, outcome: &'static str) {
        self.outcome = outcome;
    }

    /// Whether this call's ledger was the volatile in-memory arm (dedup unavailable).
    fn reservation_is_volatile(&self) -> bool {
        matches!(self.state.ledger.lock().map(|l| l.is_volatile()), Ok(true))
    }

    /// Per-call wire headers for the typed plane (TD-0022 D1, caller-owned injection):
    /// this call's minted id on the vendor's declared correlation header (ADR-0008 D6;
    /// empty when the upstream declares none) plus the caller's W3C `traceparent`, so the
    /// upstream can emit a *child* of the caller's span and the echoed trace context
    /// genuinely links back. (The transparent plane rebuilds request headers from transport
    /// config and does not forward the caller's traceparent — a known, documented gap.)
    fn per_call_wire_headers(&self) -> HeaderMap {
        let mut out = HeaderMap::new();
        if let Some(name) = sandhi_providers::client_request_id_header(&self.provider) {
            if let (Ok(name), Ok(value)) = (
                axum::http::HeaderName::try_from(name),
                axum::http::HeaderValue::from_str(&self.request_id),
            ) {
                out.insert(name, value);
            }
        }
        if let Some(traceparent) = self
            .metadata
            .trace_context
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            if let Ok(value) = axum::http::HeaderValue::from_str(traceparent) {
                out.insert(
                    axum::http::header::HeaderName::from_static("traceparent"),
                    value,
                );
            }
        }
        out
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
                if let Some(writer) = &self.state.alert_writer {
                    writer.mark_fired(alert.rule_id.clone());
                } else {
                    // In-process embeddings/tests may choose the direct store. The standalone
                    // proxy always installs the bounded writer when persistence is configured.
                    let _ = store.mark_fired(&alert.rule_id);
                }
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
        // Settle against the single neutral `billable()` (cache split included, ADR-0005 D4) so the
        // ledger and the emitted usage event count the same quantity. An unmeasured (failed /
        // cancelled) call settles `0`, which releases the lease without recording spend.
        let actual = if measured { billable(&usage) } else { 0 };
        // TD-0013 D6: the settled quantity is the measured one, even when it exceeds the ceiling
        // the call was admitted against — but the overshoot is counted.
        //
        // Clamping was the obvious move and is wrong. A ceiling is built from `input_estimate`,
        // which is bytes/4 of the request; a provider's tokenization of the same prompt can exceed
        // it (notably for scripts averaging ~3 bytes/char). Clamping would silently discard the
        // difference — recreating, one layer down, exactly the defect this TD exists to remove.
        //
        // You cannot simultaneously guarantee "spend never exceeds the cap" and "a real
        // measurement is never lost", once the provider can report more than was reserved.
        // Sandhi's product is the measurement, and the count feeds a downstream ledger it must not
        // lie to; the cap is a control that recovers on its own, because the overshoot is bounded
        // by one call and the *next* reservation is refused. So: record the truth, and make the
        // under-reservation visible instead of paying for it in silence.
        if let Some(reservation) = &self.reservation {
            if actual > reservation.ceiling {
                self.state
                    .metrics
                    .observe_settle_overshoot(actual - reservation.ceiling);
            }
        }
        // Settle the lease by id (idempotent, ADR-0005 D2), then capture the post-settle spent for
        // the alert subsystem. Alerts evaluate only on a measured call.
        let mut dedup_reused = false;
        let spent_after = blocking_section(|| {
            let mut spent_after: Option<u64> = None;
            if let Ok(mut ledger) = self.state.ledger.lock() {
                // TD-0021 P4 (D1): settle-then-record under one lock. A repeat arriving
                // between this call's admission and settlement finds the record only after
                // it exists — the insert is the linearization point.
                if let Some(reservation) = &self.reservation {
                    ledger.settle(reservation, actual);
                }
                if let Some((vkey, idem_key)) = &self.dedup {
                    if ledger.seen(vkey, idem_key).is_some() {
                        dedup_reused = true;
                    } else {
                        let reservation_id = self.reservation.as_ref().map_or(0, |r| r.id);
                        ledger.record(vkey, idem_key, reservation_id, actual);
                    }
                }
                if measured {
                    spent_after = Some(ledger.spent(&self.scope));
                }
            }
            spent_after
        });
        if dedup_reused {
            // D1: the repeat is the same LOGICAL call — its usage event is a duplicate
            // and is DROPPED here (the original event stands). Visible, never silent.
            tracing::debug!("idempotent retry metered against the original logical call");
            self.state.metrics.record_idempotent_replay();
            self.finalized = true;
            return;
        }
        if self.dedup.is_some() && self.reservation_is_volatile() {
            // D3's acceptance criterion: a counted fallback is a METRIC, not just a
            // log line. (The volatile arm cannot dedup; the call counts.)
            self.state.metrics.record_idempotent_fallback();
        }
        // P2: evaluate threshold alerts against the settled spend (best-effort — never breaks the
        // request). The configured limit comes from the budgets metadata map so a `Warn` scope (no
        // hard cap in the in-memory ledger) still has a threshold to measure against.
        if let Some(spent) = spent_after {
            let limit = self
                .state
                .budgets
                .lock()
                .ok()
                .and_then(|budgets| budgets.get(&self.scope).map(|spec| spec.limit_tokens));
            self.fire_alerts(spent, limit);
        }
        // TD-0011 D3: `actual` is exactly what the ledger settled, so the metric cannot disagree
        // with the charge. Recording here (not at observe) means one sample per logical call.
        self.state.metrics.observe_call(
            &self.metric_labels(),
            metrics::CallMeasurements {
                fresh_input: usage.tokens_in,
                cache_creation: usage.cache_creation_tokens,
                cache_read: usage.cache_read_tokens,
                output: usage.tokens_out,
                reasoning: usage.reasoning_tokens.unwrap_or(0),
                billable: actual,
                estimated: usage.basis == UsageBasis::Estimated,
                duration_ms: usage.duration_ms,
                ttft_ms: usage.time_to_first_token_ms,
            },
        );
        // Scope 5 (TD-0011 P3): record the gen_ai span + metrics from the same settled usage, one
        // OTel sample per logical call — the same single-emission discipline as `observe_call`.
        // Best-effort like the metric path: OTel must never fail the request.
        if let (Some(recorder), Some(span)) = (self.otel.as_ref(), self.otel_span.as_mut()) {
            recorder.record_usage(
                span,
                &self.provider,
                &self.model,
                &usage,
                self.finish_reason,
            );
        }
        self.state.sink.emit(&usage_event(
            &self.provider,
            &self.model,
            &self.metadata,
            &usage,
            Some(self.request_id.as_str()),
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
    full_error_detail: bool,
    permit: Arc<AdmissionPermit>,
) -> Response {
    let _permit = permit; // unary: held by the handler future, which is the whole call
    let correlation = accounting.per_call_wire_headers();
    match provider.complete_with(request, correlation).await {
        Ok(mut response) => {
            response.usage.completeness = UsageCompleteness::Final;
            response
                .usage
                .outcome
                .get_or_insert_with(|| "success".into());
            accounting.observe(&response.usage);
            accounting.finish_reason = response.finish_reason;
            accounting.set_outcome("success");
            accounting.finalize();
            Json(encode_response(dialect, &response)).into_response()
        }
        Err(error) => {
            accounting.set_outcome("error");
            accounting.finalize();
            provider_error(&error, dialect, provider.slug(), full_error_detail)
        }
    }
}

async fn stream_response(
    provider: ProviderHandle,
    request: ChatRequestV1,
    dialect: IngressDialect,
    mut accounting: RequestAccounting,
    full_error_detail: bool,
    permit: Arc<AdmissionPermit>,
) -> Response {
    let correlation = accounting.per_call_wire_headers();
    let mut upstream = match provider.stream_with(request, correlation).await {
        Ok(s) => s,
        Err(error) => {
            accounting.set_outcome("error");
            accounting.finalize();
            return provider_error(&error, dialect, provider.slug(), full_error_detail);
        }
    };

    let body = async_stream::stream! {
        // TD-0014 P2: admission slot rides the body — see the transparent twin above.
        let _permit = permit;
        let _open = accounting.state.metrics.stream_open_guard();
        let mut last_usage: Option<UsageV2> = None;
        // What the family has reported so far, for families that report before the end
        // (TD-0013 D3). `None` for a terminal-only family, for the whole stream.
        let mut running_reported: Option<ParsedUsage> = None;
        // A non-final `Usage` event is accounting-only and must not reach the client (TD-0013 D7):
        // the ingress wire shape is a TD-0010 parity guarantee, and a metering improvement that
        // adds frames to a caller's stream has broken something more important than it fixed.
        let mut accounting_only;
        let mut delta_out_bytes: u64 = 0;
        while let Some(item) = upstream.next().await {
            match item {
                Ok(event) => {
                    accounting_only = false;
                    match &event {
                        sandhi_core::ChatStreamEventV1::Usage { usage }
                            if usage.completeness != UsageCompleteness::Final =>
                        {
                            // Progress, not a verdict: it must not supersede the terminal frame.
                            running_reported = Some(reported_parts(usage));
                            accounting_only = true;
                        }
                        sandhi_core::ChatStreamEventV1::Usage { usage } => {
                            // Terminal, authoritative usage — replaces any running partial estimate.
                            accounting.observe(usage);
                            last_usage = Some(usage.clone());
                        }
                        sandhi_core::ChatStreamEventV1::TextDelta { delta }
                        | sandhi_core::ChatStreamEventV1::ReasoningDelta { delta }
                        | sandhi_core::ChatStreamEventV1::RefusalDelta { delta }
                        | sandhi_core::ChatStreamEventV1::ToolCallArgumentsDelta { delta, .. } => {
                            delta_out_bytes = delta_out_bytes.saturating_add(delta.len() as u64);
                        }
                        sandhi_core::ChatStreamEventV1::Error { .. } => {
                            accounting.set_outcome("error");
                        }
                        sandhi_core::ChatStreamEventV1::Finish { reason } => {
                            // Scope 5: capture the finish reason for `gen_ai.response.finish_reasons`.
                            accounting.finish_reason = Some(*reason);
                        }
                        _ => {}
                    }
                    // ADR-0005 D1: hold a running `Partial` until the terminal usage arrives, so a
                    // mid-stream disconnect (which fires the Drop finalizer, not the code below)
                    // settles the accumulated spend instead of releasing to zero — closing the
                    // open-stream / read-a-lot / disconnect metering-evasion hole. Per-category:
                    // real numbers where the family has reported them, the byte estimate only for
                    // output and only as far as it must (TD-0013 D4). The terminal frame overrides.
                    if last_usage.is_none() {
                        accounting.observe(&partial_usage(running_reported, delta_out_bytes));
                    }
                    if accounting_only {
                        continue;
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

/// Coarse input-token estimate: bytes of the ingress request body / 4. This replaces a full
/// re-serialization of the decoded `messages`+`tools` on every request (design audit A4) with a
/// zero-allocation length read. Direction vs the old formula: dialect envelopes wrap the same
/// content in their own field names, so for everything except schema-less tools the wire body
/// dominates the neutral serialization — the exception is the decoders' injected default tool
/// schema (`{"type":"object"}`, +31 bytes of neutral with no wire counterpart per schema-less
/// tool), which can make the estimate up to ~8 tokens *lower* per schema-less tool than the old
/// formula. That deficit is bounded, measured, and pinned by
/// `body_length_estimate_stays_within_a_bounded_deficit` — and marginal against the
/// `DEFAULT_OUTPUT_CEILING`-dominated ceiling, the load-bearing term (ADR-0005 D1); the /4
/// heuristic's own accuracy error dwarfs it either way (undercounts CJK, overcounts verbose
/// schemas). A model-aware/tokenizer estimator remains the follow-up.
fn input_estimate(body_len: usize) -> u64 {
    (body_len as u64).saturating_add(3) / 4
}

/// The reservation **ceiling** (ADR-0005 D1): input estimate + the effective output max (the
/// client's `max_output_tokens`, or [`DEFAULT_OUTPUT_CEILING`] when unbounded). Returns the ceiling
/// and the effective max so the caller can bound a capped scope's upstream request. This is a
/// conservative upper bound, not the old `+ 1` lower-bound estimate that let streams overshoot.
fn reservation_ceiling(request: &ChatRequestV1, body_len: usize) -> (u64, u64) {
    let effective_max = request.max_output_tokens.unwrap_or(DEFAULT_OUTPUT_CEILING);
    let ceiling = input_estimate(body_len)
        .saturating_add(effective_max)
        .max(1);
    (ceiling, effective_max)
}

/// A trimmed header value as an owned `String`, or `None` when absent/non-UTF-8/empty.
fn header_str(headers: &HeaderMap, name: &str) -> Option<String> {
    let value = headers.get(name)?.to_str().ok()?.trim();
    (!value.is_empty()).then(|| value.to_string())
}

/// The running `Partial` usage for a stream that has not delivered its terminal frame, used to
/// settle an interrupted stream (ADR-0005 D1) rather than releasing the reservation to zero.
///
/// Only reached when a stream ends early — a client disconnect or a mid-stream transport error. The
/// provider's terminal frame carries the real numbers and a `Final` observation always supersedes
/// this.
///
/// **The fallback is per-category, not per-call** (TD-0013 D4). `reported` is whatever the family
/// has actually announced so far ([`StreamChunk::usage_running`]), and it is used wherever it
/// exists:
///
/// - **Input, cache-creation and cache-read are taken or zero — never estimated.** No byte count
///   observed on the *response* can stand in for the tokenization of a prompt, and inventing one
///   would make Sandhi a second meter disagreeing with the provider. Anthropic announces all three
///   on `message_start`, before a single content byte, so for that family they are exact here.
/// - **Output is `max(reported, byte estimate)`.** Anthropic's `message_delta` lags the text it
///   describes, so between `message_start` and the first delta the reported output is legitimately
///   `0` while real output has flowed. Taking the max never settles below the byte-only behaviour
///   this replaced.
///
/// **The byte factor is 4 bytes per token, and its bias depends on the plane.** On the transparent
/// plane the bytes are wire bytes — SSE framing, JSON punctuation and field names included — so the
/// estimate runs *above* the true count, which is the safe direction for enforcement. On the typed
/// plane the caller counts *decoded* delta strings, which is near-neutral for English and a
/// substantial **under**-count for scripts averaging ~3 bytes/char (e.g. CJK). The result is tagged
/// [`UsageCompleteness::Partial`] either way, so a consumer can always tell an interrupted call
/// from a completed one.
///
/// For the families that report nothing until the end (OpenAI Chat and Responses, Cohere, Ollama)
/// `reported` is `None` and the estimate stands alone. That is a policy call, not an accuracy one:
/// settling zero would be the honest measurement and would let a caller stream-and-abort
/// repeatedly for free.
/// Narrow a typed `Usage` event back to the raw per-category counts.
///
/// The typed plane carries `UsageV2` while the transparent plane carries [`ParsedUsage`]; reducing
/// to the latter lets both planes share one fallback rule (TD-0013 D4) rather than growing two that
/// can drift. Only the categories a family reports mid-stream survive the narrowing, which is
/// exactly the set the fallback is allowed to use.
fn reported_parts(usage: &UsageV2) -> ParsedUsage {
    ParsedUsage {
        tokens_in: usage.tokens_in,
        tokens_out: usage.tokens_out,
        cache_creation_tokens: usage.cache_creation_tokens,
        cache_read_tokens: usage.cache_read_tokens,
        reasoning_tokens: usage.reasoning_tokens.unwrap_or(0),
    }
}

fn partial_usage(reported: Option<ParsedUsage>, delta_out_bytes: u64) -> UsageV2 {
    let estimated_out = delta_out_bytes.saturating_add(3) / 4;
    let Some(reported) = reported else {
        return UsageV2 {
            tokens_out: estimated_out,
            completeness: UsageCompleteness::Partial,
            basis: UsageBasis::Estimated,
            ..UsageV2::default()
        };
    };
    UsageV2 {
        tokens_in: reported.tokens_in,
        cache_creation_tokens: reported.cache_creation_tokens,
        cache_read_tokens: reported.cache_read_tokens,
        tokens_out: reported.tokens_out.max(estimated_out),
        reasoning_tokens: (reported.reasoning_tokens > 0).then_some(reported.reasoning_tokens),
        completeness: UsageCompleteness::Partial,
        // `Estimated` means "at least one category came from the byte fallback" — so the label
        // tracks whether the estimate actually contributed, not merely whether one was available.
        // A disconnect after `message_delta`, where every category is the provider's, is a real
        // measurement of an incomplete call and must not be tarred as a guess.
        basis: if estimated_out > reported.tokens_out {
            UsageBasis::Estimated
        } else {
            UsageBasis::ProviderReported
        },
        ..UsageV2::default()
    }
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
    minted_request_id: Option<&str>,
) -> UsageEvent {
    // Identity precedence: the upstream's own id when it gave one, else the id minted at
    // admission (the same string sent upstream on the vendor's correlation header, ADR-0008
    // D6), else a late mint for callers outside the accounting path.
    let request_id = usage
        .upstream_request_id
        .clone()
        .or_else(|| minted_request_id.map(str::to_string))
        .unwrap_or_else(next_request_id);
    UsageEvent::new(
        request_id,
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
    .with_identity(
        metadata.idempotency_key.clone(),
        metadata.run_id.clone(),
        metadata.step_id.clone(),
        metadata.parent_id.clone(),
        metadata.trace_context.clone(),
    )
    .with_tokens(usage.tokens_in, usage.tokens_out)
    .with_cache(usage.cache_creation_tokens, usage.cache_read_tokens)
    .with_measurement(
        usage.completeness,
        usage.attempts,
        usage.outcome.clone(),
        usage.upstream_request_id.clone(),
    )
    .with_basis(usage.basis)
    .with_latency(usage.duration_ms, usage.time_to_first_token_ms)
}

#[cfg(test)]
mod usage_event_tests {
    use super::*;
    use sandhi_core::{RequestMetadataV1, UsageV2};

    #[test]
    fn latency_measured_at_the_adapter_boundary_survives_into_the_persisted_event() {
        // TD-0009 P1's own acceptance criterion is "latency carries samples" — this was
        // silently dropped because usage_event()'s builder chain never called
        // .with_latency(), even though UsageV2 carries it (typed.rs stamps it
        // unconditionally) and the store/query layer is fully built to consume it.
        let usage = UsageV2 {
            duration_ms: Some(1234),
            time_to_first_token_ms: Some(56),
            ..UsageV2::default()
        };
        let event = usage_event(
            "ollama",
            "gpt-oss:20b",
            &RequestMetadataV1::default(),
            &usage,
            None,
        );
        assert_eq!(event.duration_ms, Some(1234));
        assert_eq!(event.time_to_first_token_ms, Some(56));
    }

    #[test]
    fn absent_latency_stays_absent_not_zero() {
        // Zero would read as "instant" on the dashboard's latency_cell(); None means
        // "this provider/path never reported timing" and must render as "—" instead.
        let usage = UsageV2::default();
        let event = usage_event(
            "ollama",
            "gpt-oss:20b",
            &RequestMetadataV1::default(),
            &usage,
            None,
        );
        assert_eq!(event.duration_ms, None);
        assert_eq!(event.time_to_first_token_ms, None);
    }
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

fn provider_error(
    e: &ProviderError,
    dialect: IngressDialect,
    provider: &str,
    full_detail: bool,
) -> Response {
    // TD-0021 P3 (D6): the redaction decision lives on the type — this wrapper exists so
    // the seven call sites (and the dialect-shaping tests) read unchanged.
    let error = if full_detail {
        codec::IngressError::from_provider_full(e, provider)
    } else {
        codec::IngressError::from_provider_redacted(e, provider)
    };
    error.render(dialect)
}

/// A dialect-shaped 429 carrying `Retry-After` (TD-0012 D4).
///
/// The header is the point: both the OpenAI and Anthropic SDKs honour it for backoff, and a 429
/// without one makes a well-behaved client retry immediately — turning a throttle into a hot loop
/// that costs more than the traffic it was meant to bound.
fn rate_limited_error(dialect: IngressDialect, retry_after_secs: u64) -> Response {
    let mut response = ingress_error(
        dialect,
        StatusCode::TOO_MANY_REQUESTS,
        &format!("rate limit exceeded; retry in {retry_after_secs}s"),
    );
    if let Ok(value) = retry_after_secs.to_string().parse() {
        response.headers_mut().insert("retry-after", value);
    }
    response
}

fn ingress_error(dialect: IngressDialect, status: StatusCode, msg: &str) -> Response {
    // TD-0021 P3 (D6): one construction type owns rendering; the 22 call sites unchanged.
    codec::IngressError::invalid(status, msg).render(dialect)
}

fn error(status: StatusCode, msg: &str) -> Response {
    (status, Json(json!({ "error": msg }))).into_response()
}

#[cfg(test)]
mod error_detail_tests {
    use super::*;

    fn upstream_error() -> ProviderError {
        ProviderError::Upstream {
            status: 400,
            body: Some(r#"{"error":{"message":"tool call id call_9 not found"}}"#.to_owned()),
            request_id: Some("req_abc".to_owned()),
        }
    }

    #[tokio::test]
    async fn redacted_by_default_keeps_code_status_drops_body() {
        let response = provider_error(&upstream_error(), IngressDialect::OpenAi, "moonshot", false);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let error = &value["error"];
        assert_eq!(error["code"], "upstream_error");
        assert_eq!(error["http_status"], 400);
        assert_eq!(error["message"], "upstream error");
        assert!(
            error["details"]
                .as_object()
                .map(|d| d.is_empty())
                .unwrap_or(true),
            "redacted mode must not leak details: {error}"
        );
        assert!(!bytes.windows(9).any(|w| w == b"tool call"), "body leaked");
    }

    #[tokio::test]
    async fn redaction_is_a_property_of_the_type_not_the_call_site() {
        // TD-0021 P3 (D6) acceptance: the DEFAULT construction path cannot leak an
        // upstream body regardless of which constructor a future error path reaches
        // for. Drive the type directly — not the provider_error wrapper — so the pin
        // holds even if every wrapper is deleted.
        for dialect in [
            IngressDialect::OpenAi,
            IngressDialect::Responses,
            IngressDialect::Anthropic,
            IngressDialect::Gemini,
        ] {
            let response =
                codec::IngressError::from_provider_redacted(&upstream_error(), "moonshot")
                    .render(dialect);
            let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap();
            assert!(
                !bytes.windows(9).any(|w| w == b"tool call"),
                "{dialect:?}: redacted default leaked the upstream body"
            );
            assert!(
                !bytes.windows(7).any(|w| w == b"call_9"),
                "{dialect:?}: redacted default leaked the request-internal id"
            );
        }
    }

    #[tokio::test]
    async fn protocol_refusals_never_carry_upstream_bytes() {
        // The invalid_request constructor is for operator/protocol refusals; it has no
        // upstream payload to leak by construction, and renders identically to the old
        // inline builder (the dialect-shaping contract).
        let response =
            codec::IngressError::invalid(StatusCode::UNAUTHORIZED, "invalid virtual key")
                .render(IngressDialect::Anthropic);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["type"], "error");
        assert_eq!(value["error"]["code"], "invalid_request");
        assert_eq!(value["error"]["http_status"], 401);
        assert_eq!(value["error"]["message"], "invalid virtual key");
    }

    #[tokio::test]
    async fn full_detail_forwards_bounded_body() {
        let response = provider_error(&upstream_error(), IngressDialect::OpenAi, "moonshot", true);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let details = &value["error"]["details"];
        assert!(
            details["upstream_body"]
                .as_str()
                .unwrap_or("")
                .contains("call_9"),
            "full mode must forward the bounded body: {value}"
        );
    }
}

#[cfg(test)]
mod header_egress_tests {
    use super::*;
    use axum::http::{HeaderName, HeaderValue};
    use bytes::Bytes;
    use sandhi_providers::raw::RawResponse;

    /// Build a `RawResponse` bypassing `filter_response_headers`, so the egress filter is what is
    /// under test (not the provider-layer constructor).
    fn raw_with(headers: &[(&str, &str)]) -> RawResponse {
        let mut map = HeaderMap::new();
        for (k, v) in headers {
            map.insert(
                HeaderName::from_bytes(k.as_bytes()).unwrap(),
                HeaderValue::from_str(v).unwrap(),
            );
        }
        RawResponse {
            status: 200,
            body: Bytes::from_static(b"{}"),
            headers: map,
        }
    }

    #[test]
    fn raw_response_enforces_allowlist_at_egress() {
        // A RawResponse that bypassed the provider-layer filter still cannot leak a
        // non-allowlisted header to the client (defense-in-depth at the egress boundary).
        let raw = raw_with(&[
            ("content-type", "application/json"), // allowlisted → forwarded
            ("x-should-retry", "true"),           // allowlisted → forwarded
            ("openai-organization", "org_x"),     // NOT allowlisted → stripped
            ("server", "cloudflare"),             // NOT allowlisted → stripped
            ("set-cookie", "session=secret"),     // NOT allowlisted → stripped
        ]);
        let resp = raw_response_to_axum(raw, IngressDialect::OpenAi);
        let h = resp.headers();
        assert_eq!(h.get("content-type").unwrap(), "application/json");
        assert_eq!(h.get("x-should-retry").unwrap(), "true");
        for leaked in ["openai-organization", "server", "set-cookie"] {
            assert!(
                h.get(leaked).is_none(),
                "egress leaked upstream header {leaked}"
            );
        }
    }
}

#[cfg(test)]
mod partial_accounting_tests {
    use super::*;

    /// The counts a real Anthropic `message_start` announces before any content streams —
    /// the numbers taken verbatim from `tests/fixtures/anthropic/stream_cache_split.sse`.
    const AT_MESSAGE_START: ParsedUsage = ParsedUsage {
        tokens_in: 1024,
        tokens_out: 0,
        cache_creation_tokens: 2048,
        cache_read_tokens: 4096,
        reasoning_tokens: 0,
    };

    /// The audit flagged this factor as unverified. It is an estimate by construction; what must be
    /// true is that it is tagged as one and biased in the safe direction.
    #[test]
    fn the_byte_estimate_is_marked_partial_and_biased_high() {
        // Roughly a small SSE frame's worth of wire bytes, with nothing reported (the
        // terminal-only families: OpenAI Chat and Responses, Cohere, Ollama).
        let usage = partial_usage(None, 400);
        assert_eq!(
            usage.completeness,
            UsageCompleteness::Partial,
            "an estimate must never be indistinguishable from a measured call"
        );
        assert_eq!(usage.tokens_out, 100, "4 wire bytes per token");
        // Wire bytes include SSE framing and JSON punctuation, so on the transparent plane the
        // estimate exceeds the true token count — over-consuming an abandoned stream's budget
        // rather than leaking capacity that was genuinely used.
        assert!(
            usage.tokens_out >= 400 / 4,
            "the estimate must not round below the byte-derived floor"
        );
        // Nothing is invented on the input side: with nothing reported, a disconnect says nothing
        // about prompt tokens, and guessing would make Sandhi a second meter.
        assert_eq!(usage.tokens_in, 0);
        assert_eq!(usage.cache_read_tokens, 0);
    }

    #[test]
    fn an_empty_stream_estimates_nothing() {
        let usage = partial_usage(None, 0);
        assert_eq!(
            usage.tokens_out, 0,
            "no bytes delivered means no output to charge"
        );
    }

    /// The defect TD-0013 exists to fix. Anthropic announces input and the whole cache split on
    /// `message_start`, before a single content byte — so at a disconnect there those numbers are
    /// exact, and the old byte-only fallback recorded all three as zero.
    #[test]
    fn reported_input_and_cache_are_settled_not_estimated_away() {
        // A disconnect right after `message_start`: 40 bytes of output have flowed, nothing more.
        let usage = partial_usage(Some(AT_MESSAGE_START), 40);

        assert_eq!(usage.tokens_in, 1024, "reported input must survive");
        assert_eq!(usage.cache_creation_tokens, 2048);
        assert_eq!(usage.cache_read_tokens, 4096);

        // `billable()` counts all four categories (ADR-0005 D4). The byte-only fallback would have
        // settled 10 here; the real exposure is three orders of magnitude larger, which is what
        // made this an evasion vector rather than a rounding error.
        let billed = billable(&usage);
        assert_eq!(billed, 1024 + 2048 + 4096 + 10);
        assert!(
            billed > 100 * (40 / 4),
            "settling the byte estimate alone would discard the dominant term"
        );
    }

    /// `max`, not "reported wins": Anthropic's `message_delta` lags the text it describes, so
    /// between `message_start` and the first delta the reported output is legitimately 0 while real
    /// output has flowed. Taking the reported value alone would settle *less* than the byte-only
    /// fallback this replaced — a regression disguised as an accuracy fix.
    #[test]
    fn output_never_settles_below_the_byte_floor() {
        let usage = partial_usage(Some(AT_MESSAGE_START), 400);
        assert_eq!(
            usage.tokens_out, 100,
            "reported output of 0 must not erase 400 bytes of streamed text"
        );

        // Once the provider does report output, and it exceeds the byte guess, the real number
        // wins — the estimate is a floor, never a ceiling.
        let after_delta = ParsedUsage {
            tokens_out: 256,
            ..AT_MESSAGE_START
        };
        assert_eq!(partial_usage(Some(after_delta), 400).tokens_out, 256);
    }

    /// D5 — the label tracks whether the estimate actually contributed, not merely whether one
    /// was available. Laundering a guess into an authoritative-looking number is the failure mode
    /// this field exists to prevent; so is tarring a real measurement as a guess.
    #[test]
    fn basis_distinguishes_a_measurement_from_a_guess() {
        // Nothing reported: the output number is purely byte-derived.
        assert_eq!(
            partial_usage(None, 400).basis,
            UsageBasis::Estimated,
            "a byte-derived count must never present as provider-reported"
        );

        // Reported input and cache, but output still estimated (the message_start window).
        assert_eq!(
            partial_usage(Some(AT_MESSAGE_START), 400).basis,
            UsageBasis::Estimated
        );

        // Every category reported, and the provider's output exceeds the byte floor: this is a
        // real measurement of an incomplete call, not a guess.
        let after_delta = ParsedUsage {
            tokens_out: 4_000,
            ..AT_MESSAGE_START
        };
        let usage = partial_usage(Some(after_delta), 400);
        assert_eq!(usage.basis, UsageBasis::ProviderReported);
        assert_eq!(
            usage.completeness,
            UsageCompleteness::Partial,
            "still incomplete — `basis` and `completeness` are independent axes"
        );
    }

    /// The default must match what every pre-TD-0013 code path actually did, or adding the field
    /// silently relabels historical events.
    #[test]
    fn the_default_basis_is_provider_reported() {
        assert_eq!(UsageV2::default().basis, UsageBasis::ProviderReported);
        assert_eq!(
            sandhi_core::UsageEvent::new("r", "t", "p", "m", Backend::External).usage_basis,
            UsageBasis::ProviderReported
        );
    }

    /// The narrowing that lets both planes share one fallback rule must not silently drop a
    /// category, or the typed plane would quietly meter less than the transparent one.
    #[test]
    fn narrowing_a_typed_usage_preserves_every_category() {
        let typed = UsageV2 {
            tokens_in: 11,
            tokens_out: 22,
            cache_creation_tokens: 33,
            cache_read_tokens: 44,
            reasoning_tokens: Some(55),
            ..UsageV2::default()
        };
        let parts = reported_parts(&typed);
        assert_eq!(parts.tokens_in, 11);
        assert_eq!(parts.tokens_out, 22);
        assert_eq!(parts.cache_creation_tokens, 33);
        assert_eq!(parts.cache_read_tokens, 44);
        assert_eq!(parts.reasoning_tokens, 55);
    }
}

#[cfg(all(test, feature = "otel-otlp"))]
mod otel_wiring_tests {
    //! The chokepoint integration: a request whose metadata carries the full attribution set
    //! (`subject_id`/`group_id`/`session_id`/`virtual_key_id`) flows through
    //! `RequestAccounting::new` → `finalize`, and the exported gen_ai span carries the token
    //! values but NONE of that attribution. This is the guarantee the recorder unit tests (in
    //! `otel::tests`) cannot give alone — they drive the recorder in isolation, not through the
    //! one path that actually has the attribution in hand.
    use super::*;
    use crate::metrics::Plane;
    use crate::otel::OtelRecorder;
    use opentelemetry::metrics::MeterProvider as _;
    use opentelemetry::trace::TracerProvider as _;
    use opentelemetry_sdk::metrics::SdkMeterProvider;
    use opentelemetry_sdk::trace::{InMemorySpanExporter, SdkTracerProvider};
    use sandhi_core::{InMemorySink, KeyStore, RequestMetadataV1, UsageCompleteness, UsageV2};

    /// Strings that must never reach the exported span — both the forbidden attribute *keys* and
    /// the actual attribution *values* present on this request's metadata.
    const FORBIDDEN: &[&str] = &[
        "subject_id",
        "group_id",
        "session_id",
        "virtual_key_id",
        "request_id",
        "alice",
        "platform",
        "sess-42",
        "vk_demo",
        "cost",
        "price",
        "usd",
    ];

    #[test]
    fn finalize_exports_genai_span_with_no_request_attribution() {
        let exporter = InMemorySpanExporter::default();
        let provider = SdkTracerProvider::builder()
            .with_simple_exporter(exporter.clone())
            .build();
        // A readerless meter is fine — this test inspects the span, not metrics (covered in
        // `otel::tests`); the instruments just no-op.
        let meter = SdkMeterProvider::builder().build().meter("test");
        let recorder = Arc::new(OtelRecorder::new(meter, provider.tracer("test")));

        let mut state = ProxyState::new(
            KeyStore::new(),
            ProxyLedger::in_memory(),
            Arc::new(InMemorySink::new()),
            HashMap::new(),
            None,
        );
        state.otel = Some(recorder);
        let state = Arc::new(state);

        // A request whose metadata carries the full attribution set — exactly what must NEVER
        // reach the exported span.
        let mut request: ChatRequestV1 =
            serde_json::from_str(r#"{"model":"gpt-otel","messages":[]}"#).unwrap();
        request.metadata = RequestMetadataV1 {
            subject_id: Some("alice".into()),
            group_id: Some("platform".into()),
            session_id: Some("sess-42".into()),
            virtual_key_id: Some("vk_demo".into()),
            ..Default::default()
        };

        let mut acc = RequestAccounting::new(
            Arc::clone(&state),
            "vk:alice".into(),
            None,
            "openai".into(),
            &request,
            "openai",
            Plane::Translation,
        );
        acc.observe(&UsageV2 {
            tokens_in: 40,
            cache_read_tokens: 60,
            tokens_out: 20,
            upstream_request_id: Some("resp_upstream_9".into()),
            completeness: UsageCompleteness::Final,
            ..Default::default()
        });
        // complete_response / the stream `Finish` event set this on the typed plane.
        acc.finish_reason = Some(FinishReasonV1::Stop);
        acc.set_outcome("success");
        acc.finalize();
        drop(acc);

        let spans = exporter.get_finished_spans().expect("span exported");
        assert_eq!(spans.len(), 1, "one gen_ai span per finalized call");
        let span = &spans[0];
        let blob = format!("{} {:?}", span.name, span.attributes);
        for f in FORBIDDEN {
            assert!(
                !blob.contains(f),
                "attribution/cost `{f}` leaked into the exported span: {blob}"
            );
        }
        // The trustworthy numbers DID make it through: input = fresh(40) + cache_read(60) = 100,
        // and gen_ai.response.id is the UPSTREAM id (never Sandhi's request_id).
        assert!(blob.contains("gen_ai.usage.input_tokens"));
        assert!(blob.contains("gen_ai.system"));
        assert!(blob.contains("resp_upstream_9"));
        // gen_ai.response.finish_reasons carries the mapped reason (string[]).
        assert!(
            blob.contains("gen_ai.response.finish_reasons") && blob.contains("stop"),
            "finish_reason did not land on the span: {blob}"
        );
    }
}

#[cfg(test)]
mod connection_policy_tests {
    use super::*;

    #[test]
    fn trusted_proxy_spec_parses_cidr_lists() {
        assert!(parse_trusted_proxies("").is_empty());
        assert!(parse_trusted_proxies("  ").is_empty());
        let nets = parse_trusted_proxies("10.0.0.0/8, 192.168.1.0/24");
        assert_eq!(nets.len(), 2);
        assert_eq!(nets[0].to_string(), "10.0.0.0/8");
    }

    #[test]
    #[should_panic(expected = "not a valid CIDR")]
    fn trusted_proxy_spec_rejects_garbage_loudly() {
        parse_trusted_proxies("10.0.0.0/8,bogus");
    }

    /// The WIRING test for the pure function above (adversarial review,
    /// finding 1): the pure function was tested while the middleware was
    /// shipped with a hard-coded `None` — dead wiring the pure tests could not
    /// see. This drives the REAL `resolve_client_ip` middleware through a real
    /// Router with a manual PeerCtx (as the accept loop inserts it).
    #[tokio::test]
    async fn client_ip_middleware_believes_xff_only_from_trusted_proxies() {
        use axum::extract::Request;
        use tower::ServiceExt;

        let mut state = ProxyState::new(
            KeyStore::new(),
            crate::ProxyLedger::Memory(sandhi_core::InMemoryLedger::new()),
            std::sync::Arc::new(sandhi_core::InMemorySink::new()),
            HashMap::new(),
            None,
        );
        state.trusted_proxies = parse_trusted_proxies("10.0.0.0/8");
        let state = std::sync::Arc::new(state);

        // Through the PRODUCTION ingress function, not a hand-built router —
        // the reviewer's point: the middleware could vanish from build_app and
        // a hand-built test would stay green. (build_app itself goes through
        // ingress_routes too; the cfg(test) probe route lives there.)
        let app = ingress_routes(&state).with_state(state);

        let probe = |peer: std::net::IpAddr, forwarded: Option<&str>| {
            let mut req = Request::builder()
                .uri("/__client_probe")
                .body(axum::body::Body::empty())
                .unwrap();
            req.extensions_mut().insert(PeerCtx { peer });
            if let Some(header) = forwarded {
                req.headers_mut()
                    .insert("x-forwarded-for", header.parse().unwrap());
            }
            app.clone().oneshot(req)
        };

        // Untrusted peer: the header is attacker-controlled and NEVER believed.
        let outsider: std::net::IpAddr = "203.0.113.7".parse().unwrap();
        let resp = probe(outsider, Some("1.2.3.4")).await.unwrap();
        assert_eq!(
            String::from_utf8_lossy(&axum::body::to_bytes(resp.into_body(), 64).await.unwrap()),
            "203.0.113.7"
        );

        // Trusted peer + header: the first hop is the client.
        let trusted: std::net::IpAddr = "10.0.0.9".parse().unwrap();
        let resp = probe(trusted, Some("1.2.3.4")).await.unwrap();
        assert_eq!(
            String::from_utf8_lossy(&axum::body::to_bytes(resp.into_body(), 64).await.unwrap()),
            "1.2.3.4"
        );

        // Trusted peer, no header: the peer IS the client.
        let resp = probe(trusted, None).await.unwrap();
        assert_eq!(
            String::from_utf8_lossy(&axum::body::to_bytes(resp.into_body(), 64).await.unwrap()),
            "10.0.0.9"
        );
    }

    #[test]
    fn forwarded_for_is_believed_only_from_trusted_proxies() {
        let allowlist = parse_trusted_proxies("10.0.0.0/8");
        let peer: std::net::IpAddr = "10.0.0.9".parse().unwrap();
        let outsider: std::net::IpAddr = "203.0.113.7".parse().unwrap();

        // Untrusted peer: the header is attacker-controlled, never believed.
        assert_eq!(
            resolve_forwarded_for(outsider, Some("1.2.3.4"), &allowlist),
            None
        );
        // Trusted peer without a header: the peer IS the client.
        assert_eq!(resolve_forwarded_for(peer, None, &allowlist), None);
        // Trusted peer + header: the first hop is the original client.
        assert_eq!(
            resolve_forwarded_for(peer, Some("1.2.3.4, 10.0.0.1"), &allowlist),
            Some("1.2.3.4".parse().unwrap())
        );
        // Trusted peer + garbage header: nothing to believe.
        assert_eq!(
            resolve_forwarded_for(peer, Some("not-an-ip"), &allowlist),
            None
        );
        // A claimed client that is itself inside the trusted set is refused
        // (multi-hop proxy chains are an explicit non-goal in P3).
        assert_eq!(
            resolve_forwarded_for(peer, Some("10.0.0.1"), &allowlist),
            None
        );
    }

    /// Design audit A4: `input_estimate` divides the ingress body length instead of
    /// re-serializing the decoded prompt. Adversarial review of this change falsified the
    /// original "wire body is a superset" claim: all three non-Gemini decoders inject a default
    /// `{"type":"object"}` schema for schema-less tools (+31 bytes of neutral with no wire
    /// counterpart), so Anthropic's bare tool form and Responses' flat form can carry LESS wire
    /// than their neutral serialization. What actually holds — and what this pins — is a
    /// **bounded deficit**: generous shapes dominate outright; the adversarial shapes
    /// (schema-less tools, bare-string system, empty messages) stay within ~8 tokens of the old
    /// formula per schema-less tool, noise against the DEFAULT_OUTPUT_CEILING-dominated
    /// ceiling (ADR-0005 D1). A codec that widens the deficit beyond that bound fails here.
    #[test]
    fn body_length_estimate_stays_within_a_bounded_deficit() {
        const TEXT: &str = "You are a careful assistant that explains reservation ceilings, \
             lease semantics, and the difference between what the meter counts and what the \
             ledger settles, at length and with examples.";
        const USER: &str = "Please explain the reservation ceiling semantics in detail, \
             covering the input estimate, the output bound, and the calendar window.";
        let tool_schema: serde_json::Value = serde_json::json!({
            "type": "object",
            "properties": {"query": {"type": "string", "description": "the thing to look up"}}
        });
        let cases: &[(&str, IngressDialect, serde_json::Value)] = &[
            (
                "openai",
                IngressDialect::OpenAi,
                serde_json::json!({
                    "model": "gpt-x",
                    "messages": [
                        {"role": "system", "content": TEXT},
                        {"role": "user", "content": USER}
                    ],
                    "tools": [{"type": "function", "function": {
                        "name": "lookup",
                        "description": "Look something up in the shared index",
                        "parameters": tool_schema
                    }}]
                }),
            ),
            (
                "anthropic",
                IngressDialect::Anthropic,
                serde_json::json!({
                    "model": "claude-x",
                    "system": TEXT,
                    "messages": [
                        {"role": "user", "content": [{"type": "text", "text": USER}]}
                    ],
                    "tools": [{
                        "name": "lookup",
                        "description": "Look something up in the shared index",
                        "input_schema": tool_schema
                    }]
                }),
            ),
            (
                "responses",
                IngressDialect::Responses,
                serde_json::json!({
                    "model": "gpt-x",
                    "input": [
                        {"role": "system", "content": [{"type": "input_text", "text": TEXT}]},
                        {"role": "user", "content": [{"type": "input_text", "text": USER}]}
                    ],
                    "tools": [{"type": "function",
                        "name": "lookup",
                        "description": "Look something up in the shared index",
                        "parameters": tool_schema
                    }]
                }),
            ),
            (
                "gemini",
                IngressDialect::Gemini,
                serde_json::json!({
                    "systemInstruction": {"parts": [{"text": TEXT}]},
                    "contents": [{"role": "user", "parts": [{"text": USER}]}],
                    "tools": [{"functionDeclarations": [{
                        "name": "lookup",
                        "description": "Look something up in the shared index",
                        "parameters": {"type": "OBJECT",
                            "properties": {"query": {"type": "STRING"}}}
                    }]}]
                }),
            ),
        ];
        for (name, dialect, body) in cases {
            let wire_len = serde_json::to_string(body).unwrap().len();
            let (request, _) =
                crate::codec::decode_request(*dialect, body.clone(), RequestMetadataV1::default())
                    .unwrap_or_else(|e| panic!("{name} fixture failed to decode: {e}"));
            let serialized = serde_json::to_vec(&request.messages).unwrap().len()
                + serde_json::to_vec(&request.tools).unwrap().len();
            assert!(
                wire_len >= serialized,
                "{name}: wire body {wire_len} bytes < neutral serialization {serialized} bytes \
                 — the body-length estimate would not be conservative (ADR-0005 D1)"
            );
            // And the estimate itself: body/4 covers the old serialized/4 formula.
            assert!(input_estimate(wire_len) >= (serialized as u64).saturating_add(3) / 4);
        }

        // Adversarial shapes (from the review that falsified the superset claim): schema-less
        // tools inject a default schema into the neutral form with no wire counterpart, a
        // bare-string system adds a role wrapper the wire never carried, and an empty messages
        // array is accepted. These may legitimately sit BELOW the neutral serialization — the
        // pin is that the estimate's deficit vs the old formula stays bounded by the injected
        // schemas (~8 tokens per schema-less tool = 31 bytes / 4, rounded up).
        let bare_tool = |i: usize| serde_json::json!({"name": format!("a{i}")});
        let adversarial: &[(&str, IngressDialect, serde_json::Value, usize)] = &[
            (
                "anthropic-schemaless-tools",
                IngressDialect::Anthropic,
                serde_json::json!({
                    "model": "claude-x",
                    "system": "be terse",
                    "messages": [],
                    "tools": [bare_tool(0), bare_tool(1), bare_tool(2), bare_tool(3), bare_tool(4)]
                }),
                5,
            ),
            (
                "responses-flat-schemaless-tools",
                IngressDialect::Responses,
                serde_json::json!({
                    "model": "gpt-x",
                    "input": [],
                    "tools": [
                        {"type": "function", "name": "a0"},
                        {"type": "function", "name": "a1"},
                        {"type": "function", "name": "a2"}
                    ]
                }),
                3,
            ),
            (
                "openai-schemaless-tools",
                IngressDialect::OpenAi,
                serde_json::json!({
                    "model": "gpt-x",
                    "messages": [],
                    "tools": [
                        {"type": "function", "function": {"name": "a0"}},
                        {"type": "function", "function": {"name": "a1"}}
                    ]
                }),
                2,
            ),
        ];
        for (name, dialect, body, schema_less) in adversarial {
            let wire_len = serde_json::to_string(body).unwrap().len();
            let (request, _) =
                crate::codec::decode_request(*dialect, body.clone(), RequestMetadataV1::default())
                    .unwrap_or_else(|e| panic!("{name} fixture failed to decode: {e}"));
            let serialized = serde_json::to_vec(&request.messages).unwrap().len()
                + serde_json::to_vec(&request.tools).unwrap().len();
            let old_estimate = (serialized as u64).saturating_add(3) / 4;
            let new_estimate = input_estimate(wire_len);
            let deficit = old_estimate.saturating_sub(new_estimate);
            assert!(
                deficit <= (schema_less * 8) as u64,
                "{name}: deficit {deficit} tokens exceeds the injected-schema bound \
                 ({} schema-less tools x ~8 tokens) — the estimate stopped being \
                 approximately conservative",
                schema_less
            );
        }
    }
}
