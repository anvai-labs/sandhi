//! Canonical chat v1 ↔ Cohere v2 Chat codec.

use crate::typed::{provider_request, ChatEventStream, ChatProvider};
use crate::{ByteStream, ParsedUsage, Provider, ProviderError};
use async_trait::async_trait;
use sandhi_core::{
    AssistantOutputV1, ChatMessageV1, ChatRequestV1, ChatResponseV1, ChatStreamEventV1,
    ContentPart, FinishReasonV1, MessageContent, ToolCallV1, ToolChoiceMode, ToolChoiceV1,
    UsageCompleteness, UsageV2,
};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

pub(crate) struct TypedCohere {
    raw: Arc<dyn Provider>,
}

impl TypedCohere {
    pub(crate) fn new(raw: Arc<dyn Provider>) -> Self {
        Self { raw }
    }
}

#[async_trait]
impl ChatProvider for TypedCohere {
    fn slug(&self) -> &str {
        "cohere"
    }

    async fn complete(&self, request: ChatRequestV1) -> Result<ChatResponseV1, ProviderError> {
        request.validate().map_err(ProviderError::InvalidRequest)?;
        let body = encode_cohere_request(&request)?;
        let response = self.raw.complete(provider_request(&request, body)).await?;
        let mut decoded = decode_cohere_response(response.body, response.usage, &request.model)?;
        if !request.include_native_response {
            // G8: the native body is debug metadata, not contract. Decoded
            // extensions (e.g. "reasoning") always survive.
            decoded.extensions.remove("cohere");
        }
        decoded.usage.attempts = response.attempts;
        decoded.usage.outcome = Some("success".into());
        Ok(decoded)
    }

    async fn stream(&self, request: ChatRequestV1) -> Result<ChatEventStream, ProviderError> {
        request.validate().map_err(ProviderError::InvalidRequest)?;
        let body = encode_cohere_request(&request)?;
        let raw = self.raw.stream(provider_request(&request, body)).await?;
        Ok(decode_cohere_stream(raw, request.model))
    }
}

pub fn encode_cohere_request(request: &ChatRequestV1) -> Result<Value, ProviderError> {
    request.validate().map_err(ProviderError::InvalidRequest)?;
    let mut body = request
        .extensions
        .get("cohere")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    body.insert("model".into(), Value::String(request.model.clone()));
    body.insert(
        "messages".into(),
        Value::Array(
            request
                .messages
                .iter()
                .map(encode_message)
                .collect::<Result<Vec<_>, _>>()?,
        ),
    );
    if !request.tools.is_empty() {
        body.insert(
            "tools".into(),
            Value::Array(
                request
                    .tools
                    .iter()
                    .map(|tool| {
                        json!({"type":"function", "function":{
                            "name":tool.name, "description":tool.description,
                            "parameters":tool.parameters, "strict":tool.strict
                        }})
                    })
                    .collect(),
            ),
        );
    }
    if let Some(choice) = &request.tool_choice {
        body.insert(
            "tool_choice".into(),
            match choice {
                ToolChoiceV1::Mode(ToolChoiceMode::None) => Value::String("NONE".into()),
                ToolChoiceV1::Mode(ToolChoiceMode::Auto) => Value::String("AUTO".into()),
                ToolChoiceV1::Mode(ToolChoiceMode::Required) => Value::String("REQUIRED".into()),
                ToolChoiceV1::Function { name } => {
                    json!({"type":"function", "function":{"name":name}})
                }
            },
        );
    }
    if let Some(temperature) = request.temperature {
        body.insert("temperature".into(), json!(temperature));
    }
    if let Some(max_tokens) = request.max_output_tokens {
        body.insert("max_tokens".into(), max_tokens.into());
    }
    if let Some(stop) = &request.stop {
        body.insert("stop_sequences".into(), json!(stop));
    }
    if let Some(seed) = request.seed {
        body.insert("seed".into(), seed.into());
    }
    if let Some(format) = &request.response_format {
        body.insert("response_format".into(), format.clone());
    }
    Ok(Value::Object(body))
}

fn encode_message(message: &ChatMessageV1) -> Result<Value, ProviderError> {
    match message {
        ChatMessageV1::Developer { content, .. } | ChatMessageV1::System { content, .. } => {
            Ok(json!({"role":"system", "content":cohere_content(content)?}))
        }
        ChatMessageV1::User { content, .. } => {
            Ok(json!({"role":"user", "content":cohere_content(content)?}))
        }
        ChatMessageV1::Assistant {
            content,
            tool_calls,
            refusal,
            ..
        } => {
            let mut value = json!({
                "role":"assistant",
                "content": match content {
                    Some(content) => cohere_content(content)?,
                    None => Vec::new(),
                }
            });
            if let Some(refusal) = refusal {
                value["content"]
                    .as_array_mut()
                    .expect("array")
                    .push(json!({"type":"text", "text":refusal}));
            }
            if !tool_calls.is_empty() {
                value["tool_calls"] = Value::Array(
                    tool_calls
                        .iter()
                        .map(|call| {
                            json!({
                                "id":call.id, "type":"function", "function":{
                                    "name":call.name, "arguments":call.arguments
                                }
                            })
                        })
                        .collect(),
                );
            }
            Ok(value)
        }
        ChatMessageV1::Tool {
            content,
            tool_call_id,
        } => Ok(json!({
            "role":"tool", "tool_call_id":tool_call_id, "content":cohere_content(content)?
        })),
        ChatMessageV1::Function { .. } => Err(ProviderError::InvalidRequest(
            "Cohere v2 does not support legacy function-role messages; use tool".into(),
        )),
    }
}

fn cohere_content(content: &MessageContent) -> Result<Vec<Value>, ProviderError> {
    let parts = match content {
        MessageContent::Text(text) => return Ok(vec![json!({"type":"text", "text":text})]),
        MessageContent::Parts(parts) => parts,
    };
    parts
        .iter()
        .map(|part| match part {
            ContentPart::Text { text } => Ok(json!({"type":"text", "text":text})),
            ContentPart::ImageUrl { image_url, detail } => Ok(json!({
                "type":"image_url", "image_url":{"url":image_url,"detail":detail}
            })),
            ContentPart::InputAudio { .. } | ContentPart::File { .. } => {
                Err(ProviderError::InvalidRequest(
                    "Cohere codec does not support audio/file parts".into(),
                ))
            }
        })
        .collect()
}

pub fn decode_cohere_response(
    body: Value,
    parsed_usage: ParsedUsage,
    requested_model: &str,
) -> Result<ChatResponseV1, ProviderError> {
    let message = body
        .get("message")
        .ok_or_else(|| ProviderError::Transport("Cohere response has no message".into()))?;
    let text = message
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|part| part.get("text").and_then(Value::as_str))
        .collect::<String>();
    let tool_calls = decode_tool_calls(message.get("tool_calls"));
    Ok(ChatResponseV1 {
        schema_version: sandhi_core::CHAT_SCHEMA_VERSION_V1.into(),
        id: body.get("id").and_then(Value::as_str).map(str::to_owned),
        model: requested_model.into(),
        output: AssistantOutputV1 {
            content: (!text.is_empty()).then_some(MessageContent::Text(text)),
            tool_calls,
            refusal: None,
        },
        finish_reason: body
            .get("finish_reason")
            .and_then(Value::as_str)
            .map(decode_finish_reason),
        usage: parsed_usage.into(),
        extensions: BTreeMap::from([("cohere".into(), body.clone())]),
    })
}

fn decode_tool_calls(value: Option<&Value>) -> Vec<ToolCallV1> {
    value
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|call| {
            Some(ToolCallV1 {
                id: call.get("id")?.as_str()?.into(),
                name: call.pointer("/function/name")?.as_str()?.into(),
                arguments: call.pointer("/function/arguments")?.as_str()?.into(),
                extensions: BTreeMap::new(),
            })
        })
        .collect()
}

fn decode_finish_reason(reason: &str) -> FinishReasonV1 {
    match reason {
        "COMPLETE" | "STOP_SEQUENCE" => FinishReasonV1::Stop,
        "MAX_TOKENS" => FinishReasonV1::Length,
        "TOOL_CALL" => FinishReasonV1::ToolCalls,
        _ => FinishReasonV1::Unknown,
    }
}

fn decode_cohere_stream(mut raw: ByteStream, requested_model: String) -> ChatEventStream {
    use futures_util::StreamExt;
    let stream = async_stream::try_stream! {
        // TD-0014 P1: the shared bounded splitter. One ceiling across both planes; only the
        // over-budget POLICY differs, and it is applied below. See MAX_STREAM_LINE_BYTES.
        let mut splitter = crate::linesplit::LineSplitter::new(crate::MAX_STREAM_LINE_BYTES);
        let mut started = false;
        let mut open_tools = BTreeSet::<u32>::new();
        let mut emitted_usage = false;
        // TD-0014 P2b: after the real chunks end, ONE synthetic empty chunk flushes any
        // trailing remainder (a final frame without its newline — Ollama's `done` frame) through
        // this same loop body, so per-chunk event ordering is preserved exactly and the body is
        // not duplicated. `raw.next()` returning None terminates; a false flush breaks out.
        let mut tail_pending = false;
        let mut chunks_ended = false;
        while !chunks_ended || tail_pending {
            let chunk = if chunks_ended {
                tail_pending = false;
                crate::StreamChunk {
                    data: bytes::Bytes::new(),
                    usage: None,
                    usage_running: None,
                    attempts: 1,
                }
            } else {
                match raw.next().await {
                    Some(chunk) => chunk?,
                    None => {
                        chunks_ended = true;
                        tail_pending = splitter.flush_newline();
                        continue;
                    }
                }
            };
            let attempts = chunk.attempts;
            // Unconditional: the synthetic tail chunk arrives with empty data and must still
            // drain the flushed remainder; draining after no new bytes is a no-op scan.
            // (The terminal usage-only chunk is likewise empty and previously skipped this.)
            {
                splitter.push(&chunk.data);
                while let Some(line) = splitter.next_line() {
                    let Some(value) = crate::sse_data_json(&line) else { continue; };
                    let kind = value.get("type").and_then(Value::as_str);
                    if kind == Some("message-start") && !started {
                        yield ChatStreamEventV1::ResponseStart {
                            id: value.get("id").and_then(Value::as_str).map(str::to_owned),
                            model: requested_model.clone(),
                        };
                        started = true;
                    }
                    if kind == Some("content-delta") {
                        if let Some(text) = value.pointer("/delta/message/content/text").and_then(Value::as_str) {
                            yield ChatStreamEventV1::TextDelta { delta: text.into() };
                        }
                    }
                    if kind == Some("tool-call-start") {
                        let index = value.get("index").and_then(Value::as_u64).unwrap_or(0) as u32;
                        let call = value.pointer("/delta/message/tool_calls").unwrap_or(&Value::Null);
                        open_tools.insert(index);
                        yield ChatStreamEventV1::ToolCallStart {
                            index,
                            id: call.get("id").and_then(Value::as_str).unwrap_or("").into(),
                            name: call.pointer("/function/name").and_then(Value::as_str).unwrap_or("").into(),
                        };
                    }
                    if kind == Some("tool-call-delta") {
                        let index = value.get("index").and_then(Value::as_u64).unwrap_or(0) as u32;
                        let delta = value.pointer("/delta/message/tool_calls/function/arguments")
                            .and_then(Value::as_str).unwrap_or("");
                        if !delta.is_empty() {
                            yield ChatStreamEventV1::ToolCallArgumentsDelta { index, delta: delta.into() };
                        }
                    }
                    if kind == Some("tool-call-end") {
                        let index = value.get("index").and_then(Value::as_u64).unwrap_or(0) as u32;
                        if open_tools.remove(&index) {
                            yield ChatStreamEventV1::ToolCallEnd { index };
                        }
                    }
                    if kind == Some("message-end") {
                        if let Some(reason) = value.pointer("/delta/finish_reason").and_then(Value::as_str) {
                            yield ChatStreamEventV1::Finish { reason: decode_finish_reason(reason) };
                        }
                    }
                }
                // TD-0014 P1 (gap G01): past MAX_STREAM_LINE_BYTES the upstream has sent no
                // line boundary at all, which no real provider does. The raw plane drops the
                // pending line and keeps streaming — its bytes were already forwarded, so
                // only usage suffers. A typed decoder emits decoded CONTENT, so dropping
                // silently would corrupt the response with no signal. Fail loudly instead;
                // mid-stream errors are never retried.
                if splitter.over_budget() {
                    Err(ProviderError::Transport(format!(
                        "upstream stream exceeded {} bytes with no line boundary",
                        crate::MAX_STREAM_LINE_BYTES
                    )))?;
                }
            }
            if let Some(usage) = chunk.usage {
                if !emitted_usage {
                    let mut usage: UsageV2 = usage.into();
                    usage.completeness = UsageCompleteness::Final;
                    usage.attempts = attempts;
                    usage.outcome = Some("success".into());
                    yield ChatStreamEventV1::Usage { usage };
                    emitted_usage = true;
                }
            }
        }
    };
    Box::pin(stream)
}

#[cfg(test)]
mod tests {

    /// TD-0014 P1, the opposing guard. The six `..._is_bounded_and_errors_...` tests pin the
    /// ceiling from BELOW; on their own they would all still pass with the bound set to 1 KiB,
    /// which would break every legitimate large frame. Adversarial review found five of six
    /// decoders had no test in this direction — the same one-sidedness that let the original
    /// 64 KiB ceiling ship. This pins it from above.
    #[tokio::test]
    async fn a_large_but_legitimate_frame_is_not_killed_by_the_line_bound() {
        use futures_util::StreamExt;
        // 200 KB in one terminated line: far past the old 64 KiB bound, far under the real one.
        let big = "x".repeat(200 * 1024);
        let wire = format!("data: {{\"type\":\"content-delta\",\"delta\":{{\"message\":{{\"content\":{{\"text\":\"{big}\"}}}}}}}}\n\n");
        let chunks: Vec<_> = wire
            .as_bytes()
            .chunks(16 * 1024)
            .map(|c| {
                Ok(crate::StreamChunk {
                    data: bytes::Bytes::copy_from_slice(c),
                    usage: None,
                    usage_running: None,
                    attempts: 1,
                })
            })
            .collect();
        let raw: crate::ByteStream = Box::pin(futures_util::stream::iter(chunks));
        let results: Vec<_> = super::decode_cohere_stream(raw, "m".into())
            .collect::<Vec<_>>()
            .await;
        assert!(
            !results
                .iter()
                .any(|r| matches!(r, Err(crate::ProviderError::Transport(_)))),
            "a 200 KB terminated frame is legitimate traffic and must survive"
        );
        let text: usize = results
            .iter()
            .flatten()
            .filter_map(|e| match e {
                sandhi_core::ChatStreamEventV1::TextDelta { delta } => Some(delta.len()),
                _ => None,
            })
            .sum();
        assert_eq!(text, big.len(), "the whole delta must arrive intact");
    }

    /// TD-0014 P1 (gap G01): a newline-free upstream stream must stay BOUNDED and fail loudly.
    ///
    /// The raw plane may drop an over-budget line and keep going — its bytes were already
    /// forwarded verbatim, so only *usage* is lost. A typed decoder drops decoded **content**,
    /// so dropping silently would corrupt the response with no signal at all. It errors instead.
    /// Mid-stream errors are never retried (`resilience.rs`), so this cannot loop.
    #[tokio::test]
    async fn a_newline_free_stream_is_bounded_and_errors_rather_than_growing() {
        use futures_util::StreamExt;
        // 16 MiB with no line boundary anywhere — past MAX_STREAM_LINE_BYTES (8 MiB). The bound
        // exists to stop unbounded growth, so the test input has to be genuinely pathological;
        // a merely LARGE frame is legitimate and is covered by the regression test above.
        let filler = bytes::Bytes::from(vec![b'x'; 64 * 1024]);
        let chunks: Vec<_> = (0..256)
            .map(|_| {
                Ok(crate::StreamChunk {
                    data: filler.clone(),
                    usage: None,
                    usage_running: None,
                    attempts: 1,
                })
            })
            .collect();
        let raw: crate::ByteStream = Box::pin(futures_util::stream::iter(chunks));
        let results: Vec<_> = super::decode_cohere_stream(raw, "m".into())
            .collect::<Vec<_>>()
            .await;
        assert!(
            results
                .iter()
                .any(|item| matches!(item, Err(crate::ProviderError::Transport(_)))),
            "a newline-free stream must terminate with a Transport error rather than \
             buffering without bound"
        );
    }
    use super::*;

    /// TD-0014 P1 characterisation net (gap G01). The decoder must yield identical events no
    /// matter where the transport splits the byte stream. Written BEFORE the `LineSplitter`
    /// refactor precisely so it can prove the refactor changed nothing: `cohere_typed` is one of the
    /// three typed decoders that had no boundary-invariance test at all, so the refactor would
    /// otherwise have had no net beneath it. Mirrors `anthropic_typed`'s equivalent.
    #[tokio::test]
    async fn stream_codec_is_invariant_across_arbitrary_byte_boundaries() {
        use futures_util::StreamExt;
        let wire = concat!(
            "data: {\"type\":\"message-start\",\"id\":\"c1\"}\n\n",
            "data: {\"type\":\"content-delta\",\"delta\":{\"message\":{\"content\":{\"text\":\"he\"}}}}\n\n",
            "data: {\"type\":\"tool-call-start\",\"index\":0,\"delta\":{\"message\":{\"tool_calls\":{\"id\":\"t1\",\"function\":{\"name\":\"lookup\"}}}}}\n\n",
            "data: {\"type\":\"tool-call-delta\",\"index\":0,\"delta\":{\"message\":{\"tool_calls\":{\"function\":{\"arguments\":\"{}\"}}}}}\n\n",
            "data: {\"type\":\"tool-call-end\",\"index\":0}\n\n",
            "data: {\"type\":\"content-delta\",\"delta\":{\"message\":{\"content\":{\"text\":\"llo\"}}}}\n\n",
            "data: {\"type\":\"message-end\",\"delta\":{\"finish_reason\":\"COMPLETE\"}}\n\n",
        ).as_bytes();
        for split in 0..=wire.len() {
            let raw: crate::ByteStream = Box::pin(futures_util::stream::iter(vec![
                Ok(crate::StreamChunk {
                    data: bytes::Bytes::copy_from_slice(&wire[..split]),
                    usage: None,
                    usage_running: None,
                    attempts: 1,
                }),
                Ok(crate::StreamChunk {
                    data: bytes::Bytes::copy_from_slice(&wire[split..]),
                    usage: None,
                    usage_running: None,
                    attempts: 1,
                }),
                Ok(crate::StreamChunk {
                    data: bytes::Bytes::new(),
                    usage: Some(crate::ParsedUsage {
                        tokens_in: 2,
                        tokens_out: 3,
                        cache_creation_tokens: 0,
                        cache_read_tokens: 0,
                        reasoning_tokens: 0,
                    }),
                    usage_running: None,
                    attempts: 1,
                }),
            ]));
            let events = decode_cohere_stream(raw, "command-r".into())
                .collect::<Vec<_>>()
                .await
                .into_iter()
                .collect::<Result<Vec<_>, _>>()
                .unwrap();

            assert!(
                matches!(
                    events.first(),
                    Some(sandhi_core::ChatStreamEventV1::ResponseStart { .. })
                ),
                "split {split}: first event must be ResponseStart"
            );
            // The load-bearing assertion for a line splitter: mis-split lines drop or duplicate
            // text deltas, and concatenating them is what catches that.
            let text: String = events
                .iter()
                .filter_map(|event| match event {
                    sandhi_core::ChatStreamEventV1::TextDelta { delta } => Some(delta.as_str()),
                    _ => None,
                })
                .collect();
            assert_eq!(
                text, "hello",
                "split {split}: text deltas must reassemble exactly"
            );
            assert!(
                events
                    .iter()
                    .any(|event| matches!(event, sandhi_core::ChatStreamEventV1::Finish { .. })),
                "split {split}: Finish must survive any split"
            );
            assert_eq!(
                events
                    .iter()
                    .filter(|event| matches!(
                        event,
                        sandhi_core::ChatStreamEventV1::Usage { usage }
                            if usage.completeness == sandhi_core::UsageCompleteness::Final
                    ))
                    .count(),
                1,
                "split {split}: exactly one Final usage frame"
            );
            assert!(
                events.iter().any(|event| matches!(
                    event,
                    sandhi_core::ChatStreamEventV1::ToolCallStart { id, .. } if id == "t1"
                )),
                "split {split}: ToolCallStart must survive any split"
            );
            assert!(
                events.iter().any(|event| matches!(
                    event,
                    sandhi_core::ChatStreamEventV1::ToolCallEnd { index: 0 }
                )),
                "split {split}: ToolCallEnd must survive any split"
            );
        }
    }

    #[test]
    fn w3d_fields_are_ignored_no_leak() {
        // Consumer-decision row: Cohere honors neither field — they must not
        // leak into the native body under any key.
        let request: ChatRequestV1 = serde_json::from_value(json!({
            "model": "command-r",
            "messages": [{"role": "user", "content": "hi"}],
            "reasoning_effort": "high",
            "thinking": {"enabled": true, "budget_tokens": 512}
        }))
        .unwrap();
        let body = encode_cohere_request(&request).unwrap();
        assert!(body.get("reasoning_effort").is_none());
        assert!(body.get("thinking").is_none());
    }

    #[test]
    fn request_and_response_codecs_preserve_text_tools_and_usage() {
        let request: ChatRequestV1 = serde_json::from_value(json!({
            "model":"command-r", "max_output_tokens":64,
            "messages":[{"role":"user","content":"hello"}],
            "tools":[{"name":"lookup","parameters":{"type":"object"}}]
        }))
        .unwrap();
        let encoded = encode_cohere_request(&request).unwrap();
        assert_eq!(encoded["messages"][0]["content"][0]["text"], "hello");
        assert_eq!(encoded["tools"][0]["function"]["name"], "lookup");

        let response = decode_cohere_response(
            json!({
                "id":"r1", "finish_reason":"COMPLETE",
                "message":{"content":[{"type":"text","text":"ok"}]}
            }),
            ParsedUsage {
                tokens_in: 2,
                tokens_out: 3,
                ..ParsedUsage::default()
            },
            "command-r",
        )
        .unwrap();
        assert_eq!(
            response.output.content,
            Some(MessageContent::Text("ok".into()))
        );
        assert_eq!(response.usage.tokens_out, 3);
        assert_eq!(response.finish_reason, Some(FinishReasonV1::Stop));
    }
}
