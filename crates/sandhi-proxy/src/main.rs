//! `sandhi-proxy` — the in-path (inline) reverse-proxy egress gate.
//!
//! MVP bootstrap: registers upstreams + a demo virtual key from env, then serves. A real config
//! loader (keys, budgets, multiple upstreams) is a follow-up; the request handling lives in the
//! `sandhi_proxy` library and is exercised by the integration tests.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use sandhi_core::{BudgetLedger, InMemorySink, KeyStore, Sink, VirtualKey};
use sandhi_providers::{Anthropic, OpenAiCompat, Provider};
use sandhi_proxy::{serve, ProxyState};
use sandhi_store::SqliteStore;

#[tokio::main]
async fn main() {
    let mut keys = KeyStore::new();
    let mut providers: HashMap<String, Arc<dyn Provider>> = HashMap::new();

    if let Ok(key) = std::env::var("SANDHI_OPENAI_KEY") {
        let base = std::env::var("SANDHI_OPENAI_BASE")
            .unwrap_or_else(|_| "https://api.openai.com/v1".into());
        providers.insert(
            "openai".into(),
            Arc::new(OpenAiCompat::new("openai", base, key)),
        );
        keys.insert(VirtualKey {
            id: "vk_openai_demo".into(),
            subject_id: Some("demo".into()),
            group_id: Some("demo".into()),
            upstream_ref: "openai".into(),
        });
        eprintln!("sandhi-proxy: registered openai upstream + vk_openai_demo");
    }
    if let Ok(key) = std::env::var("SANDHI_ANTHROPIC_KEY") {
        providers.insert("anthropic".into(), Arc::new(Anthropic::hosted(key)));
        keys.insert(VirtualKey {
            id: "vk_anthropic_demo".into(),
            subject_id: Some("demo".into()),
            group_id: Some("demo".into()),
            upstream_ref: "anthropic".into(),
        });
        eprintln!("sandhi-proxy: registered anthropic upstream + vk_anthropic_demo");
    }

    if providers.is_empty() {
        eprintln!(
            "sandhi-proxy: no SANDHI_OPENAI_KEY / SANDHI_ANTHROPIC_KEY set — serving /healthz only. \
             See docs/adr/0001-sandhi-architecture-and-wire-contract.md."
        );
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
    let sink: Arc<dyn Sink> = match &store {
        Some(s) => s.clone(),
        None => Arc::new(InMemorySink::new()),
    };

    let state = Arc::new(ProxyState {
        keys,
        ledger: Mutex::new(BudgetLedger::new()),
        sink,
        providers,
        store,
    });

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
