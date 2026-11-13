//! Native source preservation uses the same strict boundary for embedded and
//! durable-owner calls. Exactly one accepted HTTP invocation, including refusals.
use futures_util::StreamExt;
use sandhi_core::{
    ChatRequestV1, ChatStreamEventV1 as Event, EndpointFamilyV1 as Family, UsageCompleteness as C,
};
use sandhi_providers::{AttemptContext, ProviderError, ProviderHandle, ProviderRuntime};
use serde_json::{json, Value};
use wiremock::{matchers::method, Mock, MockServer, ResponseTemplate};
fn request() -> ChatRequestV1 {
    serde_json::from_value(json!({"model":"fixture","max_output_tokens":32,"include_native_response":false,"messages":[{"role":"user","content":"hello"}]})).unwrap()
}
fn provider(family: Family, url: String) -> ProviderHandle {
    let rt = ProviderRuntime::new();
    match family {
        Family::CohereChat => rt.cohere(
            url,
            "synthetic",
            Default::default(),
            Some(0),
            None,
            None,
            None,
        ),
        _ => rt.ollama(
            url,
            "synthetic",
            Default::default(),
            Some(0),
            None,
            None,
            None,
        ),
    }
}
fn body(family: Family) -> Value {
    match family {
        Family::CohereChat => {
            json!({"id":"cohere-fixture","message":{"role":"assistant","content":[{"type":"text","text":"reply"}]},"finish_reason":"COMPLETE","usage":{"billed_units":{"input_tokens":2,"output_tokens":1},"tokens":{"input_tokens":7,"output_tokens":4}}})
        }
        _ => {
            json!({"model":"fixture","message":{"role":"assistant","content":"reply"},"done":true,"done_reason":"stop","prompt_eval_count":7,"eval_count":4})
        }
    }
}
fn stream(family: Family, mode: &str) -> String {
    let mut values = if family == Family::CohereChat {
        vec![
            json!({"type":"message-start","id":"cohere-fixture","delta":{"message":{"role":"assistant"}}}),
            json!({"type":"content-start","index":0,"delta":{"message":{"content":{"type":"text","text":""}}}}),
            json!({"type":"content-delta","index":0,"delta":{"message":{"content":{"text":"reply"}}}}),
            json!({"type":"content-end","index":0}),
            json!({"type":"message-end","delta":{"finish_reason":"COMPLETE","usage":{"billed_units":{"input_tokens":5,"output_tokens":2},"tokens":{"input_tokens":7,"output_tokens":4}}}}),
        ]
    } else {
        vec![
            json!({"model":"fixture","message":{"role":"assistant","content":"reply"},"done":false}),
            json!({"model":"fixture","message":{"role":"assistant","content":""},"done":true,"done_reason":"stop","prompt_eval_count":7,"eval_count":4}),
        ]
    };
    let last = values.last_mut().unwrap();
    match mode {
        "missing" => {
            if family == Family::CohereChat {
                last["delta"].as_object_mut().unwrap().remove("usage");
            } else {
                last.as_object_mut().unwrap().remove("eval_count");
            }
        }
        "malformed" => {
            if family == Family::CohereChat {
                last["delta"]["usage"]["tokens"]["input_tokens"] = "7".into();
            } else {
                last["prompt_eval_count"] = "7".into();
            }
        }
        "opaque" => {
            if family == Family::CohereChat {
                values[2]["delta"]["message"]["content"]["signature"] = "opaque".into();
            } else {
                values[0]["message"]["context"] = "opaque".into();
            }
        }
        "missing_terminal" => {
            values.pop();
        }
        "post_terminal" => {
            values.push(if family == Family::CohereChat {
                json!({"type":"content-end","index":0})
            } else {
                json!({"model":"fixture","message":{"content":"late"},"done":false})
            });
        }
        "duplicate_terminal" => {
            values.push(values.last().unwrap().clone());
        }
        "provider_error" => {
            values.pop();
            values.push(json!({"error":"synthetic"}));
        }
        _ => {}
    }
    let mut wire: String = values
        .iter()
        .map(|v| {
            if family == Family::CohereChat {
                format!("data: {v}\n\n")
            } else {
                format!("{v}\n")
            }
        })
        .collect();
    if mode == "truncated" {
        wire.push_str(if family == Family::CohereChat {
            "data: {"
        } else {
            "{"
        });
    }
    if mode == "no_final_newline" && family == Family::OllamaChat {
        wire.pop();
    }
    wire
}
#[tokio::test]
async fn buffered_native_counts_preserve_reported_zero_and_refuse_unknown_without_replay() {
    for family in [Family::CohereChat, Family::OllamaChat] {
        for mode in ["good", "zero", "missing", "malformed", "negative", "opaque"] {
            let server = MockServer::start().await;
            let mut b = body(family);
            let (root, key) = if family == Family::CohereChat {
                (&mut b["usage"]["tokens"], "input_tokens")
            } else {
                (&mut b, "prompt_eval_count")
            };
            match mode {
                "zero" => {
                    root[key] = 0.into();
                    root[if family == Family::CohereChat {
                        "output_tokens"
                    } else {
                        "eval_count"
                    }] = 0.into();
                }
                "missing" => {
                    root.as_object_mut().unwrap().remove(key);
                }
                "malformed" => root[key] = "bad".into(),
                "negative" => root[key] = (-1).into(),
                _ => {}
            }
            if mode == "opaque" {
                b["message"]["opaque_state"] = "signature".into();
            }
            Mock::given(method("POST"))
                .respond_with(
                    ResponseTemplate::new(200)
                        .insert_header("x-request-id", "native-origin")
                        .set_body_json(b),
                )
                .expect(1)
                .mount(&server)
                .await;
            let handle = provider(family, server.uri());
            assert!(handle.supports_owned_buffered());
            handle.preflight_owned_buffered(&request()).unwrap();
            let (context, receiver) =
                AttemptContext::single_attempt("native-buffered", "a".repeat(64)).unwrap();
            let result = handle
                .complete_with_qualified_usage_and_attempts(
                    request(),
                    Default::default(),
                    context.clone(),
                )
                .await;
            if mode == "opaque" {
                assert!(matches!(result, Err(ProviderError::OutputProjection(_))));
                assert!(context.qualified_buffered_evidence().unwrap().usage.is_ok());
            } else {
                let result = result.unwrap();
                assert_eq!(
                    result.usage.is_ok(),
                    matches!(mode, "good" | "zero"),
                    "{family:?}/{mode}"
                );
                if let Ok(usage) = result.usage {
                    assert_eq!(
                        (usage.tokens_in, usage.tokens_out),
                        if mode == "zero" { (0, 0) } else { (7, 4) }
                    );
                    assert_eq!(
                        (usage.cache_read_tokens, usage.cache_creation_tokens),
                        (0, 0)
                    );
                    assert_eq!(usage.reasoning_tokens, None);
                }
            }
            assert_eq!(context.dispatch_count(), 1);
            let observations = receiver.drain();
            assert_eq!(observations.len(), 2);
            if let sandhi_providers::AttemptPhase::Terminal { usage, .. } = &observations[1].phase {
                assert_eq!(usage.is_some(), matches!(mode, "good" | "zero" | "opaque"));
                if let Some(usage) = usage {
                    assert_eq!(
                        (usage.tokens_in, usage.tokens_out),
                        if mode == "zero" { (0, 0) } else { (7, 4) }
                    );
                }
            } else {
                panic!("missing physical terminal");
            }
            assert_eq!(server.received_requests().await.unwrap().len(), 1);
        }
    }
}
#[tokio::test]
async fn native_stream_requires_terminal_and_clean_eof_keeps_refused_projection_source_counts() {
    for family in [Family::CohereChat, Family::OllamaChat] {
        for mode in [
            "good",
            "no_final_newline",
            "missing",
            "malformed",
            "opaque",
            "missing_terminal",
            "post_terminal",
            "duplicate_terminal",
            "truncated",
            "provider_error",
        ] {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .respond_with(
                    ResponseTemplate::new(200)
                        .insert_header("x-request-id", "native-origin")
                        .set_body_raw(
                            stream(family, mode),
                            if family == Family::CohereChat {
                                "text/event-stream"
                            } else {
                                "application/x-ndjson"
                            },
                        ),
                )
                .expect(1)
                .mount(&server)
                .await;
            let handle = provider(family, server.uri());
            assert!(handle.supports_owned_streaming());
            handle.preflight_owned_streaming(&request()).unwrap();
            let (context, receiver) =
                AttemptContext::single_attempt("native-stream", "b".repeat(64)).unwrap();
            let events = handle
                .stream_with_qualified_usage_and_attempts(
                    request(),
                    Default::default(),
                    context.clone(),
                )
                .await
                .unwrap()
                .collect::<Vec<_>>()
                .await;
            let clean = matches!(mode, "good" | "no_final_newline");
            assert_eq!(
                events.iter().any(Result::is_err),
                !clean,
                "{family:?}/{mode}: {events:?}"
            );
            assert_eq!(
                events.iter().any(|e| matches!(e, Ok(Event::Finish { .. }))),
                clean
            );
            let evidence = context.qualified_stream_evidence().unwrap();
            if clean || mode == "opaque" {
                let usage = evidence.usage.unwrap();
                assert_eq!(usage.completeness, C::Final, "{family:?}/{mode}");
                assert_eq!((usage.tokens_in, usage.tokens_out), (7, 4));
                assert_eq!(usage.upstream_request_id.as_deref(), Some("native-origin"));
            } else {
                assert!(
                    !evidence.usage.is_some_and(|u| u.completeness == C::Final),
                    "{family:?}/{mode}"
                );
            }
            assert_eq!(
                events
                    .iter()
                    .filter(|e| matches!(e,Ok(Event::Usage{usage})if usage.completeness==C::Final))
                    .count(),
                usize::from(clean)
            );
            assert_eq!(context.dispatch_count(), 1);
            assert_eq!(receiver.drain().len(), 2);
            assert_eq!(server.received_requests().await.unwrap().len(), 1);
        }
    }
}
#[test]
fn native_observation_is_split_invariant_and_does_not_finalize_without_clean_eof() {
    for family in [Family::CohereChat, Family::OllamaChat] {
        for mode in ["good", "no_final_newline", "post_terminal", "truncated"] {
            let wire = stream(family, mode);
            for split in 0..=wire.len() {
                let mut observer =
                    sandhi_providers::stream_usage::ProtocolStreamUsageObserver::for_endpoint(
                        family,
                    )
                    .unwrap();
                let a = observer.push(&wire.as_bytes()[..split]);
                let b = observer.push(&wire.as_bytes()[split..]);
                let end = observer.finish();
                let good = matches!(mode, "good" | "no_final_newline");
                assert_eq!(
                    a.is_ok() && b.is_ok() && end.is_ok(),
                    good,
                    "{family:?}/{mode}/{split}"
                );
                assert_eq!(
                    observer
                        .canonical_usage()
                        .is_some_and(|u| u.completeness == C::Final),
                    good
                );
            }
        }
    }
}
#[tokio::test]
async fn native_parallel_tools_preserve_arguments_ids_and_next_turn_results() {
    for family in [Family::CohereChat, Family::OllamaChat] {
        for streaming in [false, true] {
            let server = MockServer::start().await;
            let calls = if family == Family::CohereChat {
                json!([{"id":"call-b","type":"function","function":{"name":"beta","arguments":"{\"n\":2}"}},{"id":"call-a","type":"function","function":{"name":"alpha","arguments":"{\"n\":1}"}}])
            } else {
                json!([{"function":{"name":"beta","arguments":{"n":2}}},{"function":{"name":"alpha","arguments":{"n":1}}}])
            };
            let mut b = body(family);
            b["message"]["content"] = if family == Family::CohereChat {
                json!([])
            } else {
                json!("")
            };
            b["message"]["tool_calls"] = calls.clone();
            b[if family == Family::CohereChat {
                "finish_reason"
            } else {
                "done_reason"
            }] = if family == Family::CohereChat {
                "TOOL_CALL".into()
            } else {
                "stop".into()
            };
            let wire = if family == Family::OllamaChat {
                format!("{b}\n")
            } else {
                let mut values = vec![
                    json!({"type":"message-start","id":"tools","delta":{"message":{"role":"assistant"}}}),
                ];
                for (index, call) in calls.as_array().unwrap().iter().enumerate() {
                    values.push(json!({"type":"tool-call-start","index":index,"delta":{"message":{"tool_calls":{"id":call["id"],"function":{"name":call["function"]["name"]}}}}}));
                    values.push(json!({"type":"tool-call-delta","index":index,"delta":{"message":{"tool_calls":{"function":{"arguments":call["function"]["arguments"]}}}}}));
                    values.push(json!({"type":"tool-call-end","index":index}));
                }
                values.push(json!({"type":"message-end","delta":{"finish_reason":"TOOL_CALL","usage":b["usage"]}}));
                values.iter().map(|v| format!("data: {v}\n\n")).collect()
            };
            Mock::given(method("POST"))
                .respond_with(move |request: &wiremock::Request| {
                    let request: Value = serde_json::from_slice(&request.body).unwrap();
                    if request["stream"] == true {
                        ResponseTemplate::new(200).set_body_raw(wire.clone(), "text/event-stream")
                    } else {
                        ResponseTemplate::new(200).set_body_json(b.clone())
                    }
                })
                .mount(&server)
                .await;
            let handle = provider(family, server.uri());
            let mut returned = Vec::new();
            if streaming {
                for e in handle
                    .stream(request())
                    .await
                    .unwrap()
                    .collect::<Vec<_>>()
                    .await
                {
                    match e.unwrap() {
                        Event::ToolCallStart { index, id, name } => {
                            returned.push((index, id, name, String::new()))
                        }
                        Event::ToolCallArgumentsDelta { index, delta } => returned
                            .iter_mut()
                            .find(|v| v.0 == index)
                            .unwrap()
                            .3
                            .push_str(&delta),
                        _ => {}
                    }
                }
            } else {
                let response = handle.complete(request()).await.unwrap();
                returned = response
                    .output
                    .tool_calls
                    .into_iter()
                    .enumerate()
                    .map(|(i, c)| (i as u32, c.id, c.name, c.arguments))
                    .collect();
            }
            assert_eq!(returned.len(), 2);
            assert_ne!(returned[0].1, returned[1].1);
            assert_eq!(
                (returned[0].2.as_str(), returned[1].2.as_str()),
                ("beta", "alpha")
            );
            assert_eq!(
                serde_json::from_str::<Value>(&returned[0].3).unwrap(),
                json!({"n":2})
            );
            let mut next = request();
            next.messages.push(serde_json::from_value(json!({"role":"assistant","tool_calls":returned.iter().map(|(_,id,name,args)|json!({"id":id,"name":name,"arguments":args})).collect::<Vec<_>>()})).unwrap());
            for (_, id, name, _) in returned.iter().rev() {
                next.messages.push(
                    serde_json::from_value(
                        json!({"role":"tool","tool_call_id":id,"content":format!("result-{name}")}),
                    )
                    .unwrap(),
                );
            }
            handle.preflight_owned_buffered(&next).unwrap();
            let (context, _) =
                AttemptContext::single_attempt("tool-results", "c".repeat(64)).unwrap();
            let _ = handle
                .complete_with_qualified_usage_and_attempts(next, Default::default(), context)
                .await
                .unwrap();
            let sent = server.received_requests().await.unwrap();
            assert_eq!(sent.len(), 2);
            let final_body: Value = serde_json::from_slice(&sent[1].body).unwrap();
            let messages = final_body["messages"].as_array().unwrap();
            let last = &messages[messages.len() - 2..];
            if family == Family::OllamaChat {
                assert_eq!(last[0]["tool_name"], "alpha");
                assert_eq!(last[1]["tool_name"], "beta");
            } else {
                assert_eq!(last[0]["tool_call_id"], returned[1].1);
                assert_eq!(last[1]["tool_call_id"], returned[0].1);
            }
        }
    }
}

#[test]
fn native_source_frame_bounds_accept_real_frames_and_refuse_unbounded_input() {
    use sandhi_providers::stream_usage::ProtocolStreamUsageObserver as Observer;
    for family in [Family::CohereChat, Family::OllamaChat] {
        let start = if family == Family::CohereChat {
            "data: {\"type\":\"message-start\",\"id\":\"r\"}\n\n"
        } else {
            ""
        };
        let content = "x".repeat(200 * 1024);
        let wire = if family == Family::CohereChat {
            format!(
                "{start}data: {}\n\n",
                json!({"type":"content-delta","index":0,"delta":{"message":{"content":{"text":content}}}})
            )
        } else {
            format!(
                "{}\n",
                json!({"model":"fixture","done":false,"message":{"content":content}})
            )
        };
        let mut observer = Observer::for_endpoint(family).unwrap();
        for part in wire.as_bytes().chunks(8192) {
            observer.push(part).unwrap();
        }
        let mut observer = Observer::for_endpoint(family).unwrap();
        let mut failure = None;
        for _ in 0..=(sandhi_core::MAX_NATIVE_STREAM_FRAME_BYTES / 8192) {
            if let Err(error) = observer.push(&[b'x'; 8192]) {
                failure = Some(error);
                break;
            }
        }
        assert!(failure.is_some(), "{family:?}: unbounded line accepted");
    }
}
