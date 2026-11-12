//! Messages projection shared by embedded and accounted calls. Counts come
//! only from the shared response-owned source observer.
use super::{
    decode_stop_reason, ByteStream, ChatEventStream, ChatStreamEventV1 as Event, ProviderError,
};
use futures_util::StreamExt;
use sandhi_core::{EndpointFamilyV1, FinishReasonV1};
use serde_json::Value;
use std::collections::BTreeMap;

pub(crate) fn decode(raw: ByteStream, model: String, native: bool) -> ChatEventStream {
    crate::cache_read::decode_with_observation_policy(
        raw,
        model,
        move |raw, model| decode_inner(raw, model, native),
        native,
    )
}

fn decode_inner(mut raw: ByteStream, model: String, native: bool) -> ChatEventStream {
    Box::pin(async_stream::try_stream! {
        let mut decoder=crate::native_stream::SseDecoder::new(Some(EndpointFamilyV1::AnthropicMessages));
        let mut projection=Projection::default();
        let mut failure=None;
        while let Some(chunk)=raw.next().await {
            let chunk=match chunk {Ok(chunk)=>chunk,Err(error)=>Err(failure.take().unwrap_or(error))?,};
            if failure.is_some() {continue;}
            decoder.push(&chunk.data);
            loop {
                let (line,mut frame)=match decoder.next_line() {
                    Ok(Some(event))=>event,
                    Ok(None)=>break,
                    Err(error)=>{failure=Some(error);break;},
                };
                // Continue polling the source through EOF after message_stop.
                // The independent observer detects any later data and framing.
                if projection.stopped { continue; }
                if let Some(data)=line.strip_prefix(b"data:") {
                    let value=match sandhi_core::policy::strict_json(data) {
                        Ok(value)=>value,
                        Err(_)=>{failure=Some(invalid("invalid Messages event JSON"));break;},
                    };
                    if let Some(error)=crate::native_stream::upstream_error(&value) {
                        if native { if let Some(frame)=frame.take() { yield Event::NativeFrame{frame}; } }
                        Err(error)?;
                    }
                    match projection.event(&value,&model,native) {
                        Ok(events)=>for event in events {yield event;},
                        Err(error)=>{failure=Some(error);break;},
                    }
                }
                if native { if let Some(frame)=frame { yield Event::NativeFrame{frame}; } }
            }
            if decoder.over_budget() {failure.get_or_insert_with(||invalid("Messages SSE frame exceeds the stream limit"));}
        }
        // Refusal is a delivery failure, not cancellation of the same bounded
        // source. Preserve its final actual measurement before surfacing refusal.
        if let Some(error)=failure {Err(error)?;}
        // No flush at EOF: an unfinished SSE event cannot become content or a
        // protocol terminal. The outer qualifier publishes final usage/finish.
        if !projection.stopped { Err(invalid("Messages stream ended before message_stop"))?; }
        yield Event::Finish{reason:projection.finish.expect("validated message_stop")};
    })
}

#[derive(Default)]
struct Projection {
    started: bool,
    stopped: bool,
    next_index: u32,
    blocks: BTreeMap<u32, Block>,
    finish: Option<FinishReasonV1>,
}
enum Block {
    Text,
    Thinking,
    Tool { initial: bool, arguments: bool },
    Opaque,
}

impl Projection {
    fn event(
        &mut self,
        value: &Value,
        model: &str,
        native: bool,
    ) -> Result<Vec<Event>, ProviderError> {
        let kind = string(value, "type")?;
        if matches!(
            kind,
            "content_block_start"
                | "content_block_delta"
                | "content_block_stop"
                | "message_delta"
                | "message_stop"
        ) && (!self.started || self.finish.is_some() && kind != "message_stop")
        {
            return Err(invalid("invalid Messages event order"));
        }
        let mut events = Vec::new();
        match kind {
            "ping" => {}
            "message_start" => {
                if self.started {
                    return Err(invalid("duplicate Messages start"));
                }
                let message = value
                    .get("message")
                    .filter(|v| v.is_object())
                    .ok_or_else(|| invalid("missing Messages start object"))?;
                if !native {
                    portable_fields(
                        message,
                        &[
                            "id",
                            "type",
                            "role",
                            "model",
                            "content",
                            "stop_reason",
                            "stop_sequence",
                            "usage",
                        ],
                    )?;
                    if message
                        .get("content")
                        .is_some_and(|v| !v.as_array().is_some_and(Vec::is_empty))
                    {
                        return Err(invalid(
                            "initial Messages content requires native projection",
                        ));
                    }
                }
                events.push(Event::ResponseStart {
                    id: Some(string(message, "id")?.into()),
                    model: message
                        .get("model")
                        .and_then(Value::as_str)
                        .unwrap_or(model)
                        .into(),
                });
                self.started = true;
            }
            "content_block_start" => {
                let index = index(value)?;
                // Bound retained lifecycle state using the existing 8 MiB
                // projection budget and its minimum per-event allowance.
                if index != self.next_index || index as usize >= crate::MAX_STREAM_LINE_BYTES / 128
                {
                    return Err(invalid("invalid or excessive Messages block index"));
                }
                self.next_index += 1;
                let block = value
                    .get("content_block")
                    .filter(|v| v.is_object())
                    .ok_or_else(|| invalid("missing Messages content block"))?;
                let block_kind = match string(block, "type")? {
                    "text" => {
                        if !native {
                            portable_fields(block, &["type", "text", "citations"])?;
                            if block.get("citations").is_some_and(|v| {
                                !v.is_null() && !v.as_array().is_some_and(Vec::is_empty)
                            }) {
                                return Err(invalid(
                                    "Messages citations require native projection",
                                ));
                            }
                        }
                        let text = string(block, "text")?;
                        if !text.is_empty() {
                            events.push(Event::TextDelta { delta: text.into() });
                        }
                        Block::Text
                    }
                    "thinking" => {
                        if !native {
                            portable_fields(block, &["type", "thinking", "signature"])?;
                            if block
                                .get("signature")
                                .is_some_and(|v| !v.is_null() && v.as_str() != Some(""))
                            {
                                return Err(invalid(
                                    "Messages thinking signatures require native projection",
                                ));
                            }
                        }
                        let thinking = string(block, "thinking")?;
                        if !thinking.is_empty() {
                            events.push(Event::ReasoningDelta {
                                delta: thinking.into(),
                            });
                        }
                        Block::Thinking
                    }
                    "tool_use" => {
                        if !native {
                            portable_fields(block, &["type", "id", "name", "input"])?;
                        }
                        let input = block
                            .get("input")
                            .and_then(Value::as_object)
                            .ok_or_else(|| invalid("Messages tool input must be an object"))?;
                        events.push(Event::ToolCallStart {
                            index,
                            id: string(block, "id")?.into(),
                            name: string(block, "name")?.into(),
                        });
                        let initial = !input.is_empty();
                        if initial {
                            events.push(Event::ToolCallArgumentsDelta {
                                index,
                                delta: Value::Object(input.clone()).to_string(),
                            });
                        }
                        Block::Tool {
                            initial,
                            arguments: initial,
                        }
                    }
                    _ if native => Block::Opaque,
                    _ => return Err(invalid("Messages content block requires native projection")),
                };
                self.blocks.insert(index, block_kind);
            }
            "content_block_delta" => {
                let index = index(value)?;
                let block = self
                    .blocks
                    .get_mut(&index)
                    .ok_or_else(|| invalid("Messages delta has no open block"))?;
                let delta = value
                    .get("delta")
                    .filter(|v| v.is_object())
                    .ok_or_else(|| invalid("missing Messages content delta"))?;
                let kind = string(delta, "type")?;
                match (block, kind) {
                    (Block::Text, "text_delta") => {
                        if !native {
                            portable_fields(delta, &["type", "text"])?;
                        }
                        events.push(Event::TextDelta {
                            delta: string(delta, "text")?.into(),
                        });
                    }
                    (Block::Thinking, "thinking_delta") => {
                        if !native {
                            portable_fields(delta, &["type", "thinking"])?;
                        }
                        events.push(Event::ReasoningDelta {
                            delta: string(delta, "thinking")?.into(),
                        });
                    }
                    (Block::Thinking, "signature_delta") if native => {}
                    (Block::Tool { initial, arguments }, "input_json_delta") => {
                        if !native {
                            portable_fields(delta, &["type", "partial_json"])?;
                        }
                        let text = string(delta, "partial_json")?;
                        if *initial && !text.is_empty() {
                            return Err(invalid(
                                "Messages tool input and argument deltas conflict",
                            ));
                        }
                        if !text.is_empty() {
                            *arguments = true;
                        }
                        events.push(Event::ToolCallArgumentsDelta {
                            index,
                            delta: text.into(),
                        });
                    }
                    _ if native => {}
                    _ => {
                        return Err(invalid(
                            "Messages delta requires native projection or has a mismatched block",
                        ))
                    }
                }
            }
            "content_block_stop" => {
                let index = index(value)?;
                let block = self
                    .blocks
                    .remove(&index)
                    .ok_or_else(|| invalid("Messages stop has no open block"))?;
                if let Block::Tool { arguments, .. } = block {
                    // This is the actual, explicitly reported empty initial
                    // object, not a repair for incomplete/non-object arguments.
                    if !arguments {
                        events.push(Event::ToolCallArgumentsDelta {
                            index,
                            delta: "{}".into(),
                        });
                    }
                    events.push(Event::ToolCallEnd { index });
                }
            }
            "message_delta" => {
                if !self.blocks.is_empty() {
                    return Err(invalid("Messages delta precedes content block completion"));
                }
                let delta = value
                    .get("delta")
                    .filter(|v| v.is_object())
                    .ok_or_else(|| invalid("missing Messages delta"))?;
                if !native {
                    portable_fields(delta, &["stop_reason", "stop_sequence"])?;
                }
                if let Some(reason) = delta.get("stop_reason").filter(|v| !v.is_null()) {
                    let reason = reason
                        .as_str()
                        .ok_or_else(|| invalid("invalid Messages stop reason"))?;
                    if !native
                        && !matches!(
                            reason,
                            "end_turn"
                                | "stop_sequence"
                                | "max_tokens"
                                | "model_context_window_exceeded"
                                | "tool_use"
                        )
                    {
                        return Err(invalid("Messages stop reason requires native projection"));
                    }
                    self.finish = Some(decode_stop_reason(reason));
                }
            }
            "message_stop" => {
                if !self.blocks.is_empty() || self.finish.is_none() {
                    return Err(invalid("incomplete Messages lifecycle"));
                }
                self.stopped = true;
            }
            _ if native => {}
            _ => return Err(invalid("Messages event requires native projection")),
        }
        Ok(events)
    }
}

fn string<'a>(value: &'a Value, key: &str) -> Result<&'a str, ProviderError> {
    value
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| invalid("missing or invalid Messages string"))
}
fn index(value: &Value) -> Result<u32, ProviderError> {
    value
        .get("index")
        .and_then(Value::as_u64)
        .and_then(|n| u32::try_from(n).ok())
        .ok_or_else(|| invalid("missing or invalid Messages block index"))
}
fn portable_fields(value: &Value, allowed: &[&str]) -> Result<(), ProviderError> {
    if value.as_object().is_some_and(|fields| {
        fields
            .iter()
            .any(|(key, value)| !value.is_null() && !allowed.contains(&key.as_str()))
    }) {
        return Err(invalid("Messages extension requires native projection"));
    }
    Ok(())
}
fn invalid(message: &'static str) -> ProviderError {
    ProviderError::OutputProjection(message.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use sandhi_core::UsageCompleteness;
    use serde_json::json;

    fn wire() -> String {
        [json!({"type":"message_start","message":{"id":"msg_fixture","type":"message","role":"assistant","content":[],
            "usage":{"input_tokens":2,"output_tokens":1,"cache_read_input_tokens":6,"cache_creation_input_tokens":2,"output_tokens_details":{"thinking_tokens":0}}}}),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":"re雪"}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"ply"}}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"message_delta","delta":{"stop_reason":"max_tokens"},"usage":{"output_tokens":4}}),
        json!({"type":"message_stop"})]
        .iter().map(|value| format!("event: {}\n{}\n",value["type"].as_str().unwrap(),
            serde_json::to_string_pretty(value).unwrap().lines().map(|line|format!("data: {line}\n")).collect::<String>())).collect()
    }

    async fn events(chunks: Vec<bytes::Bytes>, native: bool) -> Vec<Result<Event, ProviderError>> {
        let source = crate::attempt::observe_qualified_stream(
            None,
            "fixture",
            Some("fixture"),
            EndpointFamilyV1::AnthropicMessages,
            Default::default(),
            Default::default(),
            |_| async move {
                let raw: crate::raw::RawChunkStream =
                    Box::pin(futures_util::stream::iter(chunks.into_iter().map(Ok)));
                Ok((raw, None))
            },
        )
        .await
        .unwrap();
        crate::qualified_stream::decode(
            decode(source.stream, "fixture".into(), native),
            source.observation,
        )
        .collect()
        .await
    }

    #[tokio::test]
    async fn messages_qualified_projection_is_invariant_at_every_multiline_byte_split() {
        for newline in ["\n", "\r\n", "\r"] {
            let wire = wire().replace('\n', newline);
            for native in [false, true] {
                for split in 0..=wire.len() {
                    let projected = events(
                        vec![
                            bytes::Bytes::copy_from_slice(&wire.as_bytes()[..split]),
                            bytes::Bytes::copy_from_slice(&wire.as_bytes()[split..]),
                        ],
                        native,
                    )
                    .await;
                    let projected: Vec<_> = projected.into_iter().map(Result::unwrap).collect();
                    assert_eq!(
                        projected
                            .iter()
                            .filter_map(|e| match e {
                                Event::TextDelta { delta } => Some(delta.as_str()),
                                _ => None,
                            })
                            .collect::<String>(),
                        "re雪ply"
                    );
                    assert_eq!(
                        projected
                            .iter()
                            .any(|e| matches!(e, Event::NativeFrame { .. })),
                        native
                    );
                    assert!(matches!(
                        projected.last(),
                        Some(Event::Finish {
                            reason: FinishReasonV1::Length
                        })
                    ));
                    let totals: Vec<_> = projected
                        .iter()
                        .filter_map(|e| match e {
                            Event::Usage { usage }
                                if usage.completeness == UsageCompleteness::Final =>
                            {
                                Some(usage)
                            }
                            _ => None,
                        })
                        .collect();
                    assert_eq!(totals.len(), 1);
                    assert_eq!(
                        (
                            totals[0].tokens_in,
                            totals[0].cache_read_tokens,
                            totals[0].cache_creation_tokens,
                            totals[0].tokens_out
                        ),
                        (2, 6, 2, 4)
                    );
                    assert_eq!(totals[0].reasoning_tokens, Some(0));
                }
            }
        }
    }

    #[tokio::test]
    async fn messages_qualified_projection_never_flushes_a_truncated_terminal_at_eof() {
        let wire = wire();
        for trim in [1, 2, 10] {
            for native in [false, true] {
                let projected = events(
                    vec![bytes::Bytes::copy_from_slice(
                        &wire.as_bytes()[..wire.len() - trim],
                    )],
                    native,
                )
                .await;
                assert!(projected.last().unwrap().is_err());
                assert!(!projected
                    .iter()
                    .any(|e| matches!(e, Ok(Event::Finish { .. }))));
                assert!(!projected.iter().any(|e|matches!(e,Ok(Event::Usage{usage})if usage.completeness==UsageCompleteness::Final)));
            }
        }
    }
}
