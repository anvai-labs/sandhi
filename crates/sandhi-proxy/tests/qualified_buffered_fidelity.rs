//! Source measurements survive buffered codec/projection failures independently.
use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use sandhi_core::{InMemorySink, KeyStore, UsageCompleteness, VirtualKey};
use sandhi_providers::{ProviderHandle, ProviderRuntime};
use sandhi_proxy::{build_app, ProxyLedger, ProxyState};
use serde_json::{json, Value};
use std::{collections::HashMap, sync::Arc, time::Duration};
use tower::ServiceExt;
use wiremock::{matchers::method, Mock, MockServer, ResponseTemplate};

fn reply(family: &str) -> Value {
    match family {
        "chat" => {
            json!({"choices":[],"usage":{"prompt_tokens":8,"completion_tokens":4,"total_tokens":12,
            "prompt_tokens_details":{"cached_tokens":6,"cache_write_tokens":0},"completion_tokens_details":{"reasoning_tokens":1}}})
        }
        "messages" => json!({"content":null,"usage":{"input_tokens":2,"cache_read_input_tokens":6,
            "cache_creation_input_tokens":0,"output_tokens":4,"output_tokens_details":{"thinking_tokens":1}}}),
        "responses" => {
            json!({"status":"completed","output":null,"usage":{"input_tokens":8,"output_tokens":4,"total_tokens":12,
            "input_tokens_details":{"cached_tokens":6,"cache_write_tokens":0},"output_tokens_details":{"reasoning_tokens":1}}})
        }
        _ => {
            json!({"candidates":[],"usageMetadata":{"promptTokenCount":8,"cachedContentTokenCount":6,
            "candidatesTokenCount":4,"thoughtsTokenCount":1,"totalTokenCount":13}})
        }
    }
}
fn provider(family: &str, uri: String) -> ProviderHandle {
    let runtime = ProviderRuntime::new();
    match family {
        "chat" => runtime.openai_compat(
            "models",
            uri,
            "synthetic",
            Default::default(),
            Some(0),
            None,
            None,
            None,
        ),
        "messages" => runtime.anthropic(
            uri,
            "synthetic",
            Default::default(),
            Default::default(),
            Some(0),
            None,
            None,
            None,
        ),
        "responses" => runtime.openai_responses(
            "models",
            uri,
            "synthetic",
            Default::default(),
            Some(0),
            None,
            None,
            None,
        ),
        _ => runtime.gemini(
            uri,
            "synthetic",
            Default::default(),
            Default::default(),
            Some(0),
            None,
            None,
            None,
        ),
    }
}

fn state(
    family: &str,
    uri: String,
    database: &std::path::Path,
    sink: Arc<InMemorySink>,
) -> Arc<ProxyState> {
    let keys = KeyStore::new();
    keys.insert(VirtualKey {
        id: "vk_fixture".into(),
        group_id: Some("fixture".into()),
        upstream_ref: "source".into(),
        ..Default::default()
    });
    let state = Arc::new(ProxyState::new(
        keys,
        ProxyLedger::Durable(
            sandhi_store::ShardedLedger::open_sharded(database.to_str().unwrap(), 1).unwrap(),
        ),
        sink,
        HashMap::from([("source".into(), provider(family, uri))]),
        None,
    ));
    sandhi_proxy::settlement::buffered::enable(
        &state,
        sandhi_proxy::settlement::buffered::Config::new(
            4,
            Duration::from_secs(2),
            Duration::from_secs(2),
        )
        .unwrap(),
    )
    .unwrap();
    state
}

fn ingress(flavor: &str) -> (&str, Value) {
    match flavor {
        "chat" => (
            "/v1/chat/completions",
            json!({"model":"fixture","max_tokens":64,"messages":[{"role":"user","content":"hello"}]}),
        ),
        "messages" => (
            "/v1/messages",
            json!({"model":"fixture","max_tokens":64,"messages":[{"role":"user","content":"hello"}]}),
        ),
        "responses" => (
            "/v1/responses",
            json!({"model":"fixture","max_output_tokens":64,"input":"hello"}),
        ),
        _ => (
            "/v1beta/models/fixture:generateContent",
            json!({"contents":[{"role":"user","parts":[{"text":"hello"}]}],"generationConfig":{"maxOutputTokens":64}}),
        ),
    }
}

fn output(family: &str, scenario: &str) -> Value {
    let mut body = reply(family);
    match family {
        "chat" => {
            body["id"] = "buffered-fixture".into();
            body["model"] = "fixture".into();
            body["choices"] = json!([{"index":0,"message":{
            "role":"assistant","content":"reply","reasoning_content":"materialized"},"finish_reason":"stop"}]);
        }
        "messages" => {
            body["id"] = "buffered-fixture".into();
            body["model"] = "fixture".into();
            body["type"] = "message".into();
            body["role"] = "assistant".into();
            body["stop_reason"] = "end_turn".into();
            body["content"] = json!([
                {"type":"thinking","thinking":"materialized","signature":""},{"type":"text","text":"reply"}]);
        }
        "responses" => {
            body["id"] = "buffered-fixture".into();
            body["model"] = "fixture".into();
            body["output"] = json!([
            {"type":"reasoning","summary":[{"type":"summary_text","text":"materialized"}]},
            {"type":"message","role":"assistant","content":[{"type":"output_text","text":"reply","annotations":[]}]}]);
        }
        _ => {
            body["responseId"] = "buffered-fixture".into();
            body["modelVersion"] = "fixture".into();
            body["candidates"] = json!([{
            "index":0,"content":{"role":"model","parts":[{"text":"materialized","thought":true},{"text":"reply"}]},"finishReason":"STOP"}]);
        }
    }
    if scenario == "opaque" {
        match family {
            "chat" => {
                body["choices"][0]["message"]["audio"] = json!({"id":"audio-fixture","data":"opaque-state","transcript":"sample","expires_at":4_000_000_000_u64})
            }
            "messages" => body["content"][0]["signature"] = "opaque-state".into(),
            "responses" => body["output"][0]["encrypted_content"] = "opaque-state".into(),
            _ => {
                body["candidates"][0]["content"]["parts"][0]["thoughtSignature"] =
                    "opaque-state".into()
            }
        }
    }
    if scenario == "bad_tools" {
        match family {
            "chat" => {
                body["choices"][0]["message"]["tool_calls"] = json!([{"id":"call","type":"function","function":{"name":"f","arguments":"[1]"}}]);
                body["choices"][0]["finish_reason"] = "tool_calls".into();
            }
            "messages" => {
                body["content"]
                    .as_array_mut()
                    .unwrap()
                    .push(json!({"type":"tool_use","id":"call","name":"f","input":[1]}));
                body["stop_reason"] = "tool_use".into();
            }
            "responses" => body["output"].as_array_mut().unwrap().push(
                json!({"type":"function_call","call_id":"call","name":"f","arguments":"[1]"}),
            ),
            _ => body["candidates"][0]["content"]["parts"]
                .as_array_mut()
                .unwrap()
                .push(json!({"functionCall":{"name":"f","args":[1]}})),
        }
    }
    if scenario == "unknown" {
        body["future_state"] = json!({"opaque":"opaque-state"});
    }
    if scenario == "zero_usage" {
        let usage_key = if family == "gemini" {
            "usageMetadata"
        } else {
            "usage"
        };
        fn zero(value: &mut Value) {
            if let Some(fields) = value.as_object_mut() {
                for value in fields.values_mut() {
                    zero(value);
                }
            } else if value.is_number() {
                *value = json!(0);
            }
        }
        zero(&mut body[usage_key]);
    }
    if scenario == "malformed_usage" {
        match family {
            "chat" => body["usage"]["completion_tokens"] = "invalid".into(),
            "gemini" => body["usageMetadata"]["candidatesTokenCount"] = "invalid".into(),
            _ => body["usage"]["output_tokens"] = "invalid".into(),
        }
    }
    if scenario == "missing_usage" {
        body.as_object_mut().unwrap().remove(if family == "gemini" {
            "usageMetadata"
        } else {
            "usage"
        });
    }
    body
}

#[tokio::test]
async fn buffered_native_state_reasoning_and_tool_refusals_join_source_usage_and_both_leases() {
    for family in ["chat", "messages", "responses", "gemini"] {
        for scenario in [
            "portable",
            "opaque",
            "bad_tools",
            "unknown",
            "missing_usage",
            "zero_usage",
            "malformed_usage",
        ] {
            for flavor in ["chat", "messages", "responses", "gemini"] {
                let upstream = MockServer::start().await;
                let original = output(family, scenario);
                Mock::given(method("POST"))
                    .respond_with(
                        ResponseTemplate::new(200)
                            .insert_header("x-request-id", "source-buffered-fixture")
                            .set_body_json(original.clone()),
                    )
                    .expect(1)
                    .mount(&upstream)
                    .await;
                let directory = tempfile::tempdir().unwrap();
                let database = directory.path().join("ledger.db");
                let sink = Arc::new(InMemorySink::new());
                let state = state(family, upstream.uri(), &database, sink.clone());
                let (route, body) = ingress(flavor);
                let response = build_app(state.clone())
                    .oneshot(
                        Request::post(route)
                            .header("authorization", "Bearer vk_fixture")
                            .header("content-type", "application/json")
                            .body(Body::from(body.to_string()))
                            .unwrap(),
                    )
                    .await
                    .unwrap();
                let qualified = !matches!(scenario, "missing_usage" | "malformed_usage");
                let succeeds = (family == flavor || matches!(scenario, "portable" | "zero_usage"))
                    && qualified;
                assert_eq!(
                    response.status(),
                    if succeeds {
                        StatusCode::OK
                    } else {
                        StatusCode::BAD_GATEWAY
                    },
                    "{family}/{flavor}/{scenario}"
                );
                let bytes = axum::body::to_bytes(response.into_body(), 100_000)
                    .await
                    .unwrap();
                let rendered: Value = serde_json::from_slice(&bytes).unwrap();
                if succeeds && family == flavor {
                    assert_eq!(rendered, original, "native envelope changed");
                }
                if succeeds && family != flavor {
                    let reasoning = match flavor {
                        "chat" => &rendered["choices"][0]["message"]["reasoning_content"],
                        "messages" => &rendered["content"][0]["thinking"],
                        "responses" => &rendered["output"][0]["summary"][0]["text"],
                        _ => &rendered["candidates"][0]["content"]["parts"][0]["text"],
                    };
                    assert_eq!(reasoning, "materialized", "{family}/{flavor}");
                    assert!(String::from_utf8_lossy(&bytes).contains("reply"));
                }
                assert_eq!(upstream.received_requests().await.unwrap().len(), 1);
                let events = sink.events();
                assert_eq!(events.len(), 1);
                assert_eq!(
                    events[0].usage_completeness,
                    if qualified {
                        UsageCompleteness::Final
                    } else {
                        UsageCompleteness::Unavailable
                    }
                );
                let connection = rusqlite::Connection::open(&database).unwrap();
                let receipts: Vec<i64> = connection
                    .prepare("SELECT charged_tokens FROM budget_settlement_outbox")
                    .unwrap()
                    .query_map([], |row| row.get(0))
                    .unwrap()
                    .map(Result::unwrap)
                    .collect();
                let charge = if scenario == "zero_usage" {
                    0
                } else if family == "gemini" {
                    13
                } else {
                    12
                };
                assert_eq!(
                    receipts,
                    if qualified { vec![charge] } else { vec![] },
                    "{family}/{flavor}/{scenario}"
                );
                assert_eq!(connection.query_row("SELECT count(*) FROM budget_reservation WHERE settled=?1 AND actual=?2",
                    if qualified {[1,charge]} else {[0,0]},|row|row.get::<_,i64>(0)).unwrap(),2);
            }
        }
    }
}

#[tokio::test]
async fn buffered_filter_terminals_never_become_successful_completion() {
    for (family, reason) in [
        ("chat", "content_filter"),
        ("responses", "content_filter"),
        ("gemini", "SAFETY"),
        ("gemini", "RECITATION"),
        ("gemini", "BLOCKLIST"),
        ("gemini", "PROHIBITED_CONTENT"),
        ("gemini", "SPII"),
        ("gemini", "MODEL_ARMOR"),
    ] {
        for flavor in ["chat", "messages", "responses", "gemini"] {
            let mut original = output(family, "portable");
            match family {
                "chat" => original["choices"][0]["finish_reason"] = reason.into(),
                "responses" => {
                    original["status"] = "incomplete".into();
                    original["incomplete_details"] = json!({"reason":reason});
                }
                _ => original["candidates"][0]["finishReason"] = reason.into(),
            }
            let upstream = MockServer::start().await;
            Mock::given(method("POST"))
                .respond_with(ResponseTemplate::new(200).set_body_json(original.clone()))
                .expect(1)
                .mount(&upstream)
                .await;
            let directory = tempfile::tempdir().unwrap();
            let database = directory.path().join("ledger.db");
            let sink = Arc::new(InMemorySink::new());
            let state = state(family, upstream.uri(), &database, sink.clone());
            let (route, request) = ingress(flavor);
            let response = build_app(state)
                .oneshot(
                    Request::post(route)
                        .header("authorization", "Bearer vk_fixture")
                        .header("content-type", "application/json")
                        .body(Body::from(request.to_string()))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(
                response.status(),
                if flavor == "messages" {
                    StatusCode::BAD_GATEWAY
                } else {
                    StatusCode::OK
                },
                "{family}/{reason}/{flavor}"
            );
            let body: Value = serde_json::from_slice(
                &axum::body::to_bytes(response.into_body(), 100_000)
                    .await
                    .unwrap(),
            )
            .unwrap();
            if family == flavor {
                assert_eq!(body, original);
            }
            if flavor == "chat" {
                assert_eq!(body["choices"][0]["finish_reason"], "content_filter");
            }
            if flavor == "responses" {
                assert_eq!(body["status"], "incomplete", "{family}/{reason}");
                assert_eq!(body["incomplete_details"]["reason"], "content_filter");
            }
            if flavor == "gemini" && family != flavor {
                assert_eq!(body["candidates"][0]["finishReason"], "SAFETY");
            }
            assert_eq!(upstream.received_requests().await.unwrap().len(), 1);
            let events = sink.events();
            assert_eq!(events.len(), 1);
            assert_eq!(events[0].usage_completeness, UsageCompleteness::Final);
            let charge = if family == "gemini" { 13 } else { 12 };
            let connection = rusqlite::Connection::open(database).unwrap();
            assert_eq!(
                connection
                    .query_row(
                        "SELECT count(*) FROM budget_settlement_outbox WHERE charged_tokens=?1",
                        [charge],
                        |row| row.get::<_, i64>(0)
                    )
                    .unwrap(),
                1
            );
            assert_eq!(
                connection
                    .query_row(
                        "SELECT count(*) FROM budget_reservation WHERE settled=1 AND actual=?1",
                        [charge],
                        |row| row.get::<_, i64>(0)
                    )
                    .unwrap(),
                2
            );
        }
    }
}

#[tokio::test]
async fn malformed_buffered_output_keeps_final_source_usage_and_one_paired_receipt() {
    for family in ["chat", "messages", "responses", "gemini"] {
        let upstream = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("x-request-id", "source-buffered-fixture")
                    .set_body_json(reply(family)),
            )
            .expect(1)
            .mount(&upstream)
            .await;
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("ledger.db");
        let sink = Arc::new(InMemorySink::new());
        let keys = KeyStore::new();
        keys.insert(VirtualKey {
            id: "vk_fixture".into(),
            group_id: Some("fixture".into()),
            upstream_ref: "source".into(),
            ..Default::default()
        });
        let state = Arc::new(ProxyState::new(
            keys,
            ProxyLedger::Durable(
                sandhi_store::ShardedLedger::open_sharded(database.to_str().unwrap(), 1).unwrap(),
            ),
            sink.clone(),
            HashMap::from([("source".into(), provider(family, upstream.uri()))]),
            None,
        ));
        sandhi_proxy::settlement::buffered::enable(
            &state,
            sandhi_proxy::settlement::buffered::Config::new(
                4,
                Duration::from_secs(2),
                Duration::from_secs(2),
            )
            .unwrap(),
        )
        .unwrap();
        // Responses ingress forces typed Chat execution as well; the other
        // families translate to Chat. No transparent path can hide codec loss.
        let (route, body) = if family == "chat" {
            (
                "/v1/responses",
                json!({"model":"fixture","input":"hello","max_output_tokens":64}),
            )
        } else {
            (
                "/v1/chat/completions",
                json!({"model":"fixture","messages":[{"role":"user","content":"hello"}],"max_tokens":64}),
            )
        };
        let response = build_app(state.clone())
            .oneshot(
                Request::post(route)
                    .header("authorization", "Bearer vk_fixture")
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY, "{family}");
        assert_eq!(response.headers()["x-should-retry"], "false");
        assert_eq!(upstream.received_requests().await.unwrap().len(), 1);
        let events = sink.events();
        assert_eq!(events.len(), 1);
        assert_eq!(
            events[0].usage_completeness,
            UsageCompleteness::Final,
            "{family}"
        );
        assert_eq!(
            (
                events[0].tokens_in,
                events[0].cache_read_tokens,
                events[0].tokens_out
            ),
            (2, 6, 4)
        );
        assert_eq!(events[0].reasoning_tokens, Some(1));
        assert_eq!(events[0].outcome.as_deref(), Some("error"));
        let charge = if family == "gemini" { 13 } else { 12 };
        let connection = rusqlite::Connection::open(database).unwrap();
        assert_eq!(
            connection
                .query_row(
                    "SELECT count(*) FROM budget_settlement_outbox WHERE charged_tokens=?1",
                    [charge],
                    |row| row.get::<_, i64>(0)
                )
                .unwrap(),
            1,
            "{family}"
        );
        assert_eq!(
            connection
                .query_row(
                    "SELECT count(*) FROM budget_reservation WHERE settled=1 AND actual=?1",
                    [charge],
                    |row| row.get::<_, i64>(0)
                )
                .unwrap(),
            2,
            "{family}"
        );
    }
}
