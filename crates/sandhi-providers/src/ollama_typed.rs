//! Canonical chat v1 ↔ Ollama native `/api/chat` codec.

use crate::typed::{provider_request_observed, ChatEventStream, ChatProvider};
use crate::{ParsedUsage, Provider, ProviderError};
use async_trait::async_trait;
use sandhi_core::{
    AssistantOutputV1, ChatMessageV1, ChatRequestV1, ChatResponseV1, ContentPart, FinishReasonV1,
    MessageContent, ToolCallV1,
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

    async fn complete_inner(
        &self,
        request: ChatRequestV1,
        call_headers: http::HeaderMap,
        attempt_context: Option<crate::AttemptContext>,
    ) -> Result<crate::QualifiedCompletion<ChatResponseV1>, ProviderError> {
        request.validate().map_err(ProviderError::InvalidRequest)?;
        let body = self.execution_request(&request)?;
        let response = self
            .raw
            .complete(provider_request_observed(
                &request,
                body,
                call_headers,
                attempt_context,
            ))
            .await?;
        let qualified = crate::buffered_completion::qualify(
            &response,
            sandhi_core::EndpointFamilyV1::OllamaChat,
            Default::default(),
        );
        if !request.include_native_response {
            crate::native_chat::validate_buffered(
                sandhi_core::EndpointFamilyV1::OllamaChat,
                &response.body,
            )?;
        }
        let mut decoded = decode_ollama_response(response.body, response.usage, &request.model)
            .map_err(crate::buffered_completion::decode_error)?;
        crate::native_chat::repair_tool_ids(&mut decoded)?;
        if !request.include_native_response {
            decoded.extensions.remove("ollama");
        }
        Ok(crate::buffered_completion::observed(
            decoded,
            qualified,
            response.attempts,
        ))
    }

    async fn stream_inner(
        &self,
        request: ChatRequestV1,
        call_headers: http::HeaderMap,
        attempt_context: Option<crate::AttemptContext>,
    ) -> Result<ChatEventStream, ProviderError> {
        request.validate().map_err(ProviderError::InvalidRequest)?;
        let body = self.execution_request(&request)?;
        let source = self
            .raw
            .stream_qualified(
                provider_request_observed(&request, body, call_headers, attempt_context),
                crate::QualifiedStreamDelivery::Streaming,
            )
            .await?;
        Ok(crate::qualified_stream::decode(
            crate::native_chat::decode(
                source.stream,
                request.model,
                sandhi_core::EndpointFamilyV1::OllamaChat,
            ),
            source.observation,
        ))
    }
}

#[async_trait]
impl ChatProvider for TypedOllama {
    fn supports_qualified_streaming(&self) -> bool {
        true
    }
    async fn stream_qualified_observed(
        &self,
        request: ChatRequestV1,
        headers: http::HeaderMap,
        context: crate::AttemptContext,
    ) -> Result<ChatEventStream, ProviderError> {
        self.stream_inner(request, headers, Some(context)).await
    }
    async fn complete_qualified(
        &self,
        request: ChatRequestV1,
        headers: http::HeaderMap,
    ) -> Result<crate::QualifiedCompletion<ChatResponseV1>, ProviderError> {
        self.complete_inner(request, headers, None).await
    }
    async fn complete_qualified_observed(
        &self,
        request: ChatRequestV1,
        headers: http::HeaderMap,
        context: crate::AttemptContext,
    ) -> Result<crate::QualifiedCompletion<ChatResponseV1>, ProviderError> {
        self.complete_inner(request, headers, Some(context)).await
    }
    fn execution_request(&self, request: &ChatRequestV1) -> Result<Value, ProviderError> {
        encode_ollama_request(request)
    }
    fn preflight_request(&self, request: &ChatRequestV1) -> Result<(), ProviderError> {
        self.execution_request(request).map(|_| ())
    }
    fn slug(&self) -> &str {
        "ollama"
    }

    async fn complete(
        &self,
        request: ChatRequestV1,
        call_headers: http::HeaderMap,
    ) -> Result<ChatResponseV1, ProviderError> {
        self.complete_inner(request, call_headers, None)
            .await
            .and_then(crate::QualifiedCompletion::into_response)
    }

    async fn stream(
        &self,
        request: ChatRequestV1,
        call_headers: http::HeaderMap,
    ) -> Result<ChatEventStream, ProviderError> {
        self.stream_inner(request, call_headers, None).await
    }

    async fn complete_observed(
        &self,
        request: ChatRequestV1,
        call_headers: http::HeaderMap,
        attempt_context: crate::AttemptContext,
    ) -> Result<ChatResponseV1, ProviderError> {
        self.complete_inner(request, call_headers, Some(attempt_context))
            .await
            .and_then(crate::QualifiedCompletion::into_response)
    }

    async fn stream_observed(
        &self,
        request: ChatRequestV1,
        call_headers: http::HeaderMap,
        attempt_context: crate::AttemptContext,
    ) -> Result<ChatEventStream, ProviderError> {
        self.stream_inner(request, call_headers, Some(attempt_context))
            .await
    }
}

pub fn encode_ollama_request(request: &ChatRequestV1) -> Result<Value, ProviderError> {
    request
        .native_input_for(sandhi_core::EndpointFamilyV1::OllamaChat)
        .map_err(ProviderError::InvalidRequest)?;
    request.validate().map_err(ProviderError::InvalidRequest)?;
    crate::native_chat::validate_history(request)?;
    let mut body = request
        .extensions
        .get("ollama")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    body.insert("model".into(), Value::String(request.model.clone()));
    let mut messages = Vec::new();
    let mut outstanding = BTreeMap::new();
    for message in &request.messages {
        let mut encoded = encode_message(message)?;
        match message {
            ChatMessageV1::Assistant { tool_calls, .. } => {
                for call in tool_calls {
                    if outstanding
                        .insert(call.id.clone(), call.name.clone())
                        .is_some()
                    {
                        return Err(ProviderError::InvalidRequest(
                            "duplicate outstanding Ollama call identity".into(),
                        ));
                    }
                }
            }
            ChatMessageV1::Tool { tool_call_id, .. } => {
                let name = outstanding.remove(tool_call_id).ok_or_else(|| {
                    ProviderError::InvalidRequest(
                        "Ollama result references unknown call identity".into(),
                    )
                })?;
                encoded["tool_name"] = Value::String(name);
            }
            _ => {}
        }
        messages.push(encoded);
    }
    body.insert("messages".into(), Value::Array(messages));
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
        body.insert("format".into(), normalize_response_format(format));
    }
    let mut body = Value::Object(body);
    crate::cache_policy::apply(
        request,
        sandhi_core::EndpointFamilyV1::OllamaChat,
        &mut body,
    )?;
    Ok(body)
}

/// Normalize a client-supplied `response_format` into what Ollama's `format` field expects: a
/// bare JSON schema object, or the string `"json"` for unstructured-but-valid-JSON mode.
///
/// `response_format` on `ChatRequestV1` is a raw passthrough of whatever the ingress dialect
/// decoded (see `sandhi-proxy::codec`), so an OpenAI Chat Completions client arrives here still
/// wrapped as `{"type":"json_schema","json_schema":{"name":..,"schema":{..}}}` — Ollama wants
/// just the inner `schema`. OpenAI's schema-less `{"type":"json_object"}` mode has no equivalent
/// shape; Ollama's analogous "valid JSON, no schema" mode is the bare string `"json"`. Any other
/// value (already a bare schema, or an Ollama-native string) passes through untouched.
fn normalize_response_format(format: &Value) -> Value {
    match format.get("type").and_then(Value::as_str) {
        Some("json_schema") => format
            .get("json_schema")
            .and_then(|j| j.get("schema"))
            .cloned()
            .unwrap_or_else(|| format.clone()),
        Some("json_object") => Value::String("json".into()),
        _ => format.clone(),
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn w3d_fields_are_ignored_no_leak() {
        // Consumer-decision row: Ollama honors neither field.
        let request: ChatRequestV1 = serde_json::from_value(json!({
            "model": "llama3",
            "messages": [{"role": "user", "content": "hi"}],
            "reasoning_effort": "high",
            "thinking": {"enabled": true}
        }))
        .unwrap();
        let body = encode_ollama_request(&request).unwrap();
        assert!(body.get("reasoning_effort").is_none());
        assert!(body.get("thinking").is_none());
        assert!(body["options"].get("thinking").is_none());
    }

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
                cache_read_observation: None,
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

    #[test]
    fn openai_json_schema_wrapper_is_unwrapped_to_bare_schema() {
        let request: ChatRequestV1 = serde_json::from_value(json!({
            "model": "llama3",
            "messages": [{"role": "user", "content": "hi"}],
            "response_format": {
                "type": "json_schema",
                "json_schema": {
                    "name": "categorize",
                    "schema": {"type": "object", "properties": {"category": {"type": "string"}}}
                }
            }
        }))
        .unwrap();
        let body = encode_ollama_request(&request).unwrap();
        assert_eq!(
            body["format"],
            json!({"type": "object", "properties": {"category": {"type": "string"}}})
        );
    }

    #[test]
    fn openai_json_object_mode_maps_to_ollama_json_string() {
        let request: ChatRequestV1 = serde_json::from_value(json!({
            "model": "llama3",
            "messages": [{"role": "user", "content": "hi"}],
            "response_format": {"type": "json_object"}
        }))
        .unwrap();
        let body = encode_ollama_request(&request).unwrap();
        assert_eq!(body["format"], json!("json"));
    }

    #[test]
    fn bare_ollama_native_schema_passes_through_unchanged() {
        let schema = json!({"type": "object", "properties": {"x": {"type": "string"}}});
        let request: ChatRequestV1 = serde_json::from_value(json!({
            "model": "llama3",
            "messages": [{"role": "user", "content": "hi"}],
            "response_format": schema
        }))
        .unwrap();
        let body = encode_ollama_request(&request).unwrap();
        assert_eq!(body["format"], schema);
    }
}
