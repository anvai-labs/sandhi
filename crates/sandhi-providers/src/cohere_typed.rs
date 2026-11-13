//! Canonical chat v1 ↔ Cohere v2 Chat codec.

use crate::typed::{provider_request_observed, ChatEventStream, ChatProvider};
use crate::{ParsedUsage, Provider, ProviderError};
use async_trait::async_trait;
use sandhi_core::{
    AssistantOutputV1, ChatMessageV1, ChatRequestV1, ChatResponseV1, ContentPart, FinishReasonV1,
    MessageContent, ToolCallV1, ToolChoiceMode, ToolChoiceV1,
};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::sync::Arc;

pub(crate) struct TypedCohere {
    raw: Arc<dyn Provider>,
}

impl TypedCohere {
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
            sandhi_core::EndpointFamilyV1::CohereChat,
            Default::default(),
        );
        if !request.include_native_response {
            crate::native_chat::validate_buffered(
                sandhi_core::EndpointFamilyV1::CohereChat,
                &response.body,
            )?;
        }
        let mut decoded = decode_cohere_response(response.body, response.usage, &request.model)
            .map_err(crate::buffered_completion::decode_error)?;
        crate::native_chat::repair_tool_ids(&mut decoded)?;
        if !request.include_native_response {
            decoded.extensions.remove("cohere");
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
                sandhi_core::EndpointFamilyV1::CohereChat,
            ),
            source.observation,
        ))
    }
}

#[async_trait]
impl ChatProvider for TypedCohere {
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
        encode_cohere_request(request)
    }
    fn preflight_request(&self, request: &ChatRequestV1) -> Result<(), ProviderError> {
        self.execution_request(request).map(|_| ())
    }
    fn slug(&self) -> &str {
        "cohere"
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

pub fn encode_cohere_request(request: &ChatRequestV1) -> Result<Value, ProviderError> {
    request
        .native_input_for(sandhi_core::EndpointFamilyV1::CohereChat)
        .map_err(ProviderError::InvalidRequest)?;
    request.validate().map_err(ProviderError::InvalidRequest)?;
    crate::native_chat::validate_history(request)?;
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
    let mut body = Value::Object(body);
    crate::cache_policy::apply(
        request,
        sandhi_core::EndpointFamilyV1::CohereChat,
        &mut body,
    )?;
    Ok(body)
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
    let mut text = message
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|part| part.get("text").and_then(Value::as_str))
        .collect::<String>();
    if let Some(plan) = message.get("tool_plan").and_then(Value::as_str) {
        text = format!("{plan}{text}");
    }
    let tool_calls = decode_tool_calls(message.get("tool_calls"));
    Ok(ChatResponseV1 {
        schema_version: sandhi_core::CHAT_SCHEMA_VERSION_V1.into(),
        id: body.get("id").and_then(Value::as_str).map(str::to_owned),
        model: requested_model.into(),
        output: AssistantOutputV1 {
            reasoning: None,
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

#[cfg(test)]
mod tests {
    use super::*;

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
                cache_read_observation: None,
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
