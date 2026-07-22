//! Canonical chat v1 ↔ Gemini `generateContent` codec.

use crate::typed::{provider_request, ChatEventStream, ChatProvider};
use crate::{ByteStream, ParsedUsage, Provider, ProviderError};
use async_trait::async_trait;
use sandhi_core::{
    AssistantOutputV1, ChatMessageV1, ChatRequestV1, ChatResponseV1, ChatStreamEventV1,
    ContentPart, FinishReasonV1, MessageContent, ToolCallV1, ToolChoiceMode, ToolChoiceV1,
    UsageCompleteness, UsageV2,
};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::sync::Arc;

pub(crate) struct TypedGemini {
    raw: Arc<dyn Provider>,
}

impl TypedGemini {
    pub(crate) fn new(raw: Arc<dyn Provider>) -> Self {
        Self { raw }
    }
}

#[async_trait]
impl ChatProvider for TypedGemini {
    fn slug(&self) -> &str {
        "gemini"
    }

    async fn complete(&self, request: ChatRequestV1) -> Result<ChatResponseV1, ProviderError> {
        request.validate().map_err(ProviderError::InvalidRequest)?;
        let body = encode_gemini_request(&request)?;
        let response = self.raw.complete(provider_request(&request, body)).await?;
        let mut decoded = decode_gemini_response(response.body, response.usage, &request.model)?;
        decoded.usage.attempts = response.attempts;
        decoded.usage.outcome = Some("success".into());
        Ok(decoded)
    }

    async fn stream(&self, request: ChatRequestV1) -> Result<ChatEventStream, ProviderError> {
        request.validate().map_err(ProviderError::InvalidRequest)?;
        let body = encode_gemini_request(&request)?;
        let raw = self.raw.stream(provider_request(&request, body)).await?;
        Ok(decode_gemini_stream(raw, request.model))
    }
}

pub fn encode_gemini_request(request: &ChatRequestV1) -> Result<Value, ProviderError> {
    request.validate().map_err(ProviderError::InvalidRequest)?;
    let mut body = request
        .extensions
        .get("gemini")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let tool_names = request
        .messages
        .iter()
        .filter_map(|message| match message {
            ChatMessageV1::Assistant { tool_calls, .. } => Some(tool_calls),
            _ => None,
        })
        .flatten()
        .map(|call| (call.id.clone(), call.name.clone()))
        .collect::<BTreeMap<_, _>>();
    let mut system_parts = Vec::new();
    let mut contents = Vec::new();
    for message in &request.messages {
        match message {
            ChatMessageV1::Developer { content, .. } | ChatMessageV1::System { content, .. } => {
                system_parts.extend(gemini_parts(content)?);
            }
            ChatMessageV1::User { content, .. } => {
                contents.push(json!({"role":"user", "parts":gemini_parts(content)?}));
            }
            ChatMessageV1::Assistant {
                content,
                tool_calls,
                refusal,
                ..
            } => {
                let mut parts = match content {
                    Some(content) => gemini_parts(content)?,
                    None => Vec::new(),
                };
                if let Some(refusal) = refusal {
                    parts.push(json!({"text":refusal}));
                }
                for call in tool_calls {
                    let args: Value = serde_json::from_str(&call.arguments).map_err(|error| {
                        ProviderError::InvalidRequest(format!(
                            "tool call {} arguments are not valid JSON: {error}",
                            call.id
                        ))
                    })?;
                    parts.push(json!({"functionCall":{"name":call.name,"args":args,"id":call.id}}));
                }
                contents.push(json!({"role":"model", "parts":parts}));
            }
            ChatMessageV1::Tool {
                content,
                tool_call_id,
            } => {
                let name = tool_names.get(tool_call_id).ok_or_else(|| {
                    ProviderError::InvalidRequest(format!(
                        "Gemini tool result references unknown tool call id {tool_call_id}"
                    ))
                })?;
                contents.push(json!({"role":"user", "parts":[{"functionResponse":{
                    "name":name, "id":tool_call_id,
                    "response":{"output":content_as_value(content)}
                }}]}));
            }
            ChatMessageV1::Function { .. } => {
                return Err(ProviderError::InvalidRequest(
                    "Gemini does not support legacy function-role messages; use tool".into(),
                ))
            }
        }
    }
    body.insert("contents".into(), Value::Array(contents));
    if !system_parts.is_empty() {
        body.insert("systemInstruction".into(), json!({"parts":system_parts}));
    }
    if !request.tools.is_empty() {
        body.insert(
            "tools".into(),
            json!([{"functionDeclarations": request.tools}]),
        );
    }
    if let Some(choice) = &request.tool_choice {
        let config = match choice {
            ToolChoiceV1::Mode(ToolChoiceMode::None) => json!({"mode":"NONE"}),
            ToolChoiceV1::Mode(ToolChoiceMode::Auto) => json!({"mode":"AUTO"}),
            ToolChoiceV1::Mode(ToolChoiceMode::Required) => json!({"mode":"ANY"}),
            ToolChoiceV1::Function { name } => {
                json!({"mode":"ANY", "allowedFunctionNames":[name]})
            }
        };
        body.insert("toolConfig".into(), json!({"functionCallingConfig":config}));
    }
    let mut generation = body
        .remove("generationConfig")
        .and_then(|value| value.as_object().cloned())
        .unwrap_or_default();
    if let Some(temperature) = request.temperature {
        generation.insert("temperature".into(), json!(temperature));
    }
    if let Some(max_tokens) = request.max_output_tokens {
        generation.insert("maxOutputTokens".into(), max_tokens.into());
    }
    if let Some(stop) = &request.stop {
        generation.insert("stopSequences".into(), json!(stop));
    }
    if !generation.is_empty() {
        body.insert("generationConfig".into(), Value::Object(generation));
    }
    Ok(Value::Object(body))
}

fn gemini_parts(content: &MessageContent) -> Result<Vec<Value>, ProviderError> {
    let parts = match content {
        MessageContent::Text(text) => return Ok(vec![json!({"text":text})]),
        MessageContent::Parts(parts) => parts,
    };
    parts
        .iter()
        .map(|part| match part {
            ContentPart::Text { text } => Ok(json!({"text":text})),
            ContentPart::ImageUrl { image_url, .. } => {
                if let Some(data) = image_url.strip_prefix("data:") {
                    let (mime_type, encoded) = data.split_once(";base64,").ok_or_else(|| {
                        ProviderError::InvalidRequest("invalid base64 image data URL".into())
                    })?;
                    Ok(json!({"inlineData":{"mimeType":mime_type,"data":encoded}}))
                } else {
                    Ok(json!({"fileData":{"fileUri":image_url}}))
                }
            }
            ContentPart::InputAudio { data, format } => Ok(json!({"inlineData":{
                "mimeType":format!("audio/{format}"), "data":data
            }})),
            ContentPart::File { file_id, .. } if file_id.is_some() => {
                Ok(json!({"fileData":{"fileUri":file_id}}))
            }
            ContentPart::File { .. } => Err(ProviderError::InvalidRequest(
                "Gemini file content requires file_id/file URI".into(),
            )),
        })
        .collect()
}

fn content_as_value(content: &MessageContent) -> Value {
    match content {
        MessageContent::Text(text) => Value::String(text.clone()),
        MessageContent::Parts(parts) => json!(parts),
    }
}

pub fn decode_gemini_response(
    body: Value,
    parsed_usage: ParsedUsage,
    requested_model: &str,
) -> Result<ChatResponseV1, ProviderError> {
    let candidate = body
        .pointer("/candidates/0")
        .ok_or_else(|| ProviderError::Transport("Gemini response has no candidate".into()))?;
    let parts = candidate
        .pointer("/content/parts")
        .and_then(Value::as_array)
        .ok_or_else(|| ProviderError::Transport("Gemini candidate has no parts".into()))?;
    let mut text = String::new();
    let mut reasoning = String::new();
    let mut tool_calls = Vec::new();
    for (index, part) in parts.iter().enumerate() {
        if let Some(value) = part.get("text").and_then(Value::as_str) {
            if part.get("thought").and_then(Value::as_bool) == Some(true) {
                reasoning.push_str(value);
            } else {
                text.push_str(value);
            }
        }
        if let Some(call) = part.get("functionCall") {
            tool_calls.push(ToolCallV1 {
                id: call
                    .get("id")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .unwrap_or_else(|| format!("gemini_call_{index}")),
                name: call
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .into(),
                arguments: serde_json::to_string(call.get("args").unwrap_or(&Value::Null))
                    .map_err(|error| ProviderError::Transport(error.to_string()))?,
                extensions: BTreeMap::new(),
            });
        }
    }
    let mut extensions = BTreeMap::from([("gemini".into(), body.clone())]);
    if !reasoning.is_empty() {
        extensions.insert("reasoning".into(), Value::String(reasoning));
    }
    Ok(ChatResponseV1 {
        schema_version: sandhi_core::CHAT_SCHEMA_VERSION_V1.into(),
        id: body
            .get("responseId")
            .and_then(Value::as_str)
            .map(str::to_owned),
        model: body
            .get("modelVersion")
            .and_then(Value::as_str)
            .unwrap_or(requested_model)
            .into(),
        output: AssistantOutputV1 {
            content: (!text.is_empty()).then_some(MessageContent::Text(text)),
            tool_calls,
            refusal: None,
        },
        finish_reason: candidate
            .get("finishReason")
            .and_then(Value::as_str)
            .map(decode_finish_reason),
        usage: parsed_usage.into(),
        extensions,
    })
}

fn decode_finish_reason(reason: &str) -> FinishReasonV1 {
    match reason {
        "STOP" => FinishReasonV1::Stop,
        "MAX_TOKENS" => FinishReasonV1::Length,
        "SAFETY" | "RECITATION" | "BLOCKLIST" | "PROHIBITED_CONTENT" => {
            FinishReasonV1::ContentFilter
        }
        _ => FinishReasonV1::Unknown,
    }
}

fn decode_gemini_stream(mut raw: ByteStream, requested_model: String) -> ChatEventStream {
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
                    let Some(value) = crate::sse_data_json(&line) else { continue; };
                    if !started {
                        yield ChatStreamEventV1::ResponseStart {
                            id: value.get("responseId").and_then(Value::as_str).map(str::to_owned),
                            model: value.get("modelVersion").and_then(Value::as_str)
                                .unwrap_or(&requested_model).into(),
                        };
                        started = true;
                    }
                    if let Some(parts) = value.pointer("/candidates/0/content/parts").and_then(Value::as_array) {
                        for (index, part) in parts.iter().enumerate() {
                            if let Some(text) = part.get("text").and_then(Value::as_str) {
                                if part.get("thought").and_then(Value::as_bool) == Some(true) {
                                    yield ChatStreamEventV1::ReasoningDelta { delta: text.into() };
                                } else {
                                    yield ChatStreamEventV1::TextDelta { delta: text.into() };
                                }
                            }
                            if let Some(call) = part.get("functionCall") {
                                let stream_index = index as u32;
                                let id = call.get("id").and_then(Value::as_str)
                                    .map(str::to_owned).unwrap_or_else(|| format!("gemini_call_{index}"));
                                let name = call.get("name").and_then(Value::as_str).unwrap_or("").to_owned();
                                let arguments = serde_json::to_string(call.get("args").unwrap_or(&Value::Null))
                                    .map_err(|error| ProviderError::Transport(error.to_string()))?;
                                yield ChatStreamEventV1::ToolCallStart { index: stream_index, id, name };
                                yield ChatStreamEventV1::ToolCallArgumentsDelta { index: stream_index, delta: arguments };
                                yield ChatStreamEventV1::ToolCallEnd { index: stream_index };
                            }
                        }
                    }
                    if let Some(reason) = value.pointer("/candidates/0/finishReason").and_then(Value::as_str) {
                        yield ChatStreamEventV1::Finish { reason: decode_finish_reason(reason) };
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
    fn request_and_response_codecs_preserve_native_tool_semantics() {
        let request: ChatRequestV1 = serde_json::from_value(json!({
            "model":"gemini-test", "max_output_tokens":64,
            "messages":[
                {"role":"system","content":"policy"},
                {"role":"assistant","tool_calls":[{"id":"c1","name":"lookup","arguments":"{\"q\":1}"}]},
                {"role":"tool","content":"done","tool_call_id":"c1"}
            ],
            "tools":[{"name":"lookup","parameters":{"type":"object"}}]
        })).unwrap();
        let encoded = encode_gemini_request(&request).unwrap();
        assert_eq!(encoded["systemInstruction"]["parts"][0]["text"], "policy");
        assert_eq!(
            encoded["contents"][0]["parts"][0]["functionCall"]["name"],
            "lookup"
        );
        assert_eq!(
            encoded["contents"][1]["parts"][0]["functionResponse"]["name"],
            "lookup"
        );

        let response = decode_gemini_response(
            json!({
                "modelVersion":"gemini-test",
                "candidates":[{"finishReason":"STOP","content":{"parts":[
                    {"text":"ok"}, {"functionCall":{"name":"lookup","args":{"q":1}}}
                ]}}]
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
        assert_eq!(response.finish_reason, Some(FinishReasonV1::Stop));
    }
}
