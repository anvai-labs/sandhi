//! Sandhi reverse-proxy — the **in-path (inline) egress gate** (AnvaiOps ADR-0047 D8).
//!
//! A client points its `base_url` at Sandhi and presents a **virtual key** (never the real
//! upstream key). The gate resolves the key → subject/group + which upstream, budget-checks,
//! normalizes the request through Sandhi's typed runtime, then emits one neutral usage event and
//! reconciles the budget. It is *in-path*, not a redirect: a client cannot bypass the meter.

/// Software release identity, independent of wire/chat contract versions.
pub const PACKAGE_VERSION: &str = env!("CARGO_PKG_VERSION");

mod access;
pub mod auth;
pub mod backend;
pub mod client_credentials;
mod codec;
pub mod config;
pub mod deadlines;
pub mod ledger;
pub mod lifecycle;
pub mod metrics;
pub mod operator;
pub mod persistence;
pub mod policy;
pub mod policy_onnx;
pub mod policy_remote;
pub mod policy_workers;
mod preparation;
pub mod profiles;
pub mod ratelimit;
mod reference_ownership;
mod routing;
mod serving;
pub mod settlement;
mod starter;
pub mod streaming;
mod text_endpoints;
pub mod topology;

/// First-party OTel/OTLP export of `gen_ai.*` spans + metrics (Scope 5, TD-0011 P3). Feature-gated
/// (`otel-otlp`, default off); provides no-op stubs when the feature is off so call sites compile
/// identically either way.
pub mod otel;

// Re-export the admin API request/response types for the `sandhi` CLI client + the startup
// rehydration helpers used by the `sandhi-proxy` binary.
pub use ledger::{reclaim_sweep_at, Admission, ProxyLedger};
pub use operator::{
    admin, build_provider_handle, build_provider_handle_for_endpoint, rehydrate_alerts,
    rehydrate_budgets, rehydrate_live_keys,
};
pub use persistence::BufferedAlertStore;
pub use serving::is_incomplete_shutdown;

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

#[cfg(test)]
use time::OffsetDateTime;

use sandhi_core::{
    AlertRegistry, Backend, ChatRequestV1, FinishReasonV1, KeyStore, LatencySource,
    RequestMetadataV1, Reservation, Sink, UsageBasis, UsageCompleteness, UsageEvent, UsageV2,
    VirtualKey,
};
use sandhi_providers::{
    token_estimate::baseline_estimate, ProviderError, ProviderFamily, ProviderHandle,
    ProviderRuntime, TokenEstimateCalibrator,
};
use sandhi_store::{hash_secret, AlertStore, SqliteStore, VaultStore, VirtualKeyStore};

use codec::{encode_response_with_emission, IngressDialect, StreamEventEncoder};
pub use operator::BudgetSpec;

/// Default output maximum for a Block-budgeted scope when the client omits `max_output_tokens`.
/// Sent upstream to constrain output and included in the admission estimate (ADR-0005 D1).
/// It is not a proven bound on total usage; input and provider-specific dimensions can exceed
/// the estimate. Unlimited scopes are never modified.
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

/// Warning emitted when the dual-scheme plaintext companion listener
/// (`SANDHI_HTTP_BIND`, TD-0017 D7) binds a non-loopback address. Unlike
/// [`plaintext_bind_warning`] TLS *is* configured — the companion exists
/// because the operator explicitly accepted cleartext for trusted-network
/// clients, so the warning names that choice rather than suggesting TLS.
/// Loopback companions stay quiet, mirroring the dev-trust posture.
#[must_use]
pub fn dual_scheme_plaintext_warning(addr: SocketAddr) -> Option<&'static str> {
    (!addr.ip().is_loopback()).then_some(
        "WARNING: the plaintext companion listener (SANDHI_HTTP_BIND) accepts bearer \
         credentials in cleartext on a non-loopback address; restrict it to the trusted \
         network segment, or unset SANDHI_HTTP_BIND to serve TLS only",
    )
}

/// Shared server state: the virtual-key store, the budget ledger, the usage sink, and the
/// registry of configured upstream providers (each already holding its real credential).
pub struct ProxyState {
    buffered_accounting: std::sync::OnceLock<Arc<settlement::buffered::Buffered>>,
    pub policy: Option<Arc<policy::PolicyGate>>,
    /// Optional deployment-topology projection (`SANDHI_TOPOLOGY_FILE`,
    /// crates/sandhi-proxy/src/topology.rs) — served read-only via
    /// `/admin/topology` and the dashboard. Absent = feature off.
    pub topology: Option<Arc<topology::TopologyFile>>,
    /// Validated startup-only buffered transport policy; absent preserves transport defaults.
    pub buffered_deadlines: Option<deadlines::BufferedDeadlines>,
    /// Operator-owned output default when a generation client omits a limit.
    pub default_max_output_tokens: u64,
    /// Startup-only, per-authorized-route streaming setup/idle/body limits.
    pub streaming_deadlines: Option<deadlines::StreamingDeadlines>,
    /// One-shot lifecycle; dispatch authorization stops atomically at shutdown cutoff.
    pub lifecycle: Arc<lifecycle::Lifecycle>,
    /// Same-port probe window, included in the total grace and clamped to leave drain time.
    pub shutdown_quiesce: Duration,
    pub keys: KeyStore,
    /// The original enforcement ledger. HTTP execution requires its validated
    /// single file-backed durable arm; read-only operator states may use memory.
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
    /// Revisioned non-secret backend definitions, separate from vault references.
    pub backends: Option<Arc<sandhi_store::BackendStore>>,
    /// Durable logical routes and compatible session bindings, separate from credentials.
    pub routes: Option<Arc<sandhi_store::routes::RouteStore>>,
    pub routing_slots: Arc<tokio::sync::Semaphore>,
    /// One admitted credential mutation (no waiting queue). Held through persistence/publication,
    /// even if its HTTP caller disconnects. Keeps blocking vault work off async workers and bounded.
    pub vault_writer: Arc<tokio::sync::Semaphore>,
    /// One bounded persisted-usage diagnostic operation, with no waiting queue.
    pub diagnostics_reader: Arc<tokio::sync::Semaphore>,
    /// Durable virtual-key store (hashes + scope), rehydrates `keys` on startup.
    pub vkeys: Option<Arc<VirtualKeyStore>>,
    /// Builds typed upstream handles from vault-resolved credentials.
    pub runtime: ProviderRuntime,
    /// Optional operator bearer (distinct from inference credentials). Without one,
    /// None mode opens the guarded starter control plane; Tokens keeps admin disabled.
    pub admin_token: Option<String>,
    /// Standalone defaults to the shared starter; library construction retains token mode.
    pub auth_mode: auth::AuthMode,
    /// An explicit server-owned provider/backend/route reference for starter calls.
    pub starter_upstream: Option<String>,
    pub starter_rate_limit_per_min: Option<u32>,
    /// Listener/public origins accepted by the unauthenticated local control plane.
    pub starter_control_origins: Vec<String>,
    /// Explicit OIDC authority. When set, legacy token/public gates cannot bypass it.
    pub oidc: Option<Arc<auth::Oidc>>,
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
    /// Budget cost-basis weight for cache-read tokens, basis points
    /// (10,000 = 1x neutral; 1,000 = 0.1x typical cached-input pricing).
    /// Startup-only env `SANDHI_CACHE_READ_WEIGHT` (issue #381, TD-0032 interim).
    pub cache_read_weight_bp: u32,
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
    /// TD-0027 S3: per-(provider, model) input estimate learned only from final measured events.
    token_estimator: Mutex<TokenEstimateCalibrator>,
    /// Declarative desired-state config file (`SANDHI_CONFIG`) backing `/admin/config` +
    /// `/admin/config/apply` — providers/budgets/alerts/vkeys as committable JSON, secrets
    /// referenced by env-var name rather than inlined. `None` disables both routes (404).
    pub config_path: Option<std::path::PathBuf>,
    /// TD-0031: optional non-interactive client-credentials registry.
    pub client_credentials: Option<Arc<client_credentials::ClientCredentialRegistry>>,
    /// Gateway-profiles store (Phase 5): the applied per-group `profiles` table + the
    /// `profile_tokens` bearer credentials for `GET /auth/profile`. Opened against the same
    /// SQLite file when `SANDHI_STORE` is set; absent otherwise.
    pub profiles: Option<Arc<sandhi_store::ProfileStore>>,
}

impl ProxyState {
    /// Build a state with the operator surface defaulted off (no vault, no admin token, no
    /// client-credentials registry). The
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
            buffered_accounting: std::sync::OnceLock::new(),
            policy: None,
            topology: None,
            lifecycle: Arc::new(lifecycle::Lifecycle::new()),
            buffered_deadlines: None,
            default_max_output_tokens: DEFAULT_OUTPUT_CEILING,
            streaming_deadlines: Some(deadlines::StreamingDeadlines::default()),
            shutdown_quiesce: Duration::from_secs(1),
            keys,
            ledger: Mutex::new(ledger),
            sink,
            providers: Mutex::new(providers),
            store,
            vault: None,
            backends: None,
            routes: None,
            routing_slots: Arc::new(tokio::sync::Semaphore::new(8)),
            vault_writer: Arc::new(tokio::sync::Semaphore::new(1)),
            diagnostics_reader: Arc::new(tokio::sync::Semaphore::new(1)),
            vkeys: None,
            runtime: ProviderRuntime::new(),
            admin_token: None,
            auth_mode: auth::AuthMode::Tokens,
            starter_upstream: None,
            starter_rate_limit_per_min: None,
            starter_control_origins: Vec::new(),
            oidc: None,
            budgets: Mutex::new(HashMap::new()),
            public_url: "http://localhost:8787".into(),
            alerts: None,
            alert_store: None,
            alert_writer: None,
            dashboard_public: false,
            error_detail_full: false,
            max_request_body_bytes: DEFAULT_MAX_REQUEST_BODY_BYTES,
            max_in_flight_ai_requests: DEFAULT_MAX_IN_FLIGHT_AI_REQUESTS,
            cache_read_weight_bp: sandhi_core::allowance::Weight::default().basis_points(),
            max_connections: DEFAULT_MAX_CONNECTIONS,
            max_connections_per_ip: DEFAULT_MAX_CONNECTIONS_PER_IP,
            header_read_timeout_secs: DEFAULT_HEADER_READ_TIMEOUT_SECS,
            trusted_proxies: Vec::new(),
            metrics: Arc::new(metrics::Metrics::new()),
            rate_limiter: Arc::new(ratelimit::RateLimiter::new()),
            token_estimator: Mutex::new(TokenEstimateCalibrator::default()),
            otel: None,
            config_path: None,
            client_credentials: None,
            profiles: None,
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
    lifecycle: Arc<lifecycle::Lifecycle>,
}

impl AdmissionLayer {
    pub(crate) fn new(max_in_flight: usize, lifecycle: Arc<lifecycle::Lifecycle>) -> Self {
        Self {
            permits: Arc::new(Semaphore::new(max_in_flight)),
            lifecycle,
        }
    }
}

impl<S> Layer<S> for AdmissionLayer {
    type Service = AdmissionService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        AdmissionService {
            inner,
            permits: self.permits.clone(),
            lifecycle: self.lifecycle.clone(),
        }
    }
}

#[derive(Clone)]
pub(crate) struct AdmissionService<S> {
    inner: S,
    permits: Arc<Semaphore>,
    lifecycle: Arc<lifecycle::Lifecycle>,
}

impl<S> TowerService<axum::http::Request<Body>> for AdmissionService<S>
where
    S: TowerService<axum::http::Request<Body>, Response = Response> + Clone + Send + 'static,
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
        let lifecycle = self.lifecycle.clone();
        let dialect = dialect_for_path(request.uri().path());
        let mut inner = self.inner.clone();
        Box::pin(async move {
            let permit = tokio::select! {
                biased;
                _ = lifecycle.cancelled() => return Ok(draining_error(dialect)),
                result = permits.acquire_owned() => match result {
                    Ok(permit) => permit,
                    Err(_) => return Ok(draining_error(dialect)),
                },
            };
            if !lifecycle.is_running() {
                return Ok(draining_error(dialect));
            }
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
    pub lifecycle: Arc<lifecycle::Lifecycle>,
    pub quiesce: Duration,
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
            lifecycle: Arc::new(lifecycle::Lifecycle::new()),
            quiesce: Duration::from_secs(1),
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
            lifecycle: state.lifecycle.clone(),
            quiesce: state.shutdown_quiesce,
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
        .route("/v1/embeddings", post(handle_embeddings))
        .route("/v1/completions", post(handle_completions))
        .route("/v1/messages", post(handle_anthropic))
        .route(
            "/v1/messages/count_tokens",
            post(handle_anthropic_count_tokens),
        )
        .route("/v1/responses", post(handle_responses))
        // Gemini's path carries the model AND the method, colon-separated
        // (`/v1beta/models/gemini-2.5-flash:generateContent`), so it matches as ONE segment and
        // is split below — axum has no pattern for a colon-suffixed verb.
        .route("/v1beta/models/:model_method", post(handle_gemini))
        .route("/v1/models/:model_method", post(handle_gemini));
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
        .layer(AdmissionLayer::new(
            state.max_in_flight_ai_requests.max(1),
            state.lifecycle.clone(),
        ))
        // TD-0014 P3 (G19): per-request trusted-proxy resolution —
        // X-Forwarded-For believed only from SANDHI_TRUSTED_PROXIES peers.
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            resolve_client_ip,
        ))
}

/// Assemble the application. A valid existing durable ledger initializes the
/// canonical owner; unsupported/read-only states retain probes/operator routes
/// but generation refuses and readiness stays unavailable.
pub fn build_app(state: Arc<ProxyState>) -> Router {
    let _ = initialize_execution(&state);
    let max_request_body_bytes = state.max_request_body_bytes;
    let ai_routes = ingress_routes(&state);
    // TD-0021 P2 (D4/R3): every response — success AND error, including ingress errors —
    // carries the chat contract version, so a consumer hitting a mismatch knows which
    // contract it is talking to at the moment it most needs to (R3: a mismatch presents
    // as an error). One middleware layer is the single source; no builder can forget it.
    Router::new()
        .route("/healthz", get(health))
        .route("/readyz", get(readiness))
        // TD-0021 P2 (D4/R2): the HTTP-path contract handshake. Ungated — versions
        // and wired dialects are public facts; the capability detail (D5) lives
        // behind the admin gate at /admin/version.
        .route("/version", get(version))
        .route("/catalog/models", get(catalog_models))
        .route("/auth/login", get(auth::login))
        .route("/auth/token", post(crate::client_credentials::client_token))
        .route("/auth/callback", get(auth::callback))
        .route("/auth/session", get(auth::session_status))
        .route("/auth/logout", post(auth::logout))
        .route("/auth/keys", post(auth::delegate_key))
        // Gateway-profiles (Phase 5): a bearer profile token (skp_…) downloads the caller's
        // assembled group profile — defaults + models, plus a shown-once minted virtual key in
        // gateway mode. Issuer-agnostic seam (subject tokens later), see profiles.rs.
        .route("/auth/profile", get(crate::profiles::profile))
        .route("/dashboard", get(dashboard_html))
        .route("/dashboard/assets/dashboard.js", get(dashboard_script))
        .route("/dashboard/assets/dashboard.css", get(dashboard_style))
        .route("/dashboard/api/usage", get(dashboard_api))
        // TD-0003 P4 dashboard read-only endpoints (masked; admin-bearer-gated when an admin
        // token is configured, unless SANDHI_DASHBOARD_PUBLIC=1 — ADR-0004 D4).
        .route("/dashboard/api/keys", get(dashboard_keys))
        .route("/dashboard/api/topology", get(dashboard_topology))
        .route("/dashboard/api/budgets", get(dashboard_budgets))
        .route("/dashboard/api/alerts", get(dashboard_alerts))
        // TD-0011 D5: /metrics reveals traffic shape and model mix, so it reuses the dashboard's
        // gate rather than inventing a second policy.
        .route("/metrics", get(metrics_endpoint))
        // SDKs share this discovery path. Native credential/client headers select
        // the response envelope; the same virtual-key authorization gates each.
        .route("/v1/models", get(list_models))
        .route("/v1beta/models", get(list_models_gemini))
        // TD-0003 P1 operator (admin) API.
        // TD-0021 P2 (D5/R2): capability detail — which optional features are on —
        // is operator information, gated like the rest of /admin.
        .route("/admin/version", get(operator::version_capabilities))
        .route("/admin/policy", get(operator::policy_status))
        .route("/admin/topology", get(operator::topology))
        .route("/admin/keys/reference", post(operator::register_reference))
        .route(
            "/admin/backends",
            post(backend::register).get(backend::list),
        )
        .route("/admin/backends/:id", delete(backend::revoke))
        .route("/admin/routes", post(routing::register).get(routing::list))
        .route("/admin/routes/:id", delete(routing::revoke))
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
        .route(
            "/admin/usage/diagnostics",
            post(operator::usage_diagnostics),
        )
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
        // Gateway-profiles: mint / revoke the profile-download tokens (skp_…) that authenticate
        // /auth/profile. Hash-only persistence; the plaintext is shown once at mint time.
        .route("/admin/profile-tokens", post(operator::mint_profile_token))
        .route(
            "/admin/profile-tokens/revoke",
            post(operator::revoke_profile_tokens),
        )
        // Management responses can contain attribution or one-time credentials. This layer
        // deliberately precedes the ingress merge: provider response headers stay untouched.
        .layer(axum::middleware::map_response(management_no_store))
        .merge(ai_routes)
        .layer(DefaultBodyLimit::max(max_request_body_bytes))
        .layer(axum::middleware::from_fn(contract_version_header))
        .with_state(state)
}

/// Finalize generation ownership against the supplied original ledger. This
/// never opens, replaces, migrates or creates another ledger.
pub fn initialize_execution(state: &Arc<ProxyState>) -> Result<(), &'static str> {
    if !(1..=1_000_000).contains(&state.default_max_output_tokens) {
        return Err("invalid configured output default");
    }
    if state.buffered_accounting.get().is_some() {
        return Ok(());
    }
    settlement::buffered::enable(state, settlement::buffered::Config::default())
}
fn require_execution_owner(state: &Arc<ProxyState>) -> std::io::Result<()> {
    initialize_execution(state).map_err(std::io::Error::other)
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
/// Once `shutdown` resolves, dispatch authorization closes. Fresh probes remain reachable for
/// a bounded quiesce interval under the existing connection limits, then accepts stop. At the
/// original deadline connections are cancelled. Blocking cleanup can outlive cancellation:
/// a timeout is reported, not a claim of completed settlement or forced embedder process exit.
///
/// Serving owns periodic recovery of the original durable ledger, including ready terminal
/// records after restart without another HTTP request. Shutdown stops recovery and checks the
/// bounded final inventory. Unknown liabilities return an error classified by
/// [`is_incomplete_shutdown`]; they are never released or replayed. Dropping this future signals
/// its owned supervisor to perform the same bounded drain, provided the host keeps Tokio running.
/// The host must await this function to observe cleanup errors; cancellation cannot return one.
pub async fn serve_with_shutdown_timeout<F>(
    state: Arc<ProxyState>,
    addr: SocketAddr,
    shutdown: F,
    grace: Duration,
) -> std::io::Result<()>
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    require_execution_owner(&state)?;
    let listener = tokio::net::TcpListener::bind(addr).await?;
    serve_scheme_listeners_with_shutdown(
        state,
        SchemeListeners {
            plaintext: Some(listener),
            tls: None,
        },
        shutdown,
        grace,
    )
    .await
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
    require_execution_owner(&state)?;
    let listener = tokio::net::TcpListener::bind(addr).await?;
    serve_scheme_listeners_with_shutdown(
        state,
        SchemeListeners {
            plaintext: None,
            tls: Some((listener, tls.acceptor)),
        },
        shutdown,
        grace,
    )
    .await
}

/// Dual-scheme counterpart to [`serve_with_shutdown_timeout`] and
/// [`serve_with_tls_shutdown_timeout`] (TD-0017 P1 amendment, D7): one process
/// serving a TLS listener **and** a plaintext HTTP listener on separate sockets.
///
/// Both sockets bind before either accepts, so a failed bind surfaces as a
/// startup error instead of a half-configured topology silently taking traffic
/// on the surviving port. The plaintext port is a deliberate operator choice
/// (`SANDHI_HTTP_BIND`, trusted-network clients that cannot speak HTTPS);
/// callers own the loud non-loopback warning. Both listeners share ONE
/// connection budget, one per-IP table, and one graceful drain — the
/// TD-0014 P3 bounds cap the process, not the port.
pub async fn serve_with_http_and_tls_shutdown_timeout<F>(
    state: Arc<ProxyState>,
    tls_addr: SocketAddr,
    http_addr: SocketAddr,
    shutdown: F,
    grace: Duration,
    tls: TlsConfig,
) -> std::io::Result<()>
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    require_execution_owner(&state)?;
    let tls_listener = tokio::net::TcpListener::bind(tls_addr).await?;
    let http_listener = tokio::net::TcpListener::bind(http_addr).await?;
    serve_scheme_listeners_with_shutdown(
        state,
        SchemeListeners {
            plaintext: Some(http_listener),
            tls: Some((tls_listener, tls.acceptor)),
        },
        shutdown,
        grace,
    )
    .await
}

/// The listeners sharing one admission domain: a single connection budget,
/// per-IP table, graceful-drain registry, and shutdown choreography. At least
/// one listener must be present.
struct SchemeListeners {
    /// Plaintext HTTP/1 listener — the loopback default, or the deliberate
    /// dual-scheme companion port.
    plaintext: Option<tokio::net::TcpListener>,
    /// TLS-wrapped HTTP/1 listener.
    tls: Option<(tokio::net::TcpListener, tokio_rustls::TlsAcceptor)>,
}

/// The next accepted connection, tagged with the scheme whose listener
/// produced it — the tag decides whether the connection task wraps the
/// stream in a TLS session.
enum AcceptedConnection {
    Plaintext(std::io::Result<(tokio::net::TcpStream, SocketAddr)>),
    Tls(std::io::Result<(tokio::net::TcpStream, SocketAddr)>),
}

/// Await the next connection on whichever listeners exist. The accept futures
/// are rebuilt each time the surrounding `select!` drops this arm; dropping an
/// accept future loses nothing — the connection stays in the kernel backlog.
async fn accept_next_connection(listeners: &SchemeListeners) -> AcceptedConnection {
    match (&listeners.plaintext, &listeners.tls) {
        (Some(plain), Some((tls_listener, _))) => tokio::select! {
            accepted = plain.accept() => AcceptedConnection::Plaintext(accepted),
            accepted = tls_listener.accept() => AcceptedConnection::Tls(accepted),
        },
        (Some(plain), None) => AcceptedConnection::Plaintext(plain.accept().await),
        (None, Some((tls_listener, _))) => AcceptedConnection::Tls(tls_listener.accept().await),
        // Unreachable by construction (every constructor passes a listener).
        (None, None) => std::future::pending().await,
    }
}

async fn serve_scheme_listeners_with_shutdown<F>(
    state: Arc<ProxyState>,
    listeners: SchemeListeners,
    shutdown: F,
    grace: Duration,
) -> std::io::Result<()>
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    debug_assert!(
        listeners.plaintext.is_some() || listeners.tls.is_some(),
        "serving requires at least one listener"
    );
    serving::supervise(state, listeners, shutdown, grace).await
}

/// Serve until `shutdown` resolves, with TD-0014 P3 connection-level defence:
/// a hard cap on concurrent connections and per-peer connections, a guaranteed
/// header-read timeout, a bounded per-connection read buffer, and a drain that
/// requests cancellation of hung connections at the grace deadline. Synchronous
/// destructors may outlive cancellation; the binary watchdog owns the hard process bound.
async fn serve_router_listener_with_shutdown<F>(
    app: Router,
    listeners: SchemeListeners,
    shutdown: F,
    grace: Duration,
    policy: ConnectionPolicy,
) -> std::io::Result<()>
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    use std::collections::HashMap;

    let slots = Arc::new(Semaphore::new(policy.max_connections));
    let per_ip: Arc<std::sync::Mutex<HashMap<std::net::IpAddr, usize>>> = Default::default();
    let graceful = Arc::new(hyper_util::server::graceful::GracefulShutdown::new());
    let mut tasks: tokio::task::JoinSet<()> = tokio::task::JoinSet::new();
    // One acceptor shared by every TLS accept; cloned per connection as an Arc bump.
    let tls_acceptor = listeners.tls.as_ref().map(|(_, acceptor)| acceptor.clone());
    tokio::pin!(shutdown);

    let mut quiescing = false;
    let mut quiesce_end = tokio::time::Instant::now();
    // Keep the same listeners for probes during bounded quiesce, without relaxing
    // connection/per-IP limits. Under transport saturation a fresh probe may still be shed.
    loop {
        tokio::select! {
            biased;
            _ = &mut shutdown, if !quiescing => {
                let deadline = policy.lifecycle.begin_quiesce(grace);
                let remaining = deadline.saturating_duration_since(std::time::Instant::now());
                quiesce_end = tokio::time::Instant::now() + policy.quiesce.min(remaining / 4);
                quiescing = true;
                tracing::info!(remaining_secs = remaining.as_secs_f64(), "shutdown cutoff; quiescing with probes reachable within connection limits");
            }
            _ = tokio::time::sleep_until(quiesce_end), if quiescing => break,
            result = tasks.join_next(), if !tasks.is_empty() => {
                if let Some(Err(error)) = result {
                    tracing::warn!(%error, "connection task failed");
                }
            }
            accepted = accept_next_connection(&listeners) => {
                // The scheme tag selects the transport wrapper; the admission
                // and metering path below is shared, by construction.
                let (acceptor, accepted) = match accepted {
                    AcceptedConnection::Plaintext(accepted) => (None, accepted),
                    AcceptedConnection::Tls(accepted) => (tls_acceptor.clone(), accepted),
                };
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
                let tls = acceptor;
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
    drop(listeners);
    policy.lifecycle.start_draining();

    // Grace window: signal every watched connection, wait until they finish or
    // the original deadline passes, then request cancellation of stragglers.
    // Their synchronous cleanup may outlive this library future.
    tracing::info!("shutdown received; draining connections");
    // `shutdown(self)` consumes; unwrap the Arc (watchers hold only the rx
    // side) and let the signal+wait run on its own task.
    let _shutdown_task = Arc::try_unwrap(graceful)
        .map(|g| tokio::spawn(g.shutdown()))
        .ok();
    let deadline = tokio::time::Instant::from_std(
        policy
            .lifecycle
            .deadline()
            .expect("shutdown established deadline"),
    );
    let mut progress = tokio::time::interval(Duration::from_secs(1));
    let mut expired = false;
    while !tasks.is_empty() {
        tokio::select! {
            biased;
            _ = tokio::time::sleep_until(deadline) => { expired = true; break; }
            _ = tasks.join_next() => {}
            _ = progress.tick() => tracing::info!(connections = tasks.len(), active_operations = policy.lifecycle.active_operations(), remaining_secs = policy.lifecycle.remaining().unwrap_or_default().as_secs_f64(), "shutdown drain progress"),
        }
    }
    if expired {
        tracing::warn!(
            grace_secs = grace.as_secs_f64(),
            "shutdown grace expired; cancelling remaining connections (and their response streams)"
        );
        tasks.abort_all();
    }
    // Never wait without a deadline for abort destructors: settlement can block in SQLite.
    // The embedder owns its runtime; the binary separately owns a process-exit watchdog.
    if expired {
        tasks.detach_all();
        return Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "shutdown deadline reached; connection cancellation requested, cleanup may remain",
        ));
    }
    if tokio::time::timeout_at(deadline, policy.lifecycle.wait_idle())
        .await
        .is_err()
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "shutdown deadline reached with admitted operations still cleaning up",
        ));
    }
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
    fn dual_scheme_companion_warning_is_only_for_non_loopback_ports() {
        // TLS is configured in dual-scheme mode by definition, so the companion
        // warning keys on the address alone.
        assert!(dual_scheme_plaintext_warning("127.0.0.1:8787".parse().unwrap()).is_none());
        assert!(dual_scheme_plaintext_warning("[::1]:8787".parse().unwrap()).is_none());
        let warning = dual_scheme_plaintext_warning("0.0.0.0:8787".parse().unwrap())
            .expect("a non-loopback plaintext companion must be loud");
        assert!(warning.contains("cleartext"), "unexpected: {warning}");
        assert!(
            warning.contains("SANDHI_HTTP_BIND"),
            "unexpected: {warning}"
        );
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
        let server = tokio::spawn(serve_scheme_listeners_with_shutdown(
            Arc::new(state),
            SchemeListeners {
                plaintext: None,
                tls: Some((listener, tls.acceptor)),
            },
            async {
                let _ = shutdown_rx.await;
            },
            Duration::from_millis(50),
        ));

        let mut stalled = tokio::net::TcpStream::connect(addr)
            .await
            .expect("connect silent TLS peer");
        tokio::time::sleep(Duration::from_millis(25)).await;
        let started = std::time::Instant::now();
        shutdown_tx.send(()).expect("signal shutdown");
        let error = tokio::time::timeout(Duration::from_secs(1), server)
            .await
            .expect("stalled TLS handshake must not outlive the grace deadline")
            .expect("server task joins")
            .expect_err("forced cancellation must not claim clean shutdown");
        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
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
        let mut state = ProxyState::new(
            KeyStore::new(),
            ProxyLedger::in_memory(),
            Arc::new(InMemorySink::new()),
            HashMap::new(),
            None,
        );
        state.shutdown_quiesce = Duration::ZERO;
        let state = Arc::new(state);

        let result = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            serve_scheme_listeners_with_shutdown(
                state,
                SchemeListeners {
                    plaintext: Some(listener),
                    tls: None,
                },
                async {},
                // This path should finish immediately; the bound is still part of the contract.
                DEFAULT_SHUTDOWN_GRACE,
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
            SchemeListeners {
                plaintext: Some(listener),
                tls: None,
            },
            async move {
                let _ = shutdown_rx.await;
            },
            Duration::from_millis(25),
            ConnectionPolicy::default(),
        ));
        let client_call = tokio::spawn(async move {
            let _ = reqwest::get(format!("http://{addr}/stuck")).await;
        });
        entered.notified().await;
        shutdown_tx.send(()).unwrap();

        let error = tokio::time::timeout(Duration::from_secs(1), server)
            .await
            .expect("bounded shutdown must return")
            .expect("server task joined")
            .expect_err("forced cancellation must not claim clean shutdown");
        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
        client_call.abort();
    }
}

#[cfg(test)]
mod drain_contract_tests {
    use super::*;
    use axum::http::Request;
    use sandhi_core::{InMemorySink, KeyStore};
    use tower::ServiceExt;

    fn state() -> (Arc<ProxyState>, Arc<InMemorySink>, tempfile::TempDir) {
        let directory = tempfile::tempdir().unwrap();
        let sink = Arc::new(InMemorySink::new());
        let state = Arc::new(ProxyState::new(
            KeyStore::new(),
            ProxyLedger::durable(directory.path().join("original.db").to_str().unwrap(), 1)
                .unwrap(),
            sink.clone(),
            HashMap::new(),
            None,
        ));
        (state, sink, directory)
    }

    #[tokio::test]
    async fn readiness_is_distinct_from_liveness_and_all_dialects_reject_drain() {
        let (state, sink, _directory) = state();
        let app = build_app(state.clone());
        let ready = app
            .clone()
            .oneshot(Request::get("/readyz").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(ready.status(), StatusCode::OK);
        assert_eq!(ready.headers()["cache-control"], "no-store");
        state.lifecycle.begin_quiesce(Duration::from_secs(2));
        for (path, expected) in [
            ("/readyz", StatusCode::SERVICE_UNAVAILABLE),
            ("/healthz", StatusCode::OK),
        ] {
            let response = app
                .clone()
                .oneshot(Request::get(path).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), expected);
        }
        for path in [
            "/v1/chat/completions",
            "/v1/responses",
            "/v1/messages",
            "/v1beta/models/test:generateContent",
        ] {
            let response = app
                .clone()
                .oneshot(Request::post(path).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE, "{path}");
            assert_eq!(response.headers()["retry-after"], "1");
            let bytes = axum::body::to_bytes(response.into_body(), 4096)
                .await
                .unwrap();
            let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            if path.starts_with("/v1beta") {
                assert_eq!(body["error"]["code"], 503);
                assert_eq!(body["error"]["status"], "UNAVAILABLE");
            } else {
                assert_eq!(body["error"]["code"], "gateway_draining");
                assert_eq!(body["error"]["retryable"], true);
            }
        }
        assert_eq!(state.lifecycle.active_operations(), 0);
        assert!(sink.is_empty());
    }

    #[tokio::test]
    async fn shutdown_metrics_preserve_auth_and_report_owned_work() {
        let (mut state, _, _directory) = state();
        Arc::get_mut(&mut state).unwrap().admin_token = Some("test-admin".into());
        let app = build_app(state.clone());
        let operation = state.lifecycle.try_operation().unwrap();
        state.lifecycle.begin_quiesce(Duration::from_secs(2));
        let denied = app
            .clone()
            .oneshot(Request::get("/metrics").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(denied.status(), StatusCode::UNAUTHORIZED);
        let response = app
            .oneshot(
                Request::get("/metrics")
                    .header("authorization", "Bearer test-admin")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 1_000_000)
            .await
            .unwrap();
        let text = std::str::from_utf8(&body).unwrap();
        assert!(text.lines().any(|line| line == "sandhi_ready 0"));
        assert!(text
            .lines()
            .any(|line| line == "sandhi_shutdown_active_operations 1"));
        let elapsed: f64 = text
            .lines()
            .find_map(|line| line.strip_prefix("sandhi_shutdown_elapsed_seconds "))
            .unwrap()
            .parse()
            .unwrap();
        assert!(elapsed >= 0.0);
        drop(operation);
        assert_eq!(state.lifecycle.active_operations(), 0);
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
    use axum::http::Request;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tower::ServiceExt;
    #[tokio::test]
    async fn concurrency_boundary_precedes_body_extraction_and_dispatch() {
        let entered = Arc::new(tokio::sync::Notify::new());
        let calls = Arc::new(AtomicUsize::new(0));
        let body_polls = Arc::new(AtomicUsize::new(0));
        let handler_entered = entered.clone();
        let handler_calls = calls.clone();
        // A middleware unit probe, not a proxy generation/accounting alternative.
        let app = Router::new()
            .route(
                "/__admission_probe",
                post(
                    move |Extension(permit): Extension<Arc<AdmissionPermit>>, _bytes: Bytes| {
                        let entered = handler_entered.clone();
                        let calls = handler_calls.clone();
                        async move {
                            let _permit = permit;
                            calls.fetch_add(1, Ordering::SeqCst);
                            entered.notify_one();
                            std::future::pending::<StatusCode>().await
                        }
                    },
                ),
            )
            .layer(AdmissionLayer::new(
                1,
                Arc::new(lifecycle::Lifecycle::new()),
            ));
        let request = |body| Request::post("/__admission_probe").body(body).unwrap();
        let first = tokio::spawn(app.clone().oneshot(request(Body::from("first"))));
        entered.notified().await;
        let polls = body_polls.clone();
        let second_body = Body::from_stream(futures_util::stream::once(async move {
            polls.fetch_add(1, Ordering::SeqCst);
            Ok::<_, std::io::Error>(Bytes::from_static(b"second"))
        }));
        let second = tokio::spawn(app.oneshot(request(second_body)));
        tokio::time::sleep(Duration::from_millis(25)).await;
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            body_polls.load(Ordering::SeqCst),
            0,
            "queued request cannot extract its body before admission"
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
        "package_version": PACKAGE_VERSION,
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
async fn require_dashboard_access(state: &ProxyState, headers: &HeaderMap) -> Result<(), Response> {
    if let Some(oidc) = &state.oidc {
        return oidc.authorize(headers, auth::Permission::Read, false).await;
    }
    if state.auth_mode == auth::AuthMode::Oidc {
        return Err(error(
            StatusCode::SERVICE_UNAVAILABLE,
            "OIDC authority unavailable",
        ));
    }
    if starter::enabled(state) {
        return operator::require_access(state, headers, auth::Permission::Read, false).await;
    }
    if state.dashboard_public || state.admin_token.is_none() {
        return Ok(());
    }
    operator::require_admin(state, headers)
}

/// Usage aggregates for the dashboard (JSON). 404 when no durable store is configured.
async fn dashboard_api(State(state): State<Arc<ProxyState>>, headers: HeaderMap) -> Response {
    if let Err(denied) = require_dashboard_access(&state, &headers).await {
        return denied;
    }
    let Some(store) = state.store.clone() else {
        return error(
            StatusCode::NOT_FOUND,
            "dashboard not configured (set SANDHI_STORE)",
        );
    };
    dashboard_read(move || {
        Ok(json!({
            "total": store.grand_total().map_err(|e| e.to_string())?,
            "by_subject": store.totals_by_subject().map_err(|e| e.to_string())?,
            "by_group": store.totals_by_group().map_err(|e| e.to_string())?,
            "by_provider": store.totals_by_provider().map_err(|e| e.to_string())?,
            "by_model": store.totals_by_model().map_err(|e| e.to_string())?,
        }))
    })
    .await
}

/// `GET /dashboard/api/topology` — the optional deployment-topology projection
/// (secret-free by construction; 404 when `SANDHI_TOPOLOGY_FILE` is unset).
async fn dashboard_topology(State(state): State<Arc<ProxyState>>, headers: HeaderMap) -> Response {
    if let Err(denied) = require_dashboard_access(&state, &headers).await {
        return denied;
    }
    let Some(file) = state.topology.clone() else {
        return error(
            StatusCode::NOT_FOUND,
            "topology not configured (set SANDHI_TOPOLOGY_FILE)",
        );
    };
    dashboard_read(move || Ok(crate::topology::topology_response(&file))).await
}

// --- TD-0003 P4 dashboard read-only endpoints ----------------------------------
//
// Reads follow require_dashboard_access: admin-gated unless explicitly public or no admin token
// is configured. Masking protects secrets; it does not replace authorization for attribution.
// Units are neutral tokens throughout — no dollars / SKU / tier (the measure-vs-price boundary).

/// `GET /dashboard/api/keys` — masked virtual keys + masked vault entries (no secrets, no hashes).
/// 404 when neither the vault nor the virtual-key store is configured.
async fn dashboard_keys(State(state): State<Arc<ProxyState>>, headers: HeaderMap) -> Response {
    if let Err(denied) = require_dashboard_access(&state, &headers).await {
        return denied;
    }
    let (vault, vkeys) = (state.vault.clone(), state.vkeys.clone());
    if vault.is_none() && vkeys.is_none() {
        return error(
            StatusCode::NOT_FOUND,
            "keys dashboard not configured (set SANDHI_STORE)",
        );
    }
    dashboard_read(move || {
        let vkey_records = vkeys
            .as_ref()
            .map(|s| s.list())
            .transpose()
            .map_err(|e| e.to_string())?
            .unwrap_or_default()
            .iter()
            .map(operator::vkey_record_response)
            .collect::<Vec<_>>();
        let vault_entries = vault
            .as_ref()
            .map(|s| s.list())
            .transpose()
            .map_err(|e| e.to_string())?
            .unwrap_or_default()
            .iter()
            .map(operator::vault_entry_response)
            .collect::<Vec<_>>();
        Ok(json!({ "virtual_keys": vkey_records, "vault": vault_entries }))
    })
    .await
}

/// `GET /dashboard/api/budgets` — every configured scope with limit / window / policy + live spent
/// (from the budget ledger). Neutral tokens; no pricing.
async fn dashboard_budgets(State(state): State<Arc<ProxyState>>, headers: HeaderMap) -> Response {
    if let Err(denied) = require_dashboard_access(&state, &headers).await {
        return denied;
    }
    dashboard_read(move || {
        let ledger = state.ledger.lock().map_err(|e| e.to_string())?;
        let budgets = state.budgets.lock().map_err(|e| e.to_string())?;
        let scopes: Result<Vec<Value>, String> = budgets
            .values()
            .map(|spec| {
                let spent = ledger.try_spent(&spec.scope)?;
                let limit = spec.limit_tokens;
                Ok(json!({
                    "scope": spec.scope,
                    "limit_tokens": limit,
                    "spent": spent,
                    "remaining": limit.saturating_sub(spent),
                    "window": spec.window,
                    "policy": spec.policy,
                }))
            })
            .collect();
        Ok(json!({ "budgets": scopes? }))
    })
    .await
}

/// `GET /dashboard/api/alerts` — recent fired alerts (rules whose threshold has tripped) plus all
/// configured rules. 404 when the alert store is not configured.
async fn dashboard_alerts(State(state): State<Arc<ProxyState>>, headers: HeaderMap) -> Response {
    if let Err(denied) = require_dashboard_access(&state, &headers).await {
        return denied;
    }
    let Some(store) = state.alert_store.clone() else {
        return error(
            StatusCode::NOT_FOUND,
            "alerts dashboard not configured (set SANDHI_STORE)",
        );
    };
    dashboard_read(move || {
        let rules = store.list().map_err(|e| e.to_string())?;
        let all: Vec<Value> = rules.iter().map(operator::alert_rule_response).collect();
        let fired: Vec<Value> = rules
            .iter()
            .filter(|r| r.last_fired_at.is_some())
            .map(operator::alert_rule_response)
            .collect();
        Ok(json!({ "rules": all, "fired": fired }))
    })
    .await
}

/// Failed observation is unavailable, never an empty or zero-valued success. SQLite reads
/// run off the async scheduler so a busy database does not park a request worker.
async fn dashboard_read(read: impl FnOnce() -> Result<Value, String> + Send + 'static) -> Response {
    match tokio::task::spawn_blocking(read).await {
        Ok(Ok(payload)) => Json(payload).into_response(),
        result => {
            tracing::warn!(?result, "dashboard read unavailable");
            error(
                StatusCode::SERVICE_UNAVAILABLE,
                "dashboard data unavailable; retry later",
            )
        }
    }
}

async fn management_no_store(mut response: Response) -> Response {
    response
        .headers_mut()
        .insert("referrer-policy", "no-referrer".parse().unwrap());
    response
        .headers_mut()
        .insert("cache-control", "no-store".parse().unwrap());
    response
}

/// The self-hosted single-node dashboard (static HTML; fetches `/dashboard/api/usage`).
async fn dashboard_html() -> Response {
    (
        [
            ("content-security-policy", "default-src 'none'; script-src 'self'; style-src 'self' 'unsafe-inline'; connect-src 'self'; base-uri 'none'; frame-ancestors 'none'; form-action 'none'"),
            ("x-content-type-options", "nosniff"),
            ("referrer-policy", "no-referrer"),
        ],
        axum::response::Html(DASHBOARD_HTML),
    ).into_response()
}

async fn dashboard_script() -> Response {
    (
        [
            ("content-type", "text/javascript; charset=utf-8"),
            ("x-content-type-options", "nosniff"),
        ],
        include_str!("dashboard/dashboard.js"),
    )
        .into_response()
}

async fn dashboard_style() -> Response {
    (
        [
            ("content-type", "text/css; charset=utf-8"),
            ("x-content-type-options", "nosniff"),
        ],
        include_str!("dashboard/dashboard.css"),
    )
        .into_response()
}

const DASHBOARD_HTML: &str = include_str!("dashboard/index.html");

async fn handle_completions(
    State(state): State<Arc<ProxyState>>,
    permit: Extension<Arc<AdmissionPermit>>,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Response {
    let body = match request_body(body, IngressDialect::Completions) {
        Ok(body) => body,
        Err(response) => return *response,
    };
    handle(
        state,
        permit.0,
        headers,
        body,
        IngressDialect::Completions,
        None,
    )
    .await
}

async fn handle_embeddings(
    State(state): State<Arc<ProxyState>>,
    permit: Extension<Arc<AdmissionPermit>>,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Response {
    let body = match request_body(body, IngressDialect::Embeddings) {
        Ok(body) => body,
        Err(response) => return *response,
    };
    handle(
        state,
        permit.0,
        headers,
        body,
        IngressDialect::Embeddings,
        None,
    )
    .await
}

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

async fn handle_anthropic_count_tokens(
    State(state): State<Arc<ProxyState>>,
    permit: Extension<Arc<AdmissionPermit>>,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Response {
    let body = match request_body(body, IngressDialect::Anthropic) {
        Ok(body) => body,
        Err(response) => return *response,
    };
    handle_count_tokens(
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
async fn readiness(State(state): State<Arc<ProxyState>>) -> Response {
    let (status, body) = if state.buffered_accounting.get().is_none() {
        (StatusCode::SERVICE_UNAVAILABLE, "execution_not_initialized")
    } else if state.lifecycle.is_running() {
        (StatusCode::OK, "ready")
    } else {
        (StatusCode::SERVICE_UNAVAILABLE, "draining")
    };
    (
        status,
        [(axum::http::header::CACHE_CONTROL, "no-store")],
        body,
    )
        .into_response()
}

/// Metrics keep their existing authorization even during quiesce.
async fn metrics_endpoint(State(state): State<Arc<ProxyState>>, headers: HeaderMap) -> Response {
    if let Err(denied) = require_dashboard_access(&state, &headers).await {
        return denied;
    }
    let mut rendered = state.metrics.render();
    use std::fmt::Write;
    let _ = write!(rendered,
        "# HELP sandhi_ready Whether this process accepts new dispatch authorization.\n\
         # TYPE sandhi_ready gauge\nsandhi_ready {}\n\
         # HELP sandhi_shutdown_active_operations Admitted operations still owning cleanup; not durable commits.\n\
         # TYPE sandhi_shutdown_active_operations gauge\nsandhi_shutdown_active_operations {}\n\
         # HELP sandhi_shutdown_elapsed_seconds Time since observed shutdown; zero before cutoff.\n\
         # TYPE sandhi_shutdown_elapsed_seconds gauge\nsandhi_shutdown_elapsed_seconds {}\n",
        u8::from(state.lifecycle.is_running()), state.lifecycle.active_operations(),
        state.lifecycle.elapsed().as_secs_f64());
    (
        StatusCode::OK,
        [(
            axum::http::header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )],
        rendered,
    )
        .into_response()
}

/// `GET /v1/models` — OpenAI, Anthropic and Gemini discovery (TD-0010 D3).
///
/// The listing is the key's *permitted* models: the upstream catalog intersected with the virtual
/// key's allowlist. A key that may call two models lists two, which makes the allowlist
/// discoverable instead of a surprise 403 at call time, and is honest in a way a static catalog
/// dump is not.
async fn list_models(State(state): State<Arc<ProxyState>>, headers: HeaderMap) -> Response {
    // This selects a wire envelope only. Gemini discovery uses the same
    // credential/route authorization as its existing /v1beta surface.
    let anthropic = headers.contains_key("x-api-key")
        || (!headers.contains_key("x-goog-api-key") && headers.contains_key("anthropic-version"));
    if !anthropic
        && (headers.contains_key("x-goog-api-key") || headers.contains_key("x-goog-api-client"))
    {
        return list_models_gemini(State(state), headers).await;
    }
    let dialect = if anthropic {
        IngressDialect::Anthropic
    } else {
        IngressDialect::OpenAi
    };
    let (vk, provider) = match resolve_for_discovery(&state, dialect, &headers).await {
        Ok(pair) => pair,
        Err(response) => return response,
    };
    let models = match &provider {
        Some(provider) => permitted_provider_models(&vk, provider),
        None => match routing::discovery(&state, &vk).await {
            Ok(models) => models,
            Err(message) => return ingress_error(dialect, StatusCode::BAD_GATEWAY, message),
        },
    };
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
                "owned_by": provider.as_ref().map_or("sandhi",ProviderHandle::slug),
            })).collect::<Vec<_>>(),
        }),
    };
    Json(body).into_response()
}

/// `GET /v1beta/models` and Google-shaped `/v1/models` — Gemini discovery.
async fn list_models_gemini(State(state): State<Arc<ProxyState>>, headers: HeaderMap) -> Response {
    let (vk, provider) = match resolve_for_discovery(&state, IngressDialect::Gemini, &headers).await
    {
        Ok(pair) => pair,
        Err(response) => return response,
    };
    let models = match &provider {
        Some(provider) => permitted_provider_models(&vk, provider),
        None => match routing::discovery(&state, &vk).await {
            Ok(models) => models,
            Err(message) => {
                return ingress_error(IngressDialect::Gemini, StatusCode::BAD_GATEWAY, message)
            }
        },
    };
    Json(json!({
        "models": models.iter().filter(|id|sandhi_core::model_id_is_path_safe(id)).map(|id| json!({
            // Gemini names a model by its resource path, and its SDK strips the prefix back off.
            "name": format!("models/{id}"),
            "displayName": id,
            "supportedGenerationMethods": ["generateContent", "streamGenerateContent"],
        })).collect::<Vec<_>>(),
    }))
    .into_response()
}

/// Shared auth + upstream resolution for the discovery endpoints.
#[allow(clippy::result_large_err)]
fn provider_for_key(
    state: &ProxyState,
    vk: &VirtualKey,
    dialect: IngressDialect,
) -> Result<Option<ProviderHandle>, Response> {
    let provider = state
        .providers
        .lock()
        .expect("providers poisoned")
        .get(&vk.upstream_ref)
        .cloned();
    // Preserve existing host-owned names, including names that predate the route/
    // namespace. Logical registration cannot replace these handles.
    if provider.is_some() || vk.upstream_ref.starts_with("route/") {
        return Ok(provider);
    }
    Err(ingress_error(
        dialect,
        StatusCode::BAD_GATEWAY,
        "no upstream registered for this key",
    ))
}

#[allow(clippy::result_large_err)] // axum::Response is intentionally large; idiomatic shape.
async fn resolve_for_discovery(
    state: &Arc<ProxyState>,
    dialect: IngressDialect,
    headers: &HeaderMap,
) -> Result<(VirtualKey, Option<ProviderHandle>), Response> {
    let token = inference_credential(state, dialect, headers)?;
    let vk = match resolve_virtual_key(state, token, headers).await {
        VirtualKeyResolution::Denied(status) => {
            return Err(ingress_error(
                dialect,
                status,
                inference_denial_message(state),
            ))
        }
        VirtualKeyResolution::Found(vk, _, _) => *vk,
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
    let provider = provider_for_key(state, &vk, dialect)?;
    Ok((vk, provider))
}

/// The models this key may actually call: the upstream's catalog filtered by the allowlist.
///
/// With no allowlist the catalog is returned as-is. With an allowlist, entries the catalog does
/// not know about are still listed — the catalog holds transport facts, not an authority on which
/// models exist, so omitting an allowed-but-uncatalogued model would under-report what works.
fn permitted_provider_models(vk: &VirtualKey, provider: &ProviderHandle) -> Vec<String> {
    if let Some(binding) = provider.backend_binding() {
        return binding
            .models
            .iter()
            .filter(|model| vk.permits_model(&model.id))
            .map(|model| model.id.clone())
            .collect();
    }
    permitted_models(vk, provider.slug())
}

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

/// `POST /{v1|v1beta}/models/{model}:{generateContent|streamGenerateContent}`.
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
            "expected /v1/models/{model}:generateContent or its /v1beta equivalent",
        );
    };
    if model.is_empty() {
        return ingress_error(
            IngressDialect::Gemini,
            StatusCode::BAD_REQUEST,
            "model is empty",
        );
    }
    if method == "countTokens" {
        return handle_count_tokens(
            state,
            permit.0,
            headers,
            body,
            IngressDialect::Gemini,
            Some(GeminiRoute {
                model: model.to_string(),
                stream: false,
            }),
        )
        .await;
    }
    let stream = match method {
        "generateContent" => false,
        "streamGenerateContent" => true,
        // embedContent and the rest are not metered surfaces yet; say so in Gemini's
        // own error shape rather than 404-ing with an empty body. (countTokens
        // is handled above, ahead of this match.)
        other => {
            return ingress_error(
                IngressDialect::Gemini,
                StatusCode::NOT_IMPLEMENTED,
                &format!("method '{other}' is not supported by this gateway"),
            );
        }
    };
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
    let receipt = Arc::new(Mutex::new(None));
    let references = Arc::new(Mutex::new(None));
    let mut response = handle_with_policy_receipt(
        state,
        permit,
        headers,
        body,
        dialect,
        gemini_route,
        receipt.clone(),
        references.clone(),
    )
    .await;
    let reference_context = references
        .lock()
        .expect("reference context poisoned")
        .take();
    response = reference_ownership::protect(response, reference_context).await;
    if let Some(id) = receipt.lock().expect("policy receipt poisoned").as_ref() {
        response.headers_mut().insert(
            "x-sandhi-policy-receipt",
            axum::http::HeaderValue::from_str(id).expect("generated receipt"),
        );
    }
    response
}

#[allow(clippy::too_many_arguments)]
async fn handle_with_policy_receipt(
    state: Arc<ProxyState>,
    permit: Arc<AdmissionPermit>,
    headers: HeaderMap,
    body: Bytes,
    dialect: IngressDialect,
    mut gemini_route: Option<GeminiRoute>,
    policy_receipt: Arc<Mutex<Option<String>>>,
    references: Arc<Mutex<Option<reference_ownership::Context>>>,
) -> Response {
    // Body extraction may have spanned shutdown even after semaphore admission succeeded.
    if !state.lifecycle.is_running() {
        return draining_error(dialect);
    }
    let Some(owned) = state.buffered_accounting.get().cloned() else {
        return settlement::buffered::no_automatic_retry(ingress_error(
            dialect,
            StatusCode::SERVICE_UNAVAILABLE,
            "durable execution owner is not initialized",
        ));
    };
    // 1. Virtual key, presented the way this dialect's own SDK presents a credential
    //    (TD-0010 D1 — `x-api-key` on `/v1/messages`, `Authorization: Bearer` on the OpenAI
    //    paths). Resolve the live key store by exact token (legacy/demo path, where the id
    //    doubles as the token) then by its hash (operator-minted path, where only the hash is
    //    the lookup key — the plaintext is never retained).
    let vk_token = match inference_credential(&state, dialect, &headers) {
        Ok(token) => token,
        Err(response) => return response,
    };
    let (vk, rate_identity, policy_identity) = match resolve_virtual_key(&state, vk_token, &headers)
        .await
    {
        VirtualKeyResolution::Denied(status) => {
            return ingress_error(dialect, status, inference_denial_message(&state))
        }
        VirtualKeyResolution::Found(vk, rate_identity, identity) => (*vk, rate_identity, identity),
        VirtualKeyResolution::Expired => {
            return ingress_error(dialect, StatusCode::UNAUTHORIZED, "virtual key expired");
        }
        VirtualKeyResolution::NotFound => {
            return ingress_error(dialect, StatusCode::UNAUTHORIZED, "unknown virtual key");
        }
    };

    // 2. The upstream this key is bound to.
    let provider = match provider_for_key(&state, &vk, dialect) {
        Ok(provider) => provider,
        Err(response) => return response,
    };

    let preparation::PreparedRequest {
        vk,
        reference_context,
        defaulted_output,
        provider,
        request,
        body,
        wants_stream,
        raw_text_route,
        transparent_eligible,
        buffered_deadline,
        streaming_deadline,
        routing,
    } = match preparation::prepare(
        preparation::PreparationContext::from(state.as_ref()),
        preparation::ResolvedRoute {
            vk,
            provider,
            rate_identity,
            policy_identity,
        },
        &headers,
        body,
        dialect,
        gemini_route.as_ref(),
        &policy_receipt,
    )
    .await
    {
        Ok(prepared) => prepared,
        Err(response) => return response,
    };
    *references.lock().expect("reference context poisoned") = reference_context;
    if let Some(plan) = &routing {
        if let Some(route) = &mut gemini_route {
            route.model = plan.actual_model.clone();
        }
        tracing::debug!(
            route_revision = plan.route_revision,
            backend_revision = plan.backend_revision,
            "logical route dispatch plan frozen"
        );
    }

    // Reserve an estimated liability, not a proven token bound. Concurrent usage and
    // separate reasoning can exceed reservations/caps; settlement records measured usage.
    let scope = budget_scope(&vk);
    // A scope is "capped" (for output-bounding) only under a `Block` policy: `Warn` never rejects,
    // so we do not shrink the client's request. An output maximum reduces exposure when the
    // client omitted it; it does not make the estimated total a strict cap (TD-0026 W03).
    let input_len = if raw_text_route {
        text_endpoints::input_bytes(&request, body.len())
    } else {
        body.len()
    };
    let estimated_input = state
        .token_estimator
        .lock()
        .map(|estimator| {
            estimator.estimate(
                provider
                    .upstream_budget_scope()
                    .strip_prefix("upstream:")
                    .unwrap_or(provider.slug()),
                &request.model,
                input_len,
            )
        })
        .unwrap_or_else(|_| baseline_estimate(input_len));
    let output_max = request.max_output_tokens;
    let (ceiling, effective_max) = reservation_ceiling(&request, estimated_input);
    // Preserve the canonical budget namespace independently of diagnostic identity.
    let upstream_scope = provider.upstream_budget_scope().to_owned();
    let mut response = settlement::buffered::handle(
        Arc::clone(&state),
        owned,
        provider,
        settlement::Selection {
            upstream_ref: vk.upstream_ref.clone(),
            routing,
        },
        request,
        body,
        dialect,
        permit,
        wants_stream,
        transparent_eligible,
        scope,
        ceiling,
        effective_max,
        input_len,
        upstream_scope,
        buffered_deadline.map(|d| d.duration()),
        streaming_deadline,
    )
    .await;
    if defaulted_output {
        response.headers_mut().insert(
            "x-sandhi-output-limit-source",
            axum::http::HeaderValue::from_static("operator_default"),
        );
        if let Some(maximum) = output_max {
            response.headers_mut().insert(
                "x-sandhi-output-max",
                maximum
                    .to_string()
                    .parse()
                    .expect("validated numeric output limit"),
            );
        }
    }
    response
}

/// Ingress dialect → a bounded metric label (TD-0011 D2). Four values, fixed by the code.
fn dialect_label(dialect: IngressDialect) -> &'static str {
    match dialect {
        IngressDialect::OpenAi => "openai",
        IngressDialect::Embeddings => "openai_embeddings",
        IngressDialect::Completions => "openai_completions",
        IngressDialect::Anthropic => "anthropic",
        IngressDialect::Responses => "responses",
        IngressDialect::Gemini => "gemini",
    }
}

/// Ingress dialect → the upstream family it maps to, for plane selection (TD-0006 Step 2).
fn ingress_family(dialect: IngressDialect) -> ProviderFamily {
    match dialect {
        IngressDialect::OpenAi | IngressDialect::Embeddings | IngressDialect::Completions => {
            ProviderFamily::OpenAiCompat
        }
        IngressDialect::Anthropic => ProviderFamily::Anthropic,
        IngressDialect::Responses => ProviderFamily::OpenAiResponses,
        IngressDialect::Gemini => ProviderFamily::Gemini,
    }
}

/// The upstream path suffix for a same-family transparent forward — mirrors each typed adapter's
/// endpoint. Only the three ingress families above ever reach the transparent plane.
fn upstream_path(
    family: ProviderFamily,
    gemini: Option<&GeminiRoute>,
    dialect: IngressDialect,
) -> String {
    match dialect {
        IngressDialect::Embeddings => return "/embeddings".into(),
        IngressDialect::Completions => return "/completions".into(),
        _ => (),
    }
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

/// Counts describe the actual emitted wire view, never credential/backend authority.
/// Upstream headers are filtered before this server-owned value is inserted.
pub(crate) fn stamp_usage_convention(
    mut response: Response,
    provider: &ProviderHandle,
    dialect: IngressDialect,
    native: bool,
) -> Response {
    response
        .extensions_mut()
        .insert(reference_ownership::NativeEmission(native));
    response
        .headers_mut()
        .remove(sandhi_core::USAGE_CONVENTION_HEADER);
    if response.status().is_success() {
        let convention = if native {
            provider.source_usage_convention()
        } else {
            sandhi_core::UsageConventionV1::standard(ingress_family(dialect).endpoint_family())
        };
        if let Ok(convention) = convention {
            response.headers_mut().insert(
                sandhi_core::USAGE_CONVENTION_HEADER,
                axum::http::HeaderValue::from_static(convention.wire_value()),
            );
        }
    }
    response
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
    let started = std::time::Instant::now();
    // Unary path: the permit lives exactly as long as this handler future, which is the whole
    // call. Held only so the compiler sees it alive to the last await.
    let _permit = permit;
    let Some(forwarder) = provider.raw_forwarder() else {
        accounting.set_outcome("error");
        let _ = accounting.finalize_owned().await;
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
    let call_headers = accounting.per_call_wire_headers();
    let Some(owned) = accounting.owned.as_ref() else {
        return settlement::buffered::accounting_error(dialect, &accounting.request_id);
    };
    let observed_forwarder = forwarder.clone().with_metered_attempt_context(
        provider.slug(),
        Some(accounting.model.clone()),
        owned.attempts.context.clone(),
    );
    let response = observed_forwarder
        .forward_qualified_with_headers(
            &upstream_path(provider.family(), gemini.as_ref(), dialect),
            body,
            session.as_deref(),
            Some(accounting.request_id.as_str()),
            &call_headers,
        )
        .await
        .map(|r| (r.response, r.usage));
    match response {
        Ok((raw, usage)) => {
            let qualified = usage.is_ok();
            let mut usage = usage.unwrap_or_else(unqualified_buffered_usage);
            reconcile_boundary_duration(&mut usage, elapsed_ms(started));
            if qualified {
                usage.completeness = UsageCompleteness::Final;
            }
            usage.outcome.get_or_insert_with(|| "success".into());
            accounting.observe(&usage);
            accounting.set_outcome("success");
            if !accounting.finalize_owned().await {
                return settlement::buffered::accounting_error(dialect, &accounting.request_id);
            }
            stamp_usage_convention(raw_response_to_axum(raw, dialect), &provider, dialect, true)
        }
        Err(err) => {
            accounting.set_outcome("error");
            if !accounting.finalize_owned().await {
                return settlement::buffered::accounting_error(dialect, &accounting.request_id);
            }
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
    accounting: RequestAccounting,
    full_error_detail: bool,
    gemini: Option<GeminiRoute>,
    permit: Arc<AdmissionPermit>,
) -> Response {
    streaming::qualified_response(
        provider,
        body,
        session,
        dialect,
        accounting,
        full_error_detail,
        gemini,
        permit,
    )
    .await
}

/// Carries the existing execution owner through response delivery and one
/// terminal observation. Unavailable usage retains durable liability; Drop never
/// releases an unknown execution or invents a settlement.
struct RequestAccounting {
    routing: Option<routing::DispatchPlan>,
    owned: Option<settlement::buffered::Accounting>,
    stream_observation: Option<streaming::OwnedObservation>,
    /// Bounded metric dimensions for this call (TD-0011 D2) — set once at dispatch.
    dialect: &'static str,
    plane: metrics::Plane,
    state: Arc<ProxyState>,
    stream_body_lifetime: Option<streaming::StreamBodyLifetime>,
    scope: String,
    /// The original paired primary reservation; absent before durable admission.
    reservation: Option<Reservation>,
    /// Back-leg scope (`upstream:{provider}`, ADR-0015) this call was also admitted against.
    /// `None` when the call never took a back-leg lease (count-tokens, embedders, tests).
    upstream_scope: Option<String>,
    provider: String,
    model: String,
    /// UTF-8 request-body length paired with the final event for estimate calibration.
    input_len: usize,
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
    // Last field: outlives finalize AND trailing field destructors (notably SpanHandle::Drop).
    // Moves into streaming bodies, so idle cannot race a late telemetry producer.
    operation: Option<lifecycle::OperationGuard>,
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
            owned: None,
            routing: None,
            stream_observation: None,
            operation: None,
            stream_body_lifetime: None,
            state,
            scope,
            reservation,
            upstream_scope: None,
            provider,
            model: request.model.clone(),
            input_len: 0,
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

    /// Per-call wire headers for both forwarding planes (TD-0022 D1, caller-owned injection):
    /// this call's minted id on the vendor's declared correlation header (ADR-0008 D6;
    /// empty when the upstream declares none) plus the caller's W3C `traceparent`, so the
    /// upstream can emit a *child* of the caller's span and the echoed trace context
    /// genuinely links back.
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
    fn fire_alerts(&self, scope: &str, spent: u64, limit: Option<u64>) {
        let Some(registry) = &self.state.alerts else {
            return;
        };
        let fired = match registry.lock() {
            Ok(mut reg) => reg.evaluate(scope, spent, limit),
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

    fn emit_usage(&mut self, usage: &UsageV2, actual: u64, canonical: bool) {
        if let Some(reservation) = &self.reservation {
            if actual > reservation.ceiling {
                self.state
                    .metrics
                    .observe_settle_overshoot(actual - reservation.ceiling);
            }
        }
        // These quantities are measured observations, independent of confirmed
        // spend; only the paired durable receipt is authoritative.
        // Recovery never re-emits observations.
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
            recorder.record_usage(span, &self.provider, &self.model, usage, self.finish_reason);
        }
        let mut event = usage_event(
            &self.provider,
            &self.model,
            &self.metadata,
            usage,
            Some(self.request_id.as_str()),
        );
        if canonical {
            event.request_id.clone_from(&self.request_id);
        }
        if let Ok(mut estimator) = self.state.token_estimator.lock() {
            let mut calibrated = event.clone();
            if let Some(namespace) = &self.upstream_scope {
                if let Some(label) = namespace.strip_prefix("upstream:") {
                    calibrated.provider = label.to_owned();
                }
            }
            estimator.observe(self.input_len, &calibrated);
        }
        self.state.sink.emit(&event);
    }

    async fn alert_after_settlement(&mut self, wait: Duration) {
        if self.state.alerts.is_none() {
            return;
        }
        let Some(operation) = self.state.lifecycle.try_operation() else {
            return;
        };
        let state = self.state.clone();
        let mut scopes = vec![self.scope.clone()];
        if let Some(scope) = &self.upstream_scope {
            scopes.push(scope.clone());
        }
        let work = tokio::task::spawn_blocking(move || {
            let _operation = operation;
            let ledger = state.ledger.lock().ok()?;
            let budgets = state.budgets.lock().ok()?;
            Some(
                scopes
                    .into_iter()
                    .map(|scope| {
                        let spent = ledger.spent(&scope);
                        let limit = budgets.get(&scope).map(|budget| budget.limit_tokens);
                        (scope, spent, limit)
                    })
                    .collect::<Vec<_>>(),
            )
        });
        if let Ok(Ok(Some(rows))) = tokio::time::timeout(wait, work).await {
            for (scope, spent, limit) in rows {
                self.fire_alerts(&scope, spent, limit);
            }
        }
    }

    async fn finalize_owned(&mut self) -> bool {
        let Some(mut owned) = self.owned.take() else {
            return false;
        };
        self.finalized = true;
        streaming::capture_terminal(self);
        if let Some(evidence) = owned.attempts.context.qualified_stream_evidence() {
            streaming::capture_provider_terminal(self, evidence);
        }
        if let Some(evidence) = owned.attempts.context.qualified_buffered_evidence() {
            let mut source = match evidence.usage {
                Ok(mut usage) => {
                    usage.outcome = Some(self.outcome.into());
                    usage
                }
                Err(error) => unqualified_buffered_usage(error),
            };
            source.upstream_request_id = evidence.upstream_request_id;
            // Projection can contribute boundary timing, never replace counts.
            if let Some(previous) = &self.usage {
                source.duration_ms = previous.duration_ms;
                source.duration_source = previous.duration_source;
                source.time_to_first_token_ms = previous.time_to_first_token_ms;
                source.time_to_first_token_source = previous.time_to_first_token_source;
            }
            self.observe(&source);
        }
        let mut usage = self.usage.take().unwrap_or_default();
        usage.attempts = owned
            .attempts
            .context
            .dispatch_count()
            .min(u64::from(u32::MAX)) as u32;
        usage.outcome.get_or_insert_with(|| self.outcome.into());
        let measured = matches!(
            usage.completeness,
            UsageCompleteness::Final | UsageCompleteness::Partial
        );
        // Retain immutable terminal evidence BEFORE invoking arbitrary best-effort
        // telemetry. A panicking sink must not erase a completed model's usage.
        let actual = if measured {
            sandhi_core::billable_weighted(&usage, owned.cache_read_weight_bp)
        } else {
            0
        };
        // Both linked leases belong to the durable execution. The canonical
        // settlement transaction charges both or neither; refusal/cancellation
        // leaves unknown liability held instead of invoking legacy settle(0).
        let pending = owned
            .execution
            .into_settlement(usage.clone())
            .with_attempt_evidence(owned.attempts.finish())
            .with_cache_read_weight(owned.cache_read_weight_bp);
        let submitted = owned
            .obligation
            .submit(settlement::jobs::Work::Settle(pending));
        // A refusal already retained the immutable snapshot in the obligation.
        let ticket = match submitted {
            Ok(ticket) => Some(ticket),
            Err(rejected) => {
                tracing::warn!(request_id = %self.request_id, reason = ?rejected.reason, "tracked terminal scheduling refused");
                None
            }
        };
        self.emit_usage(&usage, actual, true);
        let Some(ticket) = ticket else {
            return false;
        };
        let wait_started = std::time::Instant::now();
        if tokio::time::timeout(owned.wait, ticket.wait())
            .await
            .is_err()
        {
            ticket.warn_timeout(
                &self.request_id,
                "terminal_settlement",
                owned.wait,
                wait_started,
            );
            return false;
        }
        match ticket.take() {
            Some(settlement::jobs::Outcome::Settlement(settlement::Attempt::Committed {
                ..
            })) => {
                self.alert_after_settlement(owned.wait).await;
                true
            }
            outcome => {
                let reason = match outcome {
                    Some(settlement::jobs::Outcome::Settlement(
                        settlement::Attempt::Unresolved { failure, .. },
                    )) => failure.diagnostic_code(),
                    _ => "missing_or_interrupted_result",
                };
                tracing::warn!(request_id = %self.request_id, reason, "tracked terminal settlement unresolved");
                false
            }
        }
    }
}

fn unqualified_buffered_usage(error: sandhi_core::usage::BufferedUsageError) -> UsageV2 {
    UsageV2 {
        attempts: 1,
        outcome: Some(format!("buffered_qualification:{error:?}")),
        ..UsageV2::default()
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
    let started = std::time::Instant::now();
    let _permit = permit; // unary: held by the handler future, which is the whole call
    let correlation = accounting.per_call_wire_headers();
    let Some(owned) = accounting.owned.as_ref() else {
        return settlement::buffered::accounting_error(dialect, &accounting.request_id);
    };
    let result = provider
        .complete_with_qualified_usage_and_attempts(
            request,
            correlation,
            owned.attempts.context.clone(),
        )
        .await
        .map(|r| (r.response, r.usage));
    match result {
        Ok((mut response, usage)) => {
            let qualified = usage.is_ok();
            response.usage = usage.unwrap_or_else(unqualified_buffered_usage);
            if qualified {
                response.usage.completeness = UsageCompleteness::Final;
            }
            let encoded = encode_response_with_emission(dialect, &response);
            if encoded.is_err() {
                response.usage.outcome = Some("error".into());
            } else {
                response
                    .usage
                    .outcome
                    .get_or_insert_with(|| "success".into());
            }
            accounting.observe(&response.usage);
            accounting.finish_reason = response.finish_reason;
            accounting.set_outcome(if encoded.is_ok() { "success" } else { "error" });
            if !accounting.finalize_owned().await {
                return settlement::buffered::accounting_error(dialect, &accounting.request_id);
            }
            match encoded {
                Ok((body, native)) => {
                    stamp_usage_convention(Json(body).into_response(), &provider, dialect, native)
                }
                Err(message) => provider_error(
                    &ProviderError::OutputProjection(message.into()),
                    dialect,
                    provider.slug(),
                    full_error_detail,
                ),
            }
        }
        Err(error) => {
            accounting.set_outcome("error");
            let mut unavailable = UsageV2::default();
            reconcile_boundary_duration(&mut unavailable, elapsed_ms(started));
            accounting.observe(&unavailable);
            if !accounting.finalize_owned().await {
                return settlement::buffered::accounting_error(dialect, &accounting.request_id);
            }
            let response = provider_error(&error, dialect, provider.slug(), full_error_detail);
            settlement::buffered::no_automatic_retry(response)
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
    let native_stream = request.include_native_stream;
    let tracked_translation = !native_stream;
    let correlation = accounting.per_call_wire_headers();
    let Some(owned) = accounting.owned.as_ref() else {
        return settlement::buffered::accounting_error(dialect, &accounting.request_id);
    };
    let upstream = provider
        .stream_with_qualified_usage_and_attempts(
            request,
            correlation,
            owned.attempts.context.clone(),
        )
        .await;
    let mut upstream = match upstream {
        Ok(s) => s,
        Err(error) => {
            accounting.set_outcome("error");
            if !accounting.finalize_owned().await {
                return settlement::buffered::accounting_error(dialect, &accounting.request_id);
            }
            let response = provider_error(&error, dialect, provider.slug(), full_error_detail);
            return settlement::buffered::no_automatic_retry(response);
        }
    };

    let tail = (dialect == IngressDialect::OpenAi && !native_stream)
        .then(|| Bytes::from_static(b"data: [DONE]\n\n"));
    let response_provider = provider.clone();
    let body = streaming::body(accounting, permit, tail, move |accounting| {
        Box::pin(async_stream::try_stream! {
            let mut last_usage: Option<UsageV2> = None;
            // Stateful per-stream egress encoder: Anthropic's content-block
            // lifecycle and Gemini's whole-part tool calls need cross-event state.
            let mut stream_encoder = StreamEventEncoder::new(dialect);
            let mut cache_read_observation = None;
            // A non-final `Usage` event is accounting-only and must not reach the client (TD-0013 D7):
            // the ingress wire shape is a TD-0010 parity guarantee, and a metering improvement that
            // adds frames to a caller's stream has broken something more important than it fixed.
            let mut accounting_only;
            let mut native_error_sent = false;
            let mut pending_finish = None;
            let mut projection_bytes = 0usize;
            while let Some(item) = upstream.next().await {
                match item {
                    Ok(event) => {
                        if tracked_translation {
                            // Responses retains text/tool output for its final
                            // frame; Gemini retains incomplete tool arguments.
                            // Bound that projection independently of wire queue
                            // backpressure and heuristic token reservations.
                            let payload = match &event {
                                sandhi_core::ChatStreamEventV1::TextDelta{delta}
                                | sandhi_core::ChatStreamEventV1::ReasoningDelta{delta}
                                | sandhi_core::ChatStreamEventV1::RefusalDelta{delta}
                                | sandhi_core::ChatStreamEventV1::ToolCallArgumentsDelta{delta,..} => delta.len(),
                                sandhi_core::ChatStreamEventV1::ToolCallStart{id,name,..} => id.len().saturating_add(name.len()),
                                sandhi_core::ChatStreamEventV1::ResponseStart{id,model} => id.as_ref().map_or(0,String::len).saturating_add(model.len()),
                                _ => 0,
                            };
                            projection_bytes=projection_bytes.saturating_add(payload).saturating_add(128);
                            if projection_bytes > sandhi_core::MAX_NATIVE_STREAM_FRAME_BYTES {
                                accounting.set_outcome("error");
                                Err(std::io::Error::other("translated stream projection limit exceeded"))?;
                            }
                        }
                        accounting_only = false;
                        match &event {
                            sandhi_core::ChatStreamEventV1::Usage { usage }
                                if usage.completeness != UsageCompleteness::Final =>
                            {
                                // Progress, not a verdict: it must not supersede the terminal frame.
                                sandhi_core::merge_cache_read_observation(&mut cache_read_observation, usage.cache_read_observation);
                                accounting.observe(usage);
                                accounting_only = true;
                            }
                            sandhi_core::ChatStreamEventV1::Usage { usage } => {
                                sandhi_core::merge_cache_read_observation(&mut cache_read_observation, usage.cache_read_observation);
                                // Terminal, authoritative usage — replaces earlier source progress.
                                accounting.observe(usage);
                                last_usage = Some(usage.clone());
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
                        if let Some(usage) = accounting.usage.as_mut() {
                            usage.cache_read_observation = cache_read_observation;
                        }
                        if accounting_only {
                            continue;
                        }
                        if !native_stream && dialect == IngressDialect::OpenAi
                            && matches!(event,sandhi_core::ChatStreamEventV1::Usage{..})
                        {
                            // Chat's empty-choices total must remain the final
                            // chunk before DONE, after the deferred finish reason.
                            continue;
                        }
                        // Chat's finish reason can precede its final usage chunk.
                        // Defer translated terminal frames until source EOF so
                        // SDKs receive final counts before completion, and late
                        // source qualification errors cannot look clean.
                        if !native_stream {
                            if let sandhi_core::ChatStreamEventV1::Finish { reason } = &event {
                                pending_finish = Some(*reason);
                                continue;
                            }
                        }
                        if let sandhi_core::ChatStreamEventV1::NativeFrame { frame } = &event {
                            if !native_stream || frame.endpoint_family != ingress_family(dialect).endpoint_family()
                                || frame.sse.len() > sandhi_core::MAX_NATIVE_STREAM_FRAME_BYTES
                            {
                                accounting.set_outcome("error");
                                Err(std::io::Error::other("incompatible or oversized native SSE attachment"))?;
                            }
                            native_error_sent |= frame.json_data().is_some_and(|value|
                                value.get("error").is_some_and(|error| !error.is_null()) || value.get("type").and_then(serde_json::Value::as_str) == Some("error"));
                            yield Bytes::from(frame.sse.clone());
                            continue;
                        }
                        if native_stream && (!matches!(event, sandhi_core::ChatStreamEventV1::Error { .. }) || native_error_sent) {
                            continue;
                        }
                        for (event_name, value) in
                            stream_encoder.encode(&event, last_usage.as_ref()).map_err(|message| { accounting.set_outcome("error"); std::io::Error::other(message) })?
                        {
                            yield Bytes::from(sse_frame(event_name, &value));
                        }
                    }
                    Err(error) => {
                        accounting.set_outcome("error");
                        if native_stream && native_error_sent { break; }
                        let typed = sandhi_core::ChatStreamEventV1::Error {
                            error: error.as_typed(Some(provider.slug())),
                        };
                        for (event_name, value) in
                            stream_encoder.encode(&typed, last_usage.as_ref()).map_err(|message| { accounting.set_outcome("error"); std::io::Error::other(message) })?
                        {
                            yield Bytes::from(sse_frame(event_name, &value));
                        }
                        break;
                    }
                }
            }
            if accounting.outcome != "error" {
                accounting.set_outcome("success");
                if let Some(reason) = pending_finish {
                    for (event_name,value) in stream_encoder.encode(&sandhi_core::ChatStreamEventV1::Finish{reason},last_usage.as_ref()).map_err(|message| { accounting.set_outcome("error"); std::io::Error::other(message) })? {
                        yield Bytes::from(sse_frame(event_name,&value));
                    }
                }
                if !native_stream && dialect == IngressDialect::OpenAi {
                    if let Some(usage) = &last_usage {
                        for (event_name,value) in stream_encoder.encode(&sandhi_core::ChatStreamEventV1::Usage{usage:usage.clone()},Some(usage)).map_err(|message| { accounting.set_outcome("error"); std::io::Error::other(message) })? {
                            yield Bytes::from(sse_frame(event_name,&value));
                        }
                    }
                }
            } else {
                // Failed source EOF can never schedule a success tail.
                Err(std::io::Error::other("upstream stream failed"))?;
            }
        })
    });

    stamp_usage_convention(
        Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "text/event-stream")
            .body(body)
            .expect("valid streaming response"),
        &response_provider,
        dialect,
        native_stream,
    )
}

/// Cold-start input-token estimate: bytes of the ingress request body / 4. This replaces a full
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
/// schemas). TD-0027 S3 keeps this exact rule for the first 31 measured model calls, then applies
/// the bounded per-model calibration in [`TokenEstimateCalibrator`].
#[cfg(test)]
fn input_estimate(body_len: usize) -> u64 {
    baseline_estimate(body_len)
}

/// The reservation **ceiling** (ADR-0005 D1): input estimate + the effective output max (the
/// client's `max_output_tokens`, or [`DEFAULT_OUTPUT_CEILING`] when unbounded). Returns the ceiling
/// and the effective max so the caller can bound a capped scope's upstream request. This is a
/// heuristic reservation, NOT a proven upper bound. Output controls alone cannot bound input,
/// media, provider-added tools or separately reported reasoning across arbitrary providers.
fn reservation_ceiling(request: &ChatRequestV1, estimated_input: u64) -> (u64, u64) {
    let effective_max = request.max_output_tokens.unwrap_or(DEFAULT_OUTPUT_CEILING);
    let ceiling = estimated_input.saturating_add(effective_max).max(1);
    (ceiling, effective_max)
}

/// A trimmed header value as an owned `String`, or `None` when absent/non-UTF-8/empty.
fn header_str(headers: &HeaderMap, name: &str) -> Option<String> {
    let value = headers.get(name)?.to_str().ok()?.trim();
    (!value.is_empty()).then(|| value.to_string())
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
    .with_reasoning(usage.reasoning_tokens)
    .with_reasoning_included(usage.reasoning_included)
    .with_cache(usage.cache_creation_tokens, usage.cache_read_tokens)
    .with_cache_read_observation(usage.cache_read_observation)
    .with_measurement(
        usage.completeness,
        usage.attempts,
        usage.outcome.clone(),
        usage.upstream_request_id.clone(),
    )
    .with_basis(usage.basis)
    .with_latency(usage.duration_ms, usage.time_to_first_token_ms)
    .with_latency_sources(usage.duration_source, usage.time_to_first_token_source)
}

fn elapsed_ms(started: std::time::Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

fn reconcile_boundary_duration(usage: &mut UsageV2, boundary_ms: u64) {
    if usage.duration_ms.is_some() {
        usage.duration_source.get_or_insert(LatencySource::Origin);
    } else {
        usage.duration_ms = Some(boundary_ms);
        usage.duration_source = Some(LatencySource::Boundary);
    }
}

fn reconcile_boundary_ttft(usage: &mut UsageV2, boundary_ms: Option<u64>) {
    if usage.time_to_first_token_ms.is_some() {
        usage
            .time_to_first_token_source
            .get_or_insert(LatencySource::Origin);
    } else if let Some(boundary_ms) = boundary_ms {
        usage.time_to_first_token_ms = Some(boundary_ms);
        usage.time_to_first_token_source = Some(LatencySource::Boundary);
    }
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
            duration_source: Some(LatencySource::Origin),
            time_to_first_token_ms: Some(56),
            time_to_first_token_source: Some(LatencySource::Boundary),
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
        assert_eq!(event.duration_source, Some(LatencySource::Origin));
        assert_eq!(event.time_to_first_token_ms, Some(56));
        assert_eq!(
            event.time_to_first_token_source,
            Some(LatencySource::Boundary)
        );
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
    Found(Box<VirtualKey>, String, sandhi_core::policy::Identity),
    NotFound,
    Expired,
    Denied(StatusCode),
}

/// `POST /v1/messages/count_tokens` and `POST /v1beta/models/{model}:countTokens` —
/// answer the dialect's token-counting endpoint from the request body alone.
///
/// Auth, decode validation, model allowlist, the policy input gate and the
/// per-key rate limit all run exactly as on the generation path (`prepare`) —
/// but nothing is reserved, generated or metered: no model call is made, so no
/// lease and no UsageEvent. The estimate is the SAME number the gateway
/// reserves against for generation (`TokenEstimator::estimate` over the
/// request bytes), which keeps it consistent with budget enforcement, if
/// approximate against a vendor tokenizer.
async fn handle_count_tokens(
    state: Arc<ProxyState>,
    permit: Arc<AdmissionPermit>,
    headers: HeaderMap,
    body: Bytes,
    dialect: IngressDialect,
    gemini_route: Option<GeminiRoute>,
) -> Response {
    // Underscore-binding keeps the admission permit alive to the end of the
    // handler (the layer's unary invariant) without further use below.
    let _permit = permit;
    if !state.lifecycle.is_running() {
        return draining_error(dialect);
    }
    // Hold the admission slot for the handler's duration, like every unary
    // handler (parse + policy work run under it); it is released at return and
    // counting never dispatches, so no generation slot is held upstream.
    let vk_token = match inference_credential(&state, dialect, &headers) {
        Ok(token) => token,
        Err(response) => return response,
    };
    let (vk, rate_identity, policy_identity) = match resolve_virtual_key(&state, vk_token, &headers)
        .await
    {
        VirtualKeyResolution::Denied(status) => {
            return ingress_error(dialect, status, inference_denial_message(&state))
        }
        VirtualKeyResolution::Found(vk, rate_identity, identity) => (*vk, rate_identity, identity),
        VirtualKeyResolution::Expired => {
            return ingress_error(dialect, StatusCode::UNAUTHORIZED, "virtual key expired")
        }
        VirtualKeyResolution::NotFound => {
            return ingress_error(dialect, StatusCode::UNAUTHORIZED, "unknown virtual key")
        }
    };
    let provider = match provider_for_key(&state, &vk, dialect) {
        Ok(provider) => provider,
        Err(response) => return response,
    };
    let receipt = Arc::new(Mutex::new(None));
    let preparation::PreparedRequest {
        request,
        body,
        provider,
        ..
    } = match preparation::prepare(
        preparation::PreparationContext::from(state.as_ref()).for_counting(),
        preparation::ResolvedRoute {
            vk,
            provider,
            rate_identity,
            policy_identity,
        },
        &headers,
        body,
        dialect,
        gemini_route.as_ref(),
        &receipt,
    )
    .await
    {
        Ok(prepared) => prepared,
        Err(response) => return response,
    };
    let tokens = state
        .token_estimator
        .lock()
        .map(|estimator| {
            estimator.estimate(
                provider
                    .upstream_budget_scope()
                    .strip_prefix("upstream:")
                    .unwrap_or(provider.slug()),
                &request.model,
                body.len(),
            )
        })
        .unwrap_or_else(|_| sandhi_providers::token_estimate::baseline_estimate(body.len()));
    match dialect {
        IngressDialect::Gemini => Json(json!({"totalTokens": tokens})).into_response(),
        _ => Json(json!({"input_tokens": tokens})).into_response(),
    }
}

fn inference_denial_message(state: &ProxyState) -> &'static str {
    if starter::enabled(state) {
        "starter requires an available, unambiguous server-owned upstream and shared-principal admission"
    } else {
        "OIDC inference authorization failed"
    }
}

#[allow(clippy::result_large_err)]
fn inference_credential<'a>(
    state: &ProxyState,
    dialect: IngressDialect,
    headers: &'a HeaderMap,
) -> Result<&'a str, Response> {
    if state.oidc.is_none() && state.auth_mode == auth::AuthMode::Oidc {
        return Err(ingress_error(
            dialect,
            StatusCode::SERVICE_UNAVAILABLE,
            "OIDC authority unavailable",
        ));
    }
    if starter::enabled(state) {
        // A keyless listener must reject foreign browser origins and DNS
        // rebinding before discovery, token estimation or generation admission.
        // All ingress families share this gate; neutral agents need no browser
        // headers, but their Host must still name this configured listener.
        if !starter::control_allowed(state, headers, true) {
            return Err(ingress_error(
                dialect,
                StatusCode::FORBIDDEN,
                "starter request origin is not permitted",
            ));
        }
        return Ok("");
    }
    dialect.extract_credential(headers).ok_or_else(|| {
        ingress_error(
            dialect,
            StatusCode::UNAUTHORIZED,
            &format!(
                "missing virtual key: send it as {}",
                dialect.credential_hint()
            ),
        )
    })
}

/// Resolve a presented virtual-key token: exact (legacy demo) then by hash (operator-minted).
/// Filters out expired keys.
async fn resolve_virtual_key(
    state: &ProxyState,
    token: &str,
    headers: &HeaderMap,
) -> VirtualKeyResolution {
    if let Some(oidc) = &state.oidc {
        if token.starts_with("vk_") {
            return match oidc
                .delegated_key_with_identity(state, token, headers)
                .await
            {
                Ok((grant, rate_identity, identity)) => {
                    VirtualKeyResolution::Found(Box::new(grant), rate_identity, identity)
                }
                Err(status) => VirtualKeyResolution::Denied(status),
            };
        }
        return match oidc.inference_with_identity(token, headers).await {
            Ok((grant, identity)) => {
                VirtualKeyResolution::Found(Box::new(grant.clone()), grant.id, identity)
            }
            Err(r) => VirtualKeyResolution::Denied(r.status()),
        };
    }
    if starter::enabled(state) {
        return match starter::principal(state, headers).await {
            Ok(vk) => VirtualKeyResolution::Found(
                Box::new(vk.clone()),
                vk.id.clone(),
                sandhi_core::policy::Identity {
                    subject: vk.subject_id.clone(),
                    ..Default::default()
                },
            ),
            Err(status) => VirtualKeyResolution::Denied(status),
        };
    }
    if state.auth_mode == auth::AuthMode::Oidc {
        return VirtualKeyResolution::Denied(StatusCode::SERVICE_UNAVAILABLE);
    }
    let vk = state
        .keys
        .resolve(token)
        .or_else(|| state.keys.resolve(&hash_secret(token)));
    match vk {
        Some(vk) => {
            if vk.id == starter::KEY_ID {
                return VirtualKeyResolution::NotFound;
            }
            if vk.is_expired(&now_rfc3339()) {
                VirtualKeyResolution::Expired
            } else {
                VirtualKeyResolution::Found(
                    Box::new(vk.clone()),
                    vk.id.clone(),
                    sandhi_core::policy::Identity {
                        subject: vk.subject_id.clone(),
                        ..Default::default()
                    },
                )
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

fn dialect_for_path(path: &str) -> IngressDialect {
    if path.starts_with("/v1beta/") {
        IngressDialect::Gemini
    } else if path == "/v1/messages" || path == "/v1/messages/count_tokens" {
        IngressDialect::Anthropic
    } else if path == "/v1/responses" {
        IngressDialect::Responses
    } else {
        IngressDialect::OpenAi
    }
}

fn draining_error(dialect: IngressDialect) -> Response {
    let mut response = codec::IngressError::draining().render(dialect);
    response
        .headers_mut()
        .insert(axum::http::header::RETRY_AFTER, "1".parse().unwrap());
    response
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
mod token_calibration_wiring_tests {
    use super::*;
    use sandhi_core::InMemorySink;

    fn state() -> Arc<ProxyState> {
        Arc::new(ProxyState::new(
            KeyStore::new(),
            ProxyLedger::in_memory(),
            Arc::new(InMemorySink::new()),
            HashMap::new(),
            None,
        ))
    }

    fn measured_accounting(
        state: &Arc<ProxyState>,
        input_len: usize,
        input_tokens: u64,
        output_tokens: u64,
        reservation: Option<Reservation>,
    ) {
        let request: ChatRequestV1 =
            serde_json::from_str(r#"{"model":"qwen","messages":[]}"#).unwrap();
        let mut accounting = RequestAccounting::new(
            Arc::clone(state),
            "vk:test".into(),
            reservation,
            "inferflux".into(),
            &request,
            "openai",
            metrics::Plane::Transparent,
        );
        accounting.input_len = input_len;
        accounting.observe(&UsageV2 {
            tokens_in: input_tokens,
            tokens_out: output_tokens,
            completeness: UsageCompleteness::Final,
            ..Default::default()
        });
        accounting.set_outcome("success");
        let usage = accounting.usage.take().unwrap();
        accounting.emit_usage(&usage, sandhi_core::billable(&usage), false);
    }

    #[test]
    fn renamed_diagnostics_keep_estimator_learning_on_the_original_namespace() {
        let sink = Arc::new(InMemorySink::new());
        let state = Arc::new(ProxyState::new(
            KeyStore::new(),
            ProxyLedger::in_memory(),
            sink.clone(),
            HashMap::new(),
            None,
        ));
        for _ in 0..sandhi_providers::token_estimate::CALIBRATION_MIN_SAMPLES {
            let request: ChatRequestV1 =
                serde_json::from_str(r#"{"model":"qwen","messages":[]}"#).unwrap();
            let mut accounting = RequestAccounting::new(
                state.clone(),
                "vk:test".into(),
                None,
                "vendor-label".into(),
                &request,
                "openai",
                metrics::Plane::Transparent,
            );
            accounting.upstream_scope = Some("upstream:inferflux".into());
            accounting.input_len = 300;
            accounting.observe(&UsageV2 {
                tokens_in: 100,
                tokens_out: 10,
                completeness: UsageCompleteness::Final,
                ..Default::default()
            });
            let usage = accounting.usage.take().unwrap();
            accounting.emit_usage(&usage, sandhi_core::billable(&usage), false);
        }
        assert_eq!(
            state
                .token_estimator
                .lock()
                .unwrap()
                .estimate("inferflux", "qwen", 300),
            100
        );
        assert!(sink
            .events()
            .iter()
            .all(|event| event.provider == "vendor-label"));
    }

    fn reservation(ceiling: u64) -> Reservation {
        Reservation {
            id: 999_999,
            scope: "vk:test".into(),
            ceiling,
            expires_at: OffsetDateTime::now_utc() + time::Duration::minutes(1),
        }
    }

    #[test]
    fn measured_events_train_the_next_reservation_and_reduce_overshoot_metric_growth() {
        let calibrated = state();
        for _ in 0..sandhi_providers::token_estimate::CALIBRATION_MIN_SAMPLES {
            measured_accounting(&calibrated, 300, 100, 10, None);
        }
        let estimated_input =
            calibrated
                .token_estimator
                .lock()
                .unwrap()
                .estimate("inferflux", "qwen", 300);
        assert_eq!(estimated_input, 100);
        measured_accounting(
            &calibrated,
            300,
            100,
            10,
            Some(reservation(estimated_input + 10)),
        );

        let baseline = state();
        measured_accounting(
            &baseline,
            300,
            100,
            10,
            Some(reservation(input_estimate(300) + 10)),
        );

        assert!(calibrated
            .metrics
            .render()
            .contains("sandhi_settle_overshoot_tokens_total 0"));
        assert!(baseline
            .metrics
            .render()
            .contains("sandhi_settle_overshoot_tokens_total 25"));
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
        let usage = acc.usage.take().unwrap();
        acc.emit_usage(&usage, sandhi_core::billable(&usage), false);
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
#[test]
fn reasoning_survives_event_conversion() {
    for thoughts in [25, 40, 90] {
        let usage = UsageV2 {
            tokens_in: 70,
            cache_read_tokens: 30,
            tokens_out: 40,
            reasoning_tokens: Some(thoughts),
            reasoning_included: Some(false),
            ..UsageV2::default()
        };
        let event = usage_event("gemini", "m", &RequestMetadataV1::default(), &usage, None);
        assert_eq!(event.billable_tokens(), 140 + thoughts);
    }
}

#[cfg(test)]
mod upstream_budget_tests {
    use super::*;
    use axum::http::Request;
    use sandhi_core::{InMemorySink, Policy, Window};
    use tower::ServiceExt;
    use wiremock::{matchers::method, Mock, MockServer, ResponseTemplate};
    const FRONT: &str = "group:platform";
    const BACK: &str = "upstream:echo";
    struct Fixture {
        state: Arc<ProxyState>,
        origin: MockServer,
        sink: Arc<InMemorySink>,
        path: std::path::PathBuf,
        _directory: tempfile::TempDir,
    }
    async fn fixture(primary: (u64, Policy), upstream: (u64, Policy), delay: Duration) -> Fixture {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("original.db");
        let origin = MockServer::start().await;
        Mock::given(method("POST")).respond_with(ResponseTemplate::new(200).set_delay(delay).set_body_json(json!({
            "id":"fixture","object":"chat.completion","model":"model",
            "choices":[{"index":0,"message":{"role":"assistant","content":"reply"},"finish_reason":"stop"}],
            "usage":{"prompt_tokens":40,"completion_tokens":20,"total_tokens":60,"prompt_tokens_details":{"cached_tokens":0,"cache_write_tokens":0},"completion_tokens_details":{"reasoning_tokens":0}}
        }))).mount(&origin).await;
        let keys = KeyStore::new();
        keys.insert(VirtualKey {
            id: "vk-test".into(),
            group_id: Some("platform".into()),
            upstream_ref: "echo".into(),
            ..Default::default()
        });
        let provider = ProviderRuntime::new().openai_compat(
            "echo",
            format!("{}/v1", origin.uri()),
            "synthetic",
            Default::default(),
            Some(0),
            Some(5.0),
            Some(2.0),
            Some(2.0),
        );
        let mut ledger = ProxyLedger::durable(path.to_str().unwrap(), 1).unwrap();
        ledger
            .set_budget(FRONT, Some(primary.0), Window::Total, primary.1)
            .unwrap();
        ledger
            .set_budget(BACK, Some(upstream.0), Window::Total, upstream.1)
            .unwrap();
        let sink = Arc::new(InMemorySink::new());
        let mut state = ProxyState::new(
            keys,
            ledger,
            sink.clone(),
            HashMap::from([("echo".into(), provider)]),
            None,
        );
        state.alerts = Some(Arc::new(Mutex::new(AlertRegistry::default())));
        operator::rehydrate_budgets(&state.ledger.lock().unwrap(), &state.budgets);
        Fixture {
            state: Arc::new(state),
            origin,
            sink,
            path,
            _directory: directory,
        }
    }
    fn request() -> Request<Body> {
        Request::post("/v1/chat/completions")
            .header("authorization", "Bearer vk-test")
            .header("content-type", "application/json")
            .body(Body::from(
                r#"{"model":"model","messages":[{"role":"user","content":"hi"}],"max_tokens":40}"#,
            ))
            .unwrap()
    }
    #[tokio::test]
    async fn either_hard_cap_refuses_atomically_without_dispatch_or_orphan_holds() {
        for (primary, upstream) in [(10, 1000), (1000, 10)] {
            let fixture = fixture(
                (primary, Policy::Block),
                (upstream, Policy::Block),
                Duration::ZERO,
            )
            .await;
            let response = build_app(fixture.state.clone())
                .oneshot(request())
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
            assert!(fixture.origin.received_requests().await.unwrap().is_empty());
            let ledger = fixture.state.ledger.lock().unwrap();
            assert_eq!((ledger.reserved(FRONT), ledger.reserved(BACK)), (0, 0));
            assert!(fixture.sink.is_empty());
        }
    }
    #[tokio::test]
    async fn one_original_receipt_charges_both_scopes_from_qualified_source() {
        let fixture = fixture((1000, Policy::Block), (1000, Policy::Block), Duration::ZERO).await;
        let response = build_app(fixture.state.clone())
            .oneshot(request())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(fixture.origin.received_requests().await.unwrap().len(), 1);
        assert_eq!(fixture.sink.len(), 1);
        let ledger = fixture.state.ledger.lock().unwrap();
        assert_eq!((ledger.spent(FRONT), ledger.spent(BACK)), (60, 60));
        assert_eq!((ledger.reserved(FRONT), ledger.reserved(BACK)), (0, 0));
        let connection = rusqlite::Connection::open(&fixture.path).unwrap();
        let (intents,receipts,attempts):(i64,i64,i64)=connection.query_row("SELECT (SELECT count(*) FROM budget_execution_intent),(SELECT count(*) FROM budget_settlement_outbox),(SELECT count(*) FROM budget_physical_attempt)",[],|row|Ok((row.get(0)?,row.get(1)?,row.get(2)?))).unwrap();
        assert_eq!((intents, receipts, attempts), (1, 1, 1));
    }
    #[tokio::test]
    async fn omitted_output_limit_uses_visible_operator_default_on_the_actual_wire() {
        let mut fixture = fixture(
            (100_000, Policy::Block),
            (100_000, Policy::Block),
            Duration::ZERO,
        )
        .await;
        Arc::get_mut(&mut fixture.state)
            .unwrap()
            .default_max_output_tokens = 37;
        let request = Request::post("/v1/chat/completions")
            .header("authorization", "Bearer vk-test")
            .header("content-type", "application/json")
            .body(Body::from(
                r#"{"model":"model","messages":[{"role":"user","content":"hi"}]}"#,
            ))
            .unwrap();
        let response = build_app(fixture.state.clone())
            .oneshot(request)
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers()["x-sandhi-output-limit-source"],
            "operator_default"
        );
        assert_eq!(response.headers()["x-sandhi-output-max"], "37");
        let requests = fixture.origin.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1);
        let body: Value = serde_json::from_slice(&requests[0].body).unwrap();
        assert_eq!(body["max_tokens"], 37);
        assert_eq!(fixture.state.ledger.lock().unwrap().spent(FRONT), 60);
    }

    #[tokio::test]
    async fn soft_policy_can_exceed_caps_but_never_bypasses_paired_measurement() {
        for primary in [Policy::Block, Policy::Warn] {
            for upstream in [Policy::Block, Policy::Warn] {
                let fixture = fixture((1, primary), (1, upstream), Duration::ZERO).await;
                let response = build_app(fixture.state.clone())
                    .oneshot(request())
                    .await
                    .unwrap();
                if primary == Policy::Warn && upstream == Policy::Warn {
                    assert_eq!(response.status(), StatusCode::OK);
                    assert_eq!(fixture.origin.received_requests().await.unwrap().len(), 1);
                    let ledger = fixture.state.ledger.lock().unwrap();
                    assert_eq!((ledger.spent(FRONT), ledger.spent(BACK)), (60, 60));
                } else {
                    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
                    assert!(fixture.origin.received_requests().await.unwrap().is_empty());
                }
            }
        }
    }
    #[tokio::test]
    async fn both_warn_and_block_refuse_storage_failure_without_model_dispatch() {
        for policy in [Policy::Block, Policy::Warn] {
            let fixture = fixture((1000, policy), (1000, policy), Duration::ZERO).await;
            initialize_execution(&fixture.state).unwrap();
            let poisoned = fixture.state.clone();
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let _guard = poisoned.ledger.lock().unwrap();
                panic!("synthetic ledger poison")
            }));
            let response = build_app(fixture.state.clone())
                .oneshot(request())
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
            assert_eq!(response.headers()["x-should-retry"], "false");
            assert!(fixture.origin.received_requests().await.unwrap().is_empty());
            assert!(fixture.sink.is_empty());
        }
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn in_flight_pair_blocks_a_second_call_and_disconnect_cannot_release_unknown_zero() {
        let fixture = fixture(
            (1000, Policy::Block),
            (100, Policy::Block),
            Duration::from_millis(400),
        )
        .await;
        let app = build_app(fixture.state.clone());
        let first = tokio::spawn(app.clone().oneshot(request()));
        tokio::time::timeout(Duration::from_secs(2), async {
            while fixture.origin.received_requests().await.unwrap().is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let held = {
            let ledger = fixture.state.ledger.lock().unwrap();
            (ledger.reserved(FRONT), ledger.reserved(BACK))
        };
        assert!(held.0 > 0);
        assert_eq!(held.0, held.1);
        assert_eq!(
            app.oneshot(request()).await.unwrap().status(),
            StatusCode::TOO_MANY_REQUESTS
        );
        first.abort();
        tokio::time::timeout(Duration::from_secs(3), fixture.state.lifecycle.wait_idle())
            .await
            .unwrap();
        let ledger = fixture.state.ledger.lock().unwrap();
        assert_eq!((ledger.reserved(FRONT), ledger.reserved(BACK)), (0, 0));
        assert_eq!(
            (ledger.spent(FRONT), ledger.spent(BACK)),
            (60, 60),
            "owner retains the single source outcome after client disconnect"
        );
    }
}
