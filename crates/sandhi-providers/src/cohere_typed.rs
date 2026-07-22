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
