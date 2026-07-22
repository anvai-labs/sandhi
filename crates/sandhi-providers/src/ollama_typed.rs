//! Canonical chat v1 ↔ Ollama native `/api/chat` codec.

use crate::typed::{provider_request, ChatEventStream, ChatProvider};
use crate::{ByteStream, ParsedUsage, Provider, ProviderError};
use async_trait::async_trait;
use sandhi_core::{
    AssistantOutputV1, ChatMessageV1, ChatRequestV1, ChatResponseV1, ChatStreamEventV1,
    ContentPart, FinishReasonV1, MessageContent, ToolCallV1, UsageCompleteness, UsageV2,
};
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;
use std::sync::Arc;

pub(crate) struct TypedOllama {
    raw: Arc<dyn Provider>,
}

impl TypedOllama {
    pub(crate) fn new(raw: Arc<dyn Provider>) -> Self {
        Self { raw }
    }
}

#[async_trait]
impl ChatProvider for TypedOllama {
    fn slug(&self) -> &str {
        "ollama"
    }

    async fn complete(&self, request: ChatRequestV1) -> Result<ChatResponseV1, ProviderError> {
        request.validate().map_err(ProviderError::InvalidRequest)?;
        let body = encode_ollama_request(&request)?;
        let response = self.raw.complete(provider_request(&request, body)).await?;
        let mut decoded = decode_ollama_response(response.body, response.usage, &request.model)?;
        decoded.usage.attempts = response.attempts;
        decoded.usage.outcome = Some("success".into());
        Ok(decoded)
    }

    async fn stream(&self, request: ChatRequestV1) -> Result<ChatEventStream, ProviderError> {
        request.validate().map_err(ProviderError::InvalidRequest)?;
        let body = encode_ollama_request(&request)?;
        let raw = self.raw.stream(provider_request(&request, body)).await?;
        Ok(decode_ollama_stream(raw, request.model))
    }
}

pub fn encode_ollama_request(request: &ChatRequestV1) -> Result<Value, ProviderError> {
    request.validate().map_err(ProviderError::InvalidRequest)?;
    let mut body = request
        .extensions
        .get("ollama")
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
                            "parameters":tool.parameters
                        }})
                    })
                    .collect(),
            ),
        );
    }
    let mut options = body
        .remove("options")
        .and_then(|value| value.as_object().cloned())
        .unwrap_or_default();
    if let Some(temperature) = request.temperature {
        options.insert("temperature".into(), json!(temperature));
    }
    if let Some(max_tokens) = request.max_output_tokens {
        options.insert("num_predict".into(), max_tokens.into());
    }
    if let Some(stop) = &request.stop {
        options.insert("stop".into(), json!(stop));
    }
    if !options.is_empty() {
        body.insert("options".into(), Value::Object(options));
    }
    if let Some(format) = &request.response_format {
        body.insert("format".into(), format.clone());
    }
    Ok(Value::Object(body))
}

fn encode_message(message: &ChatMessageV1) -> Result<Value, ProviderError> {
    match message {
        ChatMessageV1::Developer { content, .. } | ChatMessageV1::System { content, .. } => {
            let (content, images) = content_and_images(content)?;
            Ok(message_value("system", content, images))
        }
        ChatMessageV1::User { content, .. } => {
            let (content, images) = content_and_images(content)?;
            Ok(message_value("user", content, images))
        }
        ChatMessageV1::Assistant {
            content,
            tool_calls,
            refusal,
            ..
        } => {
            let (mut text, images) = match content {
                Some(content) => content_and_images(content)?,
                None => (String::new(), Vec::new()),
            };
            if let Some(refusal) = refusal {
                text.push_str(refusal);
            }
            let mut value = message_value("assistant", text, images);
            if !tool_calls.is_empty() {
                value["tool_calls"] = Value::Array(
                    tool_calls
                        .iter()
                        .map(|call| {
                            let arguments = serde_json::from_str::<Value>(&call.arguments)
                                .map_err(|error| {
                                    ProviderError::InvalidRequest(format!(
                                        "tool call {} arguments are not valid JSON: {error}",
                                        call.id
                                    ))
                                })?;
                            Ok(json!({"function":{"name":call.name,"arguments":arguments}}))
                        })
                        .collect::<Result<Vec<_>, ProviderError>>()?,
                );
            }
            Ok(value)
        }
        ChatMessageV1::Tool { content, .. } => {
            let (content, images) = content_and_images(content)?;
            Ok(message_value("tool", content, images))
        }
        ChatMessageV1::Function { .. } => Err(ProviderError::InvalidRequest(
            "Ollama does not support legacy function-role messages; use tool".into(),
        )),
    }
}

fn message_value(role: &str, content: String, images: Vec<String>) -> Value {
    let mut message = Map::from_iter([
        ("role".into(), Value::String(role.into())),
        ("content".into(), Value::String(content)),
    ]);
    if !images.is_empty() {
        message.insert("images".into(), json!(images));
    }
    Value::Object(message)
}

fn content_and_images(content: &MessageContent) -> Result<(String, Vec<String>), ProviderError> {
    match content {
        MessageContent::Text(text) => Ok((text.clone(), Vec::new())),
        MessageContent::Parts(parts) => {
            let mut text = String::new();
            let mut images = Vec::new();
            for part in parts {
                match part {
                    ContentPart::Text { text: value } => text.push_str(value),
                    ContentPart::ImageUrl { image_url, .. } => {
                        let image = image_url
                            .split_once(";base64,")
                            .map_or(image_url.as_str(), |(_, encoded)| encoded);
                        images.push(image.into());
                    }
                    ContentPart::InputAudio { .. } | ContentPart::File { .. } => {
                        return Err(ProviderError::InvalidRequest(
                            "Ollama codec does not support audio/file content parts".into(),
                        ));
                    }
                }
            }
            Ok((text, images))
        }
    }
}

pub fn decode_ollama_response(
    body: Value,
    parsed_usage: ParsedUsage,
    requested_model: &str,
) -> Result<ChatResponseV1, ProviderError> {
    let message = body
        .get("message")
        .and_then(Value::as_object)
        .ok_or_else(|| ProviderError::Transport("Ollama response has no message".into()))?;
    let tool_calls = decode_tool_calls(message.get("tool_calls"));
    Ok(ChatResponseV1 {
        schema_version: sandhi_core::CHAT_SCHEMA_VERSION_V1.into(),
        id: body
            .get("created_at")
            .and_then(Value::as_str)
            .map(str::to_owned),
        model: body
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or(requested_model)
            .into(),
        output: AssistantOutputV1 {
            content: message
                .get("content")
                .and_then(Value::as_str)
                .filter(|text| !text.is_empty())
                .map(|text| MessageContent::Text(text.into())),
            tool_calls,
            refusal: None,
        },
        finish_reason: body
            .get("done_reason")
            .and_then(Value::as_str)
            .map(decode_done_reason),
        usage: parsed_usage.into(),
        extensions: BTreeMap::from([("ollama".into(), body.clone())]),
    })
}

fn decode_tool_calls(value: Option<&Value>) -> Vec<ToolCallV1> {
    value
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .enumerate()
        .filter_map(|(index, call)| {
            let function = call.get("function")?;
            let name = function.get("name")?.as_str()?;
            Some(ToolCallV1 {
                id: call
                    .get("id")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .unwrap_or_else(|| format!("ollama_call_{index}")),
                name: name.into(),
                arguments: serde_json::to_string(function.get("arguments").unwrap_or(&Value::Null))
                    .ok()?,
                extensions: BTreeMap::new(),
            })
        })
        .collect()
}

fn decode_done_reason(reason: &str) -> FinishReasonV1 {
    match reason {
        "stop" => FinishReasonV1::Stop,
        "length" => FinishReasonV1::Length,
        _ => FinishReasonV1::Unknown,
    }
}

fn decode_ollama_stream(mut raw: ByteStream, requested_model: String) -> ChatEventStream {
    use futures_util::StreamExt;
    let stream = async_stream::try_stream! {
        let mut buffer = Vec::<u8>::new();
        let mut started = false;
        let mut emitted_usage = false;
        while let Some(chunk) = raw.next().await {
            let chunk = chunk?;
            let attempts = chunk.attempts;
            if !chunk.data.is_empty() {
                buffer.extend_from_slice(&chunk.data);
                while let Some(pos) = buffer.iter().position(|byte| *byte == b'\n') {
                    let line: Vec<u8> = buffer.drain(..=pos).collect();
                    let Ok(value) = serde_json::from_slice::<Value>(&line) else { continue; };
                    if !started {
                        yield ChatStreamEventV1::ResponseStart {
                            id: value.get("created_at").and_then(Value::as_str).map(str::to_owned),
                            model: value.get("model").and_then(Value::as_str)
                                .unwrap_or(&requested_model).into(),
                        };
                        started = true;
                    }
                    if let Some(text) = value.pointer("/message/content").and_then(Value::as_str) {
                        if !text.is_empty() {
                            yield ChatStreamEventV1::TextDelta { delta: text.into() };
                        }
                    }
                    for (index, call) in decode_tool_calls(value.pointer("/message/tool_calls")).into_iter().enumerate() {
                        yield ChatStreamEventV1::ToolCallStart {
                            index: index as u32, id: call.id, name: call.name
                        };
                        yield ChatStreamEventV1::ToolCallArgumentsDelta {
                            index: index as u32, delta: call.arguments
                        };
                        yield ChatStreamEventV1::ToolCallEnd { index: index as u32 };
                    }
                    if value.get("done").and_then(Value::as_bool) == Some(true) {
                        let reason = value.get("done_reason").and_then(Value::as_str).unwrap_or("stop");
                        yield ChatStreamEventV1::Finish { reason: decode_done_reason(reason) };
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
    fn request_and_response_codecs_preserve_tools_images_and_usage() {
        let request: ChatRequestV1 = serde_json::from_value(json!({
            "model":"llama", "max_output_tokens":64,
            "messages":[{"role":"user","content":[
                {"type":"text","text":"look"},
                {"type":"image_url","image_url":"data:image/png;base64,abc"}
            ]}],
            "tools":[{"name":"lookup","parameters":{"type":"object"}}]
        }))
        .unwrap();
        let encoded = encode_ollama_request(&request).unwrap();
        assert_eq!(encoded["messages"][0]["images"][0], "abc");
        assert_eq!(encoded["options"]["num_predict"], 64);

        let response = decode_ollama_response(
            json!({
                "model":"llama", "done":true, "done_reason":"stop",
                "message":{"role":"assistant","content":"ok","tool_calls":[
                    {"function":{"name":"lookup","arguments":{"q":1}}}
                ]}
            }),
            ParsedUsage {
                tokens_in: 2,
                tokens_out: 3,
                ..ParsedUsage::default()
            },
            "fallback",
        )
        .unwrap();
        assert_eq!(
            response.output.content,
            Some(MessageContent::Text("ok".into()))
        );
        assert_eq!(response.output.tool_calls[0].arguments, "{\"q\":1}");
        assert_eq!(response.usage.tokens_out, 3);
    }
}
