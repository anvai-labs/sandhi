//! `sandhi-proxy` — the in-path (inline) reverse-proxy egress gate.
//!
//! Bootstrap: registers demo upstreams + virtual keys from env (the legacy single-user path),
//! then — when `SANDHI_STORE` is set — opens the TD-0003 operator surface (provider-credential
//! vault, durable virtual-key store) and rehydrates the live key store + upstream handles from
//! it. The admin API is enabled by `SANDHI_ADMIN_TOKEN`; the vault backend by
//! `SANDHI_VAULT_BACKEND=keyring|sentinelpass` (default `keyring`). Request handling lives in the
//! `sandhi_proxy` library and is exercised by the integration tests.

use axum::http::HeaderMap;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use sandhi_core::{BufferedSink, InMemorySink, KeyStore, Sink, VirtualKey};
use sandhi_providers::{AnthropicAuthScheme, GeminiAuthScheme, ProviderHandle, ProviderRuntime};
use sandhi_proxy::{
    reclaim_sweep_at, rehydrate_alerts, rehydrate_budgets, serve_with_shutdown_timeout,
    BufferedAlertStore, ProxyLedger, ProxyState, DEFAULT_HEADER_READ_TIMEOUT_SECS,
    DEFAULT_MAX_CONNECTIONS, DEFAULT_MAX_CONNECTIONS_PER_IP, DEFAULT_MAX_IN_FLIGHT_AI_REQUESTS,
    DEFAULT_MAX_REQUEST_BODY_BYTES, DEFAULT_SHUTDOWN_GRACE,
};
use sandhi_store::{AlertStore, SqliteStore, VaultStore, VirtualKeyStore};

#[tokio::main]
async fn main() {
    // TD-0011 D1: the BINARY installs the subscriber; the libraries only emit through the
    // `tracing` facade. That is what lets an in-process host (Victor) capture Sandhi's spans in
    // its own logging without Sandhi imposing a runtime or a second subscriber.
    //
    // `SANDHI_LOG` (falling back to `RUST_LOG`) controls filtering; the default keeps the
    // operator-relevant events — denials, fail-open admissions, reclaims, settle failures —
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

    // The shipped enforcement ledger and request limiter are deliberately single-node. Refuse a
    // declared multi-replica topology rather than silently multiplying rate limits or allowing
    // separate processes to make independent hard-budget decisions.
    validate_replica_topology();

    // Scope 5 (TD-0011 P3): OTLP export of gen_ai.* spans + metrics. `init()` returns None unless
    // the `otel-otlp` feature is compiled in AND `SANDHI_OTEL_EXPORT=otlp` is set — so the default
    // build is unaffected. The guard must outlive `serve()` so the OTel providers flush on shutdown.
    let (otel_recorder, _otel_guard) = sandhi_proxy::otel::init().unzip();
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
            .unwrap_or_else(|_| "https://api.openai.com/v1".into());
        providers.insert(
            "openai".into(),
            runtime.openai_compat("openai", base, key, Default::default(), None, None, None),
        );
        keys.insert(VirtualKey {
            id: "vk_openai_demo".into(),
            subject_id: Some("demo".into()),
            group_id: Some("demo".into()),
            upstream_ref: "openai".into(),
            ..Default::default()
        });
        eprintln!("sandhi-proxy: registered openai upstream + vk_openai_demo");
    }
    if let Ok(key) = std::env::var("SANDHI_ANTHROPIC_KEY") {
        // Symmetric with SANDHI_OPENAI_BASE. Without an override the Anthropic upstream could
        // only ever be the public API — no Anthropic-compatible gateway, no local mock, and no
        // way for the SDK-conformance suite to exercise this path at all.
        let base = std::env::var("SANDHI_ANTHROPIC_BASE")
            .unwrap_or_else(|_| "https://api.anthropic.com".into());
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
            ),
        );
        keys.insert(VirtualKey {
            id: "vk_anthropic_demo".into(),
            subject_id: Some("demo".into()),
            group_id: Some("demo".into()),
            upstream_ref: "anthropic".into(),
            ..Default::default()
        });
        eprintln!("sandhi-proxy: registered anthropic upstream + vk_anthropic_demo");
    }

    if let Ok(key) = std::env::var("SANDHI_GEMINI_KEY") {
        let base = std::env::var("SANDHI_GEMINI_BASE")
            .unwrap_or_else(|_| "https://generativelanguage.googleapis.com".into());
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
            ),
        );
        keys.insert(VirtualKey {
            id: "vk_gemini_demo".into(),
            subject_id: Some("demo".into()),
            group_id: Some("demo".into()),
            upstream_ref: "gemini".into(),
            ..Default::default()
        });
        eprintln!("sandhi-proxy: registered gemini upstream + vk_gemini_demo");
    }

    // Durable usage store (SQLite) + dashboard when SANDHI_STORE=<path> is set; else in-memory.
    let store = std::env::var("SANDHI_STORE")
        .ok()
        .and_then(|p| match SqliteStore::open(&p) {
            Ok(s) => {
                eprintln!("sandhi-proxy: usage store at {p} — dashboard on /dashboard");
                Some(Arc::new(s))
            }
            Err(e) => {
                eprintln!("sandhi-proxy: could not open SANDHI_STORE={p}: {e}");
                None
            }
        });

    // TD-0003 P1 operator surface: vault + virtual-key store (same path as the usage store).
    let vault = std::env::var("SANDHI_STORE").ok().and_then(|p| {
        match VaultStore::with_backend(&p, VaultStore::backend_from_env()) {
            Ok(v) => {
                eprintln!(
                    "sandhi-proxy: credential vault (backend: {}) at {p}",
                    v.backend_name()
                );
                // Rehydrate upstream handles for every active vault credential.
                rehydrate_providers_from_vault(&v, &runtime, &mut providers);
                Some(Arc::new(v))
            }
            Err(e) => {
                eprintln!("sandhi-proxy: could not open vault at {p}: {e}");
                None
            }
        }
    });
    let vkeys = std::env::var("SANDHI_STORE")
        .ok()
        .and_then(|p| match VirtualKeyStore::open(&p) {
            Ok(v) => {
                sandhi_proxy::rehydrate_live_keys(&keys, &v);
                eprintln!("sandhi-proxy: virtual-key store at {p}");
                Some(Arc::new(v))
            }
            Err(e) => {
                eprintln!("sandhi-proxy: could not open vkey store at {p}: {e}");
                None
            }
        });

    // TD-0003 P2 alert rules: durable store + live registry (rehydrated from the store; webhook
    // transport injected from this tokio runtime).
    let (alert_store, alerts) = std::env::var("SANDHI_STORE")
        .ok()
        .and_then(|p| match AlertStore::open(&p) {
            Ok(store) => {
                eprintln!("sandhi-proxy: alert-rule store at {p}");
                let registry = rehydrate_alerts(&store);
                Some((Arc::new(store), Arc::new(std::sync::Mutex::new(registry))))
            }
            Err(e) => {
                eprintln!("sandhi-proxy: could not open alert store at {p}: {e}");
                None
            }
        })
        .unzip();
    let buffered_alert_store = alert_store.as_ref().map(|store| {
        Arc::new(BufferedAlertStore::new(
            Arc::clone(store),
            positive_usize_env("SANDHI_ALERT_BUFFER_CAPACITY", 256),
        ))
    });

    let mut buffered_sink: Option<Arc<BufferedSink>> = None;
    let sink: Arc<dyn Sink> = match &store {
        Some(s) => {
            let buffered = Arc::new(BufferedSink::new(
                s.clone(),
                positive_usize_env("SANDHI_USAGE_BUFFER_CAPACITY", 1024),
            ));
            buffered_sink = Some(Arc::clone(&buffered));
            buffered
        }
        None => Arc::new(InMemorySink::new()),
    };

    // ADR-0005 step 2: the enforcement ledger is durable (crash-safe leases, calendar windows,
    // restart-surviving spend) when SANDHI_STORE is set — sharing that SQLite file, its tables are
    // disjoint from the usage store's — and volatile in-memory otherwise.
    // TD-0016 P1: scope-shard the durable ledger so different tenants never
    // serialize their durable commits. 1 (default) = the single legacy file.
    let ledger_shards = positive_usize_env("SANDHI_LEDGER_SHARDS", 1);
    assert!(
        (1..=64).contains(&ledger_shards),
        "SANDHI_LEDGER_SHARDS must be between 1 and 64, got {ledger_shards}"
    );
    let ledger = match std::env::var("SANDHI_STORE") {
        Ok(path) => match ProxyLedger::durable(&path, ledger_shards) {
            Ok(l) => {
                eprintln!(
                    "sandhi-proxy: durable enforcement ledger at {path} ({ledger_shards} shard{})",
                    if ledger_shards == 1 { "" } else { "s" }
                );
                l
            }
            Err(e) => {
                eprintln!(
                    "sandhi-proxy: durable ledger unavailable ({e}); falling back to in-memory"
                );
                ProxyLedger::in_memory()
            }
        },
        Err(_) => ProxyLedger::in_memory(),
    };

    let admin_token = std::env::var("SANDHI_ADMIN_TOKEN").ok();
    let public_url =
        std::env::var("SANDHI_PUBLIC_URL").unwrap_or_else(|_| "http://localhost:8787".into());
    if admin_token.is_some() {
        eprintln!("sandhi-proxy: admin API enabled on /admin/*");
    }

    let mut state = ProxyState::new(keys, ledger, sink, providers, store);
    state.vault = vault;
    state.vkeys = vkeys;
    state.alert_store = alert_store;
    state.alert_writer = buffered_alert_store.clone();
    state.alerts = alerts;
    state.admin_token = admin_token;
    state.public_url = public_url;
    // ADR-0004 D4: dashboard read endpoints follow the admin token unless explicitly re-opened.
    state.dashboard_public = std::env::var("SANDHI_DASHBOARD_PUBLIC").as_deref() == Ok("1");
    state.error_detail_full = std::env::var("SANDHI_ERROR_DETAIL").as_deref() == Ok("full");
    state.max_request_body_bytes = request_body_limit_from_env();
    state.max_in_flight_ai_requests = positive_usize_env(
        "SANDHI_MAX_IN_FLIGHT_AI_REQUESTS",
        DEFAULT_MAX_IN_FLIGHT_AI_REQUESTS,
    );
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
    state.trusted_proxies = sandhi_proxy::parse_trusted_proxies(
        &std::env::var("SANDHI_TRUSTED_PROXIES").unwrap_or_default(),
    );
    state.config_path = std::env::var("SANDHI_CONFIG")
        .ok()
        .map(std::path::PathBuf::from);
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
    if state.store.is_some() && state.admin_token.is_none() && !state.dashboard_public {
        eprintln!(
            "sandhi-proxy: WARNING: SANDHI_STORE is set without SANDHI_ADMIN_TOKEN — the \
             /dashboard/api/* read endpoints (subject/group usage, masked vkey metadata) are open \
             to any caller. Set SANDHI_ADMIN_TOKEN to gate them, or SANDHI_DASHBOARD_PUBLIC=1 to \
             acknowledge the open single-node posture."
        );
    }
    // Recover the operator budget metadata (policy / window / limit) persisted in the durable
    // ledger, so caps set before a restart keep their policy lookup + dashboard + alert thresholds.
    {
        let ledger = state.ledger.lock().expect("ledger poisoned");
        rehydrate_budgets(&ledger, &state.budgets);
    }
    // Scope 5: attach the OTLP recorder (None unless feature-on + configured). The `_otel_guard`
    // captured above flushes the providers when main returns.
    state.otel = otel_recorder;
    let state = Arc::new(state);

    // ADR-0005 D2: reclaim leases left dangling by a crash on a timer, so an abandoned scope's
    // held capacity is released without waiting for its next request (`reserve` also reclaims
    // opportunistically per scope; this covers scopes that go quiet). Best-effort by design.
    let reclaim_task = {
        let sweep_state = Arc::clone(&state);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(60));
            tick.tick().await; // discard the immediate first tick
            loop {
                tick.tick().await;
                let sweep = Arc::clone(&sweep_state);
                let _ = tokio::task::spawn_blocking(move || {
                    reclaim_sweep_at(&sweep.ledger, time::OffsetDateTime::now_utc())
                })
                .await;
            }
        })
    };

    let addr: SocketAddr = std::env::var("SANDHI_BIND")
        .unwrap_or_else(|_| "127.0.0.1:8787".into())
        .parse()
        .expect("SANDHI_BIND must be a valid socket address");

    eprintln!(
        "sandhi-proxy listening on http://{addr}  \
         (POST /v1/chat/completions | /v1/messages, Authorization: Bearer vk_...)"
    );
    let shutdown_grace = shutdown_grace_from_env();
    let serve_result =
        serve_with_shutdown_timeout(state, addr, shutdown_signal(), shutdown_grace).await;
    reclaim_task.abort();
    let _ = reclaim_task.await;
    if let Some(buffered) = buffered_alert_store {
        if !buffered.close(shutdown_grace) {
            tracing::error!(
                dropped = buffered.dropped_updates(),
                "alert writer did not drain before shutdown deadline"
            );
        }
    }
    if let Some(buffered) = buffered_sink {
        if !buffered.close(shutdown_grace) {
            tracing::error!(
                dropped = buffered.dropped_events(),
                "usage writer did not drain before shutdown deadline"
            );
        }
    }
    if let Err(e) = serve_result {
        eprintln!("sandhi-proxy error: {e}");
        std::process::exit(1);
    }
}

fn request_body_limit_from_env() -> usize {
    positive_usize_env(
        "SANDHI_MAX_REQUEST_BODY_BYTES",
        DEFAULT_MAX_REQUEST_BODY_BYTES,
    )
}

fn shutdown_grace_from_env() -> std::time::Duration {
    std::time::Duration::from_secs(positive_u64_env(
        "SANDHI_SHUTDOWN_GRACE_SECS",
        DEFAULT_SHUTDOWN_GRACE.as_secs(),
    ))
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

    tracing::info!("shutdown signal received; draining in-flight requests");
}

/// Build + register an upstream handle for each active vault credential, so the request path can
/// resolve `provider:label` → real handle immediately after startup.
fn rehydrate_providers_from_vault(
    vault: &VaultStore,
    runtime: &ProviderRuntime,
    providers: &mut HashMap<String, ProviderHandle>,
) {
    let Ok(entries) = vault.list() else {
        return;
    };
    for entry in entries.into_iter().filter(|e| e.status == "active") {
        if let Ok(Some((entry, secret))) = vault.resolve(&entry.provider) {
            if let Some(handle) = sandhi_proxy::build_provider_handle(
                runtime,
                &entry.provider,
                entry.base_url.as_deref(),
                &secret,
                entry.scheme,
            ) {
                providers.insert(entry.credential_id(), handle);
            }
        }
    }
}
