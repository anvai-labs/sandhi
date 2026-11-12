//! Local transport injection preserves production fixed-origin selection rules.
use super::*;
use serde_json::{json, Value};

fn ingress(flavor: &str) -> (&str, Value) {
    match flavor {
        "chat" => (
            "/v1/chat/completions",
            json!({"model":"gpt-4o-mini","max_tokens":64,"messages":[{"role":"system","content":"Write code."},{"role":"user","content":"hi"}]}),
        ),
        "messages" => (
            "/v1/messages",
            json!({"model":"gpt-4o-mini","max_tokens":64,"system":"Write code.","messages":[{"role":"user","content":"hi"}]}),
        ),
        "responses" => (
            "/v1/responses",
            json!({"model":"gpt-4o-mini","max_output_tokens":64,"instructions":"Write code.","input":"hi"}),
        ),
        _ => (
            "/v1beta/models/gpt-4o-mini:generateContent",
            json!({"systemInstruction":{"parts":[{"text":"Write code."}]},"contents":[{"role":"user","parts":[{"text":"hi"}]}],"generationConfig":{"maxOutputTokens":64}}),
        ),
    }
}

#[tokio::test]
async fn tracked_codex_buffered_four_ingress_routes_join_attempt_usage_and_both_receipts() {
    for flavor in ["chat", "messages", "responses", "gemini"] {
        for mode in [
            "final",
            "partial",
            "missing",
            "post_terminal",
            "failed",
            "unavailable",
            "zero",
        ] {
            let upstream = MockServer::start().await;
            let mut wire = format!(
                "data: {}\n\ndata: {}\n\n",
                json!({"type":"response.in_progress","response":{"id":"resp_fixture","usage":{"input_tokens":10,"output_tokens":1,"input_tokens_details":{"cached_tokens":6}}}}),
                json!({"type":"response.output_text.delta","delta":"reply"})
            );
            if matches!(mode, "unavailable" | "zero") {
                wire = format!(
                    "data: {}\n\ndata: {}\n\n",
                    json!({"type":"response.in_progress","response":{"id":"resp_fixture","usage":null}}),
                    json!({"type":"response.output_text.delta","delta":"reply"})
                );
            }
            if mode != "partial" {
                let status = if mode == "failed" {
                    "failed"
                } else {
                    "completed"
                };
                let mut response = json!({"id":"resp_fixture","status":status,"model":"gpt-4o-mini","output":[],"usage":{"input_tokens":10,"output_tokens":4,"input_tokens_details":{"cached_tokens":6,"cache_write_tokens":2},"output_tokens_details":{"reasoning_tokens":0}}});
                response["output"] = json!([{"type":"message","role":"assistant","content":[{"type":"output_text","text":"reply"}]}]);
                if mode == "zero" {
                    response["usage"] = json!({"input_tokens":0,"output_tokens":0,"input_tokens_details":{"cached_tokens":0,"cache_write_tokens":0},"output_tokens_details":{"reasoning_tokens":0}});
                }
                if matches!(mode, "missing" | "unavailable") {
                    response.as_object_mut().unwrap().remove("usage");
                }
                wire.push_str(&format!(
                    "data: {}\n\n",
                    json!({"type":format!("response.{status}"),"response":response})
                ));
            }
            if mode == "post_terminal" {
                wire.push_str("data: {}\n\n");
            }
            Mock::given(method("POST"))
                .and(path("/responses"))
                .respond_with(
                    ResponseTemplate::new(200)
                        .insert_header("x-request-id", "codex-origin")
                        .set_body_raw(wire, "text/event-stream"),
                )
                .expect(1)
                .mount(&upstream)
                .await;
            let directory = tempfile::tempdir().unwrap();
            let database = directory.path().join("owned.db");
            let sink = Arc::new(InMemorySink::new());
            let state = owned_state(
                upstream.uri(),
                &database,
                sink.clone(),
                std::time::Duration::from_secs(2),
            );
            let mut headers = axum::http::HeaderMap::new();
            headers.insert("chatgpt-account-id", "account-fixture".parse().unwrap());
            let provider = ProviderRuntime::new()
                .chatgpt_responses(
                    "openai",
                    upstream.uri(),
                    "synthetic-access",
                    headers,
                    Some(0),
                    None,
                    None,
                    None,
                )
                .with_credential_expiry(4_000_000_000)
                .with_backend_binding(sandhi_providers::BackendBinding {
                    id: "subscription".into(),
                    revision: 1,
                    provider: "openai".into(),
                    credential_ref: "openai:fixture".into(),
                    credential_generation: 1,
                    endpoint_family: sandhi_core::EndpointFamilyV1::OpenaiResponses,
                    endpoint_profile: "chatgpt_codex".into(),
                    models: vec![serde_json::from_value(json!({"id":"gpt-4o-mini"})).unwrap()],
                    cache_policy: None,
                });
            assert!(provider.raw_forwarder().is_none());
            state
                .providers
                .lock()
                .unwrap()
                .insert("up1".into(), provider);
            let (route, body) = ingress(flavor);
            let response = build_app(state.clone())
                .oneshot(owned_request(route, body))
                .await
                .unwrap();
            assert_eq!(
                response.status(),
                if matches!(mode, "final" | "zero") {
                    StatusCode::OK
                } else {
                    StatusCode::BAD_GATEWAY
                },
                "{flavor}/{mode}"
            );
            let request_id = response.headers()["x-sandhi-request-id"]
                .to_str()
                .unwrap()
                .to_owned();
            let body = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap();
            if matches!(mode, "final" | "zero") {
                assert!(
                    String::from_utf8_lossy(&body).contains("reply"),
                    "{flavor}/{mode}: {}",
                    String::from_utf8_lossy(&body)
                );
            }
            let connection = rusqlite::Connection::open(&database).unwrap();
            let (context,attempt,usage):(String,String,String)=connection.query_row(
                "SELECT c.context_json,a.evidence_json,o.usage_json FROM budget_execution_context c JOIN budget_physical_attempt a USING(execution_id) JOIN budget_terminal_observation o USING(execution_id)",[],|row|Ok((row.get(0)?,row.get(1)?,row.get(2)?))).unwrap();
            let context: Value = serde_json::from_str(&context).unwrap();
            let attempt: Value = serde_json::from_str(&attempt).unwrap();
            let usage: Value = serde_json::from_str(&usage).unwrap();
            assert_eq!(context["backend"]["endpoint_profile"], "chatgpt_codex");
            assert_eq!(context["delivery"], "buffered");
            assert_eq!(context["plane"], "translation");
            assert_eq!(attempt["terminal"]["provider_request_id"], "codex-origin");
            assert_eq!(
                attempt["terminal"]["outcome"],
                if mode == "failed" {
                    "provider_rejected"
                } else if matches!(mode, "final" | "zero") {
                    "success"
                } else {
                    "incomplete_stream"
                }
            );
            let final_usage = matches!(mode, "final" | "failed" | "zero");
            assert_eq!(
                usage["completeness"],
                if final_usage {
                    "final"
                } else if mode == "unavailable" {
                    "unavailable"
                } else {
                    "partial"
                }
            );
            assert_eq!(
                usage["cache_read_tokens"],
                if matches!(mode, "unavailable" | "zero") {
                    0
                } else {
                    6
                }
            );
            if mode != "unavailable" {
                assert_eq!(usage["upstream_request_id"], "codex-origin");
            }
            if matches!(mode, "unavailable" | "zero") {
                assert_eq!(usage["tokens_in"], 0);
                assert_eq!(usage["tokens_out"], 0);
                assert_eq!(
                    usage.get("reasoning_tokens").cloned(),
                    if mode == "zero" { Some(json!(0)) } else { None }
                );
            } else if final_usage || mode == "post_terminal" {
                assert_eq!(usage["tokens_in"], 2);
                assert_eq!(usage["cache_creation_tokens"], 2);
                assert_eq!(usage["tokens_out"], 4);
                assert_eq!(usage["reasoning_tokens"], 0);
            } else {
                assert_eq!(usage["tokens_out"], 1);
            }
            let receipts: i64 = connection
                .query_row("SELECT count(*) FROM budget_settlement_outbox", [], |row| {
                    row.get(0)
                })
                .unwrap();
            assert_eq!(receipts, i64::from(final_usage));
            let mut statement = connection
                .prepare("SELECT settled,actual FROM budget_reservation")
                .unwrap();
            let reservations: Vec<(i64, i64)> = statement
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
                .unwrap()
                .map(Result::unwrap)
                .collect();
            assert_eq!(
                reservations,
                vec![
                    if final_usage {
                        (1, if mode == "zero" { 0 } else { 14 })
                    } else {
                        (0, 0)
                    };
                    2
                ]
            );
            assert_eq!(sink.events().len(), 1);
            assert_eq!(sink.events()[0].request_id, request_id);
            assert_eq!(upstream.received_requests().await.unwrap().len(), 1);
        }
    }
}
