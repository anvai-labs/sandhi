//! PR-D whole-store wiring (ADR-0049): live-Postgres test, gated on `SANDHI_TEST_PG_DSN`
//! exactly like the PR-A/B/C store suites (`cargo test -p sandhi-proxy --features postgres`).
//!
//! Every durable surface — usage store, enforcement ledger, the eight operator stores,
//! the native-reference registry — is opened through the promoted `connect`/`connect_with`
//! constructors on ONE DSN and exercised through the object-safe `Dyn*` seams that
//! `ProxyState` holds. Rows are tagged with the test pid on the shared test database
//! (the suite's disposable live instance); reads are bounded to that tag.

#![cfg(feature = "postgres")]

use std::collections::HashMap;
use std::sync::Arc;

use sandhi_core::{KeyStore, Sink, UsageEvent};
use sandhi_proxy::ProxyLedger;
use sandhi_store::{
    CreateAlertRequest, DynAlertStore, DynBackendStore, DynProfileStore, DynReferenceStore,
    DynRouteStore, DynVaultStore, DynVirtualKeyStore, InMemoryVault, MintRequest, PostgresStore,
    UsageStore,
};

fn dsn() -> String {
    std::env::var("SANDHI_TEST_PG_DSN")
        .ok()
        .map(|dsn| dsn.trim().to_string())
        .filter(|dsn| !dsn.is_empty())
        .unwrap_or_else(|| panic!("set SANDHI_TEST_PG_DSN to run the live-Postgres wiring tests"))
}

fn tag() -> String {
    format!("pgw{}", std::process::id())
}

#[test]
fn whole_store_pg_stores_round_trip_through_the_proxy_seams() {
    // The operator stores block_on through their retained runtime handle, so the test
    // owns an explicit multi-thread runtime (like the sandhi-store suites) and keeps it
    // alive for every later blocking statement — store methods never run on an async
    // executor task here.
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("test runtime");
    let _runtime_guard = rt.handle().clone();
    let dsn = dsn();
    let tag = tag();

    let (usage, ledger, vault, backends, routes, vkeys, profiles, alert_store, references) = rt
        .block_on(async {
            // One DSN, every store. `connect_with` is the options path the promoted constructors
            // expose; `connect(dsn)` must delegate through it — exercise both.
            let usage = Arc::new(PostgresStore::connect(&dsn).await.expect("usage store"));
            let ledger = ProxyLedger::postgres_durable(&dsn)
                .await
                .expect("enforcement ledger");
            let vault: Arc<dyn DynVaultStore> = Arc::new(
                sandhi_store::PostgresVaultStore::connect(&dsn, Box::new(InMemoryVault::new()))
                    .await
                    .expect("vault metadata"),
            );
            let backends: Arc<dyn DynBackendStore> = Arc::new(
                sandhi_store::PostgresBackendStore::connect(&dsn)
                    .await
                    .expect("backends"),
            );
            let routes: Arc<dyn DynRouteStore> = Arc::new(
                sandhi_store::PostgresRouteStore::connect(&dsn)
                    .await
                    .expect("routes"),
            );
            let vkeys: Arc<dyn DynVirtualKeyStore> = Arc::new(
                sandhi_store::PostgresVirtualKeyStore::connect(&dsn)
                    .await
                    .expect("virtual keys"),
            );
            let profiles: Arc<dyn DynProfileStore> = Arc::new(
                sandhi_store::PostgresProfileStore::connect(&dsn)
                    .await
                    .expect("profiles"),
            );
            let alert_store: Arc<dyn DynAlertStore> = Arc::new(
                sandhi_store::PostgresAlertStore::connect(&dsn)
                    .await
                    .expect("alerts"),
            );
            let references: Arc<dyn DynReferenceStore> = Arc::new(
                sandhi_store::PostgresReferenceStore::connect(&dsn)
                    .await
                    .expect("native references"),
            );
            (
                usage,
                ledger,
                vault,
                backends,
                routes,
                vkeys,
                profiles,
                alert_store,
                references,
            )
        });
    // The owning runtime must outlive every blocking statement below.
    std::mem::forget(rt);

    // The proxy state the binary would hold: PG usage sink/reader, PG ledger, and the
    // operator stores behind the same seams main.rs assigns.
    let mut state = sandhi_proxy::ProxyState::new(
        KeyStore::new(),
        ledger,
        Arc::clone(&usage) as Arc<dyn Sink>,
        HashMap::new(),
        None,
    );
    state.usage_store = Some(Arc::clone(&usage) as Arc<dyn UsageStore>);
    state.vault = Some(Arc::clone(&vault));
    state.backends = Some(Arc::clone(&backends));
    state.routes = Some(Arc::clone(&routes));
    state.vkeys = Some(Arc::clone(&vkeys));
    state.profiles = Some(Arc::clone(&profiles));
    state.alert_store = Some(Arc::clone(&alert_store));
    state.postgres_native_references = Some(Arc::clone(&references));

    // Vault: metadata + secret round trip through the seam (secret stays in the backend).
    let credential_id = vault
        .set(
            "openai",
            &tag,
            sandhi_store::CredentialScheme::ApiKey,
            None,
            None,
            "wiring-secret",
        )
        .expect("vault set");
    assert_eq!(credential_id, format!("openai:{tag}"));
    assert_eq!(
        vault.get("openai", &tag).expect("vault get").as_deref(),
        Some("wiring-secret")
    );
    assert!(vault
        .list()
        .expect("vault list")
        .iter()
        .any(|entry| entry.label == tag));

    // Virtual keys: mint through the seam, resolve by secret (the inference path).
    let minted = vkeys
        .mint(MintRequest {
            subject_id: Some(tag.clone()),
            group_id: Some(tag.clone()),
            upstream_ref: "openai".into(),
            models: vec!["fixture".into()],
            budget_scope: Some(format!("vk:{tag}")),
            model_budget_scope: None,
            expires_at: None,
            rate_limit_per_min: Some(60),
        })
        .expect("mint");
    assert!(vkeys
        .find_by_secret(&minted.secret)
        .expect("find_by_secret")
        .is_some());

    // Profiles + alerts: write and read one row each.
    profiles
        .upsert_profile(&sandhi_store::ProfileRecord {
            group: tag.clone(),
            mode: "gateway".into(),
            upstream_ref: Some("openai".into()),
            models: vec!["fixture".into()],
            timeout_seconds: None,
            max_tokens: None,
            updated_at: time_rfc3339(),
        })
        .expect("profile upsert");
    assert!(profiles
        .profile_for_group(&tag)
        .expect("profile_for_group")
        .is_some());
    let rule = alert_store
        .create(CreateAlertRequest {
            scope: format!("vk:{tag}"),
            threshold_pct: 80,
            channel: sandhi_core::AlertChannel::Log,
        })
        .expect("alert create");
    assert!(alert_store
        .list()
        .expect("alert list")
        .iter()
        .any(|record| record.id == rule.id));

    // Backend + route inventories read through the seams (writes are PR-C's suites).
    backends.list().expect("backend list");
    routes.list().expect("route list");

    // Native-reference registry: issue then authorize the same batch (the proof path).
    // Binding and owner are 64-hex digests by contract; one synthetic reference.
    let binding = "c".repeat(64);
    let owner = "d".repeat(64);
    let refs = [sandhi_core::native_references::Reference {
        kind: 1,
        digest: "e".repeat(64),
    }];
    references
        .issue(&binding, &owner, &refs, (time_now() as i64) + 600)
        .expect("reference issue");
    references
        .authorize(&binding, &owner, &refs)
        .expect("reference authorize");

    // Usage: emit through the sink (the PG store) and read back through the seam.
    let event = UsageEvent::new(
        format!("req-{tag}"),
        time_rfc3339(),
        "openai",
        "fixture",
        sandhi_core::Backend::External,
    )
    .with_attribution(
        Some(format!("vk-{tag}")),
        Some(tag.clone()),
        Some(tag.clone()),
    )
    .with_tokens(11, 7);
    state.sink.emit(&event);
    let subjects = state
        .usage_store
        .as_ref()
        .expect("usage reader")
        .totals_by_subject()
        .expect("totals");
    assert!(subjects.iter().any(|bucket| bucket.key == tag));

    // Enforcement: budget, lease, settle, spend — all on the PG ledger behind the state.
    state
        .ledger
        .lock()
        .expect("ledger poisoned")
        .set_budget(
            &format!("vk:{tag}"),
            Some(1_000),
            sandhi_core::Window::Total,
            sandhi_core::Policy::Block,
        )
        .expect("set budget");
    let lease = {
        let mut guard = state.ledger.lock().expect("ledger poisoned");
        if let sandhi_proxy::Admission::Leased(lease) = guard.reserve(
            &format!("vk:{tag}"),
            100,
            time::OffsetDateTime::now_utc(),
            sandhi_core::Policy::Block,
        ) {
            lease
        } else {
            panic!("under-cap ceiling must admit a lease")
        }
    };
    state
        .ledger
        .lock()
        .expect("ledger poisoned")
        .settle(&lease, 40);
    assert_eq!(
        state
            .ledger
            .lock()
            .expect("ledger poisoned")
            .spent(&format!("vk:{tag}")),
        40
    );
    assert_eq!(
        state
            .ledger
            .lock()
            .expect("ledger poisoned")
            .limit(&format!("vk:{tag}")),
        Some(1_000)
    );
}

fn time_now() -> u64 {
    time::OffsetDateTime::now_utc().unix_timestamp() as u64
}

fn time_rfc3339() -> String {
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .expect("format timestamp")
}
