//! Protocol completion must settle measured usage before the client can close at DONE.
//! A real TCP fake upstream deliberately keeps its response open until the test releases EOF.
use std::{collections::HashMap, sync::Arc, time::Duration};

use axum::{
    body::{Body, Bytes},
    http::{Request, StatusCode},
    routing::post,
    Router,
};
use futures_util::StreamExt;
use sandhi_core::{
    InMemorySink, KeyStore, Sink, UsageBasis, UsageCompleteness, UsageEvent, VirtualKey,
};
use sandhi_providers::ProviderRuntime;
use sandhi_proxy::{build_app, ProxyLedger, ProxyState};
use sandhi_store::{
    diagnostics::{DiagnosticQuery, DiagnosticSelector},
    SqliteStore,
};
use tokio::sync::{mpsc, Mutex};
use tower::ServiceExt;

struct Capture {
    memory: InMemorySink,
    store: SqliteStore,
}

impl Sink for Capture {
    fn emit(&self, event: &UsageEvent) {
        self.memory.emit(event);
        self.store.emit(event);
    }
}

#[derive(Clone, Copy, Debug)]
enum Ending {
    CloseAtDone,
    DrainEof,
    DropBeforeUsage,
    DropAfterUsage,
    NoUsageClose,
    NoUsageDrain,
}

async fn exercise(slug: &str, newline: &str, width: usize, ending: Ending) -> UsageEvent {
    let (tx, rx) = mpsc::channel::<Bytes>(1);
    let receiver = Arc::new(Mutex::new(Some(rx)));
    let origin = Router::new().route(
        "/v1/chat/completions",
        post(move || {
            let receiver = receiver.clone();
            async move {
                let mut rx = receiver.lock().await.take().expect("one origin call");
                let bytes = async_stream::stream! {
                    while let Some(bytes) = rx.recv().await {
                        yield Ok::<Bytes, std::io::Error>(bytes);
                    }
                };
                (
                    [("content-type", "text/event-stream")],
                    Body::from_stream(bytes),
                )
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, origin).await.unwrap();
    });
    let capture = Arc::new(Capture {
        memory: InMemorySink::new(),
        store: SqliteStore::in_memory().unwrap(),
    });
    let keys = KeyStore::new();
    keys.insert(VirtualKey {
        id: "vk_terminal_fixture".into(),
        upstream_ref: "origin".into(),
        ..Default::default()
    });
    let provider = ProviderRuntime::new().openai_compat(
        slug,
        format!("http://{address}/v1"),
        "fixture-not-a-credential",
        Default::default(),
        Some(0),
        None,
        None,
    );
    let state = Arc::new(ProxyState::new(
        keys,
        ProxyLedger::in_memory(),
        capture.clone(),
        HashMap::from([("origin".into(), provider)]),
        None,
    ));
    let request = Request::builder().method("POST").uri("/v1/chat/completions")
        .header("authorization", "Bearer vk_terminal_fixture").header("content-type", "application/json")
        .header("x-sandhi-session", "terminal-session").header("x-sandhi-run-id", "terminal-run")
        .body(Body::from(r#"{"model":"fixture","messages":[{"role":"user","content":"hello"}],"stream":true,"stream_options":{"include_usage":true}}"#)).unwrap();
    let content = format!("data: {{\"choices\":[{{\"index\":0,\"delta\":{{\"content\":\"hello\"}},\"finish_reason\":null}}],\"usage\":null}}{newline}{newline}");
    let finish = format!("data: {{\"choices\":[{{\"index\":0,\"delta\":{{}},\"finish_reason\":\"stop\"}}],\"usage\":null}}{newline}{newline}");
    let usage = format!("data: {{\"choices\":[],\"usage\":{{\"prompt_tokens\":24,\"completion_tokens\":7,\"total_tokens\":31,\"prompt_tokens_details\":{{\"cached_tokens\":8}},\"duration_ms\":123}}}}{newline}{newline}");
    // Intentionally stop at the newline ending DONE, not its following blank line. SDKs
    // commonly close here; EOF must still be impossible while the sender remains alive.
    let done = format!("data: [DONE]{newline}");
    let wire = match ending {
        Ending::DropBeforeUsage => content,
        Ending::DropAfterUsage => format!("{content}{finish}{usage}"),
        Ending::NoUsageClose | Ending::NoUsageDrain => format!("{content}{finish}{done}"),
        _ => format!("{content}{finish}{usage}{done}"),
    };
    let chunks: Vec<_> = wire
        .as_bytes()
        .chunks(width)
        .map(Bytes::copy_from_slice)
        .collect();
    tx.send(chunks[0].clone()).await.unwrap();
    let response = tokio::time::timeout(
        Duration::from_secs(5),
        build_app(state.clone()).oneshot(request),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let mut body = response.into_body().into_data_stream();
    let mut received = Vec::new();
    let mut expected_len = 0;
    for (index, chunk) in chunks.iter().enumerate() {
        if index != 0 {
            tx.send(chunk.clone()).await.unwrap();
        }
        expected_len += chunk.len();
        while received.len() < expected_len {
            let bytes = tokio::time::timeout(Duration::from_secs(5), body.next())
                .await
                .unwrap()
                .expect("raw bytes before EOF")
                .unwrap();
            received.extend_from_slice(&bytes);
        }
    }
    assert_eq!(
        received,
        wire.as_bytes(),
        "transparent bytes must remain unchanged"
    );
    match ending {
        Ending::DrainEof | Ending::NoUsageDrain => {
            // The same terminal line is already delivered; only now permit transport EOF.
            drop(tx);
            assert!(tokio::time::timeout(Duration::from_secs(5), body.next())
                .await
                .unwrap()
                .is_none());
            drop(body);
        }
        _ => {
            drop(body); // accounting must finish here, without another upstream poll
            assert_eq!(
                capture.memory.len(),
                1,
                "drop emits exactly one logical call"
            );
            drop(tx);
        }
    }
    server.abort();
    let _ = server.await;
    assert_eq!(state.lifecycle.active_operations(), 0);
    let events = capture.memory.events();
    assert_eq!(events.len(), 1, "EOF/drop must not double emit");
    let event = events[0].clone();
    let diagnostics = capture
        .store
        .diagnostics(&DiagnosticQuery {
            selector: DiagnosticSelector::Run("terminal-run".into()),
            limit: 10,
        })
        .unwrap();
    assert_eq!(
        diagnostics.returned_rows, 1,
        "SQLite must persist exactly once"
    );
    let row = &diagnostics.rows[0];
    assert_eq!(row["request_id"], event.request_id);
    assert_eq!(row["tokens_in"], event.tokens_in);
    assert_eq!(row["tokens_out"], event.tokens_out);
    assert_eq!(row["cache_read_tokens"], event.cache_read_tokens);
    assert_eq!(row["session_id"], "terminal-session");
    if matches!(ending, Ending::CloseAtDone | Ending::DrainEof) {
        assert_eq!(
            state.ledger.lock().unwrap().spent("vk:vk_terminal_fixture"),
            31,
            "ledger must settle inclusive prompt plus output exactly once"
        );
        assert_eq!(row["time_to_first_token_source"], "boundary");
        assert_eq!(
            row["time_to_first_token_ms"].as_u64(),
            event.time_to_first_token_ms
        );
    }
    event
}

fn assert_authoritative(event: &UsageEvent) {
    assert_eq!(
        event.usage_completeness,
        UsageCompleteness::Final,
        "terminal wire usage must replace SSE byte estimates: input={}, output={}, cached={}",
        event.tokens_in,
        event.tokens_out,
        event.cache_read_tokens
    );
    assert_eq!(event.usage_basis, UsageBasis::ProviderReported);
    assert_eq!(
        event.tokens_out, 7,
        "SSE envelope bytes are not output tokens"
    );
    assert_eq!(event.tokens_in, 16);
    assert_eq!(event.cache_read_tokens, 8);
    assert_eq!(
        event.tokens_in + event.cache_read_tokens + event.cache_creation_tokens,
        24
    );
    assert_eq!(
        event.cache_read_observation.unwrap().status,
        sandhi_core::CacheReadStatus::Reported
    );
    assert_eq!(event.duration_ms, Some(123));
    assert!(event.time_to_first_token_ms.is_some());
    assert_eq!(
        event.time_to_first_token_source,
        Some(sandhi_core::LatencySource::Boundary)
    );
}

#[tokio::test]
async fn close_immediately_at_done_retains_terminal_usage_before_transport_eof() {
    for slug in ["openai", "inferflux"] {
        for newline in ["\n", "\r\n"] {
            for width in [usize::MAX, 7, 1] {
                let event = exercise(slug, newline, width, Ending::CloseAtDone).await;
                assert_authoritative(&event);
            }
        }
    }
}

#[tokio::test]
async fn draining_transport_eof_retains_the_same_terminal_counts_exactly_once() {
    for slug in ["openai", "inferflux"] {
        for newline in ["\n", "\r\n"] {
            let event = exercise(slug, newline, 7, Ending::DrainEof).await;
            assert_authoritative(&event);
        }
    }
}

#[tokio::test]
async fn genuinely_preterminal_disconnect_remains_partial_estimated() {
    for slug in ["openai", "inferflux"] {
        for ending in [Ending::DropBeforeUsage, Ending::DropAfterUsage] {
            let event = exercise(slug, "\r\n", 7, ending).await;
            assert_eq!(event.usage_completeness, UsageCompleteness::Partial);
            assert_eq!(event.usage_basis, UsageBasis::Estimated);
            assert!(event.tokens_out > 7);
            if matches!(ending, Ending::DropAfterUsage) {
                // A finish_reason and usage object alone are not protocol completion.
                assert_eq!(event.tokens_in, 16);
                assert_eq!(event.cache_read_tokens, 8);
            } else {
                assert_eq!(event.tokens_in, 0);
                assert_eq!(event.cache_read_tokens, 0);
            }
        }
    }
}

#[tokio::test]
async fn done_without_usage_does_not_invent_a_final_zero_measurement() {
    for ending in [Ending::NoUsageClose, Ending::NoUsageDrain] {
        for slug in ["openai", "inferflux"] {
            let event = exercise(slug, "\n", 7, ending).await;
            assert_eq!(event.usage_completeness, UsageCompleteness::Partial);
            assert_eq!(event.usage_basis, UsageBasis::Estimated);
            assert!(event.tokens_out > 0);
            assert_eq!(event.tokens_in, 0);
            assert_eq!(event.cache_read_tokens, 0);
        }
    }
}
