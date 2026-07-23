//! `sandhi-proxy` — the in-path (inline) reverse-proxy egress gate.
//!
//! Bootstrap: registers demo upstreams + virtual keys from env (the legacy single-user path),
//! then — when `SANDHI_STORE` is set — opens the TD-0003 operator surface (provider-credential
//! vault, durable virtual-key store) and rehydrates the live key store + upstream handles from
//! it. The admin API is enabled by `SANDHI_ADMIN_TOKEN`; the vault backend by
//! `SANDHI_VAULT_BACKEND=keyring|sentinelpass` (default `keyring`). Request handling lives in the
//! `sandhi_proxy` library and is exercised by the integration tests.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use sandhi_core::{BudgetLedger, InMemorySink, KeyStore, Sink, VirtualKey};
use sandhi_providers::{AnthropicAuthScheme, ProviderHandle, ProviderRuntime};
use sandhi_proxy::{rehydrate_alerts, serve, ProxyState};
use sandhi_store::{AlertStore, SqliteStore, VaultStore, VirtualKeyStore};

#[tokio::main]
async fn main() {
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
        providers.insert(
            "anthropic".into(),
            runtime.anthropic(
                "https://api.anthropic.com",
                key,
                AnthropicAuthScheme::ApiKey,
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

    let sink: Arc<dyn Sink> = match &store {
        Some(s) => s.clone(),
        None => Arc::new(InMemorySink::new()),
    };

    let admin_token = std::env::var("SANDHI_ADMIN_TOKEN").ok();
    let public_url =
        std::env::var("SANDHI_PUBLIC_URL").unwrap_or_else(|_| "http://localhost:8787".into());
    if admin_token.is_some() {
        eprintln!("sandhi-proxy: admin API enabled on /admin/*");
    }

    let mut state = ProxyState::new(keys, BudgetLedger::new(), sink, providers, store);
    state.vault = vault;
    state.vkeys = vkeys;
    state.alert_store = alert_store;
    state.alerts = alerts;
    state.admin_token = admin_token;
    state.public_url = public_url;
    let state = Arc::new(state);

    let addr: SocketAddr = std::env::var("SANDHI_BIND")
        .unwrap_or_else(|_| "127.0.0.1:8787".into())
        .parse()
        .expect("SANDHI_BIND must be a valid socket address");

    eprintln!(
        "sandhi-proxy listening on http://{addr}  \
         (POST /v1/chat/completions | /v1/messages, Authorization: Bearer vk_...)"
    );
    if let Err(e) = serve(state, addr).await {
        eprintln!("sandhi-proxy error: {e}");
        std::process::exit(1);
    }
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
