//! Typed codec for the OpenAI Responses item/event protocol.

use crate::typed::{provider_request, ChatEventStream, ChatProvider};
use crate::{
    parse_openai_responses_usage, ByteStream, OpenAiResponsesProfile, ParsedUsage, Provider,
    ProviderError,
};
use async_trait::async_trait;
use sandhi_core::{
    AssistantOutputV1, ChatMessageV1, ChatRequestV1, ChatResponseV1, ChatStreamEventV1,
    ContentPart, FinishReasonV1, MessageContent, ToolCallV1, ToolChoiceMode, ToolChoiceV1,
    UsageCompleteness, UsageV2,
};
use serde_json::{json, Map, Value};
use std::{collections::BTreeMap, sync::Arc};

pub(crate) struct TypedOpenAiResponses {
    slug: String,
    raw: Arc<dyn Provider>,
    profile: OpenAiResponsesProfile,
}

impl TypedOpenAiResponses {
    pub(crate) fn new(
        slug: String,
        raw: Arc<dyn Provider>,
        profile: OpenAiResponsesProfile,
    ) -> Self {
        Self { slug, raw, profile }
    }
}

#[async_trait]
impl ChatProvider for TypedOpenAiResponses {
    fn slug(&self) -> &str {
        &self.slug
    }

    async fn complete(&self, request: ChatRequestV1) -> Result<ChatResponseV1, ProviderError> {
        if self.profile == OpenAiResponsesProfile::ChatGptCodex {
            return aggregate_stream(self.stream(request).await?).await;
        }
        let body = encode_responses_request_for_profile(&request, self.profile)?;
        let response = self.raw.complete(provider_request(&request, body)).await?;
        let mut decoded = decode_responses_response(response.body, response.usage, &request.model)?;
        decoded.usage.attempts = response.attempts;
        Ok(decoded)
    }

    async fn stream(&self, request: ChatRequestV1) -> Result<ChatEventStream, ProviderError> {
        let body = encode_responses_request_for_profile(&request, self.profile)?;
        let stream = self.raw.stream(provider_request(&request, body)).await?;
        Ok(decode_responses_stream(stream, request.model))
    }
}

#[cfg(test)]
fn encode_responses_request(request: &ChatRequestV1) -> Result<Value, ProviderError> {
    encode_responses_request_for_profile(request, OpenAiResponsesProfile::Standard)
}

fn encode_responses_request_for_profile(
    request: &ChatRequestV1,
    profile: OpenAiResponsesProfile,
) -> Result<Value, ProviderError> {
    request.validate().map_err(ProviderError::InvalidRequest)?;
    if request.stop.is_some() {
        return Err(ProviderError::InvalidRequest(
            "OpenAI Responses does not support the Chat Completions stop field".into(),
        ));
    }
    if request.seed.is_some() {
        return Err(ProviderError::InvalidRequest(
            "OpenAI Responses does not support the Chat Completions seed field".into(),
        ));
    }
    let mut body = request
        .extensions
        .get("openai_responses")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let mut input = Vec::new();
    for message in &request.messages {
        if profile != OpenAiResponsesProfile::ChatGptCodex
            || !matches!(
                message,
                ChatMessageV1::Developer { .. } | ChatMessageV1::System { .. }
            )
        {
            input.extend(encode_input_message(message)?);
        }
    }
    if let Some(extra) = body
        .remove("input_items")
        .and_then(|value| value.as_array().cloned())
    {
        input.extend(extra);
    }
    body.insert("model".into(), Value::String(request.model.clone()));
    body.insert("input".into(), Value::Array(input));
    if !request.tools.is_empty() {
        body.insert(
            "tools".into(),
            Value::Array(
                request
                    .tools
                    .iter()
                    .map(|tool| {
                        json!({
                            "type":"function", "name":tool.name,
                            "description":tool.description, "parameters":tool.parameters,
                            "strict":tool.strict,
                        })
                    })
                    .collect(),
            ),
        );
    }
    if let Some(choice) = &request.tool_choice {
        body.insert("tool_choice".into(), encode_tool_choice(choice));
    }
    insert_optional(&mut body, "temperature", request.temperature);
    insert_optional(&mut body, "max_output_tokens", request.max_output_tokens);
    if let Some(format) = &request.response_format {
        body.insert(
            "text".into(),
            json!({"format": responses_text_format(format)}),
        );
    }
    if profile == OpenAiResponsesProfile::ChatGptCodex {
        let instructions = body
            .get("instructions")
            .and_then(Value::as_str)
            .filter(|text| !text.trim().is_empty())
            .map(str::to_owned)
            .or(codex_instructions(&request.messages)?);
        let Some(instructions) = instructions else {
            return Err(ProviderError::InvalidRequest(
                "ChatGPT Codex Responses requires a non-empty developer or system instruction"
                    .into(),
            ));
        };
        body.insert("instructions".into(), Value::String(instructions));
        body.insert("store".into(), Value::Bool(false));
        body.insert("stream".into(), Value::Bool(true));
        body.remove("temperature");
        body.remove("max_output_tokens");
        body.entry("include")
            .or_insert_with(|| json!(["reasoning.encrypted_content"]));
    }
    Ok(Value::Object(body))
}

fn codex_instructions(messages: &[ChatMessageV1]) -> Result<Option<String>, ProviderError> {
    let mut blocks = Vec::new();
    for message in messages {
        let content = match message {
            ChatMessageV1::Developer { content, .. } | ChatMessageV1::System { content, .. } => {
                content
            }
            _ => continue,
        };
        match content {
            MessageContent::Text(text) => blocks.push(text.clone()),
            MessageContent::Parts(parts) => {
                let mut text = String::new();
                for part in parts {
                    match part {
                        ContentPart::Text { text: part } => text.push_str(part),
                        _ => {
                            return Err(ProviderError::InvalidRequest(
                                "ChatGPT Codex instructions support text content only".into(),
                            ));
                        }
                    }
                }
                blocks.push(text);
            }
        }
    }
    let joined = blocks.join("\n\n");
    Ok((!joined.trim().is_empty()).then_some(joined))
}

fn encode_input_message(message: &ChatMessageV1) -> Result<Vec<Value>, ProviderError> {
    match message {
        ChatMessageV1::Developer { content, .. } => {
            Ok(vec![response_message("developer", content, false)?])
        }
        ChatMessageV1::System { content, .. } => {
            Ok(vec![response_message("system", content, false)?])
        }
        ChatMessageV1::User { content, .. } => {
            Ok(vec![response_message("user", content, false)?])
        }
        ChatMessageV1::Assistant {
            content,
            tool_calls,
            refusal,
            ..
        } => {
            let mut items = Vec::new();
            if let Some(content) = content {
                items.push(response_message("assistant", content, true)?);
            }
            if let Some(refusal) = refusal {
                items.push(json!({
                    "type":"message", "role":"assistant",
                    "content":[{"type":"refusal", "refusal":refusal}]
                }));
            }
            items.extend(tool_calls.iter().map(|call| {
                json!({
                    "type":"function_call", "call_id":call.id,
                    "name":call.name, "arguments":call.arguments
                })
            }));
            Ok(items)
        }
        ChatMessageV1::Tool {
            content,
            tool_call_id,
        } => Ok(vec![json!({
            "type":"function_call_output", "call_id":tool_call_id,
            "output":tool_output(content)
        })]),
        ChatMessageV1::Function { .. } => Err(ProviderError::InvalidRequest(
            "legacy function-role messages cannot be linked losslessly in Responses; use role=tool with tool_call_id"
                .into(),
        )),
    }
}

fn response_message(
    role: &str,
    content: &MessageContent,
    assistant: bool,
) -> Result<Value, ProviderError> {
    let parts = match content {
        MessageContent::Text(text) => vec![if assistant {
            json!({"type":"output_text", "text":text})
        } else {
            json!({"type":"input_text", "text":text})
        }],
        MessageContent::Parts(parts) => parts
            .iter()
            .map(|part| encode_response_content_part(part, assistant))
            .collect::<Result<Vec<_>, _>>()?,
    };
    Ok(json!({"type":"message", "role":role, "content":parts}))
}

fn encode_response_content_part(
    part: &ContentPart,
    assistant: bool,
) -> Result<Value, ProviderError> {
    match part {
        ContentPart::Text { text } if assistant => Ok(json!({"type":"output_text","text":text})),
        ContentPart::Text { text } => Ok(json!({"type":"input_text","text":text})),
        ContentPart::ImageUrl { image_url, detail } if !assistant => {
            Ok(json!({"type":"input_image","image_url":image_url,"detail":detail}))
        }
        ContentPart::File {
            file_id,
            file_data,
            filename,
        } if !assistant => Ok(json!({
            "type":"input_file", "file_id":file_id, "file_data":file_data, "filename":filename
        })),
        ContentPart::InputAudio { .. } => Err(ProviderError::InvalidRequest(
            "input_audio is not represented by the Responses HTTP content-item contract".into(),
        )),
        _ => Err(ProviderError::InvalidRequest(
            "assistant Responses history currently supports text output only".into(),
        )),
    }
}

fn tool_output(content: &MessageContent) -> Value {
    match content {
        MessageContent::Text(text) => Value::String(text.clone()),
        MessageContent::Parts(parts) => serde_json::to_value(parts).unwrap_or(Value::Null),
    }
}

fn encode_tool_choice(choice: &ToolChoiceV1) -> Value {
    match choice {
        ToolChoiceV1::Mode(ToolChoiceMode::None) => Value::String("none".into()),
        ToolChoiceV1::Mode(ToolChoiceMode::Auto) => Value::String("auto".into()),
        ToolChoiceV1::Mode(ToolChoiceMode::Required) => Value::String("required".into()),
        ToolChoiceV1::Function { name } => json!({"type":"function","name":name}),
    }
}

fn responses_text_format(format: &Value) -> Value {
    if format.get("type").and_then(Value::as_str) == Some("json_schema") {
        let schema = &format["json_schema"];
        json!({
            "type":"json_schema", "name":schema["name"], "schema":schema["schema"],
            "strict":schema.get("strict").cloned().unwrap_or(Value::Bool(false))
        })
    } else {
        format.clone()
    }
}

fn insert_optional<T: serde::Serialize>(
    body: &mut Map<String, Value>,
    key: &str,
    value: Option<T>,
) {
    if let Some(value) = value.and_then(|value| serde_json::to_value(value).ok()) {
        body.insert(key.into(), value);
    }
}

pub(crate) fn decode_responses_response(
    body: Value,
    parsed_usage: ParsedUsage,
    requested_model: &str,
) -> Result<ChatResponseV1, ProviderError> {
    let output = body
        .get("output")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            ProviderError::Transport("OpenAI Responses response has no output array".into())
        })?;
    let mut text = String::new();
    let mut refusal = String::new();
    let mut tool_calls = Vec::new();
    for item in output {
        match item.get("type").and_then(Value::as_str) {
            Some("message") => {
                for part in item
                    .get("content")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    match part.get("type").and_then(Value::as_str) {
                        Some("output_text") => {
                            text.push_str(part.get("text").and_then(Value::as_str).unwrap_or(""))
                        }
                        Some("refusal") => refusal
                            .push_str(part.get("refusal").and_then(Value::as_str).unwrap_or("")),
                        _ => {}
                    }
                }
            }
            Some("function_call") => {
                if let (Some(id), Some(name)) = (
                    item.get("call_id").and_then(Value::as_str),
                    item.get("name").and_then(Value::as_str),
                ) {
                    tool_calls.push(ToolCallV1 {
                        id: id.into(),
                        name: name.into(),
                        arguments: item
                            .get("arguments")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .into(),
                        extensions: BTreeMap::from([("openai_responses".into(), item.clone())]),
                    });
                }
            }
            _ => {}
        }
    }
    let usage = usage_v2(&body, parsed_usage);
    let finish_reason = response_finish_reason(&body, !tool_calls.is_empty());
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
            refusal: (!refusal.is_empty()).then_some(refusal),
        },
        finish_reason: Some(finish_reason),
        usage,
        extensions: BTreeMap::from([("openai_responses".into(), body)]),
    })
}

fn response_finish_reason(body: &Value, has_tools: bool) -> FinishReasonV1 {
    if has_tools {
        return FinishReasonV1::ToolCalls;
    }
    match body.get("status").and_then(Value::as_str) {
        Some("completed") => FinishReasonV1::Stop,
        Some("incomplete") => match body
            .pointer("/incomplete_details/reason")
            .and_then(Value::as_str)
        {
            Some("max_output_tokens") => FinishReasonV1::Length,
            Some("content_filter") => FinishReasonV1::ContentFilter,
            _ => FinishReasonV1::Unknown,
        },
        _ => FinishReasonV1::Unknown,
    }
}

fn usage_v2(body: &Value, parsed: ParsedUsage) -> UsageV2 {
    let outcome = match body.get("status").and_then(Value::as_str) {
        Some("completed") => "success",
        Some("incomplete") => "incomplete",
        Some("failed") => "error",
        _ => "unknown",
    };
    UsageV2 {
        reasoning_tokens: body
            .pointer("/usage/output_tokens_details/reasoning_tokens")
            .and_then(Value::as_u64),
        completeness: UsageCompleteness::Final,
        outcome: Some(outcome.into()),
        ..parsed.into()
    }
}

async fn aggregate_stream(mut stream: ChatEventStream) -> Result<ChatResponseV1, ProviderError> {
    use futures_util::StreamExt;
    let mut id = None;
    let mut model = String::new();
    let mut text = String::new();
    let mut refusal = String::new();
    let mut tools = BTreeMap::<u32, ToolCallV1>::new();
    let mut usage = UsageV2::default();
    let mut finish_reason = None;
    while let Some(event) = stream.next().await {
        match event? {
            ChatStreamEventV1::ResponseStart {
                id: response_id,
                model: response_model,
            } => {
                id = response_id;
                model = response_model;
            }
            ChatStreamEventV1::TextDelta { delta } => text.push_str(&delta),
            ChatStreamEventV1::RefusalDelta { delta } => refusal.push_str(&delta),
            ChatStreamEventV1::ReasoningDelta { .. } => {}
            ChatStreamEventV1::ToolCallStart { index, id, name } => {
                tools.insert(
                    index,
                    ToolCallV1 {
                        id,
                        name,
                        arguments: String::new(),
                        extensions: BTreeMap::new(),
                    },
                );
            }
            ChatStreamEventV1::ToolCallArgumentsDelta { index, delta } => {
                if let Some(call) = tools.get_mut(&index) {
                    call.arguments.push_str(&delta);
                }
            }
            ChatStreamEventV1::ToolCallEnd { .. } => {}
            ChatStreamEventV1::Usage { usage: final_usage } => usage = final_usage,
            ChatStreamEventV1::Finish { reason } => finish_reason = Some(reason),
            ChatStreamEventV1::Error { error } => {
                return Err(ProviderError::Transport(error.message));
            }
        }
    }
    Ok(ChatResponseV1 {
        schema_version: sandhi_core::CHAT_SCHEMA_VERSION_V1.into(),
        id,
        model,
        output: AssistantOutputV1 {
            content: (!text.is_empty()).then_some(MessageContent::Text(text)),
            tool_calls: tools.into_values().collect(),
            refusal: (!refusal.is_empty()).then_some(refusal),
        },
        finish_reason,
        usage,
        extensions: BTreeMap::from([(
            "openai_responses".into(),
            json!({"transport":"chatgpt_codex_stream_aggregate"}),
        )]),
    })
}

fn decode_responses_stream(mut raw: ByteStream, requested_model: String) -> ChatEventStream {
    use futures_util::StreamExt;
    let stream = async_stream::try_stream! {
        let mut buffer = Vec::<u8>::new();
        let mut started = false;
        let mut open_tools = BTreeMap::<u32, ()>::new();
        let mut emitted_usage = false;
        let mut emitted_finish = false;
        while let Some(chunk) = raw.next().await {
            let chunk = chunk?;
            let attempts = chunk.attempts;
            if !chunk.data.is_empty() {
                buffer.extend_from_slice(&chunk.data);
                while let Some(pos) = buffer.iter().position(|byte| *byte == b'\n') {
                    let line: Vec<u8> = buffer.drain(..=pos).collect();
                    let Some(event) = crate::sse_data_json(&line) else { continue; };
                    let kind = event.get("type").and_then(Value::as_str).unwrap_or("");
                    if kind == "response.created" {
                        let response = &event["response"];
                        yield ChatStreamEventV1::ResponseStart {
                            id: response.get("id").and_then(Value::as_str).map(str::to_owned),
                            model: response.get("model").and_then(Value::as_str).unwrap_or(&requested_model).into(),
                        };
                        started = true;
                    } else if !started && matches!(kind,
                        "response.output_text.delta" | "response.refusal.delta" |
                        "response.function_call_arguments.delta") {
                        yield ChatStreamEventV1::ResponseStart { id: None, model: requested_model.clone() };
                        started = true;
                    }
                    match kind {
                        "response.output_text.delta" => if let Some(delta) = event.get("delta").and_then(Value::as_str) {
                            yield ChatStreamEventV1::TextDelta { delta: delta.into() };
                        },
                        "response.refusal.delta" => if let Some(delta) = event.get("delta").and_then(Value::as_str) {
                            yield ChatStreamEventV1::RefusalDelta { delta: delta.into() };
                        },
                        "response.reasoning_summary_text.delta" => if let Some(delta) = event.get("delta").and_then(Value::as_str) {
                            yield ChatStreamEventV1::ReasoningDelta { delta: delta.into() };
                        },
                        "response.output_item.added" => {
                            let item = &event["item"];
                            if item.get("type").and_then(Value::as_str) == Some("function_call") {
                                let index = event.get("output_index").and_then(Value::as_u64).unwrap_or(0) as u32;
                                if let (Some(id), Some(name)) = (
                                    item.get("call_id").and_then(Value::as_str),
                                    item.get("name").and_then(Value::as_str),
                                ) {
                                    open_tools.insert(index, ());
                                    yield ChatStreamEventV1::ToolCallStart { index, id:id.into(), name:name.into() };
                                }
                            }
                        }
                        "response.function_call_arguments.delta" => {
                            let index = event.get("output_index").and_then(Value::as_u64).unwrap_or(0) as u32;
                            if let Some(delta) = event.get("delta").and_then(Value::as_str) {
                                yield ChatStreamEventV1::ToolCallArgumentsDelta { index, delta:delta.into() };
                            }
                        }
                        "response.output_item.done" => {
                            let index = event.get("output_index").and_then(Value::as_u64).unwrap_or(0) as u32;
                            if open_tools.remove(&index).is_some() {
                                yield ChatStreamEventV1::ToolCallEnd { index };
                            }
                        }
                        "response.completed" | "response.incomplete" | "response.failed" => {
                            let response = &event["response"];
                            if let Some(parsed) = parse_openai_responses_usage(response) {
                                let mut usage = usage_v2(response, parsed);
                                usage.attempts = attempts;
                                usage.outcome = Some(match kind {
                                    "response.completed" => "success",
                                    "response.incomplete" => "incomplete",
                                    _ => "error",
                                }.into());
                                yield ChatStreamEventV1::Usage { usage };
                                emitted_usage = true;
                            }
                            for index in open_tools.keys().copied().collect::<Vec<_>>() {
                                yield ChatStreamEventV1::ToolCallEnd { index };
                            }
                            let has_tools = !open_tools.is_empty() || response.get("output").and_then(Value::as_array)
                                .is_some_and(|items| items.iter().any(|item| item.get("type").and_then(Value::as_str) == Some("function_call")));
                            open_tools.clear();
                            yield ChatStreamEventV1::Finish { reason: response_finish_reason(response, has_tools) };
                            emitted_finish = true;
                        }
                        "error" => {
                            let message = event.pointer("/error/message").or_else(|| event.get("message"))
                                .and_then(Value::as_str).unwrap_or("OpenAI Responses stream error");
                            Err(ProviderError::Transport(message.into()))?;
                        }
                        _ => {}
                    }
                }
            }
            if let Some(parsed) = chunk.usage {
                if !emitted_usage {
                    let mut usage: UsageV2 = parsed.into();
                    usage.completeness = UsageCompleteness::Final;
                    usage.attempts = attempts;
                    usage.outcome = Some("success".into());
                    yield ChatStreamEventV1::Usage { usage };
                    emitted_usage = true;
                }
            }
        }
        if !emitted_finish {
            yield ChatStreamEventV1::Finish { reason: FinishReasonV1::Unknown };
        }
    };
    Box::pin(stream)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use futures_util::StreamExt;
    use wiremock::matchers::{body_partial_json, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn request() -> ChatRequestV1 {
        serde_json::from_value(json!({
            "model":"gpt-test",
            "messages":[
                {"role":"developer","content":"be precise"},
                {"role":"user","content":"weather?"},
                {"role":"assistant","tool_calls":[{"id":"call_1","name":"weather","arguments":"{\"city\":\"Austin\"}"}]},
                {"role":"tool","tool_call_id":"call_1","content":"sunny"}
            ],
            "tools":[{"name":"weather","parameters":{"type":"object"}}],
            "tool_choice":{"name":"weather"}
        })).unwrap()
    }

    #[test]
    fn encodes_items_tools_and_tool_outputs_without_chat_wrappers() {
        let body = encode_responses_request(&request()).unwrap();
        assert!(body.get("messages").is_none());
        assert_eq!(body["input"][0]["type"], "message");
        assert_eq!(body["input"][2]["type"], "function_call");
        assert_eq!(body["input"][3]["type"], "function_call_output");
        assert_eq!(body["tools"][0]["name"], "weather");
        assert_eq!(
            body["tool_choice"],
            json!({"type":"function","name":"weather"})
        );
    }

    #[test]
    fn decodes_text_tools_reasoning_and_cache_usage() {
        let body = json!({
            "id":"resp_1","model":"gpt-test","status":"completed",
            "output":[
                {"type":"message","content":[{"type":"output_text","text":"hi"}]},
                {"type":"function_call","call_id":"call_1","name":"weather","arguments":"{}"}
            ],
            "usage":{"input_tokens":100,"output_tokens":20,
                "input_tokens_details":{"cached_tokens":60},
                "output_tokens_details":{"reasoning_tokens":7}}
        });
        let parsed = parse_openai_responses_usage(&body).unwrap();
        let response = decode_responses_response(body, parsed, "fallback").unwrap();
        assert_eq!(
            response.output.content,
            Some(MessageContent::Text("hi".into()))
        );
        assert_eq!(response.output.tool_calls[0].id, "call_1");
        assert_eq!(response.finish_reason, Some(FinishReasonV1::ToolCalls));
        assert_eq!(response.usage.tokens_in, 40);
        assert_eq!(response.usage.cache_read_tokens, 60);
        assert_eq!(response.usage.reasoning_tokens, Some(7));
    }

    #[tokio::test]
    async fn streaming_decode_is_chunk_boundary_invariant() {
        let sse = concat!(
            "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_1\",\"model\":\"gpt-test\"}}\n\n",
            "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"function_call\",\"call_id\":\"call_1\",\"name\":\"weather\"}}\n\n",
            "data: {\"type\":\"response.function_call_arguments.delta\",\"output_index\":0,\"delta\":\"{}\"}\n\n",
            "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"function_call\"}}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"output\":[{\"type\":\"function_call\"}],\"usage\":{\"input_tokens\":3,\"output_tokens\":2}}}\n\n"
        );
        for split in 0..=sse.len() {
            let chunks = vec![
                Ok(crate::StreamChunk {
                    data: Bytes::copy_from_slice(&sse.as_bytes()[..split]),
                    usage: None,
                    attempts: 2,
                }),
                Ok(crate::StreamChunk {
                    data: Bytes::copy_from_slice(&sse.as_bytes()[split..]),
                    usage: None,
                    attempts: 2,
                }),
            ];
            let raw: ByteStream = Box::pin(futures_util::stream::iter(chunks));
            let events: Vec<_> = decode_responses_stream(raw, "fallback".into())
                .map(|event| event.unwrap())
                .collect()
                .await;
            assert!(
                matches!(
                    events.first(),
                    Some(ChatStreamEventV1::ResponseStart { .. })
                ),
                "split {split}"
            );
            assert!(events.iter().any(|event| matches!(event, ChatStreamEventV1::ToolCallStart { id, .. } if id == "call_1")), "split {split}");
            assert_eq!(
                events
                    .iter()
                    .filter(|event| matches!(event, ChatStreamEventV1::Usage { .. }))
                    .count(),
                1,
                "split {split}"
            );
            assert!(
                matches!(
                    events.last(),
                    Some(ChatStreamEventV1::Finish {
                        reason: FinishReasonV1::ToolCalls
                    })
                ),
                "split {split}"
            );
        }
    }

    #[tokio::test]
    async fn chatgpt_profile_forces_sse_constraints_and_aggregates_complete() {
        let server = MockServer::start().await;
        let sse = concat!(
            "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_1\",\"model\":\"gpt-test\"}}\n\n",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"ok\"}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"output\":[],\"usage\":{\"input_tokens\":5,\"output_tokens\":2}}}\n\n"
        );
        Mock::given(method("POST"))
            .and(path("/responses"))
            .and(body_partial_json(json!({
                "model":"gpt-test", "instructions":"be precise", "store":false, "stream":true,
                "input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]}]
            })))
            .respond_with(ResponseTemplate::new(200).set_body_raw(sse, "text/event-stream"))
            .mount(&server)
            .await;
        let request: ChatRequestV1 = serde_json::from_value(json!({
            "model":"gpt-test", "temperature":0.5, "max_output_tokens":100,
            "messages":[
                {"role":"developer","content":"be precise"},
                {"role":"user","content":"hi"}
            ]
        }))
        .unwrap();
        let response = crate::ProviderRuntime::new()
            .chatgpt_responses(
                "openai",
                server.uri(),
                "oauth",
                reqwest::header::HeaderMap::new(),
                Some(0),
                None,
                None,
            )
            .complete(request)
            .await
            .unwrap();
        assert_eq!(
            response.output.content,
            Some(MessageContent::Text("ok".into()))
        );
        assert_eq!(response.usage.tokens_in, 5);
        assert_eq!(response.usage.tokens_out, 2);
        assert_eq!(response.finish_reason, Some(FinishReasonV1::Stop));
    }
}
