//! Synthetic two-owner native state authorization, with real durable paired execution.
use axum::{
    body::{to_bytes, Body},
    http::{Request, StatusCode},
};
use sandhi_core::{EndpointFamilyV1 as Family, InMemorySink, KeyStore, Policy, VirtualKey, Window};
use sandhi_providers::{BackendBinding, ProviderHandle, ProviderRuntime};
use sandhi_proxy::{
    backend::BackendEndpoint, build_app, build_provider_handle_for_endpoint, ProxyLedger,
    ProxyState,
};
use sandhi_store::{routes::RouteStore, CredentialScheme};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};
static MATRIX_HTTP_CALLS: AtomicUsize = AtomicUsize::new(0);
use tower::ServiceExt;
use wiremock::{matchers::method, Mock, MockServer, ResponseTemplate};

fn frame(v: Value) -> String {
    format!("data: {v}\n\n")
}
fn output(family: Family, who: &str, stream: bool) -> ResponseTemplate {
    output_with_blob(family, who, who, stream)
}
fn output_with_blob(family: Family, who: &str, blob_owner: &str, stream: bool) -> ResponseTemplate {
    let opaque = format!("opaque_{blob_owner}");
    let signature = format!("signature_{blob_owner}");
    let response = match family {
        Family::OpenaiResponses => {
            json!({"id":format!("resp_{who}"),"object":"response","model":"fixture","status":"completed","output":[{"type":"reasoning","id":format!("rs_{who}"),"summary":[],"encrypted_content":opaque},{"type":"message","id":format!("msg_{who}"),"role":"assistant","status":"completed","content":[{"type":"output_text","text":"reply","annotations":[]}]}],"usage":{"input_tokens":5,"output_tokens":2,"total_tokens":7,"input_tokens_details":{"cached_tokens":0,"cache_write_tokens":0}}})
        }
        Family::AnthropicMessages => {
            json!({"id":format!("msg_{who}"),"type":"message","model":"fixture","role":"assistant","content":[{"type":"thinking","thinking":"reason","signature":signature},{"type":"text","text":"reply"}],"stop_reason":"end_turn","stop_sequence":null,"usage":{"input_tokens":5,"output_tokens":2,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}})
        }
        Family::GeminiGenerateContent => {
            json!({"candidates":[{"index":0,"content":{"role":"model","parts":[{"text":"reply","thoughtSignature":signature}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":5,"candidatesTokenCount":2,"cachedContentTokenCount":0,"totalTokenCount":7}})
        }
        _ => {
            json!({"id":format!("chat_{who}"),"object":"chat.completion","model":"fixture","choices":[{"index":0,"finish_reason":"stop","message":{"role":"assistant","content":"reply","audio":{"id":format!("audio_{who}"),"data":"YQ==","expires_at":2000000000,"transcript":"reply"}}}],"usage":{"prompt_tokens":5,"completion_tokens":2,"total_tokens":7,"prompt_tokens_details":{"cached_tokens":0,"cache_write_tokens":0}}})
        }
    };
    if !stream {
        return ResponseTemplate::new(200).set_body_json(response);
    }
    let wire = match family {
        Family::OpenaiResponses => {
            let mut started = response.clone();
            started["output"] = json!([]);
            started["usage"] = Value::Null;
            started["status"] = json!("in_progress");
            frame(json!({"type":"response.created","sequence_number":0,"response":started}))
                + &frame(
                    json!({"type":"response.output_text.delta","sequence_number":1,"output_index":1,"content_index":0,"item_id":format!("msg_{who}"),"delta":"reply"}),
                )
                + &frame(
                    json!({"type":"response.completed","sequence_number":2,"response":response}),
                )
        }
        Family::AnthropicMessages => {
            let mut started = response.clone();
            started["content"] = json!([]);
            started["stop_reason"] = Value::Null;
            started["usage"]["output_tokens"] = json!(0);
            let mut wire = frame(json!({"type":"message_start","message":started}));
            for value in [
                json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}}),
                json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"reason"}}),
                json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"signature_"}}),
                json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":blob_owner}}),
                json!({"type":"content_block_stop","index":0}),
                json!({"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}),
                json!({"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"reply"}}),
                json!({"type":"content_block_stop","index":1}),
                json!({"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":2}}),
                json!({"type":"message_stop"}),
            ] {
                wire += &frame(value)
            }
            wire
        }
        Family::GeminiGenerateContent => frame(response),
        _ => {
            frame(
                json!({"object":"chat.completion.chunk","id":format!("chat_{who}"),"model":"fixture","choices":[{"index":0,"delta":{"role":"assistant","content":"reply","audio":{"id":format!("audio_{who}")}},"finish_reason":null}]}),
            ) + &frame(
                json!({"object":"chat.completion.chunk","id":format!("chat_{who}"),"model":"fixture","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}),
            ) + &frame(
                json!({"object":"chat.completion.chunk","id":format!("chat_{who}"),"model":"fixture","choices":[],"usage":response["usage"]}),
            ) + "data: [DONE]\n\n"
        }
    };
    ResponseTemplate::new(200).set_body_raw(wire, "text/event-stream")
}
fn provider(origin: &str, family: Family, codex: bool) -> ProviderHandle {
    let runtime = ProviderRuntime::new();
    let handle = if codex {
        let mut headers = axum::http::HeaderMap::new();
        headers.insert("chatgpt-account-id", "synthetic-account".parse().unwrap());
        runtime
            .chatgpt_responses(
                "fixture",
                origin,
                "synthetic-access",
                headers,
                Some(0),
                None,
                None,
                None,
            )
            .with_credential_expiry(4_000_000_000)
    } else {
        build_provider_handle_for_endpoint(
            &runtime,
            "fixture",
            Some(origin),
            "synthetic-access",
            CredentialScheme::Bearer,
            None,
            BackendEndpoint::standard(family),
        )
        .unwrap()
    };
    handle.with_backend_binding(BackendBinding {
        id: "shared".into(),
        revision: 1,
        provider: "fixture".into(),
        credential_ref: "fixture:default".into(),
        credential_generation: 1,
        endpoint_family: family,
        endpoint_profile: if codex { "chatgpt_codex" } else { "standard" }.into(),
        models: vec![serde_json::from_value(json!({"id":"fixture"})).unwrap()],
        cache_policy: None,
    })
}
fn uninitialized_state(path: &str, provider: ProviderHandle, logical: bool) -> Arc<ProxyState> {
    let keys = KeyStore::new();
    for who in ["A", "B"] {
        keys.insert(VirtualKey {
            id: format!("tenant_{who}"),
            subject_id: Some(format!("subject_{who}")),
            upstream_ref: if logical {
                "route/agents"
            } else {
                "backend/shared"
            }
            .into(),
            models: Some(vec![if logical { "agent" } else { "fixture" }.into()]),
            ..Default::default()
        });
    }
    let mut ledger = ProxyLedger::durable(path, 1).unwrap();
    for scope in [
        "vk:tenant_A",
        "vk:tenant_B",
        provider.upstream_budget_scope(),
    ] {
        ledger
            .set_budget(scope, Some(100000), Window::Total, Policy::Block)
            .unwrap();
    }
    let mut state = ProxyState::new(
        keys,
        ledger,
        Arc::new(InMemorySink::new()),
        HashMap::from([("backend/shared".into(), provider)]),
        None,
    );
    let routes = RouteStore::open(path).unwrap();
    if routes.get("agents").unwrap().is_none() {
        routes.put(&serde_json::from_value(json!({"id":"agents","models":[{"id":"agent","targets":[{"backend":"shared","model":"fixture"}]}]})).unwrap(),None).unwrap();
    }
    state.routes = Some(Arc::new(routes));
    state.streaming_deadlines = Some(
        serde_json::from_value(
            json!({"ceiling_ms":10000,"default":{"setup_ms":1000,"idle_ms":1000,"body_ms":3000}}),
        )
        .unwrap(),
    );
    Arc::new(state)
}
fn state(path: &str, provider: ProviderHandle, logical: bool) -> Arc<ProxyState> {
    let state = uninitialized_state(path, provider, logical);
    sandhi_proxy::settlement::buffered::enable(
        &state,
        sandhi_proxy::settlement::buffered::Config::new(
            8,
            std::time::Duration::from_secs(2),
            std::time::Duration::from_secs(5),
        )
        .unwrap(),
    )
    .unwrap();
    state
}
fn body(f: Family, who: &str, model: &str, stream: bool, reference: bool) -> Value {
    match f {
        Family::OpenaiResponses => {
            if reference {
                json!({"model":model,"instructions":"Answer.","stream":stream,"max_output_tokens":32,"input":[{"type":"reasoning","id":format!("rs_{who}"),"summary":[],"encrypted_content":format!("opaque_{who}")},{"role":"user","content":format!("tenant_{who}")} ]})
            } else {
                json!({"model":model,"instructions":"Answer.","stream":stream,"max_output_tokens":32,"input":format!("tenant_{who}")})
            }
        }
        Family::AnthropicMessages => {
            let mut messages = vec![];
            if reference {
                messages.push(json!({"role":"assistant","content":[{"type":"thinking","thinking":"reason","signature":format!("signature_{who}")},{"type":"text","text":"reply"}]}))
            };
            messages.push(json!({"role":"user","content":format!("tenant_{who}")}));
            json!({"model":model,"stream":stream,"max_tokens":32,"messages":messages})
        }
        Family::GeminiGenerateContent => {
            let mut contents = vec![];
            if reference {
                contents.push(json!({"role":"model","parts":[{"text":"reply","thoughtSignature":format!("signature_{who}")}]}))
            };
            contents.push(json!({"role":"user","parts":[{"text":format!("tenant_{who}")}]}));
            json!({"contents":contents,"generationConfig":{"maxOutputTokens":32}})
        }
        _ => {
            let mut messages = vec![];
            if reference {
                messages.push(json!({"role":"assistant","content":"reply","audio":{"id":format!("audio_{who}")}}))
            };
            messages.push(json!({"role":"user","content":format!("tenant_{who}")}));
            json!({"model":model,"stream":stream,"max_tokens":32,"messages":messages})
        }
    }
}
async fn call(
    state: &Arc<ProxyState>,
    f: Family,
    who: &str,
    body: Value,
    stream: bool,
    logical: bool,
) -> (StatusCode, Vec<u8>) {
    MATRIX_HTTP_CALLS.fetch_add(1, Ordering::SeqCst);
    call_http(state, f, who, body, stream, logical).await
}
async fn call_http(
    state: &Arc<ProxyState>,
    f: Family,
    who: &str,
    body: Value,
    stream: bool,
    logical: bool,
) -> (StatusCode, Vec<u8>) {
    let path = match f {
        Family::OpenaiResponses => "/v1/responses".into(),
        Family::AnthropicMessages => "/v1/messages".into(),
        Family::GeminiGenerateContent => format!(
            "/v1beta/models/{}:{}{}",
            if logical { "agent" } else { "fixture" },
            if stream {
                "streamGenerateContent"
            } else {
                "generateContent"
            },
            if stream { "?alt=sse" } else { "" }
        ),
        _ => "/v1/chat/completions".into(),
    };
    let response = build_app(state.clone())
        .oneshot(
            Request::post(path)
                .header("authorization", format!("Bearer tenant_{who}"))
                .header("x-api-key", format!("tenant_{who}"))
                .header("x-goog-api-key", format!("tenant_{who}"))
                .header("x-sandhi-session", "same-untrusted-hint")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    (
        response.status(),
        to_bytes(response.into_body(), 2_000_000)
            .await
            .unwrap_or_else(|error| panic!("body {f:?}/{who}/{logical}/{stream}: {error}"))
            .to_vec(),
    )
}
#[tokio::test]
async fn native_issued_state_is_key_bound_before_admission_and_survives_restart() {
    let mut origin_sends = 0usize;
    for (family, codex) in [
        (Family::OpenaiResponses, false),
        (Family::OpenaiResponses, true),
        (Family::AnthropicMessages, false),
        (Family::GeminiGenerateContent, false),
        (Family::OpenaiChatCompletions, false),
    ] {
        for logical in [false, true] {
            for stream in [false, true] {
                let dir = tempfile::tempdir().unwrap();
                let path = dir.path().join("gateway.db");
                let path = path.to_str().unwrap();
                let origin = MockServer::start().await;
                Mock::given(method("POST"))
                    .respond_with(move |request: &wiremock::Request| {
                        let body: Value = request.body_json().unwrap();
                        let who = if body.to_string().contains("tenant_B") {
                            "B"
                        } else {
                            "A"
                        };
                        output(family, who, stream || codex)
                    })
                    .mount(&origin)
                    .await;
                let current = state(path, provider(&origin.uri(), family, codex), logical);
                let model = if logical { "agent" } else { "fixture" };
                for who in ["A", "B"] {
                    let (status, response) = call(
                        &current,
                        family,
                        who,
                        body(family, who, model, stream, false),
                        stream,
                        logical,
                    )
                    .await;
                    assert_eq!(
                        status,
                        StatusCode::OK,
                        "{family:?}/{codex}/{logical}/{stream}: {}",
                        String::from_utf8_lossy(&response)
                    );
                }
                assert_eq!(origin.received_requests().await.unwrap().len(), 2);
                let (status, response) = call(
                    &current,
                    family,
                    "B",
                    body(family, "A", model, stream, true),
                    stream,
                    logical,
                )
                .await;
                assert_eq!(
                    status,
                    StatusCode::FORBIDDEN,
                    "foreign {family:?}/{codex}/{logical}/{stream}: {}",
                    String::from_utf8_lossy(&response)
                );
                assert_eq!(origin.received_requests().await.unwrap().len(), 2);
                assert_eq!(current.ledger.lock().unwrap().spent("vk:tenant_B"), 7);
                assert_eq!(current.ledger.lock().unwrap().reserved("vk:tenant_B"), 0);
                // Provider-side response/item lookup needs issuer proof too; materialized
                // encrypted history is the constrained Codex continuation contract.
                if family == Family::OpenaiResponses && !codex {
                    for reference in [
                        json!({"previous_response_id":"resp_A","input":"tenant_A"}),
                        json!({"input":[{"type":"item_reference","id":"msg_A"}]}),
                    ] {
                        let mut payload = body(family, "A", model, stream, false);
                        for (key, value) in reference.as_object().unwrap() {
                            payload[key] = value.clone();
                        }
                        let (status, _) =
                            call(&current, family, "B", payload.clone(), stream, logical).await;
                        assert_eq!(status, StatusCode::FORBIDDEN);
                        assert_eq!(origin.received_requests().await.unwrap().len(), 2);
                    }
                }
                let mut external = body(family, "A", model, stream, false);
                match family {
                    Family::OpenaiResponses => {
                        external["input"] = json!([{"role":"user","content":[{"type":"input_file","file_id":"externally_created_file"}]}])
                    }
                    Family::GeminiGenerateContent => {
                        external["cachedContent"] = json!("cachedContents/externally_created")
                    }
                    Family::AnthropicMessages => {
                        external["container"] = json!("externally_created_container")
                    }
                    _ => {
                        external["messages"] = json!([{"role":"user","content":[{"type":"file","file":{"file_id":"externally_created_file"}}]}])
                    }
                }
                let (status, _) = call(&current, family, "A", external, stream, logical).await;
                assert!(
                    matches!(status, StatusCode::FORBIDDEN | StatusCode::CONFLICT)
                        || (codex && status == StatusCode::BAD_REQUEST),
                    "external {family:?}/{codex}/{logical}/{stream}: {status}"
                );
                assert_eq!(origin.received_requests().await.unwrap().len(), 2);

                if family == Family::GeminiGenerateContent {
                    for private in [
                        json!({"contents":[{"role":"user","parts":[{"fileData":{"fileUri":"files/externally_created"}}]}]}),
                        json!({"tools":[{"fileSearch":{"fileSearchStoreNames":["fileSearchStores/private"]}}]}),
                    ] {
                        let mut payload = body(family, "A", model, stream, false);
                        for (key, value) in private.as_object().unwrap() {
                            payload[key] = value.clone();
                        }
                        let (status, _) =
                            call(&current, family, "A", payload, stream, logical).await;
                        assert_eq!(status, StatusCode::FORBIDDEN);
                        assert_eq!(origin.received_requests().await.unwrap().len(), 2);
                    }
                }

                let (status, response) = call(
                    &current,
                    family,
                    "A",
                    body(family, "A", model, stream, true),
                    stream,
                    logical,
                )
                .await;
                assert_eq!(
                    status,
                    StatusCode::OK,
                    "own {family:?}/{codex}/{logical}/{stream}: {}",
                    String::from_utf8_lossy(&response)
                );
                drop(current);
                let current = state(path, provider(&origin.uri(), family, codex), logical);
                let (status, response) = call(
                    &current,
                    family,
                    "A",
                    body(family, "A", model, stream, true),
                    stream,
                    logical,
                )
                .await;
                assert_eq!(
                    status,
                    StatusCode::OK,
                    "restart {family:?}/{codex}/{logical}/{stream}: {}",
                    String::from_utf8_lossy(&response)
                );
                assert_eq!(origin.received_requests().await.unwrap().len(), 4);
                assert_eq!(current.ledger.lock().unwrap().spent("vk:tenant_A"), 21);
                assert_eq!(current.ledger.lock().unwrap().reserved("vk:tenant_A"), 0);
                if family == Family::OpenaiResponses && !codex {
                    for reference in [
                        json!({"previous_response_id":"resp_A","input":"tenant_A"}),
                        json!({"input":[{"type":"item_reference","id":"msg_A"}]}),
                    ] {
                        let mut payload = body(family, "A", model, stream, false);
                        for (key, value) in reference.as_object().unwrap() {
                            payload[key] = value.clone();
                        }
                        let (status, response) =
                            call(&current, family, "A", payload, stream, logical).await;
                        assert_eq!(
                            status,
                            StatusCode::OK,
                            "own reference: {}",
                            String::from_utf8_lossy(&response)
                        );
                    }
                    assert_eq!(origin.received_requests().await.unwrap().len(), 6);
                    assert_eq!(current.ledger.lock().unwrap().spent("vk:tenant_A"), 35);
                }
                origin_sends += origin.received_requests().await.unwrap().len();
            }
        }
    }
    assert_eq!(MATRIX_HTTP_CALLS.load(Ordering::SeqCst), 144);
    assert_eq!(origin_sends, 88);
    eprintln!("ownership matrix: HTTP144; origin88; preadmission refusals56; combinations20");
}

#[tokio::test]
async fn proof_storage_failure_does_not_expose_state_or_cancel_source_final_accounting() {
    for (family, codex) in [
        (Family::OpenaiResponses, false),
        (Family::OpenaiResponses, true),
        (Family::AnthropicMessages, false),
        (Family::GeminiGenerateContent, false),
        (Family::OpenaiChatCompletions, false),
    ] {
        for stream in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("gateway.db");
            let path = path.to_str().unwrap();
            let origin = MockServer::start().await;
            Mock::given(method("POST"))
                .respond_with(output(family, "A", stream || codex))
                .expect(1)
                .mount(&origin)
                .await;
            let p = provider(&origin.uri(), family, codex);
            let upstream = p.upstream_budget_scope().to_owned();
            let state = state(path, p, false);
            let conn = rusqlite::Connection::open(path).unwrap();
            conn.execute_batch("CREATE TRIGGER block_issuer_write BEFORE INSERT ON issued_native_references BEGIN SELECT RAISE(ABORT,'synthetic issuance failure'); END").unwrap();
            let route = match family {
                Family::OpenaiResponses => "/v1/responses".into(),
                Family::AnthropicMessages => "/v1/messages".into(),
                Family::GeminiGenerateContent => format!(
                    "/v1beta/models/fixture:{}{}",
                    if stream {
                        "streamGenerateContent"
                    } else {
                        "generateContent"
                    },
                    if stream { "?alt=sse" } else { "" }
                ),
                _ => "/v1/chat/completions".into(),
            };
            let response = build_app(state.clone())
                .oneshot(
                    Request::post(route)
                        .header("authorization", "Bearer tenant_A")
                        .header("x-api-key", "tenant_A")
                        .header("x-goog-api-key", "tenant_A")
                        .header("content-type", "application/json")
                        .body(Body::from(
                            body(family, "A", "fixture", stream, false).to_string(),
                        ))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.headers()["x-should-retry"], "false");
            let status = response.status();
            let result = to_bytes(response.into_body(), 2_000_000).await;
            if stream {
                assert_eq!(status, StatusCode::OK);
                assert!(result.is_err());
            } else {
                assert_eq!(status, StatusCode::BAD_GATEWAY, "{family:?}/{codex}");
                let body: Value = serde_json::from_slice(&result.unwrap()).unwrap();
                if family == Family::GeminiGenerateContent {
                    assert_eq!(body["error"]["code"], 502);
                    assert_eq!(body["error"]["status"], "INTERNAL");
                } else {
                    assert_eq!(body["error"]["code"], "output_projection_error");
                    assert_eq!(body["error"]["retryable"], false);
                }
            }
            assert_eq!(origin.received_requests().await.unwrap().len(), 1);
            for scope in ["vk:tenant_A", &upstream] {
                let ledger = state.ledger.lock().unwrap();
                assert_eq!(ledger.spent(scope), 7, "{family:?}/{codex}/{stream}");
                assert_eq!(ledger.reserved(scope), 0);
            }
            let joins:(i64,i64,i64,i64)=conn.query_row("SELECT (SELECT count(*) FROM budget_execution_intent),(SELECT count(*) FROM budget_settlement_outbox),(SELECT count(*) FROM budget_reservation WHERE settled=1 AND actual=7),(SELECT count(*) FROM issued_native_references)",[],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).unwrap();
            assert_eq!(joins, (1, 1, 2, 0), "{family:?}/{codex}/{stream}");
            let usage: String = conn
                .query_row(
                    "SELECT usage_json FROM budget_terminal_observation",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            let usage: Value = serde_json::from_str(&usage).unwrap();
            assert_eq!(usage["completeness"], "final");
            let attempt: String = conn
                .query_row(
                    "SELECT evidence_json FROM budget_physical_attempt",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            let attempt: Value = serde_json::from_str(&attempt).unwrap();
            assert_eq!(attempt["terminal"]["outcome"], "success");
            assert_eq!(attempt["terminal"]["provider_status"], 200);
            assert_eq!(attempt["attempt_ordinal"], 1);
        }
    }
    eprintln!("proof-write failure matrix: HTTP10; origin10; paired receipts10; settled leases20; new proof rows0");
}

#[tokio::test]
async fn independent_identical_opaque_issuance_grants_each_owner_without_input_echo_adoption() {
    let mut total_http = 0;
    let mut total_origin = 0;
    for (family, codex) in [
        (Family::OpenaiResponses, false),
        (Family::OpenaiResponses, true),
        (Family::AnthropicMessages, false),
        (Family::GeminiGenerateContent, false),
    ] {
        for stream in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("gateway.db");
            let path = path.to_str().unwrap();
            let origin = MockServer::start().await;
            let b_calls = Arc::new(AtomicUsize::new(0));
            let calls = b_calls.clone();
            Mock::given(method("POST")).respond_with(move|request:&wiremock::Request| {
    let body:Value=request.body_json().unwrap();let encoded=body.to_string();let who=if encoded.contains("tenant_B"){"B"}else{"A"};
    let blob=if who=="B" && calls.fetch_add(1,Ordering::SeqCst)==0 {"B"}else{"A"};
    if who=="B" && calls.load(Ordering::SeqCst)==2 {
     assert!(!encoded.contains("opaque_A")&&!encoded.contains("signature_A"),"independent reissuance must use materialized input, not an opaque echo");
    }
    output_with_blob(family,who,blob,stream||codex)
   }).mount(&origin).await;
            let current = state(path, provider(&origin.uri(), family, codex), true);
            for who in ["A", "B"] {
                let (status, response) = call_http(
                    &current,
                    family,
                    who,
                    body(family, who, "agent", stream, false),
                    stream,
                    true,
                )
                .await;
                total_http += 1;
                assert_eq!(
                    status,
                    StatusCode::OK,
                    "seed {family:?}/{codex}/{stream}: {}",
                    String::from_utf8_lossy(&response)
                );
            }
            let mut b_reference = body(family, "B", "agent", stream, true);
            b_reference = serde_json::from_str(
                &b_reference
                    .to_string()
                    .replace("opaque_B", "opaque_A")
                    .replace("signature_B", "signature_A"),
            )
            .unwrap();
            let (status, _) =
                call_http(&current, family, "B", b_reference.clone(), stream, true).await;
            total_http += 1;
            assert_eq!(status, StatusCode::FORBIDDEN);
            assert_eq!(origin.received_requests().await.unwrap().len(), 2);
            let (status, response) = call_http(
                &current,
                family,
                "B",
                body(family, "B", "agent", stream, false),
                stream,
                true,
            )
            .await;
            total_http += 1;
            assert_eq!(
                status,
                StatusCode::OK,
                "reissue {family:?}/{codex}/{stream}: {}",
                String::from_utf8_lossy(&response)
            );
            for (who, payload) in [
                ("A", body(family, "A", "agent", stream, true)),
                ("B", b_reference.clone()),
            ] {
                let (status, response) =
                    call_http(&current, family, who, payload, stream, true).await;
                total_http += 1;
                assert_eq!(
                    status,
                    StatusCode::OK,
                    "owned {family:?}/{codex}/{stream}/{who}: {}",
                    String::from_utf8_lossy(&response)
                );
            }
            drop(current);
            let current = state(path, provider(&origin.uri(), family, codex), true);
            let (status, response) =
                call_http(&current, family, "B", b_reference, stream, true).await;
            total_http += 1;
            assert_eq!(
                status,
                StatusCode::OK,
                "reopen {family:?}/{codex}/{stream}: {}",
                String::from_utf8_lossy(&response)
            );
            let conn = rusqlite::Connection::open(path).unwrap();
            let owners:i64=conn.query_row("SELECT max(n) FROM (SELECT count(DISTINCT owner) n FROM issued_native_references WHERE kind IN(3,6,7) GROUP BY binding,kind,reference_digest)",[],|r|r.get(0)).unwrap();
            assert_eq!(owners, 2);
            assert_eq!(origin.received_requests().await.unwrap().len(), 6);
            total_origin += 6;
        }
    }
    assert_eq!(total_http, 56);
    assert_eq!(total_origin, 48);
    eprintln!("independent opaque reissuance: HTTP56; origin48; foreign-before-own-proof refusals8; combinations8");
}

#[path = "reference_ownership/bootstrap.rs"]
mod bootstrap;
#[path = "reference_ownership/effective_payload.rs"]
mod effective_payload;
