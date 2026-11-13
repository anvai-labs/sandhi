//! Portable content fidelity is independent of durable-accounting opt-in.
use futures_util::StreamExt;
use sandhi_core::{ChatStreamEventV1 as Event, UsageCompleteness};
use sandhi_providers::{ProviderError, ProviderHandle, ProviderRuntime};
use serde_json::{json, Value};
use wiremock::{matchers::method, Mock, MockServer, ResponseTemplate};

fn provider(family: &str, uri: String, max_retries: u32) -> ProviderHandle {
    let runtime = ProviderRuntime::new();
    match family {
        "chat" => runtime.openai_compat(
            "fixture",
            uri,
            "synthetic",
            Default::default(),
            Some(max_retries),
            None,
            None,
            None,
        ),
        "responses" => runtime.openai_responses(
            "fixture",
            uri,
            "synthetic",
            Default::default(),
            Some(max_retries),
            None,
            None,
            None,
        ),
        "messages" => runtime.anthropic(
            uri,
            "synthetic",
            sandhi_providers::AnthropicAuthScheme::ApiKey,
            Default::default(),
            Some(max_retries),
            None,
            None,
            None,
        ),
        "gemini" => runtime.gemini(
            uri,
            "synthetic",
            sandhi_providers::GeminiAuthScheme::ApiKey,
            Default::default(),
            Some(max_retries),
            None,
            None,
            None,
        ),
        _ => unreachable!(),
    }
}
fn frames(family: &str, opaque: bool) -> Vec<Value> {
    match family {
        "chat" => {
            let mut delta = json!({"role":"assistant","content":"reply"});
            if opaque {
                delta["vendor_state"] = json!({"signature":"opaque"});
            }
            vec![
                json!({"object":"chat.completion.chunk","id":"chat","model":"fixture","choices":[{"index":0,"delta":delta,"finish_reason":"stop"}]}),
                json!({"object":"chat.completion.chunk","choices":[],"usage":{"prompt_tokens":10,"completion_tokens":4,"prompt_tokens_details":{"cached_tokens":6,"cache_write_tokens":2},"completion_tokens_details":{"reasoning_tokens":0}}}),
            ]
        }
        "responses" => {
            let mut values = vec![
                json!({"type":"response.created","response":{"id":"resp","model":"fixture","status":"in_progress","output":[]}}),
            ];
            if opaque {
                values.push(json!({"type":"response.output_item.added","output_index":0,"item":{"type":"reasoning","id":"rs","summary":[],"encrypted_content":"opaque"}}));
            }
            values.push(json!({"type":"response.completed","response":{"id":"resp","model":"fixture","status":"completed","output":[{"type":"message","id":"msg","role":"assistant","content":[{"type":"output_text","text":"reply"}]}],"usage":{"input_tokens":10,"output_tokens":4,"input_tokens_details":{"cached_tokens":6,"cache_write_tokens":2},"output_tokens_details":{"reasoning_tokens":0}}}}));
            values
        }
        "messages" => {
            let block = if opaque {
                json!({"type":"thinking","thinking":"materialized","signature":"opaque"})
            } else {
                json!({"type":"text","text":"reply"})
            };
            vec![
                json!({"type":"message_start","message":{"type":"message","id":"msg","model":"fixture","role":"assistant","content":[],"usage":{"input_tokens":2,"output_tokens":1,"cache_read_input_tokens":6,"cache_creation_input_tokens":2,"output_tokens_details":{"thinking_tokens":0}}}}),
                json!({"type":"content_block_start","index":0,"content_block":block}),
                json!({"type":"content_block_stop","index":0}),
                json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":4}}),
                json!({"type":"message_stop"}),
            ]
        }
        "gemini" => {
            let mut part = json!({"text":"reply"});
            if opaque {
                part["thoughtSignature"] = "opaque".into();
            }
            vec![
                json!({"responseId":"gemini","modelVersion":"fixture","candidates":[{"index":0,"content":{"role":"model","parts":[part]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":8,"cachedContentTokenCount":6,"candidatesTokenCount":4,"thoughtsTokenCount":1,"totalTokenCount":13}}),
            ]
        }
        _ => unreachable!(),
    }
}
async fn run(
    family: &str,
    opaque: bool,
    native: bool,
) -> (Vec<Result<Event, ProviderError>>, Vec<Value>) {
    let values = frames(family, opaque);
    let mut wire = values
        .iter()
        .map(|v| format!("data: {v}\n\n"))
        .collect::<String>();
    if family == "chat" {
        wire.push_str("data: [DONE]\n\n");
    }
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(wire, "text/event-stream"))
        .expect(1)
        .mount(&server)
        .await;
    let request=serde_json::from_value(json!({"model":"fixture","max_output_tokens":64,"include_native_stream":native,"messages":[{"role":"user","content":"hello"}]})).unwrap();
    let actual = provider(family, server.uri(), 0)
        .stream(request)
        .await
        .unwrap()
        .collect::<Vec<_>>()
        .await;
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
    (actual, values)
}
async fn refuses(family: &str) {
    let (actual, _) = run(family, true, false).await;
    assert!(
        actual
            .iter()
            .any(|v| matches!(v, Err(ProviderError::OutputProjection(_)))),
        "silently accepted {family}: {actual:?}"
    );
    assert!(!actual.iter().any(|v| matches!(v, Ok(Event::Finish { .. }))));
    for error in actual.iter().filter_map(|event| event.as_ref().err()) {
        assert!(!error.as_typed(Some(family)).retryable);
    }
}

#[tokio::test]
async fn every_admitted_family_refuses_hidden_retries_before_dispatch() {
    for family in ["chat", "responses", "messages", "gemini"] {
        let server = MockServer::start().await;
        let request = serde_json::from_value(
            json!({"model":"fixture","max_output_tokens":64,"messages":[{"role":"user","content":"hello"}]}),
        )
        .unwrap();
        let result = provider(family, server.uri(), 1).stream(request).await;
        assert!(
            matches!(result, Err(ProviderError::InvalidRequest(_))),
            "{family}"
        );
        assert!(server.received_requests().await.unwrap().is_empty());
    }
}
#[tokio::test]
async fn default_chat_refuses_nonportable_state() {
    refuses("chat").await;
}
#[tokio::test]
async fn default_responses_refuses_nonportable_state() {
    refuses("responses").await;
}
#[tokio::test]
async fn default_messages_refuses_nonportable_state() {
    refuses("messages").await;
}
#[tokio::test]
async fn default_gemini_refuses_nonportable_state() {
    refuses("gemini").await;
}

#[tokio::test]
async fn ordinary_portable_content_and_reported_usage_survive_all_source_families() {
    for family in ["chat", "responses", "messages", "gemini"] {
        let (actual, _) = run(family, false, false).await;
        let actual = actual
            .into_iter()
            .collect::<Result<Vec<_>, _>>()
            .unwrap_or_else(|e| panic!("{family}: {e:?}"));
        assert!(
            matches!(actual.first(), Some(Event::ResponseStart { .. })),
            "{family}"
        );
        assert_eq!(
            actual
                .iter()
                .filter_map(|v| match v {
                    Event::TextDelta { delta } => Some(delta.as_str()),
                    _ => None,
                })
                .collect::<String>(),
            "reply",
            "{family}"
        );
        let usages = actual
            .iter()
            .filter_map(|v| match v {
                Event::Usage { usage } if usage.completeness == UsageCompleteness::Final => {
                    Some(usage)
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(usages.len(), 1, "{family}");
        assert_eq!(
            (
                usages[0].tokens_in,
                usages[0].cache_read_tokens,
                usages[0].cache_creation_tokens,
                usages[0].tokens_out
            ),
            (2, 6, if family == "gemini" { 0 } else { 2 }, 4),
            "{family}"
        );
        assert_eq!(
            usages[0].reasoning_tokens,
            Some(if family == "gemini" { 1 } else { 0 }),
            "{family}"
        );
        assert!(
            matches!(actual.last(), Some(Event::Finish { .. })),
            "{family}"
        );
    }
}
#[tokio::test]
async fn native_streams_keep_the_original_opaque_frames() {
    for family in ["chat", "responses", "messages", "gemini"] {
        let (actual, expected) = run(family, true, true).await;
        let actual = actual
            .into_iter()
            .collect::<Result<Vec<_>, _>>()
            .unwrap_or_else(|e| panic!("{family}: {e:?}"));
        let frames = actual
            .iter()
            .filter_map(|v| match v {
                Event::NativeFrame { frame } => frame.json_data(),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(frames, expected, "{family}");
    }
}

#[tokio::test]
async fn cache_availability_precedes_content_when_numeric_usage_is_unqualified() {
    for family in ["gemini", "messages"] {
        let values = if family == "messages" {
            vec![
                json!({"type":"message_start","message":{"type":"message","id":"msg","model":"fixture","role":"assistant","content":[],"usage":{}}}),
                json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":"reply"}}),
                json!({"type":"content_block_stop","index":0}),
                json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{}}),
                json!({"type":"message_stop"}),
            ]
        } else {
            vec![
                json!({"responseId":"gemini","modelVersion":"fixture","candidates":[{"index":0,"content":{"role":"model","parts":[{"text":"reply"}]},"finishReason":"STOP"}]}),
            ]
        };
        let wire = values
            .iter()
            .map(|v| format!("data: {v}\n\n"))
            .collect::<String>();
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(wire, "text/event-stream"))
            .mount(&server)
            .await;
        let request=serde_json::from_value(json!({"model":"fixture","max_output_tokens":64,"messages":[{"role":"user","content":"hello"}]})).unwrap();
        let actual = provider(family, server.uri(), 0)
            .stream(request)
            .await
            .unwrap()
            .collect::<Vec<_>>()
            .await;
        assert!(actual.last().is_some_and(Result::is_err));
        assert!(!actual
            .iter()
            .any(|event| matches!(event, Ok(Event::Finish { .. }))));
        let content = actual
            .iter()
            .position(|event| matches!(event, Ok(Event::TextDelta { .. })))
            .unwrap();
        assert!(actual[..content].iter().any(|event|matches!(event,Ok(Event::Usage{usage}) if usage.completeness==UsageCompleteness::Unavailable)));
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
        assert!(actual.iter().any(|event|matches!(event,Ok(Event::Usage{usage}) if usage.completeness==UsageCompleteness::Unavailable && usage.cache_read_observation.is_some_and(|o|o.status==sandhi_core::CacheReadStatus::Absent))),"{family} discarded inspected cache absence");
    }
}
