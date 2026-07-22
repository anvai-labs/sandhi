//! Canonical chat v1 ↔ Anthropic Messages codec.

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

pub(crate) struct TypedAnthropic {
    raw: Arc<dyn Provider>,
}

impl TypedAnthropic {
    pub(crate) fn new(raw: Arc<dyn Provider>) -> Self {
        Self { raw }
    }
}

#[async_trait]
impl ChatProvider for TypedAnthropic {
    fn slug(&self) -> &str {
        "anthropic"
    }

    async fn complete(&self, request: ChatRequestV1) -> Result<ChatResponseV1, ProviderError> {
        request.validate().map_err(ProviderError::InvalidRequest)?;
        let body = encode_anthropic_request(&request)?;
        let response = self.raw.complete(provider_request(&request, body)).await?;
        let mut decoded = decode_anthropic_response(response.body, response.usage, &request.model)?;
        decoded.usage.attempts = response.attempts;
        decoded.usage.outcome = Some("success".into());
        Ok(decoded)
    }

    async fn stream(&self, request: ChatRequestV1) -> Result<ChatEventStream, ProviderError> {
        request.validate().map_err(ProviderError::InvalidRequest)?;
        let body = encode_anthropic_request(&request)?;
        let raw = self.raw.stream(provider_request(&request, body)).await?;
        Ok(decode_anthropic_stream(raw, request.model))
    }
}

pub fn encode_anthropic_request(request: &ChatRequestV1) -> Result<Value, ProviderError> {
    request.validate().map_err(ProviderError::InvalidRequest)?;
    let mut body = request
        .extensions
        .get("anthropic")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let native_system = body.get("system").and_then(Value::as_array).cloned();
    let native_tools = body.get("tools").and_then(Value::as_array).cloned();
    let mut system = Vec::new();
    let mut messages = Vec::new();
    for message in &request.messages {
        match message {
            ChatMessageV1::Developer { content, .. } | ChatMessageV1::System { content, .. } => {
                system.push(json!({"type":"text", "text":text_only(content)?}));
            }
            ChatMessageV1::User { content, .. } => {
                messages.push(json!({"role":"user", "content":anthropic_content(content)?}));
            }
            ChatMessageV1::Assistant {
                content,
                tool_calls,
                refusal,
                ..
            } => {
                let mut blocks = Vec::new();
                if let Some(content) = content {
                    blocks.extend(anthropic_content(content)?);
                }
                if let Some(refusal) = refusal {
                    blocks.push(json!({"type":"text", "text":refusal}));
                }
                for call in tool_calls {
                    let input: Value = serde_json::from_str(&call.arguments).map_err(|error| {
                        ProviderError::InvalidRequest(format!(
                            "tool call {} arguments are not valid JSON: {error}",
                            call.id
                        ))
                    })?;
                    blocks.push(json!({
                        "type":"tool_use", "id":call.id, "name":call.name, "input":input
                    }));
                }
                messages.push(json!({"role":"assistant", "content":blocks}));
            }
            ChatMessageV1::Tool {
                content,
                tool_call_id,
            } => messages.push(json!({
                "role":"user",
                "content":[{"type":"tool_result", "tool_use_id":tool_call_id,
                    "content":anthropic_content(content)?}]
            })),
            ChatMessageV1::Function { .. } => {
                return Err(ProviderError::InvalidRequest(
                    "Anthropic Messages does not support legacy function-role results; use tool"
                        .into(),
                ));
            }
        }
    }
    body.insert("model".into(), Value::String(request.model.clone()));
    body.insert("messages".into(), Value::Array(messages));
    if !system.is_empty() {
        if let Some(native_system) = native_system {
            for (index, block) in system.iter_mut().enumerate() {
                if let Some(cache_control) = native_system
                    .get(index)
                    .and_then(|native| native.get("cache_control"))
                {
                    block["cache_control"] = cache_control.clone();
                }
            }
        }
        body.insert("system".into(), Value::Array(system));
    }
    let max_tokens = request.max_output_tokens.ok_or_else(|| {
        ProviderError::InvalidRequest("Anthropic requires max_output_tokens".into())
    })?;
    body.insert("max_tokens".into(), max_tokens.into());
    if let Some(temperature) = request.temperature {
        body.insert("temperature".into(), json!(temperature));
    }
    if let Some(stop) = &request.stop {
        body.insert("stop_sequences".into(), json!(stop));
    }
    if !request.tools.is_empty() {
        let mut tools: Vec<Value> = request
            .tools
            .iter()
            .map(|tool| {
                json!({
                    "name":tool.name,
                    "description":tool.description,
                    "input_schema":tool.parameters,
                })
            })
            .collect();
        if let Some(native_tools) = native_tools {
            for tool in &mut tools {
                let name = tool.get("name").and_then(Value::as_str);
                if let Some(cache_control) = native_tools.iter().find_map(|native| {
                    (native.get("name").and_then(Value::as_str) == name)
                        .then(|| native.get("cache_control"))
                        .flatten()
                }) {
                    tool["cache_control"] = cache_control.clone();
                }
            }
        }
        body.insert("tools".into(), Value::Array(tools));
    }
    if let Some(choice) = &request.tool_choice {
        match choice {
            ToolChoiceV1::Mode(ToolChoiceMode::None) => {
                body.remove("tools");
            }
            ToolChoiceV1::Mode(ToolChoiceMode::Auto) => {
                body.insert("tool_choice".into(), json!({"type":"auto"}));
            }
            ToolChoiceV1::Mode(ToolChoiceMode::Required) => {
                body.insert("tool_choice".into(), json!({"type":"any"}));
            }
            ToolChoiceV1::Function { name } => {
                body.insert("tool_choice".into(), json!({"type":"tool", "name":name}));
            }
        }
    }
    Ok(Value::Object(body))
}

fn text_only(content: &MessageContent) -> Result<String, ProviderError> {
    match content {
        MessageContent::Text(text) => Ok(text.clone()),
        MessageContent::Parts(parts) => parts
            .iter()
            .map(|part| match part {
                ContentPart::Text { text } => Ok(text.as_str()),
                _ => Err(ProviderError::InvalidRequest(
                    "Anthropic system/developer messages support text parts only".into(),
                )),
            })
            .collect::<Result<Vec<_>, _>>()
            .map(|parts| parts.join("\n")),
    }
}

fn anthropic_content(content: &MessageContent) -> Result<Vec<Value>, ProviderError> {
    let parts = match content {
        MessageContent::Text(text) => return Ok(vec![json!({"type":"text", "text":text})]),
        MessageContent::Parts(parts) => parts,
    };
    parts
        .iter()
        .map(|part| match part {
            ContentPart::Text { text } => Ok(json!({"type":"text", "text":text})),
            ContentPart::ImageUrl { image_url, .. } => {
                if let Some(data) = image_url.strip_prefix("data:") {
                    let (media_type, encoded) = data.split_once(";base64,").ok_or_else(|| {
                        ProviderError::InvalidRequest("invalid base64 image data URL".into())
                    })?;
                    Ok(json!({"type":"image", "source":{
                        "type":"base64", "media_type":media_type, "data":encoded
                    }}))
                } else {
                    Ok(json!({"type":"image", "source":{"type":"url", "url":image_url}}))
                }
            }
            ContentPart::InputAudio { .. } | ContentPart::File { .. } => {
                Err(ProviderError::InvalidRequest(
                    "Anthropic codec does not support audio/file content parts yet".into(),
                ))
            }
        })
        .collect()
}

pub fn decode_anthropic_response(
    body: Value,
    parsed_usage: ParsedUsage,
    requested_model: &str,
) -> Result<ChatResponseV1, ProviderError> {
    let blocks = body
        .get("content")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            ProviderError::Transport("Anthropic response has no content array".into())
        })?;
    let mut text = String::new();
    let mut reasoning = String::new();
    let mut tool_calls = Vec::new();
    for block in blocks {
        match block.get("type").and_then(Value::as_str) {
            Some("text") => text.push_str(block.get("text").and_then(Value::as_str).unwrap_or("")),
            Some("thinking") => {
                reasoning.push_str(block.get("thinking").and_then(Value::as_str).unwrap_or(""))
            }
            Some("tool_use") => tool_calls.push(ToolCallV1 {
                id: block.get("id").and_then(Value::as_str).unwrap_or("").into(),
                name: block
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .into(),
                arguments: serde_json::to_string(block.get("input").unwrap_or(&Value::Null))
                    .map_err(|error| ProviderError::Transport(error.to_string()))?,
                extensions: BTreeMap::new(),
            }),
            _ => {}
        }
    }
    let mut extensions = BTreeMap::from([("anthropic".into(), body.clone())]);
    if !reasoning.is_empty() {
        extensions.insert("reasoning".into(), Value::String(reasoning));
    }
    Ok(ChatResponseV1 {
        schema_version: sandhi_core::CHAT_SCHEMA_VERSION_V1.into(),
        id: body.get("id").and_then(Value::as_str).map(str::to_owned),
        model: body
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or(requested_model)
            .into(),
        output: AssistantOutputV1 {
            content: (!text.is_empty()).then_some(MessageContent::Text(text)),
            tool_calls,
            refusal: None,
        },
        finish_reason: body
            .get("stop_reason")
            .and_then(Value::as_str)
            .map(decode_stop_reason),
        usage: UsageV2::from(parsed_usage),
        extensions,
    })
}

fn decode_stop_reason(reason: &str) -> FinishReasonV1 {
    match reason {
        "end_turn" | "stop_sequence" | "pause_turn" => FinishReasonV1::Stop,
        "max_tokens" | "model_context_window_exceeded" => FinishReasonV1::Length,
        "tool_use" => FinishReasonV1::ToolCalls,
        _ => FinishReasonV1::Unknown,
    }
}

fn decode_anthropic_stream(mut raw: ByteStream, requested_model: String) -> ChatEventStream {
    use futures_util::StreamExt;
    let stream = async_stream::try_stream! {
        let mut buffer = Vec::<u8>::new();
        let mut started = false;
        let mut open_tools = BTreeSet::<u32>::new();
        let mut emitted_usage = false;
        while let Some(chunk) = raw.next().await {
            let chunk = chunk?;
            let attempts = chunk.attempts;
            if !chunk.data.is_empty() {
                buffer.extend_from_slice(&chunk.data);
                while let Some(pos) = buffer.iter().position(|byte| *byte == b'\n') {
                    let line: Vec<u8> = buffer.drain(..=pos).collect();
                    let Some(value) = crate::sse_data_json(&line) else { continue; };
                    match value.get("type").and_then(Value::as_str) {
                        Some("message_start") if !started => {
                            let message = value.get("message").unwrap_or(&Value::Null);
                            yield ChatStreamEventV1::ResponseStart {
                                id: message.get("id").and_then(Value::as_str).map(str::to_owned),
                                model: message.get("model").and_then(Value::as_str)
                                    .unwrap_or(&requested_model).to_owned(),
                            };
                            started = true;
                        }
                        Some("content_block_start") => {
                            let index = value.get("index").and_then(Value::as_u64).unwrap_or(0) as u32;
                            let block = value.get("content_block").unwrap_or(&Value::Null);
                            if block.get("type").and_then(Value::as_str) == Some("tool_use") {
                                open_tools.insert(index);
                                yield ChatStreamEventV1::ToolCallStart {
                                    index,
                                    id: block.get("id").and_then(Value::as_str).unwrap_or("").into(),
                                    name: block.get("name").and_then(Value::as_str).unwrap_or("").into(),
                                };
                            }
                        }
                        Some("content_block_delta") => {
                            let index = value.get("index").and_then(Value::as_u64).unwrap_or(0) as u32;
                            let delta = value.get("delta").unwrap_or(&Value::Null);
                            match delta.get("type").and_then(Value::as_str) {
                                Some("text_delta") => yield ChatStreamEventV1::TextDelta {
                                    delta: delta.get("text").and_then(Value::as_str).unwrap_or("").into()
                                },
                                Some("thinking_delta") => yield ChatStreamEventV1::ReasoningDelta {
                                    delta: delta.get("thinking").and_then(Value::as_str).unwrap_or("").into()
                                },
                                Some("input_json_delta") => yield ChatStreamEventV1::ToolCallArgumentsDelta {
                                    index,
                                    delta: delta.get("partial_json").and_then(Value::as_str).unwrap_or("").into()
                                },
                                _ => {}
                            }
                        }
                        Some("content_block_stop") => {
                            let index = value.get("index").and_then(Value::as_u64).unwrap_or(0) as u32;
                            if open_tools.remove(&index) {
                                yield ChatStreamEventV1::ToolCallEnd { index };
                            }
                        }
                        Some("message_delta") => {
                            if let Some(reason) = value.pointer("/delta/stop_reason").and_then(Value::as_str) {
                                yield ChatStreamEventV1::Finish { reason: decode_stop_reason(reason) };
                            }
                        }
                        _ => {}
                    }
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
    use super::*;
    use bytes::Bytes;
    use futures_util::StreamExt;

    #[test]
    fn request_codec_maps_system_tools_and_tool_results() {
        let request: ChatRequestV1 = serde_json::from_value(json!({
            "model":"claude-test", "max_output_tokens":128,
            "messages":[
                {"role":"developer","content":"policy"},
                {"role":"user","content":"use tool"},
                {"role":"assistant","tool_calls":[{"id":"c1","name":"lookup","arguments":"{\"q\":1}"}]},
                {"role":"tool","content":"done","tool_call_id":"c1"}
            ],
            "tools":[{"name":"lookup","parameters":{"type":"object"}}],
            "extensions":{"anthropic":{
                "system":[{"type":"text","text":"policy","cache_control":{"type":"ephemeral"}}],
                "tools":[{"name":"lookup","cache_control":{"type":"ephemeral"}}]
            }}
        })).unwrap();
        let body = encode_anthropic_request(&request).unwrap();
        assert_eq!(body["system"][0]["text"], "policy");
        assert_eq!(body["messages"][1]["content"][0]["type"], "tool_use");
        assert_eq!(body["messages"][2]["content"][0]["type"], "tool_result");
        assert_eq!(body["tools"][0]["input_schema"]["type"], "object");
        assert_eq!(body["system"][0]["cache_control"]["type"], "ephemeral");
        assert_eq!(body["tools"][0]["cache_control"]["type"], "ephemeral");
    }

    #[test]
    fn response_codec_maps_text_thinking_tools_and_cache_usage() {
        let body = json!({
            "id":"m1", "model":"claude-test", "stop_reason":"tool_use",
            "content":[
                {"type":"thinking","thinking":"consider"},
                {"type":"text","text":"answer"},
                {"type":"tool_use","id":"c1","name":"lookup","input":{"q":1}}
            ]
        });
        let out = decode_anthropic_response(
            body,
            ParsedUsage {
                tokens_in: 2,
                tokens_out: 3,
                cache_creation_tokens: 4,
                cache_read_tokens: 5,
            },
            "fallback",
        )
        .unwrap();
        assert_eq!(
            out.output.content,
            Some(MessageContent::Text("answer".into()))
        );
        assert_eq!(out.output.tool_calls[0].arguments, "{\"q\":1}");
        assert_eq!(out.finish_reason, Some(FinishReasonV1::ToolCalls));
        assert_eq!(out.usage.cache_read_tokens, 5);
        assert_eq!(out.extensions["reasoning"], "consider");
    }

    #[tokio::test]
    async fn stream_codec_is_invariant_across_arbitrary_byte_boundaries() {
        let sse = concat!(
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"m1\",\"model\":\"claude-test\"}}\n\n",
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"c1\",\"name\":\"lookup\"}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{}\"}}\n\n",
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"}}\n\n",
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"
        ).as_bytes();
        for split in 0..=sse.len() {
            let raw: ByteStream = Box::pin(futures_util::stream::iter(vec![
                Ok(crate::StreamChunk {
                    data: Bytes::copy_from_slice(&sse[..split]),
                    usage: None,
                    attempts: 1,
                }),
                Ok(crate::StreamChunk {
                    data: Bytes::copy_from_slice(&sse[split..]),
                    usage: None,
                    attempts: 1,
                }),
                Ok(crate::StreamChunk {
                    data: Bytes::new(),
                    usage: Some(ParsedUsage {
                        tokens_in: 2,
                        tokens_out: 3,
                        cache_creation_tokens: 4,
                        cache_read_tokens: 5,
                    }),
                    attempts: 1,
                }),
            ]));
            let events = decode_anthropic_stream(raw, "fallback".into())
                .collect::<Vec<_>>()
                .await
                .into_iter()
                .collect::<Result<Vec<_>, _>>()
                .unwrap();
            assert!(
                matches!(
                    events.first(),
                    Some(ChatStreamEventV1::ResponseStart { .. })
                ),
                "split {split}"
            );
            assert!(events.iter().any(|event| matches!(event, ChatStreamEventV1::ToolCallStart { id, .. } if id == "c1")), "split {split}");
            assert!(
                events
                    .iter()
                    .any(|event| matches!(event, ChatStreamEventV1::ToolCallEnd { index: 0 })),
                "split {split}"
            );
            assert!(
                events.iter().any(|event| matches!(
                    event,
                    ChatStreamEventV1::Finish {
                        reason: FinishReasonV1::ToolCalls
                    }
                )),
                "split {split}"
            );
            assert_eq!(
                events
                    .iter()
                    .filter(|event| matches!(event, ChatStreamEventV1::Usage { .. }))
                    .count(),
                1,
                "split {split}"
            );
        }
    }
}
