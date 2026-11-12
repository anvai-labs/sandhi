//! Cache categories must survive each client dialect, backend profile and delivery mode.
use std::{collections::HashMap, sync::Arc, time::Duration};

use axum::{
    body::{to_bytes, Body},
    http::{Request, StatusCode},
};
use sandhi_core::{InMemorySink, KeyStore, Policy, Sink, UsageEvent, VirtualKey, Window};
use sandhi_providers::ProviderRuntime;
use sandhi_proxy::{build_app, ProxyLedger, ProxyState};
use sandhi_store::{
    diagnostics::{DiagnosticQuery, DiagnosticSelector},
    SqliteStore,
};
use serde_json::{json, Value};
use tower::ServiceExt;
use wiremock::{
    matchers::{header, method, path},
    Mock, MockServer, ResponseTemplate,
};

struct Capture {
    memory: InMemorySink,
    store: SqliteStore,
}

impl Sink for Capture {
    fn emit(&self, event: &UsageEvent) {
        self.store.emit(event);
        self.memory.emit(event);
    }
}

#[derive(Clone, Copy, Debug)]
enum Backend {
    Chat,
    Responses,
    Codex,
}

async fn exercise(backend: Backend, dialect: &str, stream: bool) {
    let upstream = MockServer::start().await;
    let responses = !matches!(backend, Backend::Chat);
    let upstream_stream = stream || matches!(backend, Backend::Codex);
    let usage = if responses {
        json!({"input_tokens":100,"output_tokens":20,"total_tokens":120,
            "input_tokens_details":{"cached_tokens":60,"cache_write_tokens":30},
            "output_tokens_details":{"reasoning_tokens":7}})
    } else {
        json!({"prompt_tokens":100,"completion_tokens":20,"total_tokens":120,
            "prompt_tokens_details":{"cached_tokens":60,"cache_write_tokens":30},
            "completion_tokens_details":{"reasoning_tokens":7}})
    };
    let completed = if responses {
        json!({"id":"resp_cache","object":"response","model":"fixture","status":"completed",
            "output":[{"type":"message","role":"assistant",
                "content":[{"type":"output_text","text":"hello"}]}],"usage":usage})
    } else {
        json!({"id":"chat_cache","object":"chat.completion","created":1,"model":"fixture",
            "choices":[{"index":0,"message":{"role":"assistant","content":"hello"},
                "finish_reason":"stop"}],"usage":usage})
    };
    let template = if upstream_stream {
        let frames = if responses {
            vec![
                json!({"type":"response.created","response":{"id":"resp_cache","model":"fixture"}}),
                json!({"type":"response.output_text.delta","delta":"hello"}),
                json!({"type":"response.completed","response":completed}),
            ]
        } else {
            vec![
                json!({"object":"chat.completion.chunk","choices":[{"index":0,
                    "delta":{"content":"hello"},"finish_reason":null}]}),
                json!({"object":"chat.completion.chunk","choices":[{"index":0,
                    "delta":{},"finish_reason":"stop"}]}),
                json!({"object":"chat.completion.chunk","choices":[],"usage":usage}),
            ]
        };
        let mut sse = frames
            .iter()
            .map(|frame| format!("data: {frame}\n\n"))
            .collect::<String>();
        if !responses {
            sse.push_str("data: [DONE]\n\n");
        }
        ResponseTemplate::new(200)
            .insert_header("content-type", "text/event-stream")
            .set_body_string(sse)
    } else {
        ResponseTemplate::new(200).set_body_json(completed)
    };
    Mock::given(method("POST"))
        .and(path(if responses {
            "/responses"
        } else {
            "/chat/completions"
        }))
        .and(header("authorization", "Bearer synthetic-upstream"))
        .respond_with(template)
        .expect(1)
        .mount(&upstream)
        .await;
    let runtime = ProviderRuntime::new();
    let provider = match backend {
        Backend::Chat => runtime.openai_compat(
            "openai",
            upstream.uri(),
            "synthetic-upstream",
            Default::default(),
            Some(0),
            None,
            None,
            None,
        ),
        Backend::Responses => runtime.openai_responses(
            "openai",
            upstream.uri(),
            "synthetic-upstream",
            Default::default(),
            Some(0),
            None,
            None,
            None,
        ),
        Backend::Codex => runtime.chatgpt_responses(
            "openai",
            upstream.uri(),
            "synthetic-upstream",
            Default::default(),
            Some(0),
            None,
            None,
            None,
        ),
    };
    let keys = KeyStore::new();
    keys.insert(VirtualKey {
        id: "vk_cache".into(),
        group_id: Some("cache".into()),
        upstream_ref: "backend".into(),
        ..Default::default()
    });
    let capture = Arc::new(Capture {
        memory: InMemorySink::new(),
        store: SqliteStore::in_memory().unwrap(),
    });
    let mut ledger = ProxyLedger::in_memory();
    for scope in ["group:cache", "upstream:openai"] {
        ledger
            .set_budget(scope, Some(10000), Window::Total, Policy::Block)
            .unwrap();
    }
    let mut state = ProxyState::new(
        keys,
        ledger,
        capture.clone(),
        HashMap::from([("backend".into(), provider)]),
        None,
    );
    state.cache_read_weight_bp = 1000;
    let state = Arc::new(state);
    let (route, request) = match dialect {
        "chat" => (
            "/v1/chat/completions",
            json!({"model":"fixture","stream":stream,"max_tokens":64,
            "messages":[{"role":"system","content":"Be precise"},{"role":"user","content":"question"}]}),
        ),
        "anthropic" => (
            "/v1/messages",
            json!({"model":"fixture","stream":stream,"max_tokens":64,
            "system":"Be precise","messages":[{"role":"user","content":"question"}]}),
        ),
        "responses" => (
            "/v1/responses",
            json!({"model":"fixture","stream":stream,"max_output_tokens":64,
            "instructions":"Be precise","input":[{"role":"user","content":"question"}]}),
        ),
        "gemini" => (
            if stream {
                "/v1beta/models/fixture:streamGenerateContent?alt=sse"
            } else {
                "/v1beta/models/fixture:generateContent"
            },
            json!({"systemInstruction":{"parts":[{"text":"Be precise"}]},
                "contents":[{"role":"user","parts":[{"text":"question"}]}],
                "generationConfig":{"maxOutputTokens":64}}),
        ),
        _ => unreachable!(),
    };
    let response = build_app(state.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(route)
                .header(
                    match dialect {
                        "anthropic" => "x-api-key",
                        "gemini" => "x-goog-api-key",
                        _ => "authorization",
                    },
                    if matches!(dialect, "anthropic" | "gemini") {
                        "vk_cache"
                    } else {
                        "Bearer vk_cache"
                    },
                )
                .header("content-type", "application/json")
                .header("x-sandhi-run-id", "cache-run")
                .body(Body::from(request.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "{backend:?}/{dialect}/{stream}"
    );
    let bytes = to_bytes(response.into_body(), 100_000).await.unwrap();
    let documents: Vec<Value> = if stream {
        std::str::from_utf8(&bytes)
            .unwrap()
            .lines()
            .filter_map(|line| line.strip_prefix("data: "))
            .filter(|data| *data != "[DONE]")
            .map(|data| serde_json::from_str(data).unwrap())
            .collect()
    } else {
        vec![serde_json::from_slice(&bytes).unwrap()]
    };
    let wire_usage = documents
        .iter()
        .rev()
        .find_map(|document| {
            document
                .get("response")
                .unwrap_or(document)
                .get(if dialect == "gemini" {
                    "usageMetadata"
                } else {
                    "usage"
                })
                .filter(|usage| !usage.is_null())
        })
        .expect("terminal wire usage");
    match dialect {
        "chat" => {
            assert_eq!(wire_usage["prompt_tokens"], 100);
            assert_eq!(wire_usage["prompt_tokens_details"]["cached_tokens"], 60);
            assert_eq!(
                wire_usage["prompt_tokens_details"]["cache_write_tokens"],
                30
            );
            assert_eq!(wire_usage["completion_tokens"], 20);
        }
        "responses" => {
            assert_eq!(wire_usage["input_tokens"], 100);
            assert_eq!(wire_usage["input_tokens_details"]["cached_tokens"], 60);
            assert_eq!(wire_usage["input_tokens_details"]["cache_write_tokens"], 30);
            assert_eq!(wire_usage["output_tokens"], 20);
        }
        "anthropic" => {
            assert_eq!(wire_usage["input_tokens"], 10);
            assert_eq!(wire_usage["cache_read_input_tokens"], 60);
            assert_eq!(wire_usage["cache_creation_input_tokens"], 30);
            assert_eq!(wire_usage["output_tokens"], 20);
        }
        "gemini" => {
            // Gemini has no separate write-token field. Preserve inclusive input,
            // reads and reasoning on its wire; retain writes in neutral evidence.
            assert_eq!(wire_usage["promptTokenCount"], 100);
            assert_eq!(wire_usage["cachedContentTokenCount"], 60);
            assert_eq!(wire_usage["candidatesTokenCount"], 13);
            assert_eq!(wire_usage["thoughtsTokenCount"], 7);
            assert_eq!(wire_usage["totalTokenCount"], 120);
        }
        _ => unreachable!(),
    }
    let received = upstream.received_requests().await.unwrap();
    assert_eq!(received.len(), 1);
    let forwarded: Value = serde_json::from_slice(&received[0].body).unwrap();
    let history = forwarded[if responses { "input" } else { "messages" }]
        .as_array()
        .unwrap();
    assert!(
        history.iter().any(|message| message["role"] == "user"
            && message["content"].to_string().contains("question")),
        "{forwarded}"
    );
    tokio::time::timeout(Duration::from_secs(2), async {
        while capture.memory.events().is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let events = capture.memory.events();
    assert_eq!(events.len(), 1);
    let event = &events[0];
    assert_eq!(
        (
            event.tokens_in,
            event.cache_read_tokens,
            event.cache_creation_tokens,
            event.tokens_out
        ),
        (10, 60, 30, 20)
    );
    assert_eq!(event.reasoning_tokens, Some(7));
    assert_eq!(event.reasoning_included, Some(true));
    assert_eq!(event.billable_tokens(), 120);
    let persisted = capture
        .store
        .diagnostics(&DiagnosticQuery {
            selector: DiagnosticSelector::Run("cache-run".into()),
            limit: 10,
        })
        .unwrap();
    assert_eq!(persisted.returned_rows, 1);
    let row = &persisted.rows[0];
    assert_eq!(row["tokens_in"], 10);
    assert_eq!(row["cache_read_tokens"], 60);
    assert_eq!(row["cache_creation_tokens"], 30);
    for scope in ["group:cache", "upstream:openai"] {
        let ledger = state.ledger.lock().unwrap();
        assert_eq!(ledger.reserved(scope), 0);
        assert_eq!(
            ledger.spent(scope),
            66,
            "only the read category is discounted"
        );
    }
}

#[tokio::test]
async fn cache_writes_survive_all_dialects_and_openai_backend_profiles() {
    for backend in [Backend::Chat, Backend::Responses, Backend::Codex] {
        for dialect in ["chat", "anthropic", "responses", "gemini"] {
            for stream in [false, true] {
                exercise(backend, dialect, stream).await;
            }
        }
    }
}
