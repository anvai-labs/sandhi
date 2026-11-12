//! Actual HTTP source evidence is independent of Chat output projection.
use futures_util::StreamExt;
use sandhi_core::{ChatStreamEventV1 as Event, UsageCompleteness};
use sandhi_providers::{AttemptContext, ProviderRuntime};
use serde_json::{json, Value};
use wiremock::{matchers::method, Mock, MockServer, ResponseTemplate};

fn frames(mode: &str) -> Vec<Value> {
    let mut delta = json!({"role":"assistant","content":"reply"});
    match mode {
        "audio" => delta["audio"] = json!({"id":"opaque-audio"}),
        "opaque" => delta["vendor_state"] = json!({"signature":"opaque-state"}),
        "reasoning" => delta["reasoning_content"] = "materialized".into(),
        _ => {}
    }
    let mut values = vec![
        json!({"id":"chat-source","model":"fixture","object":"chat.completion.chunk",
        "choices":[{"index":0,"delta":delta,"finish_reason":null}]}),
    ];
    let mut finish = "stop";
    if matches!(
        mode,
        "fragments" | "invalid_args" | "duplicate_ids" | "after_tool"
    ) {
        values[0]["choices"][0]["delta"] = json!({"role":"assistant","tool_calls":[
            {"index":7,"id":"call_","type":"function","function":{"name":"look","arguments":"{"}}]});
        values.push(json!({"choices":[{"index":0,"delta":{"tool_calls":[
            {"index":7,"id":"123","function":{"name":"up","arguments":"}"}}]},"finish_reason":null}]}));
        if mode == "invalid_args" {
            values[0]["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"] = "[".into();
            values[1]["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"] = "]".into();
        }
        if mode == "duplicate_ids" {
            values[1]["choices"][0]["delta"]["tool_calls"]
                .as_array_mut()
                .unwrap()
                .push(
                    json!({"index":8,"id":"call_123","function":{"name":"other","arguments":"{}"}}),
                );
        }
        finish = "tool_calls";
    }
    if matches!(mode, "id_drift" | "model_drift") {
        let mut next =
            json!({"choices":[{"index":0,"delta":{"content":"other"},"finish_reason":null}]});
        next[if mode == "id_drift" { "id" } else { "model" }] = "different".into();
        values.push(next);
    }
    if mode == "after_tool" {
        values.push(
            json!({"choices":[{"index":0,"delta":{"content":"late prose"},"finish_reason":null}]}),
        );
    }
    values.push(json!({"choices":[{"index":0,"delta":{},"finish_reason":finish}]}));
    if mode == "multiple" {
        values[0]["choices"]
            .as_array_mut()
            .unwrap()
            .push(json!({"index":1,"delta":{"content":"other"},"finish_reason":null}));
        values.last_mut().unwrap()["choices"]
            .as_array_mut()
            .unwrap()
            .push(json!({"index":1,"delta":{},"finish_reason":"stop"}));
    }
    values.push(json!({"object":"chat.completion.chunk","choices":[],"usage":{"prompt_tokens":10,"completion_tokens":4,"total_tokens":14,
        "prompt_tokens_details":{"cached_tokens":6,"cache_write_tokens":2},"completion_tokens_details":{"reasoning_tokens":0}}}));
    for value in &mut values {
        value["object"] = "chat.completion.chunk".into();
    }
    values
}

#[tokio::test]
async fn chat_native_or_portable_projection_joins_one_http_attempt_and_final_source_counts() {
    for mode in [
        "text",
        "audio",
        "opaque",
        "reasoning",
        "fragments",
        "invalid_args",
        "duplicate_ids",
        "multiple",
        "after_tool",
        "id_drift",
        "model_drift",
    ] {
        for native in [false, true] {
            let values = frames(mode);
            let wire = values
                .iter()
                .map(|value| format!("data: {value}\n\n"))
                .collect::<String>()
                + "data: [DONE]\n\n";
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .respond_with(
                    ResponseTemplate::new(200)
                        .insert_header("x-request-id", "chat-fidelity-origin")
                        .set_body_raw(wire, "text/event-stream"),
                )
                .expect(1)
                .mount(&server)
                .await;
            let provider = ProviderRuntime::new().openai_compat(
                "fixture",
                server.uri(),
                "synthetic",
                Default::default(),
                Some(0),
                None,
                None,
                None,
            );
            let request = serde_json::from_value(json!({"model":"fixture","max_output_tokens":64,
                "include_native_stream":native,"messages":[{"role":"user","content":"hello"}]}))
            .unwrap();
            let (context, receiver) =
                AttemptContext::single_attempt("chat-fidelity", "e".repeat(64)).unwrap();
            let actual = provider
                .stream_with_qualified_usage_and_attempts(
                    request,
                    Default::default(),
                    context.clone(),
                )
                .await
                .unwrap()
                .collect::<Vec<_>>()
                .await;
            let clean = native || matches!(mode, "text" | "reasoning" | "fragments");
            assert_eq!(
                actual.iter().any(Result::is_err),
                !clean,
                "{native}/{mode}: {actual:?} / {:?}",
                context.qualified_stream_evidence()
            );
            assert_eq!(
                actual
                    .iter()
                    .filter(|event| matches!(event, Ok(Event::Finish { .. })))
                    .count(),
                usize::from(clean)
            );
            if native {
                let originals = actual
                    .iter()
                    .filter_map(|event| match event {
                        Ok(Event::NativeFrame { frame }) => frame.json_data(),
                        _ => None,
                    })
                    .collect::<Vec<_>>();
                assert_eq!(originals, values, "native source changed: {mode}");
            } else if mode == "fragments" {
                assert!(actual.iter().any(|event|matches!(event,Ok(Event::ToolCallStart{index:7,id,name}) if id=="call_123" && name=="lookup")));
            } else if mode == "reasoning" {
                assert!(actual.iter().any(|event|matches!(event,Ok(Event::ReasoningDelta{delta}) if delta=="materialized")));
            }
            let evidence = context.qualified_stream_evidence().unwrap();
            let usage = evidence.usage.unwrap();
            assert_eq!(
                usage.completeness,
                UsageCompleteness::Final,
                "{native}/{mode}: {actual:?} / {:?}",
                context.qualified_stream_evidence()
            );
            assert_eq!(
                (
                    usage.tokens_in,
                    usage.cache_read_tokens,
                    usage.cache_creation_tokens,
                    usage.tokens_out
                ),
                (2, 6, 2, 4)
            );
            assert_eq!(usage.reasoning_tokens, Some(0));
            assert_eq!(context.dispatch_count(), 1);
            assert_eq!(receiver.drain().len(), 2);
            assert_eq!(server.received_requests().await.unwrap().len(), 1);
        }
    }
}
