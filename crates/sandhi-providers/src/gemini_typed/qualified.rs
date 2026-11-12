//! Complete-event projection for tracked Gemini sources. The independent source
//! observer owns counts; this decoder never flushes or stops transport early.
use super::{
    ByteStream, ChatEventStream, ChatStreamEventV1 as Event, FinishReasonV1, ProviderError,
};
use futures_util::StreamExt;
use sandhi_core::EndpointFamilyV1;
use serde_json::Value;
use std::collections::BTreeSet;

pub(super) fn decode(mut raw: ByteStream, model: String, native: bool) -> ChatEventStream {
    Box::pin(async_stream::try_stream! {
        let mut decoder = crate::native_stream::SseDecoder::new(Some(EndpointFamilyV1::GeminiGenerateContent));
        let mut projection = Projection::default();
        let mut failure = None;
        while let Some(chunk) = raw.next().await {
            let chunk = chunk?;
            // A portable projection refusal does not cancel the physical source.
            // Drain it under the existing owner deadlines so clean source EOF
            // can still qualify actual counts independently of failed delivery.
            if failure.is_some() {continue;}
            decoder.push(&chunk.data);
            loop {
                let (line,mut frame) = match decoder.next_line() {
                    Ok(Some(event)) => event,
                    Ok(None) => break,
                    Err(error) => {failure=Some(error);break;}
                };
                if let Some(data) = line.strip_prefix(b"data:") {
                    let value = match sandhi_core::policy::strict_json(data) {
                        Ok(value)=>value,
                        Err(_)=>{failure=Some(invalid("invalid Gemini event JSON"));break;}
                    };
                    if let Some(error) = crate::native_stream::upstream_error(&value) {
                        if native { if let Some(frame) = frame.take() {yield Event::NativeFrame{frame};} }
                        failure=Some(error);break;
                    }
                    if !projection.started {
                        projection.started = true;
                        yield Event::ResponseStart {
                            id:value.get("responseId").and_then(Value::as_str).map(str::to_owned),
                            model:value.get("modelVersion").and_then(Value::as_str).unwrap_or(&model).into(),
                        };
                    }
                    if native {
                        // Native consumers get every original complete event,
                        // including multiple candidates and opaque state. Avoid
                        // inventing a lossy portable projection beside those bytes.
                        if let Some(reason) = value.pointer("/candidates/0/finishReason").and_then(Value::as_str)
                            .filter(|reason|!reason.is_empty() && *reason!="FINISH_REASON_UNSPECIFIED") {
                            projection.finish = Some(match reason {
                                "STOP" => FinishReasonV1::Stop,
                                "MAX_TOKENS" => FinishReasonV1::Length,
                                _ => FinishReasonV1::Unknown,
                            });
                        }
                        if value.pointer("/promptFeedback/blockReason").and_then(Value::as_str)
                            .is_some_and(|reason|reason!="BLOCK_REASON_UNSPECIFIED") {
                            projection.finish = Some(FinishReasonV1::ContentFilter);
                        }
                    } else {
                        match projection.event(&value) {
                            Ok(events)=>for event in events {yield event;},
                            Err(error)=>{failure=Some(error);break;}
                        }
                    }
                }
                if native { if let Some(frame) = frame {yield Event::NativeFrame{frame};} }
            }
            if decoder.over_budget() {failure.get_or_insert_with(||invalid("Gemini SSE event exceeds the stream limit"));}
        }
        if let Some(error)=failure {Err(error)?;}
        // EOF cannot supply a missing SSE blank line or a generation terminal.
        // The outer qualifier independently requires terminal source evidence.
        yield Event::Finish{reason:projection.finish.ok_or_else(||invalid("Gemini stream has no portable finish"))?};
    })
}

#[derive(Default)]
struct Projection {
    started: bool,
    finish: Option<FinishReasonV1>,
    calls: BTreeSet<String>,
    next_call: u32,
    generated_ids: super::GeneratedToolIds,
}

impl Projection {
    fn event(&mut self, value: &Value) -> Result<Vec<Event>, ProviderError> {
        fields(
            value,
            &[
                "responseId",
                "modelVersion",
                "candidates",
                "usageMetadata",
                "createTime",
                "promptFeedback",
            ],
        )?;
        let mut events = Vec::new();
        if let Some(feedback) = value.get("promptFeedback") {
            fields(feedback, &["blockReason"])?;
            if let Some(reason) = feedback.get("blockReason") {
                if !matches!(reason.as_str(), Some("BLOCK_REASON_UNSPECIFIED")) {
                    self.finish = Some(FinishReasonV1::ContentFilter);
                }
            }
        }
        let Some(candidates) = value.get("candidates") else {
            return Ok(events);
        };
        let candidates = candidates
            .as_array()
            .ok_or_else(|| invalid("invalid Gemini candidates"))?;
        if candidates.is_empty() {
            return Ok(events);
        };
        if candidates.len() != 1 {
            return Err(invalid(
                "multiple Gemini candidates require native delivery",
            ));
        }
        let candidate = &candidates[0];
        fields(candidate, &["index", "content", "finishReason"])?;
        if candidate
            .get("index")
            .is_some_and(|v| v.as_u64() != Some(0))
            || self.finish.is_some()
        {
            return Err(invalid("invalid Gemini candidate lifecycle"));
        }
        if let Some(content) = candidate.get("content") {
            fields(content, &["role", "parts"])?;
            if content
                .get("role")
                .is_some_and(|role| role.as_str() != Some("model"))
            {
                return Err(invalid("invalid Gemini output role"));
            }
            if let Some(parts) = content.get("parts") {
                let parts = parts
                    .as_array()
                    .ok_or_else(|| invalid("invalid Gemini output parts"))?;
                for part in parts {
                    fields(part, &["text", "thought", "functionCall"])?;
                    if let Some(thought) = part.get("thought") {
                        if !thought.is_boolean() {
                            return Err(invalid("invalid Gemini thought flag"));
                        }
                    }
                    match (part.get("text"), part.get("functionCall")) {
                        (Some(Value::String(text)), None) => {
                            events.push(
                                if part.get("thought").and_then(Value::as_bool) == Some(true) {
                                    Event::ReasoningDelta {
                                        delta: text.clone(),
                                    }
                                } else {
                                    Event::TextDelta {
                                        delta: text.clone(),
                                    }
                                },
                            );
                        }
                        (None, Some(call))
                            if part.get("thought").is_none_or(|v| v == &Value::Bool(false)) =>
                        {
                            fields(call, &["id", "name", "args"])?;
                            let name = string(call, "name")?.to_owned();
                            let args =
                                call.get("args").filter(|v| v.is_object()).ok_or_else(|| {
                                    invalid("Gemini function arguments must be an explicit object")
                                })?;
                            if self.next_call >= 1024 {
                                return Err(invalid("too many Gemini function calls"));
                            }
                            let index = self.next_call;
                            let id = if call.get("id").is_some() {
                                string(call, "id")?.into()
                            } else {
                                self.generated_ids.id(index as usize)?
                            };
                            if !self.calls.insert(id.clone()) {
                                return Err(invalid("duplicate Gemini function call identity"));
                            }
                            self.next_call += 1;
                            events.push(Event::ToolCallStart { index, id, name });
                            events.push(Event::ToolCallArgumentsDelta {
                                index,
                                delta: args.to_string(),
                            });
                            events.push(Event::ToolCallEnd { index });
                        }
                        _ => return Err(invalid("Gemini output part requires native delivery")),
                    }
                }
            }
        }
        if let Some(reason) = candidate.get("finishReason") {
            self.finish = match reason.as_str() {
                Some("" | "FINISH_REASON_UNSPECIFIED") => None,
                Some("STOP") => Some(if self.next_call > 0 {
                    FinishReasonV1::ToolCalls
                } else {
                    FinishReasonV1::Stop
                }),
                Some("MAX_TOKENS") => Some(FinishReasonV1::Length),
                Some(
                    "SAFETY"
                    | "RECITATION"
                    | "BLOCKLIST"
                    | "PROHIBITED_CONTENT"
                    | "SPII"
                    | "MODEL_ARMOR"
                    | "IMAGE_SAFETY"
                    | "IMAGE_PROHIBITED_CONTENT"
                    | "IMAGE_RECITATION",
                ) => Some(FinishReasonV1::ContentFilter),
                _ => return Err(invalid("Gemini finish reason requires native delivery")),
            };
        }
        Ok(events)
    }
}

fn fields(value: &Value, allowed: &[&str]) -> Result<(), ProviderError> {
    let object = value
        .as_object()
        .ok_or_else(|| invalid("invalid Gemini event object"))?;
    if object.keys().any(|key| !allowed.contains(&key.as_str())) {
        return Err(invalid("Gemini output fields require native delivery"));
    }
    Ok(())
}
fn string<'a>(value: &'a Value, key: &str) -> Result<&'a str, ProviderError> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|v| !v.is_empty() && v.len() <= 256 && !v.chars().any(char::is_control))
        .ok_or_else(|| invalid("invalid Gemini function identity"))
}
fn invalid(message: &str) -> ProviderError {
    ProviderError::Transport(message.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn stream(bytes: &[u8], split: usize) -> ByteStream {
        Box::pin(futures_util::stream::iter(vec![
            Ok(crate::StreamChunk {
                data: bytes::Bytes::copy_from_slice(&bytes[..split]),
                ..Default::default()
            }),
            Ok(crate::StreamChunk {
                data: bytes::Bytes::copy_from_slice(&bytes[split..]),
                ..Default::default()
            }),
        ]))
    }
    fn frame(value: Value) -> String {
        serde_json::to_string_pretty(&value)
            .unwrap()
            .lines()
            .map(|line| format!("data: {line}\n"))
            .collect::<String>()
            + "\n"
    }

    #[tokio::test]
    async fn qualified_gemini_projection_uses_complete_events_and_stable_tool_indices() {
        let wire = format!(
            "\u{feff}: ping\n\n{}{}{}{}",
            frame(
                json!({"responseId":"g1","candidates":[{"content":{"role":"model","parts":[{"text":"雪"}]}}]})
            ),
            frame(
                json!({"candidates":[{"content":{"parts":[{"functionCall":{"name":"lookup","args":{"a":1}}}]}}]})
            ),
            frame(
                json!({"candidates":[{"content":{"parts":[{"functionCall":{"id":"call-b","name":"lookup","args":{}}}]}}]})
            ),
            frame(json!({"candidates":[{"finishReason":"STOP"}],"usageMetadata":{}}))
        );
        for newline in ["\n", "\r\n", "\r"] {
            let wire = wire.replace('\n', newline);
            for native in [false, true] {
                for split in 0..=wire.len() {
                    let events = decode(stream(wire.as_bytes(), split), "fixture".into(), native)
                        .collect::<Vec<_>>()
                        .await
                        .into_iter()
                        .collect::<Result<Vec<_>, _>>()
                        .unwrap();
                    assert_eq!(
                        events
                            .iter()
                            .any(|e| matches!(e, Event::NativeFrame { .. })),
                        native
                    );
                    assert!(
                        !events.iter().any(|e| matches!(e, Event::Usage { .. })),
                        "decoder cannot authorize counts"
                    );
                    if !native {
                        let starts = events
                            .iter()
                            .filter_map(|e| match e {
                                Event::ToolCallStart { index, id, .. } => {
                                    Some((*index, id.as_str()))
                                }
                                _ => None,
                            })
                            .collect::<Vec<_>>();
                        assert_eq!(starts.len(), 2);
                        assert_eq!(starts[0].0, 0);
                        assert!(
                            starts[0].1.starts_with("gemini_call_") && starts[0].1.ends_with("_0")
                        );
                        assert_eq!(starts[1], (1, "call-b"));
                        assert!(matches!(
                            events.last(),
                            Some(Event::Finish {
                                reason: FinishReasonV1::ToolCalls
                            })
                        ));
                        assert!(events
                            .iter()
                            .any(|e| matches!(e,Event::TextDelta{delta} if delta=="雪")));
                    }
                }
            }
        }
    }

    #[tokio::test]
    async fn qualified_gemini_native_state_and_bad_tools_are_preserved_or_refused() {
        let mut values = vec![
            json!({"candidates":[{"content":{"parts":[{"text":"x","thoughtSignature":"opaque"}]},"finishReason":"STOP"}]}),
            json!({"candidates":[{"content":{"parts":[{"inlineData":{"mimeType":"image/png","data":"opaque"}}]},"finishReason":"STOP"}]}),
            json!({"candidates":[{"finishReason":"STOP","groundingMetadata":{"opaque":true}}]}),
            json!({"candidates":[{"index":0,"finishReason":"STOP"},{"index":1,"finishReason":"STOP"}]}),
        ];
        for args in [Value::Null, json!([]), json!("{}"), json!(42)] {
            values.push(json!({"candidates":[{"content":{"parts":[{"functionCall":{"name":"lookup","args":args}}]},"finishReason":"STOP"}]}));
        }
        values.push(json!({"candidates":[{"content":{"parts":[{"functionCall":{"name":"lookup"}}]},"finishReason":"STOP"}]}));
        values.push(json!({"candidates":[{"content":{"parts":[{"functionCall":{"name":"lookup","args":{},"partialArgs":[{}]}}]},"finishReason":"STOP"}]}));
        for value in values {
            let wire = frame(value.clone());
            for native in [false, true] {
                let events = decode(
                    stream(wire.as_bytes(), wire.len()),
                    "fixture".into(),
                    native,
                )
                .collect::<Vec<_>>()
                .await;
                assert_eq!(events.iter().any(Result::is_err), !native, "{value}");
                if native {
                    let frames = events
                        .iter()
                        .filter_map(|e| match e {
                            Ok(Event::NativeFrame { frame }) => frame.json_data(),
                            _ => None,
                        })
                        .collect::<Vec<_>>();
                    assert_eq!(frames, vec![value.clone()]);
                } else {
                    assert!(!events
                        .iter()
                        .any(|e| matches!(e, Ok(Event::ToolCallStart { .. }))));
                }
            }
        }
    }

    #[tokio::test]
    async fn qualified_gemini_never_flushes_a_finish_at_truncated_eof() {
        let wire = frame(json!({"candidates":[{"finishReason":"STOP"}]}));
        for trim in [1, 2, 10] {
            let bytes = &wire.as_bytes()[..wire.len() - trim];
            let events = decode(stream(bytes, bytes.len()), "fixture".into(), false)
                .collect::<Vec<_>>()
                .await;
            assert!(events.iter().any(Result::is_err));
            assert!(!events.iter().any(|e| matches!(e, Ok(Event::Finish { .. }))));
        }
    }
}
