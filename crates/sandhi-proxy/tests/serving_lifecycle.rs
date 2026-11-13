//! Actual public serving lifecycle over the original durable ledger; no inference replay.
use axum::{body::Body, http::Request};
use sandhi_core::{
    allowance::Weight, EndpointFamilyV1, InMemorySink, KeyStore, UsageCompleteness, UsageV2,
};
use sandhi_providers::ProviderRuntime;
use sandhi_proxy::{
    build_app, is_incomplete_shutdown, lifecycle::Phase, serve_with_http_and_tls_shutdown_timeout,
    serve_with_shutdown_timeout, serve_with_tls_shutdown_timeout, ProxyLedger, ProxyState,
    TlsConfig,
};
use sandhi_store::{
    ledger::evidence::{
        context::{AccountingPolicy, Delivery, ExecutionContext, Operation, Plane},
        AttemptEvidence, IntentAdmission, ObservedProvider, RecoveryState, TransportOutcome,
        TransportTerminal,
    },
    SqliteLedger,
};
use std::{collections::HashMap, net::SocketAddr, path::Path, sync::Arc, time::Duration};
use tower::ServiceExt;
use wiremock::MockServer;

const CLIENT: &str = "vk:removed";
const SOURCE: &str = "upstream:openai";
const GRACE: Duration = Duration::from_millis(500);

fn context() -> ExecutionContext {
    ExecutionContext {
        version: 1,
        ingress: EndpointFamilyV1::OpenaiChatCompletions,
        operation: Some(Operation::Chat),
        delivery: Delivery::Buffered,
        plane: Plane::Transparent,
        upstream_ref: "removed-route".into(),
        upstream_budget_scope: SOURCE.into(),
        requested_model: "fixture".into(),
        actual_model: "fixture".into(),
        provider: "openai".into(),
        endpoint_family: EndpointFamilyV1::OpenaiChatCompletions,
        transport_binding_sha256: "0".repeat(64),
        route: None,
        backend: None,
        cache: None,
        accounting_policy: AccountingPolicy {
            version: 1,
            cache_read_weight: Weight::new(10_000).unwrap(),
        },
        content_policy_revision: None,
    }
}

fn seed(path: &Path, terminal: bool) -> String {
    let mut ledger = SqliteLedger::open(path.to_str().unwrap()).unwrap();
    let IntentAdmission::Admitted(intent) = ledger
        .reserve_paired_contextual_durable(
            CLIENT,
            34,
            time::OffsetDateTime::now_utc(),
            time::Duration::minutes(5),
            100,
            "original-request",
            &context(),
        )
        .unwrap()
    else {
        panic!("synthetic admission denied")
    };
    ledger
        .authorize_dispatch_durable(CLIENT, &intent.execution_id)
        .unwrap();
    if terminal {
        let attempt = AttemptEvidence {
            version: 2,
            attempt_id: ledger
                .physical_attempt_durable(CLIENT, &intent.execution_id)
                .unwrap()
                .unwrap()
                .attempt_id,
            attempt_ordinal: 1,
            dispatch_observed_at_ms: Some(1000),
            terminal: Some(TransportTerminal {
                observed_at_ms: 1001,
                outcome: TransportOutcome::Success,
                provider_status: Some(200),
                provider_request_id: Some("original-source-request".into()),
            }),
            dropped_observations: 0,
            invalid: false,
            observed_provider: ObservedProvider::from_labels(
                "openai".into(),
                Some("fixture".into()),
            ),
        };
        ledger
            .record_terminal_with_attempt_durable(
                CLIENT,
                &intent.execution_id,
                &UsageV2 {
                    tokens_in: 7,
                    tokens_out: 3,
                    cache_read_tokens: 4,
                    completeness: UsageCompleteness::Final,
                    ..Default::default()
                },
                10_000,
                &attempt,
            )
            .unwrap();
    }
    intent.execution_id
}

fn state(path: &Path, origin: &str) -> (Arc<ProxyState>, Arc<InMemorySink>) {
    let sink = Arc::new(InMemorySink::new());
    let handle = ProviderRuntime::new().openai_compat(
        "openai",
        origin,
        "synthetic",
        Default::default(),
        Some(0),
        None,
        None,
        None,
    );
    let mut state = ProxyState::new(
        KeyStore::new(),
        ProxyLedger::durable(path.to_str().unwrap(), 1).unwrap(),
        sink.clone(),
        HashMap::from([("unused".into(), handle)]),
        None,
    );
    state.shutdown_quiesce = Duration::ZERO;
    (Arc::new(state), sink)
}
fn address() -> SocketAddr {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
}
fn tls() -> TlsConfig {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/tls");
    TlsConfig::from_pem_files(
        path.join("localhost-cert.pem"),
        path.join("localhost-key.pem"),
    )
    .unwrap()
}
async fn wait_until(mut ready: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(3), async {
        let mut tick = tokio::time::interval(Duration::from_millis(10));
        while !ready() {
            tick.tick().await;
        }
    })
    .await
    .expect("bounded condition did not become true");
}
async fn bound(addr: SocketAddr) {
    tokio::time::timeout(Duration::from_secs(3), async {
        let mut tick = tokio::time::interval(Duration::from_millis(10));
        loop {
            if let Ok(stream) = tokio::net::TcpStream::connect(addr).await {
                drop(stream);
                break;
            }
            tick.tick().await;
        }
    })
    .await
    .expect("public serving did not bind");
}
fn receipts(path: &Path) -> i64 {
    let conn = rusqlite::Connection::open(path).unwrap();
    conn.query_row("SELECT count(*) FROM budget_settlement_outbox", [], |r| {
        r.get(0)
    })
    .unwrap()
}

#[tokio::test]
async fn public_serving_recovers_ready_receipt_and_both_legs_without_an_http_request() {
    for scheme in ["plain", "tls", "dual"] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("original.db");
        let id = seed(&path, true);
        assert_eq!(receipts(&path), 0);
        let original_attempt = SqliteLedger::open(path.to_str().unwrap())
            .unwrap()
            .physical_attempt_durable(CLIENT, &id)
            .unwrap()
            .unwrap();
        let source = MockServer::start().await;
        let (state, sink) = state(&path, &source.uri());
        let (tx, rx) = tokio::sync::oneshot::channel();
        let serving = state.clone();
        let primary = address();
        let server = tokio::spawn(async move {
            let shutdown = async {
                let _ = rx.await;
            };
            match scheme {
                "plain" => serve_with_shutdown_timeout(serving, primary, shutdown, GRACE).await,
                "tls" => {
                    serve_with_tls_shutdown_timeout(serving, primary, shutdown, GRACE, tls()).await
                }
                _ => {
                    serve_with_http_and_tls_shutdown_timeout(
                        serving,
                        primary,
                        address(),
                        shutdown,
                        GRACE,
                        tls(),
                    )
                    .await
                }
            }
        });
        // No client request is needed to drive restart recovery, including a removed
        // key/route that can never produce a new inference request.
        wait_until(|| receipts(&path) == 1).await;
        let mut ledger = SqliteLedger::open(path.to_str().unwrap()).unwrap();
        let entries = ledger
            .recovery_page_durable(CLIENT, None, 10)
            .unwrap()
            .entries;
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].execution_id, id);
        assert_eq!(
            ledger
                .physical_attempt_durable(CLIENT, &id)
                .unwrap()
                .unwrap(),
            original_attempt
        );
        assert!(matches!(&entries[0].state, RecoveryState::Settled(r) if r.charged_tokens == 14));
        for scope in [CLIENT, SOURCE] {
            assert_eq!(ledger.spent_durable(scope).unwrap(), 14);
            assert_eq!(ledger.reserved_durable(scope).unwrap(), 0);
        }
        assert_eq!(
            ledger.request_id_durable(CLIENT, &id).unwrap().as_deref(),
            Some("original-request")
        );
        assert_eq!(
            state.lifecycle.deadline(),
            None,
            "startup must not consume shutdown grace"
        );
        tx.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(1), server)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(state.lifecycle.phase(), Phase::Stopped);
        assert_eq!(receipts(&path), 1);
        assert!(source.received_requests().await.unwrap().is_empty());
        assert!(
            sink.events().is_empty(),
            "recovery never replays usage telemetry"
        );
    }
}

#[tokio::test]
async fn unknown_source_remains_held_and_public_shutdown_returns_honest_bounded_error() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("original.db");
    let id = seed(&path, false);
    let source = MockServer::start().await;
    let (state, sink) = state(&path, &source.uri());
    let (tx, rx) = tokio::sync::oneshot::channel();
    let serving = state.clone();
    let addr = address();
    let server = tokio::spawn(serve_with_shutdown_timeout(
        serving,
        addr,
        async {
            let _ = rx.await;
        },
        GRACE,
    ));
    bound(addr).await;
    let started = std::time::Instant::now();
    tx.send(()).unwrap();
    let error = tokio::time::timeout(Duration::from_secs(1), server)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert!(is_incomplete_shutdown(&error));
    assert!(error.to_string().contains("durable uncertainty"));
    assert!(started.elapsed() < Duration::from_secs(1));
    assert_ne!(state.lifecycle.phase(), Phase::Stopped);
    let mut ledger = SqliteLedger::open(path.to_str().unwrap()).unwrap();
    assert_eq!(
        ledger
            .recovery_page_durable(CLIENT, None, 10)
            .unwrap()
            .entries[0]
            .state,
        RecoveryState::MayHaveDispatched
    );
    assert!(ledger.terminal_durable(CLIENT, &id).unwrap().is_none());
    for scope in [CLIENT, SOURCE] {
        assert_eq!(ledger.spent_durable(scope).unwrap(), 0);
        assert_eq!(ledger.reserved_durable(scope).unwrap(), 34);
    }
    assert_eq!(receipts(&path), 0);
    assert!(source.received_requests().await.unwrap().is_empty());
    assert!(sink.events().is_empty());
}

#[tokio::test]
async fn cancelling_public_serve_stops_owned_acceptor_and_recovery_without_exiting_host() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("original.db");
    let source = MockServer::start().await;
    let (state, _) = state(&path, &source.uri());
    let addr = address();
    let server = tokio::spawn(serve_with_shutdown_timeout(
        state.clone(),
        addr,
        std::future::pending(),
        GRACE,
    ));
    bound(addr).await;
    server.abort();
    assert!(server.await.unwrap_err().is_cancelled());
    wait_until(|| state.lifecycle.phase() == Phase::Stopped && Arc::strong_count(&state) == 1)
        .await;
    let _rebound =
        std::net::TcpListener::bind(addr).expect("cancelled serving left acceptor alive");
    assert_eq!(state.lifecycle.active_operations(), 0);
    assert!(source.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn bind_failure_starts_no_driver_and_does_not_consume_state_or_grace() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("original.db");
    seed(&path, true);
    let source = MockServer::start().await;
    let (state, _) = state(&path, &source.uri());
    let occupied = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let error = serve_with_shutdown_timeout(
        state.clone(),
        occupied.local_addr().unwrap(),
        async {},
        GRACE,
    )
    .await
    .unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::AddrInUse);
    assert_eq!(state.lifecycle.deadline(), None);
    assert_eq!(state.lifecycle.phase(), Phase::Running);
    assert_eq!(Arc::strong_count(&state), 1);
    assert_eq!(receipts(&path), 0, "failed bind must not start recovery");
    serve_with_shutdown_timeout(state.clone(), address(), async {}, GRACE)
        .await
        .unwrap();
    assert_eq!(
        receipts(&path),
        1,
        "corrected bind uses original ready record"
    );
    assert_eq!(state.lifecycle.phase(), Phase::Stopped);
    assert!(source.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn one_state_cannot_run_duplicate_recovery_supervisors() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("original.db");
    let source = MockServer::start().await;
    let (state, _) = state(&path, &source.uri());
    let addr = address();
    let (tx, rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(serve_with_shutdown_timeout(
        state.clone(),
        addr,
        async {
            let _ = rx.await;
        },
        GRACE,
    ));
    bound(addr).await;
    let error = serve_with_shutdown_timeout(state.clone(), address(), async {}, GRACE)
        .await
        .unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
    assert_eq!(state.lifecycle.phase(), Phase::Running);
    tx.send(()).unwrap();
    server.await.unwrap().unwrap();
    wait_until(|| Arc::strong_count(&state) == 1).await;
}

#[tokio::test]
async fn readonly_router_probes_neither_bind_nor_replace_original_memory_ledger() {
    let state = Arc::new(ProxyState::new(
        KeyStore::new(),
        ProxyLedger::in_memory(),
        Arc::new(InMemorySink::new()),
        HashMap::new(),
        None,
    ));
    let app = build_app(state.clone());
    assert_eq!(
        app.clone()
            .oneshot(Request::get("/healthz").body(Body::empty()).unwrap())
            .await
            .unwrap()
            .status(),
        200
    );
    assert_eq!(
        app.oneshot(Request::get("/readyz").body(Body::empty()).unwrap())
            .await
            .unwrap()
            .status(),
        503
    );
    assert!(
        sandhi_proxy::initialize_execution(&state).is_err(),
        "probe construction must not manufacture another durable ledger"
    );
    assert_eq!(state.lifecycle.deadline(), None);
    assert_eq!(state.lifecycle.phase(), Phase::Running);
    assert_eq!(Arc::strong_count(&state), 1);
    assert_eq!(state.lifecycle.active_operations(), 0);
    let error = serve_with_shutdown_timeout(state.clone(), address(), async {}, GRACE)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("single file-backed ledger"));
    assert_eq!(Arc::strong_count(&state), 1);
}

#[tokio::test]
async fn failed_second_bind_closes_first_socket_without_starting_recovery() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("original.db");
    seed(&path, true);
    let source = MockServer::start().await;
    let (state, _) = state(&path, &source.uri());
    let primary = address();
    let occupied = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let error = serve_with_http_and_tls_shutdown_timeout(
        state.clone(),
        primary,
        occupied.local_addr().unwrap(),
        async {},
        GRACE,
        tls(),
    )
    .await
    .unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::AddrInUse);
    let _primary_rebound = std::net::TcpListener::bind(primary)
        .expect("first dual-scheme socket leaked after second bind failed");
    assert_eq!(state.lifecycle.deadline(), None);
    assert_eq!(state.lifecycle.phase(), Phase::Running);
    assert_eq!(state.lifecycle.active_operations(), 0);
    assert_eq!(Arc::strong_count(&state), 1);
    assert_eq!(receipts(&path), 0);
    assert!(source.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn admitted_work_reports_original_deadline_without_terminating_embedder() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("original.db");
    let source = MockServer::start().await;
    let (state, _) = state(&path, &source.uri());
    let addr = address();
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(serve_with_shutdown_timeout(
        state.clone(),
        addr,
        async {
            let _ = shutdown_rx.await;
        },
        GRACE,
    ));
    bound(addr).await;
    let owned_work = state
        .lifecycle
        .try_operation()
        .expect("authorize admitted cleanup work");
    let started = std::time::Instant::now();
    shutdown_tx.send(()).unwrap();
    let result = tokio::time::timeout(Duration::from_secs(2), server).await;
    // Only the host can release this deliberately retained admitted operation.
    drop(owned_work);
    let error = result
        .expect("library shutdown exceeded original bound")
        .unwrap()
        .unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
    assert!(is_incomplete_shutdown(&error));
    assert!(started.elapsed() < Duration::from_secs(2));
    assert!(
        state
            .lifecycle
            .deadline()
            .unwrap()
            .saturating_duration_since(started)
            <= GRACE + Duration::from_millis(20)
    );
    assert_ne!(state.lifecycle.phase(), Phase::Stopped);
    wait_until(|| state.lifecycle.active_operations() == 0 && Arc::strong_count(&state) == 1).await;
    let _rebound = std::net::TcpListener::bind(addr).expect("timed-out server retained acceptor");
    assert!(source.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn public_driver_recovers_after_bounded_busy_storage_refusal_without_inference_replay() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("original.db");
    seed(&path, true);
    let source = MockServer::start().await;
    let (state, _) = state(&path, &source.uri());
    sandhi_proxy::initialize_execution(&state).unwrap();
    let (locked_tx, locked_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let locked_state = state.clone();
    let holder = std::thread::spawn(move || {
        let _original_ledger = locked_state.ledger.lock().unwrap();
        locked_tx.send(()).unwrap();
        let _ = release_rx.recv_timeout(Duration::from_secs(10));
    });
    locked_rx.await.unwrap();
    let reason = sandhi_proxy::settlement::buffered::recover(state.clone())
        .await
        .unwrap_err();
    assert_eq!(reason, "accounting recovery unresolved");
    let addr = address();
    let (tx, rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(serve_with_shutdown_timeout(
        state.clone(),
        addr,
        async {
            let _ = rx.await;
        },
        GRACE,
    ));
    bound(addr).await;
    assert_eq!(
        receipts(&path),
        0,
        "busy original store cannot be replaced or bypassed"
    );
    assert!(source.received_requests().await.unwrap().is_empty());
    release_tx.send(()).unwrap();
    holder.join().unwrap();
    wait_until(|| receipts(&path) == 1).await;
    tx.send(()).unwrap();
    server.await.unwrap().unwrap();
    assert_eq!(receipts(&path), 1);
    assert!(source.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn exhausted_grace_never_claims_clean_shutdown_or_exits_embedding_process() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("original.db");
    let source = MockServer::start().await;
    let (state, _) = state(&path, &source.uri());
    let error = serve_with_shutdown_timeout(state.clone(), address(), async {}, Duration::ZERO)
        .await
        .unwrap_err();
    assert!(is_incomplete_shutdown(&error));
    assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
    assert_eq!(state.lifecycle.remaining(), Some(Duration::ZERO));
    assert_ne!(state.lifecycle.phase(), Phase::Stopped);
    wait_until(|| Arc::strong_count(&state) == 1).await;
    assert!(source.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn panicking_host_shutdown_closes_authorization_and_reports_incomplete_cleanup() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("original.db");
    let source = MockServer::start().await;
    let (state, _) = state(&path, &source.uri());
    let addr = address();
    let error = serve_with_shutdown_timeout(
        state.clone(),
        addr,
        async {
            panic!("synthetic host shutdown panic");
        },
        GRACE,
    )
    .await
    .unwrap_err();
    assert!(is_incomplete_shutdown(&error));
    assert!(!state.lifecycle.is_running());
    assert!(state.lifecycle.try_operation().is_none());
    wait_until(|| state.lifecycle.active_operations() == 0 && Arc::strong_count(&state) == 1).await;
    let _rebound =
        std::net::TcpListener::bind(addr).expect("interrupted supervisor left acceptor alive");
    assert!(source.received_requests().await.unwrap().is_empty());
}
