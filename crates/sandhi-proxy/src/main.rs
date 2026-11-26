//! `sandhi-proxy` — the in-path (inline) reverse-proxy egress gate.
//!
//! Bootstrap: registers demo upstreams + virtual keys from env (the legacy single-user path),
//! requires the original durable `SANDHI_STORE`, opens the TD-0003 operator surface (provider-credential
//! vault, durable virtual-key store) and rehydrates the live key store + upstream handles from
//! it. The admin API is enabled by `SANDHI_ADMIN_TOKEN`; the vault backend by
//! `SANDHI_VAULT_BACKEND=keyring|sentinelpass|credstore` (default `keyring`). Request handling lives in the
//! `sandhi_proxy` library and is exercised by the integration tests.

use axum::http::HeaderMap;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{mpsc, Arc, OnceLock};
use std::time::{Duration, Instant};

use sandhi_core::{AlertRegistry, BufferedSink, KeyStore, Sink, VirtualKey};
use sandhi_providers::{
    AnthropicAuthScheme, GeminiAuthScheme, ProviderFamily, ProviderHandle, ProviderRuntime,
};
use sandhi_proxy::{
    dual_scheme_plaintext_warning, liability_release_sweep_at, plaintext_bind_warning,
    reclaim_sweep_at, rehydrate_alerts, rehydrate_budgets,
    serve_with_http_and_tls_shutdown_timeout, serve_with_shutdown_timeout,
    serve_with_tls_shutdown_timeout, BufferedAlertStore, ProxyLedger, ProxyState, TlsConfig,
    DEFAULT_HEADER_READ_TIMEOUT_SECS, DEFAULT_MAX_CONNECTIONS, DEFAULT_MAX_CONNECTIONS_PER_IP,
    DEFAULT_MAX_IN_FLIGHT_AI_REQUESTS, DEFAULT_MAX_REQUEST_BODY_BYTES, DEFAULT_SHUTDOWN_GRACE,
};
use sandhi_store::{
    AlertStore, BackendStore, DynAlertStore, DynBackendStore, DynPolicyAuditStore, DynProfileStore,
    DynReferenceStore, DynRouteStore, DynVaultStore, DynVirtualKeyStore, ProfileStore, SqliteStore,
    UsageStore, VaultStore, VirtualKeyStore,
};

#[derive(clap::Parser)]
#[command(name = "sandhi-proxy", version = sandhi_proxy::PACKAGE_VERSION,
    about = "Sandhi usage gateway. Configure the listener, providers and authentication through SANDHI_* environment variables.")]
struct Args {}

fn main() {
    // Informational flags must exit before runtime, credentials, config or listener initialization.
    let _ = <Args as clap::Parser>::parse();
    // Owning the runtime explicitly prevents Tokio's implicit, potentially unbounded wait for
    // spawn_blocking tasks after async main returns. The process watchdog remains armed through
    // runtime teardown, including destructors which do not cooperate with async cancellation.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("create proxy runtime");
    let shutdown = Arc::new(ShutdownWatchdog::new());
    let status = runtime.block_on(run(Arc::clone(&shutdown)));
    // Configuration failures can return before a listener arms shutdown. Give runtime
    // cleanup a bounded deadline too; arm() never extends an existing shutdown deadline.
    shutdown.arm(Instant::now() + DEFAULT_SHUTDOWN_GRACE);
    runtime.shutdown_timeout(shutdown.remaining());
    if status == 124 || !shutdown.complete() {
        force_exit(124);
    }
    std::process::exit(status);
}

/// Binary-only emergency termination after cleanup has failed or exhausted its deadline.
/// This must bypass C exit handlers and Rust's ordinary exit serialization: either may be
/// the cleanup operation which is already blocked. Never call this from an embedded library.
#[cfg(unix)]
fn force_exit(status: i32) -> ! {
    // SAFETY: _exit terminates this process without running user callbacks. All successful
    // cleanup belongs to the preceding explicit shutdown path, not this emergency fallback.
    unsafe { libc::_exit(status) }
}

#[cfg(windows)]
fn force_exit(status: i32) -> ! {
    // SAFETY: GetCurrentProcess returns this process's pseudo-handle. TerminateProcess skips
    // CRT exit callbacks and DLL detach notifications, preserving the emergency exit status.
    unsafe {
        windows_sys::Win32::System::Threading::TerminateProcess(
            windows_sys::Win32::System::Threading::GetCurrentProcess(),
            status as u32,
        );
    }
    // Self-termination does not return on success. If the OS unexpectedly rejects it, abort
    // rather than re-entering the potentially blocked ordinary exit path or hanging forever.
    std::process::abort()
}

#[cfg(not(any(unix, windows)))]
compile_error!("sandhi-proxy emergency termination requires a Unix or Windows OS backend");

async fn run(shutdown: Arc<ShutdownWatchdog>) -> i32 {
    // TD-0011 D1: the BINARY installs the subscriber; the libraries only emit through the
    // `tracing` facade. That is what lets an in-process host (Victor) capture Sandhi's spans in
    // its own logging without Sandhi imposing a runtime or a second subscriber.
    //
    // `SANDHI_LOG` (falling back to `RUST_LOG`) controls filtering; the default keeps the
    // operator-relevant events — denials, retained uncertainty, reclaims, settle failures —
    // without the per-request debug chatter.
    let filter = std::env::var("SANDHI_LOG")
        .or_else(|_| std::env::var("RUST_LOG"))
        .unwrap_or_else(|_| "sandhi_proxy=info,sandhi_core=info,sandhi_providers=info,warn".into());
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::new(filter))
        .with_target(true)
        // stderr, not the default stdout: a server's diagnostics must not interleave with
        // anything a caller might pipe, and operators expect logs on fd 2.
        .with_writer(std::io::stderr)
        .init();

    // No infrastructure is required for the shared starter. Supplying OIDC
    // configuration selects OIDC; explicit errors never become unauthenticated.
    let auth_value = match std::env::var("SANDHI_AUTH_MODE") {
        Ok(value) => Some(value),
        Err(std::env::VarError::NotPresent) => None,
        Err(_) => {
            eprintln!("sandhi-proxy: invalid SANDHI_AUTH_MODE");
            return 1;
        }
    };
    let oidc_path = std::env::var_os("SANDHI_OIDC_CONFIG");
    let auth_mode =
        match sandhi_proxy::auth::AuthMode::select(auth_value.as_deref(), oidc_path.is_some()) {
            Ok(mode) => mode,
            Err(error) => {
                eprintln!("sandhi-proxy: {error}");
                return 1;
            }
        };
    let admin_token = match std::env::var("SANDHI_ADMIN_TOKEN") {
        Err(std::env::VarError::NotPresent) => None,
        Ok(value)
            if !value.is_empty()
                && value.len() <= 16384
                && value.is_ascii()
                && !value.chars().any(|c| c.is_whitespace() || c.is_control()) =>
        {
            Some(value)
        }
        _ => {
            eprintln!("sandhi-proxy: SANDHI_ADMIN_TOKEN must be a nonempty printable token without whitespace");
            return 1;
        }
    };
    let starter_upstream = match std::env::var("SANDHI_STARTER_UPSTREAM") {
        Err(std::env::VarError::NotPresent) => None,
        Ok(value)
            if !value.is_empty()
                && value.len() <= 512
                && !value.chars().any(|c| c.is_whitespace() || c.is_control()) =>
        {
            Some(value)
        }
        _ => {
            eprintln!("sandhi-proxy: invalid SANDHI_STARTER_UPSTREAM");
            return 1;
        }
    };
    let starter_rate_limit_per_min = match std::env::var("SANDHI_STARTER_RATE_LIMIT_PER_MIN") {
        Err(std::env::VarError::NotPresent) => None,
        Ok(value) => match value.parse::<u32>() {
            Ok(n) if n > 0 => Some(n),
            _ => {
                eprintln!("sandhi-proxy: invalid SANDHI_STARTER_RATE_LIMIT_PER_MIN");
                return 1;
            }
        },
        _ => {
            eprintln!("sandhi-proxy: invalid SANDHI_STARTER_RATE_LIMIT_PER_MIN");
            return 1;
        }
    };
    let addr: SocketAddr = match std::env::var("SANDHI_BIND")
        .unwrap_or_else(|_| "127.0.0.1:8787".into())
        .parse()
    {
        Ok(addr) => addr,
        Err(_) => {
            eprintln!("sandhi-proxy: SANDHI_BIND must be a valid socket address");
            return 1;
        }
    };
    let config_path = std::env::var("SANDHI_CONFIG")
        .ok()
        .map(std::path::PathBuf::from);
    let StartupConfig {
        tls,
        buffered_deadlines,
        streaming_deadlines,
    } = match startup_config_from_file(config_path.as_deref()) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("sandhi-proxy: startup configuration error: {error}");
            std::process::exit(1);
        }
    };
    // Validate transport before authority requests, durable state or listener binding.
    let http_addr = match http_bind_from_env(tls.is_some(), addr) {
        Ok(http_addr) => http_addr,
        Err(error) => {
            eprintln!("sandhi-proxy: {error}");
            return 1;
        }
    };

    let allow_loopback_http = match loopback_http_from_env() {
        Ok(value) => value,
        Err(error) => {
            eprintln!("sandhi-proxy: {error}");
            return 1;
        }
    };
    let credential_surface = auth_mode != sandhi_proxy::auth::AuthMode::None
        || admin_token.is_some()
        || std::env::var_os("SANDHI_CLIENT_CREDENTIALS_FILE").is_some();
    if let Err(error) = validate_authenticated_transport(
        credential_surface,
        addr,
        tls.is_some(),
        http_addr,
        allow_loopback_http,
    ) {
        eprintln!("sandhi-proxy: {error}");
        return 1;
    }

    let oidc = if auth_mode == sandhi_proxy::auth::AuthMode::Oidc {
        match sandhi_proxy::auth::Oidc::from_file(std::path::Path::new(
            oidc_path.as_ref().expect("validated OIDC configuration"),
        ))
        .await
        {
            Ok(oidc) => Some(Arc::new(oidc)),
            Err(error) => {
                eprintln!("sandhi-proxy: OIDC configuration error: {error}");
                return 1;
            }
        }
    } else {
        None
    };

    // The shipped enforcement ledger and request limiter are deliberately single-node. Refuse a
    // declared multi-replica topology rather than silently multiplying rate limits or allowing
    // separate processes to make independent hard-budget decisions.
    validate_replica_topology();
    // Whole-store backend selection (ADR-0049): `SANDHI_STORE_DSN` moves every store —
    // enforcement ledger, evidence, the operator tables, the native-reference registry
    // and usage history — onto ONE Postgres database (pure replace, no mirror window;
    // the SQLite `SANDHI_STORE` file is not opened for store purposes in this mode).
    // Unset keeps today's byte-identical SQLite behavior, where the original ordinary
    // durable path is mandatory: startup failures must never replace persistent caps or
    // holds with a volatile ledger.
    let pg_mode = match store_mode_dsn() {
        StoreModeDsn::Postgres(dsn) => Some(dsn),
        StoreModeDsn::Refuse => return 1,
        StoreModeDsn::None => None,
    };
    let store_path = if pg_mode.is_some() {
        None
    } else {
        Some(configured_store_path())
    };
    // Validate the execution topology before usage/vault/ledger initialization:
    // opening a legacy ledger with multiple shards can move historical rows.
    if std::env::var_os("SANDHI_BUFFERED_ACCOUNTING").is_some() {
        eprintln!("sandhi-proxy: SANDHI_BUFFERED_ACCOUNTING was removed; durable execution is mandatory; remove this setting");
        return 2;
    }
    if std::env::var_os("SANDHI_LEDGER_SHARDS").is_some_and(|value| value != "1") {
        eprintln!("sandhi-proxy: durable execution requires SANDHI_LEDGER_SHARDS=1; offline topology migration is required");
        return 2;
    }

    let default_max_output_tokens = match std::env::var("SANDHI_DEFAULT_MAX_OUTPUT_TOKENS") {
        Err(std::env::VarError::NotPresent) => 4096,
        Ok(value) => match value.parse::<u64>() {
            Ok(maximum) if (1..=1_000_000).contains(&maximum) => maximum,
            _ => startup_store_fatal("output_default"),
        },
        _ => startup_store_fatal("output_default"),
    };

    // Scope 5 (TD-0011 P3): OTLP export of gen_ai.* spans + metrics. `init()` returns None unless
    // the `otel-otlp` feature is compiled in AND `SANDHI_OTEL_EXPORT=otlp` is set — so the default
    // build is unaffected. The guard must outlive `serve()` so the OTel providers flush on shutdown.
    let (otel_recorder, otel_guard) = sandhi_proxy::otel::init().unzip();
    if otel_recorder.is_some() {
        eprintln!(
            "sandhi-proxy: OTLP export ON — gen_ai.* spans + metrics to {} (feature `otel-otlp`, TD-0011 P3)",
            std::env::var("SANDHI_OTEL_ENDPOINT").unwrap_or_else(|_| "http://localhost:4318".into())
        );
    }

    let runtime = ProviderRuntime::new();
    let keys = KeyStore::new();
    let mut providers: HashMap<String, ProviderHandle> = HashMap::new();

    // Legacy demo path: pre-register upstreams + virtual keys from env.
    if let Ok(key) = std::env::var("SANDHI_OPENAI_KEY") {
        let base = std::env::var("SANDHI_OPENAI_BASE")
            .unwrap_or_else(|_| ProviderFamily::OpenAiCompat.default_base_url().to_owned());
        providers.insert(
            "openai".into(),
            runtime.openai_compat(
                "openai",
                base,
                key,
                Default::default(),
                None,
                None,
                None,
                None,
            ),
        );
        if auth_mode == sandhi_proxy::auth::AuthMode::Tokens {
            keys.insert(VirtualKey {
                id: "vk_openai_demo".into(),
                subject_id: Some("demo".into()),
                group_id: Some("demo".into()),
                upstream_ref: "openai".into(),
                ..Default::default()
            });
            eprintln!("sandhi-proxy: registered openai upstream + vk_openai_demo");
        } else {
            eprintln!("sandhi-proxy: registered openai upstream");
        }
    }
    if let Ok(key) = std::env::var("SANDHI_INFERFLUX_KEY") {
        let base = std::env::var("SANDHI_INFERFLUX_BASE")
            .unwrap_or_else(|_| "http://127.0.0.1:8080/v1".into());
        providers.insert(
            "inferflux".into(),
            runtime.openai_compat(
                "inferflux",
                base,
                key,
                Default::default(),
                None,
                None,
                None,
                None,
            ),
        );
        if auth_mode == sandhi_proxy::auth::AuthMode::Tokens {
            keys.insert(VirtualKey {
                id: "vk_inferflux_demo".into(),
                subject_id: Some("demo".into()),
                group_id: Some("demo".into()),
                upstream_ref: "inferflux".into(),
                ..Default::default()
            });
            eprintln!("sandhi-proxy: registered inferflux upstream + vk_inferflux_demo");
        } else {
            eprintln!("sandhi-proxy: registered inferflux upstream");
        }
    }
    if let Ok(key) = std::env::var("SANDHI_ANTHROPIC_KEY") {
        // Symmetric with SANDHI_OPENAI_BASE. Without an override the Anthropic upstream could
        // only ever be the public API — no Anthropic-compatible gateway, no local mock, and no
        // way for the SDK-conformance suite to exercise this path at all.
        let base = std::env::var("SANDHI_ANTHROPIC_BASE")
            .unwrap_or_else(|_| ProviderFamily::Anthropic.default_base_url().to_owned());
        providers.insert(
            "anthropic".into(),
            runtime.anthropic(
                base,
                key,
                AnthropicAuthScheme::ApiKey,
                HeaderMap::new(),
                None,
                None,
                None,
                None,
            ),
        );
        if auth_mode == sandhi_proxy::auth::AuthMode::Tokens {
            keys.insert(VirtualKey {
                id: "vk_anthropic_demo".into(),
                subject_id: Some("demo".into()),
                group_id: Some("demo".into()),
                upstream_ref: "anthropic".into(),
                ..Default::default()
            });
            eprintln!("sandhi-proxy: registered anthropic upstream + vk_anthropic_demo");
        } else {
            eprintln!("sandhi-proxy: registered anthropic upstream");
        }
    }

    if let Ok(key) = std::env::var("SANDHI_GEMINI_KEY") {
        let base = std::env::var("SANDHI_GEMINI_BASE")
            .unwrap_or_else(|_| ProviderFamily::Gemini.default_base_url().to_owned());
        providers.insert(
            "gemini".into(),
            runtime.gemini(
                base,
                key,
                GeminiAuthScheme::ApiKey,
                HeaderMap::new(),
                None,
                None,
                None,
                None,
            ),
        );
        if auth_mode == sandhi_proxy::auth::AuthMode::Tokens {
            keys.insert(VirtualKey {
                id: "vk_gemini_demo".into(),
                subject_id: Some("demo".into()),
                group_id: Some("demo".into()),
                upstream_ref: "gemini".into(),
                ..Default::default()
            });
            eprintln!("sandhi-proxy: registered gemini upstream + vk_gemini_demo");
        } else {
            eprintln!("sandhi-proxy: registered gemini upstream");
        }
    }

    // The whole-store Postgres mode opens EVERY durable surface on the one DSN: usage
    // history, enforcement ledger + evidence, the eight operator tables, the policy
    // receipts and the native-reference registry (ADR-0049). Pure replace: no SQLite
    // mirror window, single writer. A configured backend that cannot open is fatal
    // (exit 1) — never a silent downgrade. The DSN itself is never logged.
    // Whole-store wiring: the single open path for every durable surface, SQLite
    // (default) or Postgres (`SANDHI_STORE_DSN`, ADR-0049).
    let (stores, _runtime, keys, providers) = open_store_set(
        pg_mode.clone(),
        store_path.as_deref(),
        runtime,
        keys,
        providers,
    )
    .await;
    let StoreSet {
        store,
        usage_store,
        usage_sink,
        ledger,
        vault,
        backends,
        routes,
        vkeys,
        profiles,
        alerts,
        alert_store,
        references,
        policy_audit,
        ..
    } = stores;
    let buffered_alert_store = alert_store.as_ref().map(|store| {
        Arc::new(BufferedAlertStore::from_store(
            Arc::clone(store),
            positive_usize_env("SANDHI_ALERT_BUFFER_CAPACITY", 256),
        ))
    });

    let buffered = Arc::new(BufferedSink::new(
        usage_sink,
        positive_usize_env("SANDHI_USAGE_BUFFER_CAPACITY", 1024),
    ));
    let buffered_sink = Some(Arc::clone(&buffered));
    let sink: Arc<dyn Sink> = buffered;

    // ADR-0049 no-downgrade visibility: name the owning backend per subsystem, never the DSN.
    eprintln!(
        "sandhi-proxy: store backend: {} — enforcement ledger + evidence, usage history, operator tables{}",
        ledger.backend_label(),
        if references.is_some() {
            ", native-reference registry"
        } else {
            ""
        }
    );
    tracing::info!(
        target: "sandhi.proxy.store_backend",
        ledger = ledger.backend_label(),
        operator_stores = ledger.backend_label(),
        "store backend selected (ADR-0049)"
    );

    let public_url = std::env::var("SANDHI_PUBLIC_URL").unwrap_or_else(|_| {
        format!(
            "{}://{}",
            if tls.is_some() { "https" } else { "http" },
            addr
        )
    });
    if admin_token.is_some() {
        eprintln!("sandhi-proxy: admin API enabled on /admin/*");
    }

    // TD-0031: optional non-interactive client-credentials registry. A malformed
    // file is fatal (fail-closed): a partially-trusted client list must never run.
    let client_credentials = match std::env::var("SANDHI_CLIENT_CREDENTIALS_FILE") {
        Ok(path) if !path.trim().is_empty() => {
            match sandhi_proxy::client_credentials::ClientCredentialRegistry::load(
                std::path::Path::new(&path),
            ) {
                Ok(registry) => {
                    eprintln!("sandhi-proxy: client-credentials grant enabled");
                    Some(Arc::new(registry))
                }
                Err(error) => {
                    eprintln!("sandhi-proxy: {error}");
                    std::process::exit(1);
                }
            }
        }
        _ => None,
    };
    // Recover the operator budget metadata (policy / window / limit) persisted in the durable
    // ledger, so caps set before a restart keep their policy lookup + dashboard + alert
    // thresholds. Blocking statement: the Postgres ledger arm blocks on its retained runtime
    // handle, so run it on a worker thread regardless of the selected backend.
    let (ledger, budgets) = {
        let budgets = std::sync::Mutex::new(HashMap::new());
        tokio::task::spawn_blocking(move || {
            rehydrate_budgets(&ledger, &budgets);
            (ledger, budgets)
        })
        .await
        .expect("budget rehydration worker")
    };
    let mut state = ProxyState::new(keys, ledger, sink, providers, store);
    state.budgets = budgets;
    // Selected above: the SQLite store (default) or the Postgres usage store (SANDHI_USAGE_DSN).
    state.usage_store = usage_store;
    if oidc.is_some() && client_credentials.is_some() {
        // In OIDC mode every vk_ token is routed through delegation metadata
        // that a plain mint never writes — a client-credentials token would be
        // minted successfully and then 401 at inference. Fail the startup.
        eprintln!(
            "sandhi-proxy: SANDHI_CLIENT_CREDENTIALS_FILE is not supported in OIDC auth mode; \
             use POST /auth/keys (IdP-authenticated delegation) instead"
        );
        std::process::exit(1);
    }
    state.client_credentials = client_credentials;
    state.buffered_deadlines = buffered_deadlines;
    state.set_streaming_deadlines(Some(streaming_deadlines.unwrap_or_default()));
    state.default_max_output_tokens = default_max_output_tokens;
    if let Some(policy) = &state.buffered_deadlines {
        if let Err(error) = policy.validate_endpoints(state.registered_reference()) {
            eprintln!("sandhi-proxy: {error}");
            std::process::exit(1);
        }
    }
    if let Some(policy) = state.streaming_policy() {
        if let Err(error) = policy.validate_endpoints(state.registered_reference()) {
            eprintln!("sandhi-proxy: {error}");
            std::process::exit(1);
        }
    }
    state.postgres_native_references = references;
    state.vault = vault;
    state.backends = backends;
    state.routes = routes;
    state.vkeys = vkeys;
    state.profiles = profiles;
    state.alert_store = alert_store;
    state.alert_writer = buffered_alert_store.clone();
    state.alerts = alerts;
    state.admin_token = admin_token;
    state.auth_mode = auth_mode;
    state.starter_upstream = starter_upstream;
    state.starter_rate_limit_per_min = starter_rate_limit_per_min;
    state.starter_control_origins = control_origins(addr, tls.is_some(), http_addr);
    state.oidc = oidc;
    if (std::env::var_os("SANDHI_POLICY_WORKERS").is_some()
        || std::env::var_os("SANDHI_POLICY_ONNX").is_some()
        || std::env::var_os("SANDHI_POLICY_REMOTE").is_some())
        && std::env::var_os("SANDHI_POLICY_CONFIG").is_none()
    {
        eprintln!("sandhi-proxy: policy evaluators require a policy configuration");
        return 1;
    }
    if let Some(path) = std::env::var_os("SANDHI_POLICY_CONFIG") {
        let loaded = (|| -> Result<_, ()> {
            use std::io::Read;
            let mut bytes = Vec::new();
            std::fs::File::open(path)
                .map_err(|_| ())?
                .take(131073)
                .read_to_end(&mut bytes)
                .map_err(|_| ())?;
            let worker_path = std::env::var_os("SANDHI_POLICY_WORKERS");
            let mut registry = sandhi_proxy::policy_workers::load_registry(
                worker_path.as_deref().map(std::path::Path::new),
            )
            .map_err(|_| ())?;
            let onnx_path = std::env::var_os("SANDHI_POLICY_ONNX");
            sandhi_proxy::policy_onnx::load_models(
                &mut registry,
                onnx_path.as_deref().map(std::path::Path::new),
            )
            .map_err(|_| ())?;
            let remote_path = std::env::var_os("SANDHI_POLICY_REMOTE");
            sandhi_proxy::policy_remote::load_remotes(
                &mut registry,
                remote_path.as_deref().map(std::path::Path::new),
            )
            .map_err(|_| ())?;
            let engine = sandhi_core::policy::Engine::from_slice_with_registry(&bytes, &registry)
                .map_err(|_| ())?;
            let audit: Arc<dyn sandhi_store::DynPolicyAuditStore> = if pg_mode.is_some() {
                policy_audit.ok_or(())?
            } else {
                let store = std::env::var_os("SANDHI_STORE").ok_or(())?;
                Arc::new(
                    sandhi_store::policy::PolicyAuditStore::open(
                        std::path::Path::new(&store),
                        100_000,
                    )
                    .map_err(|_| ())?,
                )
            };
            Ok(sandhi_proxy::policy::PolicyGate::new(engine, audit))
        })();
        match loaded {
            Ok(policy) => state.policy = Some(Arc::new(policy)),
            Err(()) => {
                eprintln!("sandhi-proxy: invalid policy or unavailable mandatory policy store");
                return 1;
            }
        }
    }

    // Optional deployment-topology file (generated by operator tooling, e.g.
    // anvaiops `anvai_topology.py show --sandhi-topology`). Absent = feature
    // off; malformed = fail-closed before bind, mirroring the policy loader.
    if let Some(path) = std::env::var_os("SANDHI_TOPOLOGY_FILE") {
        match sandhi_proxy::topology::load_topology_file(std::path::Path::new(&path)) {
            Ok(file) => state.topology = Some(Arc::new(file)),
            Err(error) => {
                eprintln!("sandhi-proxy: {error}");
                return 1;
            }
        }
    }

    state.public_url = public_url;
    // ADR-0004 D4: dashboard read endpoints follow the admin token unless explicitly re-opened.
    state.dashboard_public = std::env::var("SANDHI_DASHBOARD_PUBLIC").as_deref() == Ok("1");
    state.error_detail_full = std::env::var("SANDHI_ERROR_DETAIL").as_deref() == Ok("full");
    // Design audit A2: label values are caller-supplied (model on the typed plane), so the
    // registry is capped — excess distinct series fold into "(overflow)". Never let telemetry
    // fail a request, and never let a valid key grow it without bound.
    state.metrics = Arc::new(
        sandhi_proxy::metrics::Metrics::with_max_series(positive_usize_env(
            "SANDHI_METRICS_MAX_SERIES",
            sandhi_proxy::metrics::DEFAULT_MAX_METRIC_SERIES,
        ))
        .with_buffer_observers(
            buffered_sink.as_ref().map(|writer| writer.observer()),
            buffered_alert_store
                .as_ref()
                .map(|writer| writer.observer()),
        ),
    );
    state.max_request_body_bytes = request_body_limit_from_env();
    state.max_in_flight_ai_requests = positive_usize_env(
        "SANDHI_MAX_IN_FLIGHT_AI_REQUESTS",
        DEFAULT_MAX_IN_FLIGHT_AI_REQUESTS,
    );
    // Budget cost-basis weight for cache-read tokens, basis points
    // (10,000 = 1x neutral; 1,000 = 0.1x typical cached-input pricing).
    // Startup-only: applies to new settlements (issue #381, TD-0032 interim).
    state.cache_read_weight_bp = match std::env::var("SANDHI_CACHE_READ_WEIGHT") {
        Ok(raw) => sandhi_core::allowance::Weight::new(raw.parse::<u32>().unwrap_or_else(|_| {
            panic!("SANDHI_CACHE_READ_WEIGHT must be basis points (u32), got {raw:?}")
        }))
        .unwrap_or_else(|_| {
            panic!(
                "SANDHI_CACHE_READ_WEIGHT must be within 0..=1_000_000 basis points, got {raw:?}"
            )
        })
        .basis_points(),
        Err(_) => sandhi_core::allowance::Weight::default().basis_points(),
    };
    // TD-0014 P3: connection-level limits (see the field docs for semantics).
    state.max_connections = positive_usize_env("SANDHI_MAX_CONNECTIONS", DEFAULT_MAX_CONNECTIONS);
    // 0 is the DEFAULT and DISABLES the per-IP cap (opt-in control — see the
    // field doc); positive_usize_env would panic on exactly the value the docs
    // tell operators to set behind a proxy.
    state.max_connections_per_ip = match std::env::var("SANDHI_MAX_CONNECTIONS_PER_IP") {
        Ok(raw) => raw.parse::<usize>().unwrap_or_else(|_| {
            panic!("SANDHI_MAX_CONNECTIONS_PER_IP must be a non-negative integer, got {raw:?}")
        }),
        Err(_) => DEFAULT_MAX_CONNECTIONS_PER_IP,
    };
    state.header_read_timeout_secs = positive_u64_env(
        "SANDHI_HEADER_READ_TIMEOUT_SECS",
        DEFAULT_HEADER_READ_TIMEOUT_SECS,
    );
    state.shutdown_quiesce = shutdown_quiesce_from_env();
    state.trusted_proxies = sandhi_proxy::parse_trusted_proxies(
        &std::env::var("SANDHI_TRUSTED_PROXIES").unwrap_or_default(),
    );
    state.config_path = config_path;
    if let Some(path) = &state.config_path {
        eprintln!(
            "sandhi-proxy: declarative config at {} (GET/POST /admin/config*)",
            path.display()
        );
    }
    // ADR-0004 D4 footgun: with no admin token, the /dashboard/api/* read endpoints (subject/group
    // usage aggregates, masked vkey metadata) stay open. That is the documented single-node dev
    // trust posture, but it must not be silent when a real store is configured — surface it loudly
    // rather than fail-closed (which would break every dev who sets SANDHI_STORE without a token).
    if state.auth_mode == sandhi_proxy::auth::AuthMode::Tokens
        && state.oidc.is_none()
        && (state.store.is_some() || state.usage_store.is_some())
        && state.admin_token.is_none()
        && !state.dashboard_public
    {
        eprintln!(
            "sandhi-proxy: WARNING: SANDHI_STORE is set without SANDHI_ADMIN_TOKEN — the \
             /dashboard/api/* read endpoints (subject/group usage, masked vkey metadata) are open \
             to any caller. Set SANDHI_ADMIN_TOKEN to gate them, or SANDHI_DASHBOARD_PUBLIC=1 to \
             acknowledge the open single-node posture."
        );
    }
    // Scope 5: attach the OTLP recorder (None unless feature-on + configured). Its guard is
    // explicitly dropped below while the process-wide shutdown deadline still applies.
    state.otel = otel_recorder;
    if auth_mode == sandhi_proxy::auth::AuthMode::None {
        eprintln!("sandhi-proxy: shared starter enabled; all inference uses one server-owned principal and durable budget scope starter:shared:v1");
        if !addr.ip().is_loopback() {
            eprintln!("sandhi-proxy: WARNING: unauthenticated shared starter explicitly bound to a non-loopback address; reachable callers share its authority and budget");
        }
    }
    let state = Arc::new(state);
    {
        let config = sandhi_proxy::settlement::buffered::Config::new(
            64,
            std::time::Duration::from_secs(2),
            std::time::Duration::from_secs(120),
        )
        .expect("static bounds");
        if let Err(error) = sandhi_proxy::settlement::buffered::enable(&state, config) {
            eprintln!("sandhi-proxy: {error}");
            return 2;
        }
    }

    // ADR-0005 D2: reclaim leases left dangling by a crash on a timer, so an abandoned scope's
    // held capacity is released without waiting for its next request (`reserve` also reclaims
    // opportunistically per scope; this covers scopes that go quiet). Best-effort by design.
    let reclaim_task = {
        let sweep_state = Arc::clone(&state);
        // ADR-0048: retention window for aged unresolved durable liabilities.
        // Admission age past this cutoff releases the held budget capacity
        // (evidence retained, nothing fabricated). Zero disables the sweep.
        // Invalid supplied values refuse startup — never silently downgrade.
        // Values beyond the ceiling would overflow the cutoff arithmetic
        // (time subtraction panics out of range), so they refuse too.
        const LIABILITY_RETENTION_MAX_SECS: i64 = 1_000_000_000;
        let liability_retention_secs: i64 = match std::env::var(
            "SANDHI_LIABILITY_RELEASE_AFTER_SECS",
        ) {
            Ok(raw) => match raw.parse::<i64>() {
                Ok(secs) if (0..=LIABILITY_RETENTION_MAX_SECS).contains(&secs) => secs,
                _ => {
                    eprintln!(
                        "sandhi-proxy: SANDHI_LIABILITY_RELEASE_AFTER_SECS must be an integer of seconds between 0 and {LIABILITY_RETENTION_MAX_SECS}, got {raw:?}"
                    );
                    std::process::exit(1);
                }
            },
            Err(_) => 86_400,
        };
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(60));
            tick.tick().await; // discard the immediate first tick
            loop {
                tick.tick().await;
                let sweep = Arc::clone(&sweep_state);
                let _ = tokio::task::spawn_blocking(move || {
                    let now = time::OffsetDateTime::now_utc();
                    if liability_retention_secs > 0 {
                        let cutoff = now - time::Duration::seconds(liability_retention_secs);
                        liability_release_sweep_at(&sweep.ledger, now, cutoff);
                    }
                    reclaim_sweep_at(&sweep.ledger, now)
                })
                .await;
            }
        })
    };

    if let Some(warning) = plaintext_bind_warning(addr, tls.is_some()) {
        eprintln!("sandhi-proxy: {warning}");
    }
    if let Some(warning) = http_addr.and_then(dual_scheme_plaintext_warning) {
        eprintln!("sandhi-proxy: {warning}");
    }
    let listener_scheme = if tls.is_some() { "https" } else { "http" };
    match http_addr {
        Some(http_addr) => eprintln!(
            "sandhi-proxy listening on {listener_scheme}://{addr} + http://{http_addr}  \
             (OpenAI Chat/Responses | Anthropic Messages | Gemini, virtual-key auth)"
        ),
        None => eprintln!(
            "sandhi-proxy listening on {listener_scheme}://{addr}  \
             (OpenAI Chat/Responses | Anthropic Messages | Gemini, virtual-key auth)"
        ),
    }
    let shutdown_grace = shutdown_grace_from_env();
    let signal = {
        let lifecycle = Arc::clone(&state.lifecycle);
        let shutdown = Arc::clone(&shutdown);
        async move {
            shutdown_signal().await;
            // Arm before tracing: a blocked stderr must not defeat the deadline.
            shutdown.arm(lifecycle.begin_quiesce(shutdown_grace));
            tracing::info!("shutdown signal received; quiescing and draining in-flight requests");
        }
    };
    let serve_result = match (tls, http_addr) {
        (Some(tls), Some(http_addr)) => {
            serve_with_http_and_tls_shutdown_timeout(
                Arc::clone(&state),
                addr,
                http_addr,
                signal,
                shutdown_grace,
                tls,
            )
            .await
        }
        (Some(tls), None) => {
            serve_with_tls_shutdown_timeout(Arc::clone(&state), addr, signal, shutdown_grace, tls)
                .await
        }
        // http_addr is Some only when TLS is configured (validated above).
        (None, _) => {
            serve_with_shutdown_timeout(Arc::clone(&state), addr, signal, shutdown_grace).await
        }
    };
    // Listener failures also need bounded cleanup. begin_quiesce preserves the original signal
    // deadline, so no stage below receives another fresh grace period.
    let deadline = state.lifecycle.begin_quiesce(shutdown_grace);
    shutdown.arm(deadline);
    reclaim_task.abort();
    let _ = tokio::time::timeout_at(deadline.into(), reclaim_task).await;
    // Detached admission/settlement work can outlive an HTTP connection task. It must stop
    // producing usage and alert updates BEFORE the respective queue admissions are closed.
    if tokio::time::timeout_at(deadline.into(), state.lifecycle.wait_idle())
        .await
        .is_err()
    {
        return 124;
    }
    // Public serving owns recovery and the final accounting inventory. Binary-only
    // writer/OTel cleanup below still shares the signal's original watchdog deadline.
    let mut cleanup_complete = !serve_result
        .as_ref()
        .is_err_and(sandhi_proxy::is_incomplete_shutdown);
    if let Some(buffered) = buffered_alert_store {
        if !buffered.close(shutdown.remaining()) {
            cleanup_complete = false;
            tracing::error!(
                dropped = buffered.dropped_updates(),
                "alert writer shutdown incomplete; persistence may be missing"
            );
        }
    }
    if let Some(buffered) = buffered_sink {
        if !buffered.close(shutdown.remaining()) {
            cleanup_complete = false;
            tracing::error!(
                dropped = buffered.dropped_events(),
                "usage writer shutdown incomplete; persistence may be missing"
            );
        }
    }
    // OTel SDK shutdown and arbitrary provider/store destructors are synchronous. The watchdog
    // enforces the same deadline even if one never returns; the library itself never exits.
    #[allow(clippy::drop_non_drop)] // The no-OTLP build uses a zero-sized, no-Drop guard.
    drop(otel_guard);
    state.lifecycle.stop();
    if !cleanup_complete || Instant::now() >= deadline {
        return 124;
    }
    if let Err(e) = serve_result {
        if sandhi_proxy::is_incomplete_shutdown(&e) {
            return 124;
        }
        eprintln!("sandhi-proxy error: {e}");
        return 1;
    }
    0
}

/// Configuration/setup failure is distinct from an incomplete shutdown (124). No
/// listener has bound here; exit directly so the unarmed watchdog cannot rewrite 2.
/// Never include raw backend errors, SQL or supplied configuration in this diagnostic.
fn startup_store_fatal(component: &'static str) -> ! {
    eprintln!("sandhi-proxy: startup failed: configured_store_unavailable component={component}");
    std::process::exit(2);
}

fn configured_store_path() -> String {
    match std::env::var("SANDHI_STORE") {
        // SQLite URI options can request volatile databases or interact with shard
        // suffixes. The binary accepts ordinary filesystem paths, not URI syntax.
        Ok(path) if !path.trim().is_empty() && path != ":memory:" && !path.starts_with("file:") => {
            path
        }
        _ => startup_store_fatal("configuration"),
    }
}

/// Strict, deliberate exception for disposable loopback tests or a local TLS terminator.
/// Never infer transport security from caller-controlled forwarding headers.
fn loopback_http_from_env() -> Result<bool, &'static str> {
    match std::env::var("SANDHI_ALLOW_LOOPBACK_HTTP") {
        Err(std::env::VarError::NotPresent) => Ok(false),
        Ok(value) if value == "0" => Ok(false),
        Ok(value) if value == "1" => Ok(true),
        _ => Err("SANDHI_ALLOW_LOOPBACK_HTTP must be unset, 0 or 1"),
    }
}

fn validate_authenticated_transport(
    credential_surface: bool,
    primary: SocketAddr,
    tls: bool,
    companion: Option<SocketAddr>,
    allow_loopback_http: bool,
) -> Result<(), &'static str> {
    if !credential_surface {
        return Ok(());
    }
    let allowed = |addr: SocketAddr| allow_loopback_http && addr.ip().is_loopback();
    if (!tls && !allowed(primary)) || companion.is_some_and(|addr| !allowed(addr)) {
        return Err("authenticated listeners require TLS and no plaintext companion; \
                    SANDHI_ALLOW_LOOPBACK_HTTP=1 permits only loopback HTTP for tests or local TLS termination");
    }
    Ok(())
}

/// Resolve the optional plaintext companion listener from `SANDHI_HTTP_BIND`.
/// See [`parse_http_bind`] for the validation contract.
fn http_bind_from_env(
    tls_configured: bool,
    primary: SocketAddr,
) -> Result<Option<SocketAddr>, String> {
    match std::env::var("SANDHI_HTTP_BIND") {
        Ok(raw) => parse_http_bind(Some(&raw), tls_configured, primary),
        Err(std::env::VarError::NotPresent) => parse_http_bind(None, tls_configured, primary),
        Err(std::env::VarError::NotUnicode(error)) => Err(format!(
            "SANDHI_HTTP_BIND must be a valid socket address, got non-UTF-8 value: {error:?}"
        )),
    }
}

/// The plaintext companion port (dual-scheme serving, TD-0017 D7) is meaningful
/// only alongside listener TLS — without it, the primary `SANDHI_BIND` socket
/// already serves plaintext, so a companion would either silently duplicate the
/// scheme or collide on the same address. Both are refused at startup rather
/// than absorbed, so a stale env var can never quietly reshape the topology.
fn parse_http_bind(
    raw: Option<&str>,
    tls_configured: bool,
    primary: SocketAddr,
) -> Result<Option<SocketAddr>, String> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    let addr: SocketAddr = raw
        .parse()
        .map_err(|_| format!("SANDHI_HTTP_BIND must be a valid socket address, got {raw:?}"))?;
    if !tls_configured {
        return Err(
            "SANDHI_HTTP_BIND requires listener TLS (tls.cert/tls.key in SANDHI_CONFIG); \
             without TLS the SANDHI_BIND listener already serves plaintext HTTP"
                .into(),
        );
    }
    if binds_collide(addr, primary) {
        return Err(
            "SANDHI_HTTP_BIND must not share a listening socket with SANDHI_BIND; \
             pick a distinct port for each scheme"
                .into(),
        );
    }
    Ok(Some(addr))
}

/// Two TCP bind addresses collide when they share a port within one address
/// family and at least one side is the family wildcard (`0.0.0.0` covers every
/// specific IPv4 address on that port, `[::]` every IPv6 one). Distinct
/// specific addresses coexist, and v4 never collides with v6 — std forces
/// `IPV6_V6ONLY`, so `[::]:P` and `0.0.0.0:P` are genuinely distinct sockets.
/// This is startup diagnostics ahead of the bind; the kernel stays the
/// authority for anything stranger (mapped addresses, exotic scopes), and a
/// late `AddrInUse` still fails closed.
fn binds_collide(a: SocketAddr, b: SocketAddr) -> bool {
    a.port() == b.port()
        && a.is_ipv4() == b.is_ipv4()
        && (a.ip().is_unspecified() || b.ip().is_unspecified() || a.ip() == b.ip())
}

enum WatchdogMessage {
    Arm(Instant),
    Complete,
}

/// Binary-only hard-stop policy. Never put logging on the timeout path: stderr, tracing or an
/// exporter may be the cleanup operation that is blocked. Exit 124 means cleanup was not proven
/// complete, not that persistence loss has been repaired or every provider call was cancelled.
struct ShutdownWatchdog {
    deadline: OnceLock<Instant>,
    tx: mpsc::Sender<WatchdogMessage>,
}

impl ShutdownWatchdog {
    fn new() -> Self {
        let (tx, rx) = mpsc::channel();
        std::thread::Builder::new()
            .name("sandhi-shutdown-watchdog".into())
            .spawn(move || {
                let Ok(WatchdogMessage::Arm(deadline)) = rx.recv() else {
                    return;
                };
                match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                    Ok(WatchdogMessage::Complete) if Instant::now() < deadline => return,
                    Err(mpsc::RecvTimeoutError::Disconnected) => {
                        // A panic/drop is not successful cleanup. Retain the original deadline.
                        std::thread::sleep(deadline.saturating_duration_since(Instant::now()));
                    }
                    _ => {}
                }
                force_exit(124);
            })
            .expect("create shutdown watchdog");
        Self {
            deadline: OnceLock::new(),
            tx,
        }
    }

    fn arm(&self, deadline: Instant) {
        if self.deadline.set(deadline).is_ok()
            && self.tx.send(WatchdogMessage::Arm(deadline)).is_err()
        {
            force_exit(124);
        }
    }

    fn remaining(&self) -> Duration {
        self.deadline
            .get()
            .map(|deadline| deadline.saturating_duration_since(Instant::now()))
            .unwrap_or(Duration::ZERO)
    }

    fn complete(&self) -> bool {
        self.deadline
            .get()
            .is_some_and(|deadline| Instant::now() < *deadline)
            && self.tx.send(WatchdogMessage::Complete).is_ok()
    }
}

fn request_body_limit_from_env() -> usize {
    positive_usize_env(
        "SANDHI_MAX_REQUEST_BODY_BYTES",
        DEFAULT_MAX_REQUEST_BODY_BYTES,
    )
}

/// Resolve listener TLS before binding. `SANDHI_CONFIG` stores only paths; the
/// private-key bytes remain in their separately permissioned file. A configured
/// but unreadable/invalid pair fails startup rather than silently downgrading to
/// plaintext.
struct StartupConfig {
    tls: Option<TlsConfig>,
    buffered_deadlines: Option<sandhi_proxy::deadlines::BufferedDeadlines>,
    streaming_deadlines: Option<sandhi_proxy::deadlines::StreamingDeadlines>,
}

fn startup_config_from_file(path: Option<&std::path::Path>) -> Result<StartupConfig, String> {
    let Some(path) = path else {
        return Ok(StartupConfig {
            tls: None,
            buffered_deadlines: None,
            streaming_deadlines: None,
        });
    };
    let text = std::fs::read_to_string(path)
        .map_err(|error| format!("reading {}: {error}", path.display()))?;
    let deadlines = sandhi_proxy::deadlines::from_json(&text)
        .map_err(|error| format!("buffered deadline configuration: {error}"))?;
    let streaming = sandhi_proxy::deadlines::streaming_from_json(&text)
        .map_err(|error| format!("streaming deadline configuration: {error}"))?;
    let tls = listener_tls_entry_from_json(&text, path)?
        .map(|tls| {
            TlsConfig::from_pem_files(&tls.cert, &tls.key).map_err(|error| error.to_string())
        })
        .transpose()?;
    Ok(StartupConfig {
        tls,
        buffered_deadlines: deadlines,
        streaming_deadlines: streaming,
    })
}

/// Bootstrap needs only the transport projection. Deserializing the complete
/// operator desired-state schema here would make an unrelated provider or
/// budget validation error newly fatal to the data plane. The admin endpoints
/// continue to parse and validate the complete schema when it is previewed or
/// applied.
fn listener_tls_entry_from_json(
    text: &str,
    path: &std::path::Path,
) -> Result<Option<sandhi_proxy::config::TlsEntry>, String> {
    #[derive(serde::Deserialize)]
    struct ListenerConfigProjection {
        #[serde(default)]
        tls: Option<sandhi_proxy::config::TlsEntry>,
    }

    serde_json::from_str::<ListenerConfigProjection>(text)
        .map(|config| config.tls)
        .map_err(|error| format!("parsing {}: {error}", path.display()))
}

fn shutdown_grace_from_env() -> std::time::Duration {
    std::time::Duration::from_secs(positive_u64_env(
        "SANDHI_SHUTDOWN_GRACE_SECS",
        DEFAULT_SHUTDOWN_GRACE.as_secs(),
    ))
}

fn shutdown_quiesce_from_env() -> Duration {
    let millis = match std::env::var("SANDHI_SHUTDOWN_QUIESCE_MS") {
        Ok(raw) => raw.parse::<u64>().unwrap_or_else(|_| {
            panic!("SANDHI_SHUTDOWN_QUIESCE_MS must be a non-negative integer, got {raw:?}")
        }),
        Err(_) => 1000,
    };
    Duration::from_millis(millis)
}

fn positive_usize_env(name: &str, default: usize) -> usize {
    let Ok(raw) = std::env::var(name) else {
        return default;
    };
    raw.parse::<usize>()
        .ok()
        .filter(|value| *value > 0)
        .unwrap_or_else(|| panic!("{name} must be a positive integer, got {raw:?}"))
}

fn positive_u64_env(name: &str, default: u64) -> u64 {
    let Ok(raw) = std::env::var(name) else {
        return default;
    };
    raw.parse::<u64>()
        .ok()
        .filter(|value| *value > 0)
        .unwrap_or_else(|| panic!("{name} must be a positive integer, got {raw:?}"))
}

/// A configured DSN, if any. A whitespace-only value counts as unset — some process
/// managers can only set variables, not unset them.
fn normalize_dsn(raw: Option<String>) -> Option<String> {
    raw.map(|dsn| dsn.trim().to_string())
        .filter(|dsn| !dsn.is_empty())
}

/// The legacy (#518) usage-only DSN. Superseded whenever the whole-store
/// `SANDHI_STORE_DSN` (ADR-0049) is configured — that one wins.
fn configured_usage_dsn() -> Option<String> {
    normalize_dsn(std::env::var("SANDHI_USAGE_DSN").ok())
}

/// Whole-store backend selection (ADR-0049). `SANDHI_STORE_DSN` selects the one-Postgres-
/// database mode for every store; when it wins over a also-configured `SANDHI_USAGE_DSN`
/// the narrower legacy variable is simply superseded (reported, never logged).
#[allow(dead_code)] // the Postgres arm exists only in `postgres` builds
enum StoreModeDsn {
    Postgres(String),
    /// DSN configured but unusable: refuse startup rather than downgrade.
    Refuse,
    None,
}

#[cfg(feature = "postgres")]
fn store_mode_dsn() -> StoreModeDsn {
    let store_raw = std::env::var("SANDHI_STORE_DSN").ok();
    let usage_raw = std::env::var("SANDHI_USAGE_DSN").ok();
    match whole_store_dsn(store_raw, usage_raw) {
        Some(dsn) => {
            if normalize_dsn(std::env::var("SANDHI_USAGE_DSN").ok()).is_some() {
                eprintln!(
                    "sandhi-proxy: SANDHI_STORE_DSN supersedes SANDHI_USAGE_DSN — all stores use the one whole-store database"
                );
            }
            StoreModeDsn::Postgres(dsn)
        }
        None => StoreModeDsn::None,
    }
}

/// The whole-store DSN selection: `SANDHI_STORE_DSN` when configured (trimmed; a
/// whitespace-only value counts as unset — some process managers can only set
/// variables, not unset them), otherwise no whole-store mode. The narrower legacy
/// `SANDHI_USAGE_DSN` (#518) keeps running the usage-only topology when it is the only
/// one set; when both are set, `SANDHI_STORE_DSN` wins (ADR-0049 precedence).
#[cfg_attr(not(feature = "postgres"), allow(dead_code))] // test-exercised selection logic
fn whole_store_dsn(store_raw: Option<String>, _usage_raw: Option<String>) -> Option<String> {
    // ADR-0049 precedence: whole-store mode requires the whole-store DSN. The legacy
    // usage-only DSN is deliberately ignored here — it never selects whole-store mode
    // by itself (it feeds the #518 usage-only topology via `configured_usage_dsn`);
    // when both are set, `store_mode_dsn` logs the supersession. Present in the
    // signature so the precedence contract is type-visible and test-pinned.
    normalize_dsn(store_raw)
}

/// Compiled without the `postgres` feature but configured with a whole-store DSN: refuse
/// to start rather than silently running the SQLite topology the operator did not ask for.
#[cfg(not(feature = "postgres"))]
fn store_mode_dsn() -> StoreModeDsn {
    if normalize_dsn(std::env::var("SANDHI_STORE_DSN").ok()).is_some() {
        eprintln!(
            "sandhi-proxy: startup failed: SANDHI_STORE_DSN is set but this binary was built \
             without the `postgres` feature; rebuild with `--features postgres` (a configured \
             store never downgrades to SQLite)"
        );
        return StoreModeDsn::Refuse;
    }
    StoreModeDsn::None
}

/// Open the Postgres usage store when `SANDHI_USAGE_DSN` is configured. House posture: a
/// configured backend that cannot open is fatal (exit 1) — never a silent downgrade to
/// SQLite. The DSN itself is never logged (it carries the credential).
#[cfg(feature = "postgres")]
async fn open_postgres_usage_store(dsn: &str) -> (Option<Arc<dyn UsageStore>>, Arc<dyn Sink>) {
    match sandhi_store::PostgresStore::connect(dsn).await {
        Ok(store) => {
            eprintln!("sandhi-proxy: Postgres usage store active (SANDHI_USAGE_DSN)");
            tracing::info!(
                target: "sandhi.proxy.usage_store",
                backend = "postgres",
                "Postgres usage store active (SANDHI_USAGE_DSN); durable usage history is PostgreSQL, enforcement ledger stays on SQLite"
            );
            let store = Arc::new(store);
            let sink = Arc::clone(&store) as Arc<dyn Sink>;
            let reader = store as Arc<dyn UsageStore>;
            (Some(reader), sink)
        }
        Err(error) => {
            eprintln!("sandhi-proxy: startup failed: postgres usage store unavailable: {error}");
            std::process::exit(1);
        }
    }
}

/// Compiled without the `postgres` feature but configured with a DSN: refuse to start rather
/// than silently writing the usage history somewhere the operator did not ask for.
#[cfg(not(feature = "postgres"))]
async fn open_postgres_usage_store(dsn: &str) -> ! {
    let _ = dsn;
    eprintln!(
        "sandhi-proxy: startup failed: SANDHI_USAGE_DSN is set but this binary was built \
         without the `postgres` feature; rebuild with `--features postgres` (a configured \
         usage store never downgrades to SQLite)"
    );
    std::process::exit(1);
}

/// The whole-store Postgres backend set (ADR-0049): one DSN, every durable surface.
#[cfg(feature = "postgres")]
struct PgStores {
    usage: Arc<sandhi_store::PostgresStore>,
    ledger: ProxyLedger,
    vault: Option<Arc<dyn DynVaultStore>>,
    backends: Option<Arc<dyn DynBackendStore>>,
    routes: Option<Arc<dyn DynRouteStore>>,
    vkeys: Option<Arc<dyn DynVirtualKeyStore>>,
    profiles: Option<Arc<dyn DynProfileStore>>,
    alerts: Option<AlertRegistryHandle>,
    alert_store: Option<Arc<dyn DynAlertStore>>,
    policy_audit: Option<Arc<dyn DynPolicyAuditStore>>,
    references: Arc<dyn DynReferenceStore>,
}

#[cfg(feature = "postgres")]
impl PgStores {
    fn usage_store(&self) -> Option<Arc<dyn UsageStore>> {
        Some(Arc::clone(&self.usage) as Arc<dyn UsageStore>)
    }
    fn usage_sink(&self) -> Arc<dyn Sink> {
        Arc::clone(&self.usage) as Arc<dyn Sink>
    }
}

/// Open every store on the one whole-store DSN (ADR-0049). Connect failure is fatal
/// (exit 1) — never a downgrade; the DSN is never logged. Provider handles and the live
/// key store rehydrate exactly as the SQLite mode does.
#[cfg(feature = "postgres")]
/// Connect every whole-store store on `dsn` (phase 1: async connects only). The blocking
/// rehydration runs separately in [`rehydrate_postgres_stores`] — the PG stores block on
/// their retained runtime handle, which must never execute inside an async context.
#[cfg(feature = "postgres")]
async fn connect_postgres_stores(dsn: &str) -> Result<PgStores, String> {
    // The DSN carries its own connect parameters (e.g. `?application_name=sandhi-proxy`);
    // every Postgres constructor parses them from the DSN, keeping this binary sqlx-free.
    let usage = Arc::new(
        sandhi_store::PostgresStore::connect(dsn)
            .await
            .map_err(|error| format!("usage store: {error}"))?,
    );
    let ledger = ProxyLedger::postgres_durable(dsn)
        .await
        .map_err(|error| format!("enforcement ledger: {error}"))?;
    // Secrets stay in the OS keyring / SentinelPass backend (SANDHI_VAULT_BACKEND);
    // only the metadata index moves to Postgres.
    let backend = VaultStore::backend_from_env()?;
    let vault = sandhi_store::PostgresVaultStore::connect(dsn, backend)
        .await
        .map_err(|error| format!("vault metadata: {error}"))?;
    let vault: Arc<dyn DynVaultStore> = Arc::new(vault);
    let backends = sandhi_store::PostgresBackendStore::connect(dsn)
        .await
        .map_err(|error| format!("backends: {error}"))?;
    let backends: Arc<dyn DynBackendStore> = Arc::new(backends);
    // The policy-receipt audit store is opened eagerly in whole-store mode so a
    // policy-configured startup always has it (ADR-0049 Decision); it is created
    // even when no policy is configured — the table is the bounded-receipt sink.
    let policy_audit: Arc<dyn DynPolicyAuditStore> = Arc::new(
        sandhi_store::PostgresPolicyAuditStore::connect(dsn, 100_000)
            .await
            .map_err(|error| format!("policy receipts: {error}"))?,
    );
    let routes = sandhi_store::PostgresRouteStore::connect(dsn)
        .await
        .map_err(|error| format!("routes: {error}"))?;
    let routes: Arc<dyn DynRouteStore> = Arc::new(routes);
    let vkeys = sandhi_store::PostgresVirtualKeyStore::connect(dsn)
        .await
        .map_err(|error| format!("virtual keys: {error}"))?;
    let vkeys: Arc<dyn DynVirtualKeyStore> = Arc::new(vkeys);
    let profiles = sandhi_store::PostgresProfileStore::connect(dsn)
        .await
        .map_err(|error| format!("profiles: {error}"))?;
    let profiles: Arc<dyn DynProfileStore> = Arc::new(profiles);
    let alert_store = sandhi_store::PostgresAlertStore::connect(dsn)
        .await
        .map_err(|error| format!("alerts: {error}"))?;
    let alert_store: Arc<dyn DynAlertStore> = Arc::new(alert_store);
    let references = sandhi_store::PostgresReferenceStore::connect(dsn)
        .await
        .map_err(|error| format!("native references: {error}"))?;
    let references: Arc<dyn DynReferenceStore> = Arc::new(references);
    Ok(PgStores {
        usage,
        ledger,
        vault: Some(vault),
        backends: Some(backends),
        routes: Some(routes),
        vkeys: Some(vkeys),
        profiles: Some(profiles),
        alerts: None,
        alert_store: Some(alert_store),
        policy_audit: Some(policy_audit),
        references,
    })
}

/// Phase 2: the blocking startup rehydration (plain statements on an ordinary thread —
/// every PG store call here blocks on its retained runtime handle). Fatal failures keep
/// the exit-1 posture.
#[cfg(feature = "postgres")]
fn rehydrate_postgres_stores(
    pg: &mut PgStores,
    runtime: &ProviderRuntime,
    keys: &KeyStore,
    providers: &mut HashMap<String, ProviderHandle>,
) -> Result<(), String> {
    let vault = pg.vault.as_ref().expect("vault connected");
    let backends = pg.backends.as_ref().expect("backends connected");
    let routes = pg.routes.as_ref().expect("routes connected");
    let vkeys = pg.vkeys.as_ref().expect("vkeys connected");
    rehydrate_providers_from_dyn_vault(vault.as_ref(), runtime, providers);
    sandhi_proxy::backend::rehydrate_backends(
        vault.as_ref(),
        backends.as_ref(),
        runtime,
        providers,
    )
    .map_err(|_| "backends: rehydration failed".to_string())?;
    routes
        .list()
        .map_err(|_| "routes: inventory failed".to_string())?;
    sandhi_proxy::rehydrate_live_keys(keys, vkeys.as_ref());
    pg.alerts = pg
        .alert_store
        .as_ref()
        .map(|store| Arc::new(std::sync::Mutex::new(rehydrate_alerts(store.as_ref()))));
    eprintln!("sandhi-proxy: credential vault metadata (backend: postgres)");
    eprintln!("sandhi-proxy: operator stores on the whole-store database");
    Ok(())
}

/// [`rehydrate_providers_from_vault`] over the object-safe vault seam.
#[cfg(feature = "postgres")]
fn rehydrate_providers_from_dyn_vault(
    vault: &dyn DynVaultStore,
    runtime: &ProviderRuntime,
    providers: &mut HashMap<String, ProviderHandle>,
) {
    let Ok(entries) = vault.list() else {
        tracing::warn!("vault inventory unavailable; provider rehydration incomplete");
        return;
    };
    for entry in entries.into_iter().filter(|e| e.status == "active") {
        let secret = match vault.get(&entry.provider, &entry.label) {
            Ok(Some(secret)) => secret,
            result => {
                let reason = match result {
                    Ok(None) => "missing",
                    Err(sandhi_store::VaultError::Locked) => "locked",
                    Err(sandhi_store::VaultError::Denied) => "denied",
                    Err(sandhi_store::VaultError::Timeout) => "timeout",
                    Err(sandhi_store::VaultError::Configuration) => "configuration",
                    _ => "unavailable",
                };
                tracing::warn!(provider = %entry.provider, label = %entry.label, reason,
                    "credential not activated; recover via explicit reference registration");
                continue;
            }
        };
        if let Some(handle) = sandhi_proxy::build_provider_handle(
            runtime,
            &entry.provider,
            entry.base_url.as_deref(),
            &secret,
            entry.scheme,
            entry.user_agent.as_deref(),
        ) {
            providers.insert(entry.credential_id(), handle);
        }
    }
}

/// The live alert registry handle (`Option<Arc<Mutex<AlertRegistry>>>`).
type AlertRegistryHandle = Arc<std::sync::Mutex<AlertRegistry>>;

/// The selected store backend set, whatever `SANDHI_STORE_DSN` selected (ADR-0049).
/// `pg: true` marks the whole-store Postgres mode: no SQLite file is opened, the
/// usage store is the PG sink+reader, and the native-reference registry came from PG.
struct StoreSet {
    store: Option<Arc<SqliteStore>>,
    usage_store: Option<Arc<dyn UsageStore>>,
    usage_sink: Arc<dyn Sink>,
    ledger: ProxyLedger,
    vault: Option<Arc<dyn DynVaultStore>>,
    backends: Option<Arc<dyn DynBackendStore>>,
    routes: Option<Arc<dyn DynRouteStore>>,
    vkeys: Option<Arc<dyn DynVirtualKeyStore>>,
    profiles: Option<Arc<dyn DynProfileStore>>,
    alerts: Option<AlertRegistryHandle>,
    alert_store: Option<Arc<dyn DynAlertStore>>,
    references: Option<Arc<dyn DynReferenceStore>>,
    policy_audit: Option<Arc<dyn DynPolicyAuditStore>>,
}

/// Open every durable surface on the selected backend. Postgres whole-store mode is a
/// pure replace (no mirror window): either every store opens on the one DSN or startup
/// fails (exit 1). The SQLite default keeps today's same-file topology unchanged.
#[allow(clippy::type_complexity)] // one startup shape: (stores, seed state handed back)
async fn open_store_set(
    pg_mode: Option<String>,
    store_path: Option<&str>,
    runtime: ProviderRuntime,
    keys: KeyStore,
    mut providers: HashMap<String, ProviderHandle>,
) -> (
    StoreSet,
    ProviderRuntime,
    KeyStore,
    HashMap<String, ProviderHandle>,
) {
    #[cfg(feature = "postgres")]
    if let Some(dsn) = pg_mode.as_deref() {
        let dsn = dsn.to_string();
        // Every PG store blocks on its retained runtime handle — the exact opposite of an
        // async task. Phase 1 (async connects) is driven on a blocking worker via
        // `Handle::block_on`; phase 2 (the blocking rehydration) runs on the same worker
        // AFTER block_on has returned, when the thread again carries no async context.
        // Ownership of the seed state moves through the worker and back.
        let (mut pg, runtime, keys, providers) = tokio::task::spawn_blocking(move || {
            let mut pg = match tokio::runtime::Handle::current()
                .block_on(connect_postgres_stores(&dsn))
            {
                Ok(pg) => pg,
                Err(error) => {
                    eprintln!("sandhi-proxy: startup failed: postgres store unavailable: {error}");
                    std::process::exit(1);
                }
            };
            if let Err(error) = rehydrate_postgres_stores(&mut pg, &runtime, &keys, &mut providers)
            {
                eprintln!("sandhi-proxy: startup failed: postgres store unavailable: {error}");
                std::process::exit(1);
            }
            (pg, runtime, keys, providers)
        })
        .await
        .expect("whole-store startup worker");
        let set = StoreSet {
            store: None,
            usage_store: pg.usage_store(),
            usage_sink: pg.usage_sink(),
            ledger: pg.ledger,
            vault: pg.vault,
            backends: pg.backends,
            routes: pg.routes,
            vkeys: pg.vkeys,
            profiles: pg.profiles,
            alerts: pg.alerts,
            alert_store: pg.alert_store,
            references: Some(pg.references),
            policy_audit: pg.policy_audit.take(),
        };
        return (set, runtime, keys, providers);
    }
    let _ = pg_mode;

    // Required same-file durable usage store and dashboard.
    let store = store_path.map(|p| {
        let store = SqliteStore::open(p).unwrap_or_else(|_| startup_store_fatal("usage"));
        eprintln!("sandhi-proxy: usage store at {p} — dashboard on /dashboard");
        Arc::new(store)
    });

    // TD-0003 P1 operator surface: vault + virtual-key store (same path as the usage store).
    let vault: Option<Arc<dyn DynVaultStore>> = store_path.map(|p| {
        // The library reports backend misconfiguration as Err; the binary owns
        // the exit — a bad SANDHI_CREDENTIALS_DIR is fatal at startup.
        let backend = VaultStore::backend_from_env().unwrap_or_else(|error| {
            eprintln!("sandhi-proxy: {error}");
            std::process::exit(1);
        });
        let vault =
            VaultStore::with_backend(p, backend).unwrap_or_else(|_| startup_store_fatal("vault"));
        eprintln!(
            "sandhi-proxy: credential vault (backend: {}) at {p}",
            vault.backend_name()
        );
        // Metadata must open, but unavailable external secret authority only disables
        // the affected provider handle; locked-broker recovery must remain possible.
        rehydrate_providers_from_vault(&vault, &runtime, &mut providers);
        Arc::new(vault) as Arc<dyn DynVaultStore>
    });
    let backends: Option<Arc<dyn DynBackendStore>> = store_path.map(|path| {
        let store = BackendStore::open(path).unwrap_or_else(|_| startup_store_fatal("backends"));
        if let Some(vault) = vault.as_deref() {
            sandhi_proxy::backend::rehydrate_backends(vault, &store, &runtime, &mut providers)
                .unwrap_or_else(|_| startup_store_fatal("backends"));
        }
        Arc::new(store) as Arc<dyn DynBackendStore>
    });
    let routes: Option<Arc<dyn DynRouteStore>> = store_path.map(|path| {
        let routes = sandhi_store::routes::RouteStore::open(path)
            .unwrap_or_else(|_| startup_store_fatal("routes"));
        routes
            .list()
            .unwrap_or_else(|_| startup_store_fatal("routes"));
        Arc::new(routes) as Arc<dyn DynRouteStore>
    });
    let vkeys: Option<Arc<dyn DynVirtualKeyStore>> = store_path.map(|p| {
        let vkeys = VirtualKeyStore::open(p).unwrap_or_else(|_| startup_store_fatal("vkeys"));
        sandhi_proxy::rehydrate_live_keys(&keys, &vkeys);
        eprintln!("sandhi-proxy: virtual-key store at {p}");
        Arc::new(vkeys) as Arc<dyn DynVirtualKeyStore>
    });

    // Gateway-profiles (Phase 5): per-group `profiles` rows (applied via POST /admin/config/apply)
    // + the `skp_…` profile tokens that authenticate GET /auth/profile. Same file, disjoint tables.
    let profiles: Option<Arc<dyn DynProfileStore>> = store_path.map(|p| {
        let profiles = ProfileStore::open(p).unwrap_or_else(|_| startup_store_fatal("profiles"));
        eprintln!("sandhi-proxy: profile store at {p}");
        Arc::new(profiles) as Arc<dyn DynProfileStore>
    });

    // TD-0003 P2 alert rules: durable store + live registry (rehydrated from the store; webhook
    // transport injected from this tokio runtime).
    let (alert_store, alerts): (Option<Arc<dyn DynAlertStore>>, Option<AlertRegistryHandle>) =
        store_path
            .map(|p| {
                let store = AlertStore::open(p).unwrap_or_else(|_| startup_store_fatal("alerts"));
                eprintln!("sandhi-proxy: alert-rule store at {p}");
                let registry = rehydrate_alerts(&store);
                let store: Arc<dyn DynAlertStore> = Arc::new(store);
                (Some(store), Some(Arc::new(std::sync::Mutex::new(registry))))
            })
            .unwrap_or((None, None));

    // Usage sink + usage reads: PostgreSQL when the legacy narrower `SANDHI_USAGE_DSN` is
    // configured (#518 — usage-only; the `SANDHI_STORE` file stays mandatory because the
    // enforcement ledger, vault, virtual keys and the other operator tables live there and
    // never read usage_events, ADR-0043). Default: the same-file SQLite usage store.
    let (usage_store, usage_sink): (Option<Arc<dyn UsageStore>>, Arc<dyn Sink>) =
        match configured_usage_dsn() {
            // Diverges (exit 1) when the DSN is set without the `postgres` feature.
            Some(dsn) => open_postgres_usage_store(&dsn).await,
            None => {
                let sqlite = store
                    .as_ref()
                    .expect("validated durable usage store")
                    .clone();
                let reader = Arc::clone(&sqlite) as Arc<dyn UsageStore>;
                (Some(reader), sqlite)
            }
        };

    // Enforcement shares the original usage-store file and fixed single-file
    // topology. No startup path substitutes volatile caps or leases.
    let ledger = ProxyLedger::durable(store_path.expect("validated durable path"), 1)
        .unwrap_or_else(|_| startup_store_fatal("ledger"));

    (
        StoreSet {
            store,
            usage_store,
            usage_sink,
            ledger,
            vault,
            backends,
            routes,
            vkeys,
            profiles,
            alerts,
            alert_store,
            references: None,
            policy_audit: None,
        },
        runtime,
        keys,
        providers,
    )
}

fn validate_replica_topology() {
    let replicas = positive_usize_env("SANDHI_REPLICA_COUNT", 1);
    assert!(
        replicas == 1,
        "SANDHI_REPLICA_COUNT={replicas} is unsupported: the current budget ledger and rate \
         limiter are single-node. Run one replica until a shared backend passes the enforcement \
         conformance suite."
    );
}

/// Resolve on the process signals used by terminals and container orchestrators. Axum then stops
/// accepting new connections and waits for active model streams to finish, so their terminal
/// usage frame can settle the enforcement lease before the OTel guard flushes on return from
/// `main`.
async fn shutdown_signal() {
    let ctrl_c = async {
        if let Err(error) = tokio::signal::ctrl_c().await {
            tracing::error!(%error, "failed to install Ctrl-C handler");
            std::future::pending::<()>().await;
        }
    };

    #[cfg(unix)]
    {
        let terminate = async {
            match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
                Ok(mut signal) => {
                    signal.recv().await;
                }
                Err(error) => {
                    tracing::error!(%error, "failed to install SIGTERM handler");
                    std::future::pending::<()>().await;
                }
            }
        };

        tokio::select! {
            () = ctrl_c => {},
            () = terminate => {},
        }
    }

    #[cfg(not(unix))]
    ctrl_c.await;
}

/// Build + register an upstream handle for each active vault credential, so the request path can
/// resolve `provider:label` → real handle immediately after startup.
fn rehydrate_providers_from_vault(
    vault: &VaultStore,
    runtime: &ProviderRuntime,
    providers: &mut HashMap<String, ProviderHandle>,
) {
    let Ok(entries) = vault.list() else {
        tracing::warn!("vault inventory unavailable; provider rehydration incomplete");
        return;
    };
    for entry in entries.into_iter().filter(|e| e.status == "active") {
        // Resolve the exact label; provider-wide resolution would reload the first label
        // repeatedly and leave every other credential unavailable after restart.
        let secret = match vault.get(&entry.provider, &entry.label) {
            Ok(Some(secret)) => secret,
            result => {
                let reason = match result {
                    Ok(None) => "missing",
                    Err(sandhi_store::VaultError::Locked) => "locked",
                    Err(sandhi_store::VaultError::Denied) => "denied",
                    Err(sandhi_store::VaultError::Timeout) => "timeout",
                    Err(sandhi_store::VaultError::Configuration) => "configuration",
                    _ => "unavailable",
                };
                tracing::warn!(provider = %entry.provider, label = %entry.label, reason,
                    "credential not activated; recover via explicit reference registration");
                continue;
            }
        };
        if let Some(handle) = sandhi_proxy::build_provider_handle(
            runtime,
            &entry.provider,
            entry.base_url.as_deref(),
            &secret,
            entry.scheme,
            entry.user_agent.as_deref(),
        ) {
            providers.insert(entry.credential_id(), handle);
        }
    }
}

/// Listener-owned origins; arbitrary request Host values never become authority.
fn control_origins(addr: SocketAddr, tls: bool, http_addr: Option<SocketAddr>) -> Vec<String> {
    let mut origins = Vec::new();
    for (addr, scheme) in std::iter::once((addr, if tls { "https" } else { "http" }))
        .chain(http_addr.map(|addr| (addr, "http")))
    {
        origins.push(format!("{scheme}://{addr}"));
        if addr.ip().is_loopback() {
            for host in ["localhost", "127.0.0.1", "[::1]"] {
                origins.push(format!("{scheme}://{host}:{}", addr.port()));
            }
        }
    }
    origins
}

#[cfg(test)]
mod tests {
    use super::{listener_tls_entry_from_json, normalize_dsn, parse_http_bind};

    use std::path::Path;

    #[test]
    fn usage_dsn_treats_missing_and_whitespace_only_as_unset() {
        // The store-selection posture: unset and blank both mean "use the SQLite usage store";
        // anything non-blank selects the Postgres backend verbatim (validated by connect()).
        assert_eq!(normalize_dsn(None), None);
        assert_eq!(normalize_dsn(Some(String::new())), None);
        assert_eq!(normalize_dsn(Some("   ".into())), None);
        assert_eq!(normalize_dsn(Some("\t\n ".into())), None);
        let dsn = "postgres://user:secret@db:5432/sandhi";
        assert_eq!(normalize_dsn(Some(dsn.into())), Some(dsn.into()));
        // Surrounding whitespace is trimmed, not rejected — but the credential-carrying body
        // is preserved byte-exact.
        assert_eq!(normalize_dsn(Some(format!("  {dsn}  "))), Some(dsn.into()));
    }

    #[test]
    fn whole_store_dsn_selection_and_precedence() {
        // The store-selection posture (ADR-0049): unset and blank both mean "the SQLite
        // topology, unchanged"; anything non-blank selects the whole-store Postgres mode.
        // (Feature-off refusal is compile-time: `store_mode_dsn` returns `Refuse` in a
        // build without `postgres`, and that path exits before any store opens.)
        let dsn = "postgres://user:secret@db:5432/sandhi";
        assert_eq!(super::whole_store_dsn(None, None), None);
        assert_eq!(super::whole_store_dsn(Some(String::new()), None), None);
        assert_eq!(super::whole_store_dsn(Some("   ".into()), None), None);
        // Surrounding whitespace is trimmed, not rejected — the credential-carrying body
        // is preserved byte-exact and never logged.
        assert_eq!(
            super::whole_store_dsn(Some(format!("  {dsn}  ")), None),
            Some(dsn.into())
        );
        // Precedence is a pure selection: the whole-store variable wins whenever it is
        // present (even when the legacy usage-only DSN is also set); the legacy
        // usage-only DSN alone never selects whole-store mode (that stays the #518
        // usage-only topology).
        assert_eq!(
            super::whole_store_dsn(
                Some(dsn.into()),
                Some("postgres://legacy:usage@db/old".into())
            ),
            Some(dsn.into()),
            "SANDHI_STORE_DSN wins over SANDHI_USAGE_DSN"
        );
        assert_eq!(
            super::whole_store_dsn(None, Some("postgres://usage-only@db/old".into())),
            None,
            "SANDHI_USAGE_DSN alone does not select whole-store mode"
        );
    }

    #[test]
    fn http_companion_bind_requires_tls_and_a_distinct_socket() {
        let primary: std::net::SocketAddr = "127.0.0.1:8787".parse().unwrap();
        // Unset stays None in every mode: single-scheme serving is untouched.
        assert_eq!(parse_http_bind(None, true, primary).unwrap(), None);
        assert_eq!(parse_http_bind(None, false, primary).unwrap(), None);
        assert_eq!(
            parse_http_bind(Some("0.0.0.0:18789"), true, primary).unwrap(),
            Some("0.0.0.0:18789".parse().unwrap())
        );
        // Without TLS the primary socket is already plaintext — refuse rather
        // than silently ignore a stale companion setting.
        let error = parse_http_bind(Some("0.0.0.0:18789"), false, primary).unwrap_err();
        assert!(
            error.contains("requires listener TLS"),
            "unexpected: {error}"
        );
        let error = parse_http_bind(Some("127.0.0.1:8787"), true, primary).unwrap_err();
        assert!(
            error.contains("must not share a listening socket"),
            "unexpected: {error}"
        );
        let error = parse_http_bind(Some("not a socket address"), true, primary).unwrap_err();
        assert!(
            error.contains("valid socket address"),
            "unexpected: {error}"
        );
    }

    #[test]
    fn companion_socket_collision_covers_family_wildcards() {
        let v4_wildcard: std::net::SocketAddr = "0.0.0.0:8787".parse().unwrap();
        // The wildcard shadows every specific address of its family on the port.
        for overlapping in ["127.0.0.1:8787", "0.0.0.0:8787", "192.168.1.10:8787"] {
            let other: std::net::SocketAddr = overlapping.parse().unwrap();
            assert!(super::binds_collide(v4_wildcard, other), "{overlapping}");
        }
        // v6 is a distinct socket family (std forces IPV6_V6ONLY)…
        let v6_wildcard: std::net::SocketAddr = "[::]:8787".parse().unwrap();
        assert!(!super::binds_collide(v4_wildcard, v6_wildcard));
        assert!(super::binds_collide(
            v6_wildcard,
            "[::1]:8787".parse().unwrap()
        ));
        // …and a different port never collides, wildcard or not.
        assert!(!super::binds_collide(
            v4_wildcard,
            "0.0.0.0:8788".parse().unwrap()
        ));
        assert!(!super::binds_collide(
            "127.0.0.1:8787".parse().unwrap(),
            "127.0.0.1:8788".parse().unwrap()
        ));
        // Distinct specific addresses on one port coexist (multi-homed hosts).
        assert!(!super::binds_collide(
            "127.0.0.1:8787".parse().unwrap(),
            "192.168.1.10:8787".parse().unwrap()
        ));
    }

    #[test]
    fn completion_without_a_live_deadline_is_not_success() {
        let watchdog = super::ShutdownWatchdog::new();
        assert_eq!(watchdog.remaining(), std::time::Duration::ZERO);
        assert!(!watchdog.complete());
        // Exercise the completion race check without arming a process-exiting test watchdog.
        watchdog.deadline.set(std::time::Instant::now()).unwrap();
        assert!(!watchdog.complete());
    }

    #[test]
    fn shutdown_watchdog_child() {
        let Ok(mode) = std::env::var("SANDHI_TEST_SHUTDOWN_WATCHDOG") else {
            return;
        };
        let watchdog = super::ShutdownWatchdog::new();
        let original = std::time::Instant::now() + std::time::Duration::from_millis(200);
        watchdog.arm(original);
        // Repeated notification must not give a blocked cleanup another grace period.
        watchdog.arm(original + std::time::Duration::from_secs(30));
        assert_eq!(watchdog.deadline.get(), Some(&original));
        match mode.as_str() {
            "complete" => assert!(watchdog.complete()),
            "blocked_cleanup" => {
                // A permanently blocked destructor/exporter, independent of the Tokio runtime.
                std::sync::Barrier::new(2).wait();
                unreachable!("watchdog must terminate the process");
            }
            "blocked_exit_handler" | "concurrent_exit_handler" => {
                extern "C" fn blocked_exit_handler() {
                    loop {
                        std::thread::park();
                    }
                }
                // SAFETY: the synthetic handler is a static C-ABI function which never
                // unwinds. This branch exists only inside an isolated test subprocess.
                assert_eq!(unsafe { libc::atexit(blocked_exit_handler) }, 0);
                if mode == "concurrent_exit_handler" {
                    // On Unix this also holds Rust's ordinary exit serialization while
                    // its C callback is blocked. Windows uses the CRT path explicitly:
                    // std::process::exit need not run CRT callbacks there.
                    #[cfg(unix)]
                    std::process::exit(0);
                    #[cfg(windows)]
                    unsafe {
                        libc::exit(0);
                    }
                }
                // In the old implementation the watchdog's ordinary exit itself enters
                // the blocked callback. Emergency OS termination must bypass it instead.
                std::sync::Barrier::new(2).wait();
                unreachable!("watchdog must bypass blocked process-exit handlers");
            }
            "blocked_runtime" => {
                let runtime = tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(1)
                    .build()
                    .unwrap();
                let (started, ready) = std::sync::mpsc::channel();
                runtime.spawn_blocking(move || {
                    started.send(()).unwrap();
                    std::sync::Barrier::new(2).wait();
                });
                ready.recv().unwrap();
                // Model a non-cooperative teardown; the independent watchdog still owns the
                // original process deadline, regardless of Tokio's requested timeout.
                runtime.shutdown_timeout(std::time::Duration::from_secs(30));
                unreachable!("watchdog must terminate the process");
            }
            _ => panic!("unknown child scenario"),
        }
    }

    fn assert_watchdog_scenario(mode: &str, expected: i32) {
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "tests::shutdown_watchdog_child", "--nocapture"])
            .env("SANDHI_TEST_SHUTDOWN_WATCHDOG", mode)
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        // Bound the test itself if the watchdog regresses; never hang the whole test suite.
        let test_deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                assert_eq!(status.code(), Some(expected), "scenario {mode}");
                break;
            }
            if std::time::Instant::now() >= test_deadline {
                child.kill().unwrap();
                let _ = child.wait();
                panic!("watchdog did not terminate {mode}");
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    #[test]
    fn watchdog_bounds_blocked_cleanup_and_runtime_without_extending_deadline() {
        for (mode, expected) in [
            ("complete", 0),
            ("blocked_cleanup", 124),
            ("blocked_runtime", 124),
        ] {
            assert_watchdog_scenario(mode, expected);
        }
    }

    #[test]
    fn watchdog_bypasses_blocked_exit_handler() {
        assert_watchdog_scenario("blocked_exit_handler", 124);
    }

    #[test]
    fn watchdog_bypasses_concurrent_exit_handler() {
        assert_watchdog_scenario("concurrent_exit_handler", 124);
    }

    #[test]
    fn startup_rehydrates_each_exact_label() {
        let vault = sandhi_store::VaultStore::in_memory().unwrap();
        for label in ["first", "second"] {
            vault
                .set(
                    "openai",
                    label,
                    sandhi_store::CredentialScheme::ApiKey,
                    None,
                    None,
                    "synthetic-key",
                )
                .unwrap();
        }
        let mut providers = std::collections::HashMap::new();
        super::rehydrate_providers_from_vault(
            &vault,
            &sandhi_providers::ProviderRuntime::new(),
            &mut providers,
        );
        assert!(providers.contains_key("openai:first"));
        assert!(providers.contains_key("openai:second"));
    }

    #[test]
    fn tls_bootstrap_does_not_validate_unrelated_operator_sections() {
        let config = r#"{
            "providers": "validated by the operator API, not bootstrap",
            "tls": null
        }"#;
        assert!(
            listener_tls_entry_from_json(config, Path::new("sandhi.json"))
                .expect("valid JSON without TLS remains a plaintext bootstrap")
                .is_none()
        );
    }

    #[test]
    fn tls_bootstrap_rejects_an_incomplete_present_identity() {
        let config = r#"{"tls":{"cert":"/run/tls/fullchain.pem"}}"#;
        let error = listener_tls_entry_from_json(config, Path::new("sandhi.json"))
            .expect_err("a present TLS identity is an atomic certificate/key pair");
        assert!(error.contains("missing field `key`"), "unexpected: {error}");
    }
}
