//! Complete-event portable Chat projection over the existing qualified source.
//! Transport deadlines, source measurements and settlement remain with that owner.
use crate::buffered_fidelity::{
    array, fields, id, optional_role, optional_text, string_args, text,
};
use crate::{ByteStream, ChatEventStream, ProviderError};
use futures_util::StreamExt;
use sandhi_core::{ChatStreamEventV1 as Event, EndpointFamilyV1 as Family, FinishReasonV1};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

fn invalid() -> ProviderError {
    ProviderError::OutputProjection(
        "Chat stream output requires compatible native delivery or has invalid portable content"
            .into(),
    )
}

pub(crate) fn decode(raw: ByteStream, model: String) -> ChatEventStream {
    crate::cache_read::decode_with_observation(raw, model, decode_inner)
}

fn decode_inner(mut raw: ByteStream, model: String) -> ChatEventStream {
    Box::pin(async_stream::try_stream! {
        let mut decoder = crate::native_stream::SseDecoder::new(Some(Family::OpenaiChatCompletions));
        let mut projection = Projection::default();
        let mut failure = None;
        let mut started = false;
        while let Some(chunk) = raw.next().await {
            let chunk = chunk?;
            // Keep polling the SAME bounded source after projection refusal. The
            // source observer must see real terminal counters and HTTP closure.
            if failure.is_some() {continue;}
            decoder.push(&chunk.data);
            loop {
                let (line, _) = match decoder.next_line() {
                    Ok(Some(event)) => event,
                    Ok(None) => break,
                    Err(error) => {failure=Some(error);break;}
                };
                let Some(data) = line.strip_prefix(b"data:") else {
                    continue;
                };
                let data = data.strip_prefix(b" ").unwrap_or(data);
                if data == b"[DONE]" {
                    if projection.done {failure=Some(invalid());break;}
                    projection.done = true;
                } else {
                    let value = match sandhi_core::policy::strict_json(data) {
                        Ok(value)=>value,
                        Err(_)=>{failure=Some(invalid());break;}
                    };
                    if let Some(error) = crate::native_stream::upstream_error(&value) {
                        failure=Some(error);break;
                    }
                    if !started {
                        started=true;
                        yield Event::ResponseStart {
                            id:value.get("id").and_then(Value::as_str).map(str::to_owned),
                            model:value.get("model").and_then(Value::as_str).unwrap_or(&model).into(),
                        };
                    }
                    match projection.event(&value) {
                        Ok(events)=>for event in events {yield event;},
                        Err(error)=>{failure=Some(error);break;}
                    }
                }
            }
            if decoder.over_budget() {failure.get_or_insert_with(invalid);}
        }
        if let Some(error)=failure {Err(error)?;}
        if !projection.done {Err(invalid())?;}
        yield Event::Finish{reason:projection.finish.ok_or_else(invalid)?};
    })
}

#[derive(Default)]
struct Call {
    id: String,
    name: String,
    arguments: String,
}
#[derive(Default)]
struct Projection {
    done: bool,
    finish: Option<FinishReasonV1>,
    calls: BTreeMap<u32, Call>,
    order: Vec<u32>,
    retained: usize,
}
impl Projection {
    fn event(&mut self, value: &Value) -> Result<Vec<Event>, ProviderError> {
        if self.done {
            return Err(invalid());
        }
        fields(
            value,
            &[
                "id",
                "object",
                "created",
                "model",
                "choices",
                "usage",
                "system_fingerprint",
                "service_tier",
                "request_id",
            ],
        )?;
        let choices = array(value, "choices")?;
        if choices.is_empty() {
            return if value.get("usage").is_some_and(|usage| usage.is_object()) {
                Ok(vec![])
            } else {
                Err(invalid())
            };
        }
        if choices.len() != 1 || self.finish.is_some() {
            return Err(invalid());
        }
        let choice = &choices[0];
        fields(choice, &["index", "delta", "finish_reason"])?;
        if choice.get("index").is_some_and(|v| v.as_u64() != Some(0)) {
            return Err(invalid());
        }
        let delta = choice.get("delta").ok_or_else(invalid)?;
        fields(
            delta,
            &[
                "role",
                "content",
                "refusal",
                "reasoning_content",
                "reasoning",
                "tool_calls",
            ],
        )?;
        optional_role(delta, "role", "assistant")?;
        let mut events = Vec::new();
        if let Some(delta) = optional_text(delta, "content")? {
            events.push(Event::TextDelta { delta });
        }
        let reasoning = optional_text(delta, "reasoning_content")?;
        let alternate = optional_text(delta, "reasoning")?;
        if reasoning
            .as_ref()
            .zip(alternate.as_ref())
            .is_some_and(|(a, b)| a != b)
        {
            return Err(invalid());
        }
        if let Some(delta) = reasoning.or(alternate) {
            events.push(Event::ReasoningDelta { delta });
        }
        if let Some(delta) = optional_text(delta, "refusal")? {
            events.push(Event::RefusalDelta { delta });
        }
        // Calls are assembled until their identities and arguments can be
        // validated. Emitting later prose ahead of them would reorder output.
        if !self.calls.is_empty()
            && events.iter().any(|event| match event {
                Event::TextDelta { delta }
                | Event::ReasoningDelta { delta }
                | Event::RefusalDelta { delta } => !delta.is_empty(),
                _ => false,
            })
        {
            return Err(invalid());
        }
        if delta.get("tool_calls").is_some_and(|v| !v.is_null()) {
            for call in array(delta, "tool_calls")? {
                fields(call, &["index", "id", "type", "function"])?;
                if call
                    .get("type")
                    .is_some_and(|v| !v.is_null() && v.as_str() != Some("function"))
                {
                    return Err(invalid());
                }
                let index = call
                    .get("index")
                    .and_then(Value::as_u64)
                    .and_then(|i| u32::try_from(i).ok())
                    .ok_or_else(invalid)?;
                if !self.calls.contains_key(&index) {
                    if self.calls.len() >= 1024 {
                        return Err(invalid());
                    }
                    self.order.push(index);
                }
                let target = self.calls.entry(index).or_default();
                if let Some(id) = optional_text(call, "id")? {
                    self.retained = self.retained.saturating_add(id.len());
                    target.id.push_str(&id);
                }
                if let Some(function) = call.get("function").filter(|v| !v.is_null()) {
                    fields(function, &["name", "arguments"])?;
                    if let Some(name) = optional_text(function, "name")? {
                        self.retained = self.retained.saturating_add(name.len());
                        target.name.push_str(&name);
                    }
                    if let Some(args) = optional_text(function, "arguments")? {
                        self.retained = self.retained.saturating_add(args.len());
                        target.arguments.push_str(&args);
                    }
                }
                if self.retained > crate::MAX_STREAM_LINE_BYTES
                    || target.id.len() > 256
                    || target.name.len() > 256
                {
                    return Err(invalid());
                }
            }
        }
        if choice.get("finish_reason").is_some_and(|v| !v.is_null()) {
            let reason = text(choice, "finish_reason")?;
            if !matches!(reason, "stop" | "length" | "content_filter" | "tool_calls") {
                return Err(invalid());
            }
            if reason == "tool_calls" && self.calls.is_empty() {
                return Err(invalid());
            }
            let mut ids = BTreeSet::new();
            for index in &self.order {
                let call = &self.calls[index];
                let identity = serde_json::json!({"id":call.id,"name":call.name});
                id(&identity, "id")?;
                id(&identity, "name")?;
                if !ids.insert(&call.id) {
                    return Err(invalid());
                }
                string_args(&Value::String(call.arguments.clone()))?;
            }
            for index in self.order.drain(..) {
                let call = self.calls.remove(&index).expect("ordered open call");
                events.push(Event::ToolCallStart {
                    index,
                    id: call.id,
                    name: call.name,
                });
                events.push(Event::ToolCallArgumentsDelta {
                    index,
                    delta: call.arguments,
                });
                events.push(Event::ToolCallEnd { index });
            }
            self.finish = Some(crate::typed::decode_finish_reason(reason));
        }
        Ok(events)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn chunk(delta: Value, finish: Value) -> Value {
        json!({"id":"fixture","object":"chat.completion.chunk","choices":[{"index":0,"delta":delta,"finish_reason":finish}]})
    }
    #[tokio::test]
    async fn fragmented_tool_identity_and_arguments_survive_complete_events_at_every_byte_split() {
        let events = [
            chunk(
                json!({"role":"assistant","tool_calls":[{"index":9,"id":"call_","type":"function","function":{"name":"look","arguments":"{"}}]}),
                Value::Null,
            ),
            chunk(
                json!({"tool_calls":[{"index":9,"id":"123","function":{"name":"up","arguments":"}"}}]}),
                "tool_calls".into(),
            ),
        ];
        for newline in ["\n", "\r\n", "\r"] {
            let wire = (events
                .iter()
                .map(|value| {
                    serde_json::to_string_pretty(value)
                        .unwrap()
                        .lines()
                        .map(|line| format!("data: {line}\n"))
                        .collect::<String>()
                        + "\n"
                })
                .collect::<String>()
                + "data: [DONE]\n\n")
                .replace('\n', newline);
            for split in 0..=wire.len() {
                let chunks = vec![
                    wire.as_bytes()[..split].to_vec(),
                    wire.as_bytes()[split..].to_vec(),
                ];
                let raw: ByteStream =
                    Box::pin(futures_util::stream::iter(chunks.into_iter().map(|data| {
                        Ok(crate::StreamChunk {
                            data: bytes::Bytes::from(data),
                            ..crate::StreamChunk::default()
                        })
                    })));
                let actual = decode(raw, "fixture".into()).collect::<Vec<_>>().await;
                let actual = actual.into_iter().collect::<Result<Vec<_>, _>>().unwrap();
                assert!(actual.iter().any(|event|matches!(event,Event::ToolCallStart{index:9,id,name} if id=="call_123" && name=="lookup")),"{newline:?}/{split}");
                assert!(actual.iter().any(|event|matches!(event,Event::ToolCallArgumentsDelta{index:9,delta} if delta=="{}")));
                assert!(matches!(
                    actual.last(),
                    Some(Event::Finish {
                        reason: FinishReasonV1::ToolCalls
                    })
                ));
            }
        }
    }

    #[test]
    fn opaque_fields_invalid_tool_shapes_and_conflicting_aliases_refuse_instead_of_disappearing() {
        for delta in [
            json!({"content":"reply","audio":{"id":"opaque"}}),
            json!({"content":"reply","vendor_state":{"opaque":true}}),
            json!({"reasoning":"a","reasoning_content":"b"}),
            json!({"function_call":{"name":"lookup","arguments":"{}"}}),
            json!({"tool_calls":[{"index":0,"id":"t","function":{"name":"f","arguments":"[]"}}]}),
            json!({"tool_calls":[{"index":u64::MAX,"id":"t","function":{"name":"f","arguments":"{}"}}]}),
        ] {
            let reason = if delta.get("tool_calls").is_some() {
                "tool_calls"
            } else {
                "stop"
            };
            let result = Projection::default().event(&chunk(delta, reason.into()));
            assert!(matches!(result, Err(ProviderError::OutputProjection(_))));
        }
        let mut state = Projection::default();
        state.event(&chunk(json!({"tool_calls":[{"index":0,"id":"t","function":{"name":"f","arguments":"{}"}}]}),"tool_calls".into())).unwrap();
        assert!(state
            .event(&chunk(
                json!({"tool_calls":[{"index":0,"function":{"arguments":"x"}}]}),
                Value::Null
            ))
            .is_err());
    }

    #[test]
    fn ambiguous_order_and_excess_tool_state_refuse_before_inventing_calls() {
        let mut state = Projection::default();
        state.event(&chunk(json!({"tool_calls":[{"index":0,"id":"c","function":{"name":"f","arguments":"{}"}}]}), Value::Null)).unwrap();
        assert!(state
            .event(&chunk(
                json!({"content":"after the call"}),
                "tool_calls".into()
            ))
            .is_err());
        let mut state = Projection::default();
        let calls = (0..1024).map(|i|json!({"index":i,"id":format!("c{i}"),"function":{"name":"f","arguments":"{}"}})).collect::<Vec<_>>();
        state
            .event(&chunk(json!({"tool_calls":calls}), Value::Null))
            .unwrap();
        assert!(state.event(&chunk(json!({"tool_calls":[{"index":1024,"id":"overflow","function":{"name":"f","arguments":"{}"}}]}),Value::Null)).is_err());
        assert_eq!(state.calls.len(), 1024);
        let mut state = Projection {
            retained: crate::MAX_STREAM_LINE_BYTES,
            ..Projection::default()
        };
        assert!(state.event(&chunk(json!({"tool_calls":[{"index":0,"id":"c","function":{"name":"f","arguments":"{}"}}]}),Value::Null)).is_err());
    }

    #[tokio::test]
    async fn refusal_drains_remaining_source_chunks_without_publishing_a_successful_finish() {
        use std::sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        };
        let polled = Arc::new(AtomicUsize::new(0));
        let counter = polled.clone();
        let frames = vec![
            format!(
                "data: {}\n\n",
                chunk(json!({"audio":{"id":"opaque"}}), Value::Null)
            ),
            format!(
                "data: {}\n\ndata: [DONE]\n\n",
                chunk(json!({}), "stop".into())
            ),
        ];
        let raw = Box::pin(futures_util::stream::iter(frames).map(move |data| {
            counter.fetch_add(1, Ordering::SeqCst);
            Ok(crate::StreamChunk {
                data: bytes::Bytes::from(data),
                ..crate::StreamChunk::default()
            })
        }));
        let events = decode(raw, "fixture".into()).collect::<Vec<_>>().await;
        assert_eq!(polled.load(Ordering::SeqCst), 2);
        assert!(matches!(
            events.last(),
            Some(Err(ProviderError::OutputProjection(_)))
        ));
        assert!(!events
            .iter()
            .any(|event| matches!(event, Ok(Event::Finish { .. }))));
    }
}
