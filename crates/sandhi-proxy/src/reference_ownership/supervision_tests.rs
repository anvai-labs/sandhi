use super::*;
use sandhi_core::{InMemorySink, KeyStore, RequestMetadataV1};
use sandhi_providers::ProviderRuntime;
use serde_json::json;
use std::time::{Duration, Instant};

#[tokio::test]
async fn cancelled_issuance_waiter_cannot_hide_its_blocking_storage_work_from_shutdown() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("original.db");
    let state = Arc::new(ProxyState::new(
        KeyStore::new(),
        crate::ProxyLedger::durable(path.to_str().unwrap(), 1).unwrap(),
        Arc::new(InMemorySink::new()),
        Default::default(),
        None,
    ));
    crate::initialize_execution(&state).unwrap();
    let provider = ProviderRuntime::new().openai_responses(
        "fixture",
        "http://127.0.0.1:1",
        "synthetic",
        Default::default(),
        Some(0),
        None,
        None,
        None,
    );
    let request = crate::codec::decode_request(
        IngressDialect::Responses,
        json!({"model":"fixture","input":"ping","max_output_tokens":8}),
        RequestMetadataV1::default(),
    )
    .unwrap()
    .0;
    let ctx = prepare(
        &state,
        &VirtualKey {
            id: "owner".into(),
            upstream_ref: "fixed".into(),
            ..Default::default()
        },
        &provider,
        &request,
        IngressDialect::Responses,
    )
    .await
    .unwrap()
    .unwrap();
    let slots = ctx.slots.clone();
    let connection = rusqlite::Connection::open(&path).unwrap();
    connection.execute_batch("BEGIN IMMEDIATE").unwrap();
    let refs = response_refs(
        &json!({"object":"response","id":"source-id","output":[]}),
        Family::OpenaiResponses,
    )
    .unwrap();
    let waiter = tokio::spawn(async move { ctx.issue(refs).await });
    let until = Instant::now() + Duration::from_millis(100);
    while slots.available_permits() == 4 && Instant::now() < until {
        tokio::task::yield_now().await;
    }
    assert_eq!(
        slots.available_permits(),
        3,
        "issuance must enter the owned worker"
    );
    waiter.abort();
    let _ = waiter.await;
    let premature_idle =
        tokio::time::timeout(Duration::from_millis(10), state.lifecycle.wait_idle())
            .await
            .is_ok();
    let active = state.lifecycle.active_operations();
    connection.execute_batch("ROLLBACK").unwrap();
    tokio::time::timeout(Duration::from_secs(1), state.lifecycle.wait_idle())
        .await
        .unwrap();
    assert!(
        !premature_idle && active == 1,
        "cancelled proof work was invisible to shutdown: idle={premature_idle}, active={active}"
    );
}

async fn fixture(duration: Duration) -> (tempfile::TempDir, Arc<ProxyState>, Context) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("original.db");
    let state = Arc::new(ProxyState::new(
        KeyStore::new(),
        crate::ProxyLedger::durable(path.to_str().unwrap(), 1).unwrap(),
        Arc::new(InMemorySink::new()),
        Default::default(),
        None,
    ));
    crate::initialize_execution(&state).unwrap();
    let provider = ProviderRuntime::new().openai_responses(
        "fixture",
        "http://127.0.0.1:1",
        "synthetic",
        Default::default(),
        Some(0),
        None,
        None,
        None,
    );
    let request = crate::codec::decode_request(
        IngressDialect::Responses,
        json!({"model":"fixture","input":"ping","max_output_tokens":8}),
        RequestMetadataV1::default(),
    )
    .unwrap()
    .0;
    let mut ctx = prepare(
        &state,
        &VirtualKey {
            id: "owner".into(),
            upstream_ref: "fixed".into(),
            ..Default::default()
        },
        &provider,
        &request,
        IngressDialect::Responses,
    )
    .await
    .unwrap()
    .unwrap();
    ctx.budget.original = Instant::now() + duration;
    ctx.streaming = true;
    (dir, state, ctx)
}
fn references(value: &str) -> Vec<Reference> {
    response_refs(
        &json!({"object":"response","id":value,"output":[]}),
        Family::OpenaiResponses,
    )
    .unwrap()
}
fn native(id: &str) -> (Response, String) {
    let wire = format!(
        "data: {}\n\n",
        json!({"object":"response","id":id,"output":[]})
    );
    let mut response = Response::new(Body::from(wire.clone()));
    response.extensions_mut().insert(NativeEmission(true));
    (response, wire)
}
async fn entered(ctx: &Context) {
    tokio::time::timeout(Duration::from_secs(1), async {
        while ctx.slots.available_permits() == 4 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}
#[tokio::test]
async fn actual_writer_contention_waits_for_identical_proof_without_exposing_a_native_frame() {
    for already_issued in [false, true] {
        let (dir, state, ctx) = fixture(Duration::from_secs(2)).await;
        let refs = references("source-id");
        if already_issued {
            ctx.issue(refs.clone()).await.unwrap();
        }
        let writer = rusqlite::Connection::open(dir.path().join("original.db")).unwrap();
        writer.execute_batch("BEGIN IMMEDIATE").unwrap();
        let (response, wire) = native("source-id");
        let context = ctx.clone();
        let mut delivery = tokio::spawn(async move {
            to_bytes(protect(response, Some(context)).await.into_body(), LIMIT).await
        });
        entered(&ctx).await;
        let early = tokio::time::timeout(Duration::from_millis(150), &mut delivery).await;
        // The public failure occurred at100ms. Keep a real writer beyond that
        // interval, then allow only the original metadata transaction to retry.
        writer.execute_batch("ROLLBACK").unwrap();
        let (premature, result) = match early {
            Ok(result) => (true, result),
            Err(_) => (false, delivery.await),
        };
        assert!(
            !premature,
            "contended proof became visible or refused before its original deadline"
        );
        assert_eq!(result.unwrap().unwrap().as_ref(), wire.as_bytes());
        ctx.store
            .authorize(&ctx.binding, &ctx.owner, &refs)
            .unwrap();
        tokio::time::timeout(Duration::from_secs(1), state.lifecycle.wait_idle())
            .await
            .unwrap();
    }
}
#[tokio::test]
async fn busy_proof_refuses_at_original_deadline_without_any_grant_or_native_bytes() {
    let (dir, state, ctx) = fixture(Duration::from_millis(60)).await;
    let writer = rusqlite::Connection::open(dir.path().join("original.db")).unwrap();
    writer.execute_batch("BEGIN IMMEDIATE").unwrap();
    let (response, _) = native("never-issued");
    let result = tokio::time::timeout(
        Duration::from_secs(1),
        to_bytes(
            protect(response, Some(ctx.clone())).await.into_body(),
            LIMIT,
        ),
    )
    .await
    .unwrap();
    assert!(result.is_err());
    tokio::time::timeout(Duration::from_secs(1), state.lifecycle.wait_idle())
        .await
        .unwrap();
    writer.execute_batch("ROLLBACK").unwrap();
    assert!(ctx
        .store
        .authorize(&ctx.binding, &ctx.owner, &references("never-issued"))
        .is_err());
    assert_eq!(state.lifecycle.active_operations(), 0);
}
#[tokio::test]
async fn each_frame_keeps_one_original_deadline_and_cannot_restart_its_storage_budget() {
    let (dir, state, ctx) = fixture(Duration::from_millis(400)).await;
    let original = ctx.budget.original;
    ctx.issue(references("first")).await.unwrap();
    let writer = rusqlite::Connection::open(dir.path().join("original.db")).unwrap();
    writer.execute_batch("BEGIN IMMEDIATE").unwrap();
    tokio::time::sleep_until((original - Duration::from_millis(30)).into()).await;
    assert!(ctx.issue(references("second")).await.is_err());
    assert_eq!(ctx.budget.original, original);
    writer.execute_batch("ROLLBACK").unwrap();
    tokio::time::timeout(Duration::from_secs(1), state.lifecycle.wait_idle())
        .await
        .unwrap();
    assert!(ctx
        .store
        .authorize(&ctx.binding, &ctx.owner, &references("first"))
        .is_ok());
    assert!(ctx
        .store
        .authorize(&ctx.binding, &ctx.owner, &references("second"))
        .is_err());
}
#[tokio::test]
async fn shutdown_after_job_start_shortens_waiter_and_worker_without_extending_request_deadline() {
    let (dir, state, ctx) = fixture(Duration::from_secs(10)).await;
    let original = ctx.budget.original;
    let writer = rusqlite::Connection::open(dir.path().join("original.db")).unwrap();
    writer.execute_batch("BEGIN IMMEDIATE").unwrap();
    let context = ctx.clone();
    let waiter = tokio::spawn(async move { context.issue(references("cut-off")).await });
    entered(&ctx).await;
    let cutoff = state.lifecycle.begin_quiesce(Duration::from_millis(40));
    assert!(tokio::time::timeout(Duration::from_secs(1), waiter)
        .await
        .unwrap()
        .unwrap()
        .is_err());
    tokio::time::timeout(Duration::from_secs(1), state.lifecycle.wait_idle())
        .await
        .unwrap();
    assert_eq!(ctx.budget.original, original);
    assert_eq!(ctx.budget.deadline(), cutoff);
    writer.execute_batch("ROLLBACK").unwrap();
    assert!(ctx
        .store
        .authorize(&ctx.binding, &ctx.owner, &references("cut-off"))
        .is_err());
}
#[tokio::test]
async fn inert_unpolled_context_is_not_an_operation_and_sealed_completion_cannot_register_late_jobs(
) {
    let (_dir, state, ctx) = fixture(Duration::from_secs(2)).await;
    let mut response = Response::new(Body::from_stream(futures_util::stream::pending::<
        Result<Bytes, std::io::Error>,
    >()));
    response.extensions_mut().insert(NativeEmission(true));
    let held = protect(response, Some(ctx.clone())).await;
    assert_eq!(
        state.lifecycle.active_operations(),
        0,
        "holding an unpolled body is not a proof worker"
    );
    state.lifecycle.begin_quiesce(Duration::from_secs(1));
    state.lifecycle.seal_completion();
    state.lifecycle.wait_idle().await;
    assert!(ctx.issue(references("late")).await.is_err());
    state.lifecycle.stop();
    assert_eq!(state.lifecycle.phase(), crate::lifecycle::Phase::Stopped);
    drop(held);
}

#[tokio::test]
#[allow(clippy::await_holding_lock)] // Deliberately model storage stuck in an uninterruptible mutex wait.
async fn original_deadline_bounds_the_caller_but_held_mutex_work_remains_visible_until_it_exits() {
    let (_dir, state, ctx) = fixture(Duration::from_millis(50)).await;
    let mutex = Arc::new(std::sync::Mutex::new(()));
    let held = mutex.lock().unwrap();
    let worker_mutex = mutex.clone();
    let (tx, rx) = tokio::sync::oneshot::channel();
    let context = ctx.clone();
    let waiter = tokio::spawn(async move {
        context
            .storage(move |_| {
                let _ = tx.send(());
                let _held = worker_mutex.lock().unwrap();
                Ok(())
            })
            .await
    });
    rx.await.unwrap();
    assert!(tokio::time::timeout(Duration::from_secs(1), waiter)
        .await
        .unwrap()
        .unwrap()
        .is_err());
    let premature_idle =
        tokio::time::timeout(Duration::from_millis(10), state.lifecycle.wait_idle())
            .await
            .is_ok();
    let active = state.lifecycle.active_operations();
    drop(held);
    tokio::time::timeout(Duration::from_secs(1), state.lifecycle.wait_idle())
        .await
        .unwrap();
    assert!(
        !premature_idle && active == 1,
        "a timed-out waiter cannot claim its mutex-blocked worker has completed"
    );
}
#[tokio::test]
#[allow(clippy::await_holding_lock)] // Keep actual worker cleanup blocked across the serving deadline.
async fn public_serve_reports_incomplete_shutdown_while_a_cancelled_proof_worker_holds_a_mutex() {
    let (_dir, state, ctx) = fixture(Duration::from_secs(10)).await;
    let mutex = Arc::new(std::sync::Mutex::new(()));
    let held = mutex.lock().unwrap();
    let worker_mutex = mutex.clone();
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let context = ctx.clone();
    let waiter = tokio::spawn(async move {
        context
            .storage(move |_| {
                let _ = entered_tx.send(());
                let _held = worker_mutex.lock().unwrap();
                Ok(())
            })
            .await
    });
    entered_rx.await.unwrap();
    waiter.abort();
    let _ = waiter.await;
    let serving = state.clone();
    let result = tokio::time::timeout(
        Duration::from_secs(1),
        crate::serve_with_shutdown_timeout(
            serving,
            "127.0.0.1:0".parse().unwrap(),
            async {},
            Duration::from_millis(40),
        ),
    )
    .await;
    let active = state.lifecycle.active_operations();
    let phase = state.lifecycle.phase();
    drop(held);
    tokio::time::timeout(Duration::from_secs(1), state.lifecycle.wait_idle())
        .await
        .unwrap();
    let error = result.unwrap().unwrap_err();
    assert!(crate::is_incomplete_shutdown(&error));
    assert_eq!(active, 1);
    assert_ne!(phase, crate::lifecycle::Phase::Stopped);
    assert!(ctx.issue(references("after-seal")).await.is_err());
}
#[tokio::test]
async fn completion_authority_requires_running_admission_and_premature_stop_keeps_its_noop_contract(
) {
    let (_dir, state, ctx) = fixture(Duration::from_secs(1)).await;
    state.lifecycle.stop();
    assert!(state.lifecycle.is_running());
    ctx.issue(references("before-cutoff")).await.unwrap();
    state.lifecycle.begin_quiesce(Duration::from_secs(1));
    assert!(state.lifecycle.completion_authority().is_none());
    // Authority admitted before cutoff permits only metadata cleanup during the
    // existing grace. It cannot authorize another source inference.
    ctx.issue(references("admitted-cleanup")).await.unwrap();
    state.lifecycle.seal_completion();
    assert!(ctx.issue(references("late-cleanup")).await.is_err());
}

#[test]
fn queued_blocking_job_stays_owned_after_its_waiter_times_out() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(1)
        .build()
        .unwrap();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let (held_tx, held_rx) = std::sync::mpsc::channel();
    let occupied = runtime.spawn_blocking(move || {
        held_tx.send(()).unwrap();
        let _ = release_rx.recv_timeout(Duration::from_secs(5));
    });
    held_rx.recv_timeout(Duration::from_secs(1)).unwrap();
    runtime.block_on(async {
        let (_dir, state, ctx) = fixture(Duration::from_millis(40)).await;
        assert!(
            tokio::time::timeout(Duration::from_secs(1), ctx.issue(references("queued")))
                .await
                .unwrap()
                .is_err()
        );
        let active = state.lifecycle.active_operations();
        let premature_idle =
            tokio::time::timeout(Duration::from_millis(10), state.lifecycle.wait_idle())
                .await
                .is_ok();
        release_tx.send(()).unwrap();
        occupied.await.unwrap();
        tokio::time::timeout(Duration::from_secs(1), state.lifecycle.wait_idle())
            .await
            .unwrap();
        assert_eq!(active, 1);
        assert!(!premature_idle);
        assert!(ctx
            .store
            .authorize(&ctx.binding, &ctx.owner, &references("queued"))
            .is_err());
    });
    runtime.shutdown_timeout(Duration::from_secs(1));
}
