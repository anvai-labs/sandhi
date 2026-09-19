//! ADR-0011 admin boundary tests. No live credentials, model calls, or body capture.
use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    task::Poll,
    time::Duration,
};

use axum::{
    body::{to_bytes, Body, Bytes},
    http::{Request, StatusCode},
    response::Response,
};
use sandhi_core::{
    Backend, CacheReadObservation, CacheReadStatus, InMemorySink, KeyStore, LatencySource, Sink,
    UsageEvent, VirtualKey,
};
use sandhi_proxy::{build_app, ProxyLedger, ProxyState};
use sandhi_store::{
    diagnostics::{MAX_REQUEST_BYTES, MAX_RESPONSE_BYTES},
    SqliteStore,
};
use serde_json::{json, Value};
use tower::ServiceExt;

const ADMIN: &str = "admin-credential-canary";
const VIRTUAL: &str = "virtual-credential-canary";
const ROUTE: &str = "/admin/usage/diagnostics";

fn state(
    store: Option<Arc<SqliteStore>>,
    admin: Option<&str>,
    dashboard_public: bool,
) -> Arc<ProxyState> {
    let keys = KeyStore::new();
    keys.insert(VirtualKey {
        id: VIRTUAL.into(),
        upstream_ref: "unused".into(),
        ..Default::default()
    });
    let mut state = ProxyState::new(
        keys,
        ProxyLedger::in_memory(),
        Arc::new(InMemorySink::new()),
        HashMap::new(),
        store,
    );
    state.admin_token = admin.map(str::to_owned);
    state.dashboard_public = dashboard_public;
    Arc::new(state)
}

fn request(token: Option<&str>, body: Body) -> Request<Body> {
    let mut builder = Request::builder()
        .method("POST")
        .uri(ROUTE)
        .header("content-type", "application/json");
    if let Some(token) = token {
        builder = builder.header("authorization", format!("Bearer {token}"));
    }
    builder.body(body).unwrap()
}

fn query(kind: &str, value: &str) -> Value {
    json!({"selector":{"kind":kind,"value":value}})
}

async fn decoded(response: Response, expected: StatusCode) -> (Value, Bytes) {
    assert_eq!(response.status(), expected);
    assert_eq!(response.headers().get("cache-control").unwrap(), "no-store");
    let bytes = to_bytes(response.into_body(), MAX_RESPONSE_BYTES + 1)
        .await
        .unwrap();
    assert!(bytes.len() <= MAX_RESPONSE_BYTES);
    (serde_json::from_slice(&bytes).unwrap(), bytes)
}

async fn call(state: Arc<ProxyState>, body: Value) -> Value {
    let response = build_app(state)
        .oneshot(request(Some(ADMIN), Body::from(body.to_string())))
        .await
        .unwrap();
    decoded(response, StatusCode::OK).await.0
}

fn event(request: &str, session: &str, run: &str, input: u64) -> UsageEvent {
    let mut event = UsageEvent::new(
        request,
        "2026-09-18T01:02:03Z",
        "openai",
        "fixture-model",
        Backend::External,
    )
    .with_tokens(input, 2)
    .with_cache(3, 0);
    event.session_id = Some(session.into());
    event.run_id = Some(run.into());
    event.step_id = Some("step-1".into());
    event.parent_id = Some("parent-1".into());
    event
}

#[tokio::test]
async fn admin_auth_precedes_body_polling_even_with_public_dashboard() {
    for (configured, presented, expected) in [
        (Some(ADMIN), None, StatusCode::UNAUTHORIZED),
        (Some(ADMIN), Some("wrong"), StatusCode::UNAUTHORIZED),
        (Some(ADMIN), Some(VIRTUAL), StatusCode::UNAUTHORIZED),
        (Some(ADMIN), Some("request-id"), StatusCode::UNAUTHORIZED),
        (None, Some(ADMIN), StatusCode::FORBIDDEN),
    ] {
        let polls = Arc::new(AtomicUsize::new(0));
        let body_polls = polls.clone();
        let body = Body::from_stream(futures_util::stream::poll_fn(move |_| {
            body_polls.fetch_add(1, Ordering::SeqCst);
            Poll::<Option<Result<Bytes, std::io::Error>>>::Pending
        }));
        let response = build_app(state(None, configured, true))
            .oneshot(request(presented, body))
            .await
            .unwrap();
        let (_, bytes) = decoded(response, expected).await;
        assert_eq!(
            polls.load(Ordering::SeqCst),
            0,
            "unauthorized bodies must not be decoded"
        );
        let text = std::str::from_utf8(&bytes).unwrap();
        assert!(!text.contains(ADMIN) && !text.contains(VIRTUAL));
    }
}

#[tokio::test]
async fn authenticated_missing_store_is_unavailable_not_empty_success() {
    let response = build_app(state(None, Some(ADMIN), true))
        .oneshot(request(
            Some(ADMIN),
            Body::from("invalid-json-secret-canary"),
        ))
        .await
        .unwrap();
    let (body, bytes) = decoded(response, StatusCode::SERVICE_UNAVAILABLE).await;
    assert!(body.get("error").is_some());
    assert!(!std::str::from_utf8(&bytes)
        .unwrap()
        .contains("secret-canary"));
}

#[tokio::test]
async fn selectors_reject_unknown_duplicate_ambiguous_and_invalid_fields() {
    let store = Arc::new(SqliteStore::in_memory().unwrap());
    let state = state(Some(store), Some(ADMIN), false);
    let invalid = [
        "{}",
        "null",
        "[]",
        "{",
        r#"{"selector":null}"#,
        r#"{"selector":{"kind":"request"}}"#,
        r#"{"selector":{"value":"x"}}"#,
        r#"{"selector":{"kind":"all","value":"x"}}"#,
        r#"{"selector":{"kind":"request","value":17}}"#,
        r#"{"selector":{"kind":"request","value":"x","extra":true}}"#,
        r#"{"selector":{"kind":"request","kind":"session","value":"x"}}"#,
        r#"{"selector":{"kind":"request","value":"x","value":"y"}}"#,
        r#"{"selector":{"kind":"request","value":"x"},"selector":{"kind":"run","value":"y"}}"#,
        r#"{"selector":{"kind":"request","value":"x"},"limit":1,"limit":2}"#,
        r#"{"selector":{"kind":"request","value":"x"},"prompt":"prompt-secret-canary"}"#,
        r#"{"selector":{"kind":"request","value":"x"},"limit":0}"#,
        r#"{"selector":{"kind":"request","value":"x"},"limit":501}"#,
        r#"{"selector":{"kind":"request","value":"x"},"limit":-1}"#,
        r#"{"selector":{"kind":"request","value":"x"},"limit":1.5}"#,
        r#"{"selector":{"kind":"request","value":"x"},"limit":"10"}"#,
        r#"{"selector":{"kind":"request","value":"x"},"limit":null}"#,
        r#"{"selector":{"kind":"request","value":""}}"#,
        r#"{"selector":{"kind":"request","value":"  "}}"#,
        r#"{"selector":{"kind":"request","value":"x\ny"}}"#,
        r#"{"selector":{"kind":"request","value":"x\u0000y"}}"#,
    ];
    for body in invalid {
        let response = build_app(state.clone())
            .oneshot(request(Some(ADMIN), Body::from(body)))
            .await
            .unwrap();
        let (_, bytes) = decoded(response, StatusCode::BAD_REQUEST).await;
        assert!(!std::str::from_utf8(&bytes)
            .unwrap()
            .contains("secret-canary"));
        assert_eq!(state.diagnostics_reader.available_permits(), 1);
    }
}

#[tokio::test]
async fn selector_length_is_utf8_bytes_and_request_body_limit_is_exact() {
    let state = state(
        Some(Arc::new(SqliteStore::in_memory().unwrap())),
        Some(ADMIN),
        false,
    );
    for value in ["a".repeat(256), "é".repeat(128)] {
        assert_eq!(
            call(state.clone(), query("request", &value)).await["returned_rows"],
            0
        );
    }
    for value in ["a".repeat(257), "é".repeat(129), "\u{2003}".into()] {
        let response = build_app(state.clone())
            .oneshot(request(
                Some(ADMIN),
                Body::from(query("request", &value).to_string()),
            ))
            .await
            .unwrap();
        decoded(response, StatusCode::BAD_REQUEST).await;
    }
    let base = query("request", "missing").to_string();
    for (size, expected) in [
        (MAX_REQUEST_BYTES, StatusCode::OK),
        (MAX_REQUEST_BYTES + 1, StatusCode::PAYLOAD_TOO_LARGE),
    ] {
        let body = format!("{base}{}", " ".repeat(size - base.len()));
        let response = build_app(state.clone())
            .oneshot(request(Some(ADMIN), Body::from(body)))
            .await
            .unwrap();
        decoded(response, expected).await;
        assert_eq!(state.diagnostics_reader.available_permits(), 1);
    }
}

#[tokio::test]
async fn exact_selectors_preserve_duplicates_and_use_newest_insertion_order() {
    let store = Arc::new(SqliteStore::in_memory().unwrap());
    let injection = "x' OR 1=1 --";
    for row in [
        event("duplicate", "session-a", "run-a", 1),
        event("duplicate", "session-b", "run-a", 2),
        event("third", "session-a", "run-b", 3),
        event(injection, "other", "other", 4),
    ] {
        store.emit(&row);
    }
    let state = state(Some(store), Some(ADMIN), false);
    for (kind, value, expected) in [
        ("request", "duplicate", vec![2, 1]),
        ("session", "session-a", vec![3, 1]),
        ("run", "run-a", vec![2, 1]),
        ("request", injection, vec![4]),
        ("request", "missing", vec![]),
    ] {
        let result = call(state.clone(), query(kind, value)).await;
        let actual: Vec<_> = result["rows"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| row["tokens_in"].as_u64().unwrap())
            .collect();
        assert_eq!(actual, expected);
        assert_eq!(result["returned_rows"], expected.len());
        assert_eq!(result["truncated"], false);
    }
}

#[tokio::test]
async fn projection_excludes_credentials_attribution_and_unpersisted_evidence() {
    let store = Arc::new(SqliteStore::in_memory().unwrap());
    let mut row = event("persisted-id", "session", "run", 9);
    row.virtual_key_id = Some(VIRTUAL.into());
    row.subject_id = Some("subject-secret-canary".into());
    row.group_id = Some("group-secret-canary".into());
    row.route = Some("https://body-secret-canary.invalid".into());
    row.trace_context = Some("trace-secret-canary".into());
    row.idempotency_key = Some("idempotency-secret-canary".into());
    row.upstream_request_id = Some("upstream-secret-canary".into());
    row.outcome = Some("outcome-secret-canary".into());
    row.attempts = 7;
    row.duration_ms = Some(23);
    row.duration_source = Some(LatencySource::Origin);
    row.time_to_first_token_ms = Some(4);
    row.time_to_first_token_source = Some(LatencySource::Boundary);
    row.cache_read_observation = Some(CacheReadObservation::origin(CacheReadStatus::Reported));
    store.emit(&row);
    let state = state(Some(store), Some(ADMIN), false);
    let result = call(state, query("request", "persisted-id")).await;
    let exported = &result["rows"][0];
    assert_eq!(exported["cache_read_tokens"], 0);
    assert_eq!(exported["cache_read_observation"]["status"], "reported");
    assert_eq!(exported["duration_source"], "origin");
    assert_eq!(exported["time_to_first_token_source"], "boundary");
    for forbidden in [
        "virtual_key_id",
        "subject_id",
        "group_id",
        "route",
        "trace_context",
        "idempotency_key",
        "upstream_request_id",
        "outcome",
        "attempts",
        "usage_completeness",
        "usage_basis",
        "raw_usage",
        "prompt",
        "headers",
        "body",
        "provider_metadata",
    ] {
        assert!(
            exported.get(forbidden).is_none(),
            "forbidden field {forbidden}"
        );
    }
    let encoded = result.to_string();
    assert!(
        !encoded.contains("secret-canary")
            && !encoded.contains(ADMIN)
            && !encoded.contains(VIRTUAL)
    );
    for missing in [
        "raw_origin_usage",
        "completeness",
        "basis",
        "outcome",
        "physical_attempts",
        "request_id_provenance",
        "separate_upstream_request_id",
        "separate_admission_request_id",
    ] {
        assert!(result["unavailable_evidence"]
            .as_array()
            .unwrap()
            .iter()
            .any(|value| value == missing));
    }
    assert!(result["request_id_notice"]
        .as_str()
        .unwrap()
        .contains("provenance is not persisted"));
}

#[tokio::test]
async fn response_limits_are_enforced_on_the_actual_http_json_envelope() {
    let store = Arc::new(SqliteStore::in_memory().unwrap());
    for input in 0..501 {
        let mut row = event(&"\\".repeat(256), &"\"".repeat(256), "wide-run", input);
        row.model = "\"".repeat(256);
        row.provider = "\\".repeat(256);
        row.step_id = Some("\\".repeat(256));
        row.parent_id = Some("\"".repeat(256));
        store.emit(&row);
    }
    let state = state(Some(store), Some(ADMIN), false);
    let result = call(
        state.clone(),
        json!({"selector":{"kind":"run","value":"wide-run"},"limit":1}),
    )
    .await;
    assert_eq!(result["returned_rows"], 1);
    assert_eq!(result["rows"][0]["tokens_in"], 500);
    assert_eq!(result["truncation_reason"], "row_limit");
    let result = call(
        state,
        json!({"selector":{"kind":"run","value":"wide-run"},"limit":500}),
    )
    .await;
    assert_eq!(result["truncated"], true);
    assert_eq!(result["truncation_reason"], "byte_limit");
    assert!(result["returned_rows"].as_u64().unwrap() > 0);
    assert!(result["returned_rows"].as_u64().unwrap() < 500);
    assert_eq!(
        result["returned_rows"],
        result["rows"].as_array().unwrap().len()
    );
}

#[tokio::test]
async fn occupied_admission_fails_before_reading_body_and_recovers() {
    let state = state(
        Some(Arc::new(SqliteStore::in_memory().unwrap())),
        Some(ADMIN),
        false,
    );
    let permit = state
        .diagnostics_reader
        .clone()
        .try_acquire_owned()
        .unwrap();
    let polls = Arc::new(AtomicUsize::new(0));
    let body_polls = polls.clone();
    let body = Body::from_stream(futures_util::stream::poll_fn(move |_| {
        body_polls.fetch_add(1, Ordering::SeqCst);
        Poll::<Option<Result<Bytes, std::io::Error>>>::Pending
    }));
    let response = build_app(state.clone())
        .oneshot(request(Some(ADMIN), body))
        .await
        .unwrap();
    decoded(response, StatusCode::SERVICE_UNAVAILABLE).await;
    assert_eq!(polls.load(Ordering::SeqCst), 0);
    drop(permit);
    assert_eq!(
        call(state, query("request", "missing")).await["returned_rows"],
        0
    );
}

#[tokio::test]
async fn shutdown_cutoff_keeps_auth_first_and_never_polls_new_bodies() {
    for phase in ["quiescing", "draining", "stopped"] {
        let state = state(
            Some(Arc::new(SqliteStore::in_memory().unwrap())),
            Some(ADMIN),
            true,
        );
        state.lifecycle.begin_quiesce(Duration::from_secs(1));
        if phase != "quiescing" {
            state.lifecycle.start_draining();
        }
        if phase == "stopped" {
            state.lifecycle.stop();
        }
        for (token, expected) in [
            (None, StatusCode::UNAUTHORIZED),
            (Some("wrong"), StatusCode::UNAUTHORIZED),
            (Some(ADMIN), StatusCode::SERVICE_UNAVAILABLE),
        ] {
            let polls = Arc::new(AtomicUsize::new(0));
            let body_polls = polls.clone();
            let body = Body::from_stream(futures_util::stream::poll_fn(move |_| {
                body_polls.fetch_add(1, Ordering::SeqCst);
                Poll::<Option<Result<Bytes, std::io::Error>>>::Pending
            }));
            let response = tokio::time::timeout(
                Duration::from_secs(1),
                build_app(state.clone()).oneshot(request(token, body)),
            )
            .await
            .expect("cutoff must reject without waiting for body bytes")
            .unwrap();
            decoded(response, expected).await;
            assert_eq!(polls.load(Ordering::SeqCst), 0, "phase: {phase}");
            assert_eq!(state.lifecycle.active_operations(), 0);
            assert_eq!(state.diagnostics_reader.available_permits(), 1);
        }
    }
}

#[tokio::test]
async fn body_read_error_is_sanitized_and_releases_admission() {
    let state = state(
        Some(Arc::new(SqliteStore::in_memory().unwrap())),
        Some(ADMIN),
        false,
    );
    let body = Body::from_stream(futures_util::stream::once(async {
        Err::<Bytes, _>(std::io::Error::other("transport-secret-canary"))
    }));
    let response = build_app(state.clone())
        .oneshot(request(Some(ADMIN), body))
        .await
        .unwrap();
    let (_, bytes) = decoded(response, StatusCode::PAYLOAD_TOO_LARGE).await;
    assert!(!std::str::from_utf8(&bytes)
        .unwrap()
        .contains("secret-canary"));
    assert_eq!(state.lifecycle.active_operations(), 0);
    assert_eq!(state.diagnostics_reader.available_permits(), 1);
    assert_eq!(
        call(state, query("request", "missing")).await["returned_rows"],
        0
    );
}

#[tokio::test]
async fn cancelling_body_read_releases_admission_before_any_sql_work() {
    let state = state(
        Some(Arc::new(SqliteStore::in_memory().unwrap())),
        Some(ADMIN),
        false,
    );
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let mut started_tx = Some(started_tx);
    let body = Body::from_stream(futures_util::stream::poll_fn(move |_| {
        if let Some(tx) = started_tx.take() {
            let _ = tx.send(());
        }
        Poll::<Option<Result<Bytes, std::io::Error>>>::Pending
    }));
    let task = tokio::spawn(build_app(state.clone()).oneshot(request(Some(ADMIN), body)));
    tokio::time::timeout(Duration::from_secs(1), started_rx)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(state.diagnostics_reader.available_permits(), 0);
    assert_eq!(state.lifecycle.active_operations(), 1);
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    assert_eq!(state.diagnostics_reader.available_permits(), 1);
    assert_eq!(state.lifecycle.active_operations(), 0);
    assert_eq!(
        call(state, query("request", "missing")).await["returned_rows"],
        0
    );
}
