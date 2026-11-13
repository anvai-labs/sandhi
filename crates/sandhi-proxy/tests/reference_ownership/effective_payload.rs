use super::*;
fn plain(f: Family, who: &str, stream: bool, container: bool) -> ResponseTemplate {
    let mut body = match f {
        Family::OpenaiResponses => {
            json!({"object":"response","id":format!("resp_{who}"),"model":"fixture","status":"completed","output":[{"id":format!("msg_{who}"),"type":"message","role":"assistant","status":"completed","content":[{"type":"output_text","text":"reply","annotations":[]}]}],"usage":{"input_tokens":5,"output_tokens":2,"total_tokens":7,"input_tokens_details":{"cached_tokens":0,"cache_write_tokens":0}}})
        }
        Family::AnthropicMessages => {
            json!({"id":format!("msg_{who}"),"type":"message","role":"assistant","model":"fixture","content":[{"type":"text","text":"reply"}],"stop_reason":"end_turn","stop_sequence":null,"usage":{"input_tokens":5,"output_tokens":2,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}})
        }
        Family::GeminiGenerateContent => {
            json!({"candidates":[{"index":0,"content":{"role":"model","parts":[{"text":"reply"}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":5,"candidatesTokenCount":2,"cachedContentTokenCount":0,"totalTokenCount":7}})
        }
        _ => {
            json!({"object":"chat.completion","id":format!("chat_{who}"),"model":"fixture","choices":[{"index":0,"message":{"role":"assistant","content":"reply"},"finish_reason":"stop"}],"usage":{"prompt_tokens":5,"completion_tokens":2,"total_tokens":7,"prompt_tokens_details":{"cached_tokens":0,"cache_write_tokens":0}}})
        }
    };
    if container {
        body["container"] =
            json!({"id":format!("container_{who}"),"expires_at":"2030-01-01T00:00:00Z"});
    }
    if !stream {
        return ResponseTemplate::new(200).set_body_json(body);
    }
    let wire = match f {
        Family::OpenaiResponses => {
            let mut start = body.clone();
            start["output"] = json!([]);
            start["usage"] = Value::Null;
            start["status"] = json!("in_progress");
            frame(json!({"type":"response.created","response":start}))
                + &frame(
                    json!({"type":"response.output_text.delta","output_index":0,"item_id":format!("msg_{who}"),"content_index":0,"delta":"reply"}),
                )
                + &frame(json!({"type":"response.completed","response":body}))
        }
        Family::AnthropicMessages => {
            let mut start = body.clone();
            start["content"] = json!([]);
            start["stop_reason"] = Value::Null;
            start["usage"]["output_tokens"] = json!(0);
            let mut wire = frame(json!({"type":"message_start","message":start}));
            for v in [
                json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
                json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"reply"}}),
                json!({"type":"content_block_stop","index":0}),
                json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":2}}),
                json!({"type":"message_stop"}),
            ] {
                wire += &frame(v)
            }
            wire
        }
        Family::GeminiGenerateContent => frame(body),
        _ => {
            frame(
                json!({"object":"chat.completion.chunk","id":format!("chat_{who}"),"model":"fixture","choices":[{"index":0,"delta":{"content":"reply"},"finish_reason":null}]}),
            ) + &frame(
                json!({"object":"chat.completion.chunk","id":format!("chat_{who}"),"model":"fixture","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}),
            ) + &frame(
                json!({"object":"chat.completion.chunk","id":format!("chat_{who}"),"model":"fixture","choices":[],"usage":body["usage"]}),
            ) + "data: [DONE]\n\n"
        }
    };
    ResponseTemplate::new(200).set_body_raw(wire, "text/event-stream")
}
fn chat_probe(who: &str, stream: bool, omitted: bool, bucket: &str, extra: Value) -> Value {
    let mut body = json!({"model":"agent","messages":[{"role":"user","content":format!("tenant_{who} cross_probe admitted original input")}],"stream":stream});
    let mut extensions = serde_json::Map::new();
    extensions.insert(bucket.into(), extra);
    body["sandhi_extensions"] = Value::Object(extensions);
    if !omitted {
        body["max_tokens"] = json!(123);
    }
    body
}
#[tokio::test]
async fn unbound_account_only_codex_without_a_lease_never_admits_http_issuance() {
    for token in ["old_synthetic_token", "new_synthetic_token"] {
        for stream in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("gateway.db");
            let origin = MockServer::start().await;
            Mock::given(method("POST"))
                .respond_with(ResponseTemplate::new(500))
                .expect(0)
                .mount(&origin)
                .await;
            let mut headers = axum::http::HeaderMap::new();
            headers.insert("chatgpt-account-id", "synthetic-account".parse().unwrap());
            let p = ProviderRuntime::new().chatgpt_responses(
                "fixture",
                origin.uri(),
                token,
                headers,
                Some(0),
                None,
                None,
                None,
            );
            assert!(p.backend_binding().is_none());
            assert!(p.credential_is_current());
            assert!(!p.reference_credential_is_current());
            let state = state(path.to_str().unwrap(), p, false);
            let (status, _) = call_http(
                &state,
                Family::OpenaiResponses,
                "A",
                body(Family::OpenaiResponses, "A", "fixture", stream, false),
                stream,
                false,
            )
            .await;
            // Provider-credential rejection retains the central gateway error
            // mapping; it is distinct from virtual-key authentication failure.
            assert_eq!(status, StatusCode::BAD_GATEWAY);
            assert!(origin.received_requests().await.unwrap().is_empty());
            let conn = rusqlite::Connection::open(path).unwrap();
            let counts:(i64,i64)=conn.query_row("SELECT (SELECT count(*) FROM issued_native_references),(SELECT count(*) FROM budget_reservation)", [], |r|Ok((r.get(0)?,r.get(1)?))).unwrap();
            assert_eq!(counts, (0, 0));
        }
    }
}
#[tokio::test]
async fn selected_extra_state_cannot_establish_a_new_logical_session() {
    for stream in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("gateway.db");
        let origin = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(plain(Family::OpenaiResponses, "A", stream, false))
            .mount(&origin)
            .await;
        let state = state(
            path.to_str().unwrap(),
            provider(&origin.uri(), Family::OpenaiResponses, false),
            true,
        );
        let (status, _) = call_http(
            &state,
            Family::OpenaiResponses,
            "A",
            body(Family::OpenaiResponses, "A", "agent", stream, false),
            stream,
            true,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let payload = chat_probe(
            "A",
            stream,
            true,
            "openai_responses",
            json!({"previous_response_id":"resp_A"}),
        );
        let response = build_app(state.clone())
            .oneshot(
                Request::post("/v1/chat/completions")
                    .header("authorization", "Bearer tenant_A")
                    .header("x-sandhi-session", "different-unestablished-session")
                    .header("content-type", "application/json")
                    .body(Body::from(payload.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CONFLICT);
        let bytes = to_bytes(response.into_body(), 100000).await.unwrap();
        assert!(String::from_utf8_lossy(&bytes).contains("established compatible routing session"));
        assert_eq!(origin.received_requests().await.unwrap().len(), 1);
        assert_eq!(state.ledger.lock().unwrap().spent("vk:tenant_A"), 7);
        let conn = rusqlite::Connection::open(path).unwrap();
        let pins: i64 = conn
            .query_row("SELECT count(*) FROM route_affinity", [], |r| r.get(0))
            .unwrap();
        assert_eq!(pins, 1);
    }
}
#[tokio::test]
async fn selected_encoded_reference_fields_are_authorized_before_admission_in_both_modes_and_default_planes(
) {
    let mut probes = 0;
    for stream in [false, true] {
        for omitted in [false, true] {
            for (family, selector) in [
                (Family::OpenaiResponses, "previous"),
                (Family::OpenaiResponses, "items"),
                (Family::AnthropicMessages, "container"),
                (Family::GeminiGenerateContent, "cached"),
                (Family::GeminiGenerateContent, "stores"),
            ] {
                let dir = tempfile::tempdir().unwrap();
                let path = dir.path().join("gateway.db");
                let path = path.to_str().unwrap();
                let origin = MockServer::start().await;
                Mock::given(method("POST"))
                    .respond_with(move |r: &wiremock::Request| {
                        let b: Value = r.body_json().unwrap();
                        let who = if b.to_string().contains("tenant_B") {
                            "B"
                        } else {
                            "A"
                        };
                        let probe = b.to_string().contains("cross_probe");
                        if !probe && family == Family::OpenaiResponses {
                            output(family, who, stream)
                        } else {
                            plain(
                                family,
                                who,
                                stream,
                                !probe && family == Family::AnthropicMessages,
                            )
                        }
                    })
                    .mount(&origin)
                    .await;
                let mut p = provider(&origin.uri(), family, false);
                let mut binding = p.backend_binding().unwrap().clone();
                binding.models[0].max_output_tokens = Some(17);
                p = p.with_backend_binding(binding);
                let state = state(path, p, true);
                for who in ["A", "B"] {
                    let (s, b) = call_http(
                        &state,
                        family,
                        who,
                        body(family, who, "agent", stream, false),
                        stream,
                        true,
                    )
                    .await;
                    assert_eq!(
                        s,
                        StatusCode::OK,
                        "seed {family:?}/{stream}: {}",
                        String::from_utf8_lossy(&b)
                    );
                }
                let (bucket, extra) = match selector {
                    "previous" => (
                        "openai_responses",
                        json!({"previous_response_id":"resp_A","input":[{"role":"user","content":"must_not_replace_admitted_input"}]}),
                    ),
                    "items" => (
                        "openai_responses",
                        json!({"input_items":[{"type":"reasoning","id":"rs_A","summary":[],"encrypted_content":"opaque_A"}]}),
                    ),
                    "container" => ("anthropic", json!({"container":"container_A"})),
                    "cached" => (
                        "gemini",
                        json!({"cachedContent":"cachedContents/external_A"}),
                    ),
                    _ => (
                        "gemini",
                        json!({"tools":[{"fileSearch":{"fileSearchStoreNames":["fileSearchStores/external_A"]}}]}),
                    ),
                };
                let (s, b) = call_http(
                    &state,
                    Family::OpenaiChatCompletions,
                    "B",
                    chat_probe("B", stream, omitted, bucket, extra.clone()),
                    stream,
                    true,
                )
                .await;
                probes += 1;
                assert_eq!(
                    s,
                    StatusCode::FORBIDDEN,
                    "foreign {family:?}/{selector}/{stream}/{omitted}: {}",
                    String::from_utf8_lossy(&b)
                );
                assert_eq!(origin.received_requests().await.unwrap().len(), 2);
                assert_eq!(state.ledger.lock().unwrap().spent("vk:tenant_B"), 7);
                assert_eq!(state.ledger.lock().unwrap().reserved("vk:tenant_B"), 0);
                let (s, b) = call_http(
                    &state,
                    Family::OpenaiChatCompletions,
                    "A",
                    chat_probe("A", stream, omitted, bucket, extra),
                    stream,
                    true,
                )
                .await;
                probes += 1;
                if family == Family::GeminiGenerateContent {
                    assert_eq!(s, StatusCode::FORBIDDEN);
                    assert_eq!(origin.received_requests().await.unwrap().len(), 2);
                    continue;
                }
                assert_eq!(
                    s,
                    StatusCode::OK,
                    "own {family:?}/{selector}/{stream}/{omitted}: {}",
                    String::from_utf8_lossy(&b)
                );
                let sent = origin.received_requests().await.unwrap();
                assert_eq!(sent.len(), 3);
                let actual: Value = sent[2].body_json().unwrap();
                let encoded = actual.to_string();
                assert!(encoded.contains("tenant_A cross_probe admitted original input"));
                assert!(!encoded.contains("must_not_replace_admitted_input"));
                let output = if family == Family::AnthropicMessages {
                    &actual["max_tokens"]
                } else {
                    &actual["max_output_tokens"]
                };
                assert_eq!(output.as_u64(), Some(if omitted { 17 } else { 123 }));
                match selector {
                    "previous" => assert_eq!(actual["previous_response_id"], "resp_A"),
                    "items" => assert!(encoded.contains("opaque_A")),
                    _ => assert_eq!(actual["container"], "container_A"),
                }
            }
        }
    }
    assert_eq!(probes, 40);
    eprintln!("effective selected payload: probes40; foreign20; own-source-reference12; external-unadopted8; source seeds40; source sends52");
}
#[tokio::test]
async fn inactive_buckets_are_inert_and_explicit_raw_or_default_typed_wire_preserves_admitted_input(
) {
    for stream in [false, true] {
        for omitted in [false, true] {
            for ingress in [
                Family::OpenaiChatCompletions,
                Family::OpenaiResponses,
                Family::AnthropicMessages,
                Family::GeminiGenerateContent,
            ] {
                let dir = tempfile::tempdir().unwrap();
                let path = dir.path().join("gateway.db");
                let origin = MockServer::start().await;
                Mock::given(method("POST"))
                    .respond_with(plain(Family::OpenaiChatCompletions, "A", stream, false))
                    .expect(1)
                    .mount(&origin)
                    .await;
                let state = state(
                    path.to_str().unwrap(),
                    provider(&origin.uri(), Family::OpenaiChatCompletions, false),
                    true,
                );
                let mut payload = body(ingress, "A", "agent", stream, false);
                payload["sandhi_extensions"] = json!({"openai_responses":{"previous_response_id":"unverified_but_unused","input_items":[{"type":"item_reference","id":"unused_item"}]}});
                if omitted {
                    if let Some(o) = payload.as_object_mut() {
                        o.remove("max_tokens");
                        o.remove("max_output_tokens");
                        if ingress == Family::GeminiGenerateContent {
                            o.remove("generationConfig");
                        }
                    }
                }
                let (s, b) = call_http(&state, ingress, "A", payload, stream, true).await;
                assert_eq!(
                    s,
                    StatusCode::OK,
                    "unused {ingress:?}/{stream}/{omitted}: {}",
                    String::from_utf8_lossy(&b)
                );
                let actual: Value = origin.received_requests().await.unwrap()[0]
                    .body_json()
                    .unwrap();
                assert!(!actual.to_string().contains("unverified_but_unused"));
                assert!(!actual.to_string().contains("unused_item"));
                assert!(actual.to_string().contains("tenant_A"));
            }
        }
    }
}
#[tokio::test]
async fn omitted_output_default_reaches_messages_route_candidate_and_clamps_only_the_selected_default(
) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("gateway.db");
    let origin = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(plain(Family::AnthropicMessages, "A", false, false))
        .expect(2)
        .mount(&origin)
        .await;
    let mut p = provider(&origin.uri(), Family::AnthropicMessages, false);
    let mut binding = p.backend_binding().unwrap().clone();
    binding.models[0].max_output_tokens = Some(17);
    p = p.with_backend_binding(binding);
    let state = state(path.to_str().unwrap(), p, true);
    for omitted in [true, false] {
        let payload = chat_probe("A", false, omitted, "unused_bucket", json!({}));
        let response = build_app(state.clone())
            .oneshot(
                Request::post("/v1/chat/completions")
                    .header("authorization", "Bearer tenant_A")
                    .header("x-sandhi-session", "output-default")
                    .header("content-type", "application/json")
                    .body(Body::from(payload.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        if omitted {
            assert_eq!(
                response.headers()["x-sandhi-output-limit-source"],
                "operator_default"
            );
            assert_eq!(response.headers()["x-sandhi-output-max"], "17");
        } else {
            assert!(!response
                .headers()
                .contains_key("x-sandhi-output-limit-source"));
        }
        to_bytes(response.into_body(), 100000).await.unwrap();
    }
    let requests = origin.received_requests().await.unwrap();
    assert_eq!(requests[0].body_json::<Value>().unwrap()["max_tokens"], 17);
    assert_eq!(requests[1].body_json::<Value>().unwrap()["max_tokens"], 123);
}

#[tokio::test]
async fn count_only_routing_needs_no_generation_max_and_creates_no_source_pin_lease_or_proof() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("gateway.db");
    let path = path.to_str().unwrap();
    let origin = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&origin)
        .await;
    let state = state(
        path,
        provider(&origin.uri(), Family::AnthropicMessages, false),
        true,
    );
    let payload = json!({"model":"agent","messages":[{"role":"user","content":"tenant_A count materialized input"}]});
    let response = build_app(state.clone())
        .oneshot(
            Request::post("/v1/messages/count_tokens")
                .header("x-api-key", "tenant_A")
                .header("x-sandhi-session", "count-only")
                .header("content-type", "application/json")
                .body(Body::from(payload.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(!response
        .headers()
        .contains_key("x-sandhi-output-limit-source"));
    assert!(!response.headers().contains_key("x-sandhi-output-max"));
    let response: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), 100000).await.unwrap()).unwrap();
    let mut counted = payload;
    counted["model"] = json!("fixture");
    let estimate = sandhi_providers::token_estimate::baseline_estimate(counted.to_string().len());
    assert_eq!(response["input_tokens"], estimate);
    assert!(origin.received_requests().await.unwrap().is_empty());
    let conn = rusqlite::Connection::open(path).unwrap();
    let counts:(i64,i64,i64)=conn.query_row("SELECT (SELECT count(*) FROM route_affinity),(SELECT count(*) FROM budget_reservation),(SELECT count(*) FROM issued_native_references)",[],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).unwrap();
    assert_eq!(counts, (0, 0, 0));
}
#[tokio::test]
async fn cross_family_fidelity_refusals_keep_bad_request_classification_and_no_dispatch() {
    for (ingress, target, payload) in [
        (
            Family::OpenaiResponses,
            Family::OpenaiChatCompletions,
            json!({"model":"fixture","max_output_tokens":32,"input":[{"type":"function_call","call_id":"call_1","name":"tool","arguments":"{}"},{"type":"function_call_output","call_id":"call_1","output":[{"type":"input_text","text":"structured native output"}]}]}),
        ),
        (
            Family::OpenaiResponses,
            Family::GeminiGenerateContent,
            json!({"model":"fixture","max_output_tokens":32,"input":[{"type":"reasoning","encrypted_content":"unverified opaque"},{"role":"user","content":"continue"}]}),
        ),
        (
            Family::GeminiGenerateContent,
            Family::OpenaiResponses,
            json!({"contents":[{"role":"user","parts":[{"fileData":{"fileUri":"files/unverified"}}]}],"generationConfig":{"maxOutputTokens":32}}),
        ),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("gateway.db");
        let origin = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(500))
            .expect(0)
            .mount(&origin)
            .await;
        let state = state(
            path.to_str().unwrap(),
            provider(&origin.uri(), target, false),
            false,
        );
        let (status, response) = call_http(&state, ingress, "A", payload, false, false).await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "{ingress:?}->{target:?}: {}",
            String::from_utf8_lossy(&response)
        );
        assert!(String::from_utf8_lossy(&response).contains("cannot be translated"));
        assert!(origin.received_requests().await.unwrap().is_empty());
        assert_eq!(state.ledger.lock().unwrap().spent("vk:tenant_A"), 0);
        assert_eq!(state.ledger.lock().unwrap().reserved("vk:tenant_A"), 0);
    }
}
