//! Real HTTP source facts survive projection; no live subscription qualification.
use futures_util::StreamExt;
use sandhi_core::{ChatRequestV1, ChatStreamEventV1 as Event, UsageCompleteness};
use sandhi_providers::{AttemptContext, ProviderHandle, ProviderRuntime};
use serde_json::{json, Value};
use wiremock::{matchers::method, Mock, MockServer, ResponseTemplate};

fn source(profile: &str, uri: String) -> ProviderHandle {
    let runtime = ProviderRuntime::new();
    if profile == "codex" {
        runtime
            .chatgpt_responses(
                "fixture",
                uri,
                "synthetic",
                Default::default(),
                Some(0),
                None,
                None,
                None,
            )
            .with_credential_expiry(4_000_000_000)
    } else {
        runtime.openai_responses(
            "fixture",
            uri,
            "synthetic",
            Default::default(),
            Some(0),
            None,
            None,
            None,
        )
    }
}
fn request(native: bool) -> ChatRequestV1 {
    serde_json::from_value(json!({"model":"fixture","max_output_tokens":64,"include_native_stream":native,"include_native_response":native,
        "messages":[{"role":"developer","content":"Code."},{"role":"user","content":"hello"}]})).unwrap()
}
fn frames(mode: &str) -> Vec<Value> {
    let message = json!({"type":"message","id":"msg_0","role":"assistant","status":"completed","content":[{"type":"output_text","text":"reply","annotations":[],"logprobs":[]}]});
    let tool = json!({"type":"function_call","id":"fc_0","call_id":"call_0","name":"inspect","arguments":"{}","status":"completed"});
    let mut output = vec![];
    let mut values = vec![
        json!({"type":"response.created","response":{"id":"resp_fixture","model":"fixture","status":"in_progress","output":[]}}),
    ];
    match mode {
        "interim_encrypted"=>values.push(json!({"type":"response.output_item.added","output_index":0,"item":{"type":"reasoning","id":"rs_0","summary":[],"encrypted_content":"opaque-interim"}})),
        "interim_done_encrypted"=>{
            values.push(json!({"type":"response.output_item.added","output_index":0,"item":{"type":"reasoning","id":"rs_0","summary":[],"encrypted_content":"partial"}}));
            values.push(json!({"type":"response.output_item.done","output_index":0,"item":{"type":"reasoning","id":"rs_0","summary":[],"encrypted_content":"complete-replay-state"}}));
        }
        "terminal_text"=>output.push(message),
        "terminal_tool"=>output.push(tool),
        "obfuscation"=>{
            values.push(json!({"type":"response.output_text.delta","output_index":0,"content_index":0,"item_id":"msg_0","delta":"reply","obfuscation":"padding-only"}));
            output.push(message);
        }
        "tool_identity_drift"=>{
            values.push(json!({"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","id":"fc_0","call_id":"call_0","name":"inspect","arguments":""}}));
            values.push(json!({"type":"response.function_call_arguments.delta","output_index":0,"item_id":"fc_0","delta":"{}"}));
            let mut changed=tool;changed["call_id"]="different".into();
            values.push(json!({"type":"response.output_item.done","output_index":0,"item":changed.clone()}));
            output.push(changed);
        }
        _=>panic!("scenario"),
    }
    values.push(json!({"type":"response.completed","response":{"id":"resp_fixture","model":"fixture","status":"completed","output":output,
        "usage":{"input_tokens":10,"output_tokens":4,"total_tokens":14,"input_tokens_details":{"cached_tokens":6,"cache_write_tokens":2},"output_tokens_details":{"reasoning_tokens":0}}}}));
    values
}
async fn fixture(mode: &str) -> (MockServer, Vec<Value>) {
    let values = frames(mode);
    let wire = values
        .iter()
        .map(|v| format!("data: {v}\n\n"))
        .collect::<String>();
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("x-request-id", "responses-fidelity-origin")
                .set_body_raw(wire, "text/event-stream"),
        )
        .expect(1)
        .mount(&server)
        .await;
    (server, values)
}
fn final_counts(context: &AttemptContext) {
    let usage = context.qualified_stream_evidence().unwrap().usage.unwrap();
    assert_eq!(usage.completeness, UsageCompleteness::Final);
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
}

#[tokio::test]
async fn portable_responses_refuses_interim_native_state_and_identity_drift_without_losing_source_counts(
) {
    for profile in ["responses", "codex"] {
        for mode in [
            "interim_encrypted",
            "interim_done_encrypted",
            "tool_identity_drift",
        ] {
            let (server, _) = fixture(mode).await;
            let (context, receiver) =
                AttemptContext::single_attempt("responses-fidelity", "e".repeat(64)).unwrap();
            let events = source(profile, server.uri())
                .stream_with_qualified_usage_and_attempts(
                    request(false),
                    Default::default(),
                    context.clone(),
                )
                .await
                .unwrap()
                .collect::<Vec<_>>()
                .await;
            assert!(
                events.iter().any(Result::is_err),
                "silent loss: {profile}/{mode}: {events:?}"
            );
            assert!(!events.iter().any(|v| matches!(v, Ok(Event::Finish { .. }))));
            final_counts(&context);
            assert_eq!(receiver.drain().len(), 2);
        }
    }
}

#[tokio::test]
async fn portable_responses_materializes_terminal_only_output_and_accepts_transport_padding() {
    for profile in ["responses", "codex"] {
        for mode in ["terminal_text", "terminal_tool", "obfuscation"] {
            let (server, _) = fixture(mode).await;
            let (context, receiver) =
                AttemptContext::single_attempt("responses-fidelity", "e".repeat(64)).unwrap();
            let events = source(profile, server.uri())
                .stream_with_qualified_usage_and_attempts(
                    request(false),
                    Default::default(),
                    context.clone(),
                )
                .await
                .unwrap()
                .collect::<Vec<_>>()
                .await;
            let events = events.into_iter().collect::<Result<Vec<_>, _>>().unwrap();
            if mode == "terminal_tool" {
                assert!(events.iter().any(|v|matches!(v,Event::ToolCallStart{id,name,..} if id=="call_0" && name=="inspect")),"lost terminal tool: {profile}");
                assert!(events
                    .iter()
                    .any(|v| matches!(v,Event::ToolCallArgumentsDelta{delta,..} if delta=="{}")));
            } else {
                assert_eq!(
                    events
                        .iter()
                        .filter_map(|v| match v {
                            Event::TextDelta { delta } => Some(delta.as_str()),
                            _ => None,
                        })
                        .collect::<String>(),
                    "reply",
                    "lost terminal text: {profile}/{mode}"
                );
            }
            final_counts(&context);
            assert_eq!(receiver.drain().len(), 2);
        }
    }
}

#[tokio::test]
async fn native_responses_stream_retains_original_frames_including_interim_state() {
    for profile in ["responses", "codex"] {
        for mode in [
            "interim_encrypted",
            "interim_done_encrypted",
            "terminal_text",
            "terminal_tool",
            "obfuscation",
        ] {
            let (server, values) = fixture(mode).await;
            let (context, _) =
                AttemptContext::single_attempt("responses-fidelity", "e".repeat(64)).unwrap();
            let events = source(profile, server.uri())
                .stream_with_qualified_usage_and_attempts(
                    request(true),
                    Default::default(),
                    context.clone(),
                )
                .await
                .unwrap()
                .collect::<Vec<_>>()
                .await;
            let originals = events
                .into_iter()
                .collect::<Result<Vec<_>, _>>()
                .unwrap()
                .into_iter()
                .filter_map(|v| match v {
                    Event::NativeFrame { frame } => frame.json_data(),
                    _ => None,
                })
                .collect::<Vec<_>>();
            assert_eq!(originals, values);
            final_counts(&context);
        }
    }
}

#[tokio::test]
async fn buffered_codex_does_not_drop_interim_state_or_terminal_only_portable_output() {
    for native in [false, true] {
        for mode in [
            "interim_encrypted",
            "interim_done_encrypted",
            "terminal_text",
            "terminal_tool",
            "obfuscation",
        ] {
            let (server, _) = fixture(mode).await;
            let (context, _) =
                AttemptContext::single_attempt("responses-fidelity", "e".repeat(64)).unwrap();
            let result = source("codex", server.uri())
                .complete_with_qualified_usage_and_attempts(
                    request(native),
                    Default::default(),
                    context.clone(),
                )
                .await;
            if mode == "interim_encrypted" || (mode == "interim_done_encrypted" && !native) {
                assert!(result.is_err(), "lost interim state: native={native}");
            } else if mode == "interim_done_encrypted" {
                let result = result.unwrap();
                assert_eq!(
                    result.response.extensions["openai_responses"]["output"][0]
                        ["encrypted_content"],
                    "complete-replay-state"
                );
            } else {
                let result = result.unwrap();
                if !native {
                    if mode == "terminal_tool" {
                        assert_eq!(result.response.output.tool_calls.len(), 1);
                    } else {
                        assert!(
                            matches!(result.response.output.content.as_ref(),Some(sandhi_core::MessageContent::Text(text)) if text=="reply")
                        );
                    }
                }
            }
            final_counts(&context);
        }
    }
}
