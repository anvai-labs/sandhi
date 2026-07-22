//! HTTP ingress/egress codecs for the typed gateway front door.
//!
//! These codecs translate public OpenAI Chat Completions and Anthropic Messages documents to
//! Sandhi's canonical chat contract. Provider-native JSON never reaches the runtime handle.

use std::collections::BTreeMap;

use sandhi_core::{
    ChatMessageV1, ChatRequestV1, ChatResponseV1, ChatStreamEventV1, ContentPart, FinishReasonV1,
    MessageContent, RequestMetadataV1, ToolCallV1, ToolChoiceMode, ToolChoiceV1, ToolDefinitionV1,
    UsageV2, CHAT_SCHEMA_VERSION_V1,
};
use serde_json::{json, Map, Value};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IngressDialect {
    OpenAi,
    Anthropic,
}

pub(crate) fn decode_request(
    dialect: IngressDialect,
    body: Value,
    metadata: RequestMetadataV1,
) -> Result<(ChatRequestV1, bool), String> {
    match dialect {
        IngressDialect::OpenAi => decode_openai_request(body, metadata),
        IngressDialect::Anthropic => decode_anthropic_request(body, metadata),
    }
}

fn decode_openai_request(
    body: Value,
    metadata: RequestMetadataV1,
) -> Result<(ChatRequestV1, bool), String> {
    let object = body
        .as_object()
        .ok_or_else(|| "request body must be a JSON object".to_string())?;
    let model = required_string(object, "model")?;
    let messages = object
        .get("messages")
        .and_then(Value::as_array)
        .ok_or_else(|| "messages must be an array".to_string())?
        .iter()
        .map(decode_openai_message)
        .collect::<Result<Vec<_>, _>>()?;
    let tools = object
        .get("tools")
        .and_then(Value::as_array)
        .map(|values| {
            values
                .iter()
                .map(|value| {
                    let function = value
                        .get("function")
                        .and_then(Value::as_object)
                        .ok_or_else(|| "tools entries must contain function objects".to_string())?;
                    Ok(ToolDefinitionV1 {
                        name: required_string(function, "name")?,
                        description: optional_string(function, "description")?,
                        parameters: function
                            .get("parameters")
                            .cloned()
                            .unwrap_or_else(|| json!({"type":"object"})),
                        strict: function.get("strict").and_then(Value::as_bool),
                    })
                })
                .collect::<Result<Vec<_>, String>>()
        })
        .transpose()?
        .unwrap_or_default();
    let request = ChatRequestV1 {
        schema_version: CHAT_SCHEMA_VERSION_V1.into(),
        model,
        messages,
        tools,
        tool_choice: object
            .get("tool_choice")
            .map(decode_openai_tool_choice)
            .transpose()?,
        temperature: object.get("temperature").and_then(Value::as_f64),
        max_output_tokens: object
            .get("max_completion_tokens")
            .or_else(|| object.get("max_tokens"))
            .and_then(Value::as_u64),
        stop: decode_stop(object.get("stop"))?,
        response_format: object.get("response_format").cloned(),
        seed: object.get("seed").and_then(Value::as_i64),
        metadata,
        extensions: BTreeMap::from([("openai".into(), body.clone())]),
    };
    request.validate()?;
    Ok((
        request,
        object
            .get("stream")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    ))
}

fn decode_openai_message(value: &Value) -> Result<ChatMessageV1, String> {
    let object = value
        .as_object()
        .ok_or_else(|| "message must be an object".to_string())?;
    let role = required_string(object, "role")?;
    let name = optional_string(object, "name")?;
    match role.as_str() {
        "developer" => Ok(ChatMessageV1::Developer {
            content: decode_openai_content(required(object, "content")?)?,
            name,
        }),
        "system" => Ok(ChatMessageV1::System {
            content: decode_openai_content(required(object, "content")?)?,
            name,
        }),
        "user" => Ok(ChatMessageV1::User {
            content: decode_openai_content(required(object, "content")?)?,
            name,
        }),
        "assistant" => Ok(ChatMessageV1::Assistant {
            content: object
                .get("content")
                .filter(|value| !value.is_null())
                .map(decode_openai_content)
                .transpose()?,
            name,
            tool_calls: object
                .get("tool_calls")
                .and_then(Value::as_array)
                .map(|calls| calls.iter().map(decode_openai_tool_call).collect())
                .transpose()?
                .unwrap_or_default(),
            refusal: optional_string(object, "refusal")?,
        }),
        "tool" => Ok(ChatMessageV1::Tool {
            content: decode_openai_content(required(object, "content")?)?,
            tool_call_id: required_string(object, "tool_call_id")?,
        }),
        "function" => Ok(ChatMessageV1::Function {
            content: decode_openai_content(required(object, "content")?)?,
            name: name.ok_or_else(|| "function message requires name".to_string())?,
        }),
        other => Err(format!("unsupported OpenAI message role: {other}")),
    }
}

fn decode_openai_content(value: &Value) -> Result<MessageContent, String> {
    if let Some(text) = value.as_str() {
        return Ok(MessageContent::Text(text.into()));
    }
    let parts = value
        .as_array()
        .ok_or_else(|| "message content must be a string or array".to_string())?;
    Ok(MessageContent::Parts(
        parts
            .iter()
            .map(|part| {
                let kind = part.get("type").and_then(Value::as_str).unwrap_or("");
                match kind {
                    "text" | "input_text" => Ok(ContentPart::Text {
                        text: part
                            .get("text")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .into(),
                    }),
                    "image_url" => {
                        let image = part.get("image_url").unwrap_or(&Value::Null);
                        let image_url = image
                            .as_str()
                            .or_else(|| image.get("url").and_then(Value::as_str))
                            .ok_or_else(|| "image_url part requires a URL".to_string())?;
                        Ok(ContentPart::ImageUrl {
                            image_url: image_url.into(),
                            detail: image
                                .get("detail")
                                .and_then(Value::as_str)
                                .map(str::to_owned),
                        })
                    }
                    "input_audio" => {
                        let audio = part.get("input_audio").unwrap_or(&Value::Null);
                        Ok(ContentPart::InputAudio {
                            data: audio
                                .get("data")
                                .and_then(Value::as_str)
                                .ok_or_else(|| "input_audio requires data".to_string())?
                                .into(),
                            format: audio
                                .get("format")
                                .and_then(Value::as_str)
                                .ok_or_else(|| "input_audio requires format".to_string())?
                                .into(),
                        })
                    }
                    "file" | "input_file" => {
                        let file = part.get("file").unwrap_or(part);
                        Ok(ContentPart::File {
                            file_id: file
                                .get("file_id")
                                .and_then(Value::as_str)
                                .map(str::to_owned),
                            file_data: file
                                .get("file_data")
                                .and_then(Value::as_str)
                                .map(str::to_owned),
                            filename: file
                                .get("filename")
                                .and_then(Value::as_str)
                                .map(str::to_owned),
                        })
                    }
                    other => Err(format!("unsupported OpenAI content part: {other}")),
                }
            })
            .collect::<Result<Vec<_>, _>>()?,
    ))
}

fn decode_openai_tool_call(value: &Value) -> Result<ToolCallV1, String> {
    Ok(ToolCallV1 {
        id: value
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(|| "tool call requires id".to_string())?
            .into(),
        name: value
            .pointer("/function/name")
            .and_then(Value::as_str)
            .ok_or_else(|| "tool call requires function.name".to_string())?
            .into(),
        arguments: value
            .pointer("/function/arguments")
            .and_then(Value::as_str)
            .unwrap_or("")
            .into(),
        extensions: BTreeMap::new(),
    })
}

fn decode_openai_tool_choice(value: &Value) -> Result<ToolChoiceV1, String> {
    if let Some(mode) = value.as_str() {
        return Ok(ToolChoiceV1::Mode(match mode {
            "none" => ToolChoiceMode::None,
            "auto" => ToolChoiceMode::Auto,
            "required" => ToolChoiceMode::Required,
            other => return Err(format!("unsupported tool_choice mode: {other}")),
        }));
    }
    value
        .pointer("/function/name")
        .and_then(Value::as_str)
        .map(|name| ToolChoiceV1::Function { name: name.into() })
        .ok_or_else(|| "tool_choice must be a mode or named function".to_string())
}

fn decode_anthropic_request(
    body: Value,
    metadata: RequestMetadataV1,
) -> Result<(ChatRequestV1, bool), String> {
    let object = body
        .as_object()
        .ok_or_else(|| "request body must be a JSON object".to_string())?;
    let mut messages = Vec::new();
    if let Some(system) = object.get("system") {
        messages.push(ChatMessageV1::System {
            content: decode_anthropic_text_content(system)?,
            name: None,
        });
    }
    for message in object
        .get("messages")
        .and_then(Value::as_array)
        .ok_or_else(|| "messages must be an array".to_string())?
    {
        decode_anthropic_message(message, &mut messages)?;
    }
    let tools = object
        .get("tools")
        .and_then(Value::as_array)
        .map(|tools| {
            tools
                .iter()
                .map(|tool| {
                    Ok(ToolDefinitionV1 {
                        name: tool
                            .get("name")
                            .and_then(Value::as_str)
                            .ok_or_else(|| "Anthropic tool requires name".to_string())?
                            .into(),
                        description: tool
                            .get("description")
                            .and_then(Value::as_str)
                            .map(str::to_owned),
                        parameters: tool
                            .get("input_schema")
                            .cloned()
                            .unwrap_or_else(|| json!({"type":"object"})),
                        strict: None,
                    })
                })
                .collect::<Result<Vec<_>, String>>()
        })
        .transpose()?
        .unwrap_or_default();
    let request = ChatRequestV1 {
        schema_version: CHAT_SCHEMA_VERSION_V1.into(),
        model: required_string(object, "model")?,
        messages,
        tools,
        tool_choice: object
            .get("tool_choice")
            .map(decode_anthropic_tool_choice)
            .transpose()?,
        temperature: object.get("temperature").and_then(Value::as_f64),
        max_output_tokens: object.get("max_tokens").and_then(Value::as_u64),
        stop: decode_stop(object.get("stop_sequences"))?,
        response_format: None,
        seed: None,
        metadata,
        extensions: BTreeMap::from([("anthropic".into(), body.clone())]),
    };
    request.validate()?;
    Ok((
        request,
        object
            .get("stream")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    ))
}

fn decode_anthropic_message(
    value: &Value,
    messages: &mut Vec<ChatMessageV1>,
) -> Result<(), String> {
    let role = value
        .get("role")
        .and_then(Value::as_str)
        .ok_or_else(|| "Anthropic message requires role".to_string())?;
    let content = value
        .get("content")
        .ok_or_else(|| "Anthropic message requires content".to_string())?;
    let blocks = if let Some(text) = content.as_str() {
        vec![json!({"type":"text", "text":text})]
    } else {
        content
            .as_array()
            .cloned()
            .ok_or_else(|| "Anthropic content must be a string or array".to_string())?
    };
    match role {
        "assistant" => {
            let mut parts = Vec::new();
            let mut calls = Vec::new();
            for block in &blocks {
                match block.get("type").and_then(Value::as_str) {
                    Some("text") => parts.push(ContentPart::Text {
                        text: block
                            .get("text")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .into(),
                    }),
                    Some("tool_use") => calls.push(ToolCallV1 {
                        id: block.get("id").and_then(Value::as_str).unwrap_or("").into(),
                        name: block
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .into(),
                        arguments: serde_json::to_string(
                            block.get("input").unwrap_or(&Value::Null),
                        )
                        .map_err(|error| error.to_string())?,
                        extensions: BTreeMap::new(),
                    }),
                    Some("thinking") | Some("redacted_thinking") => {}
                    Some(other) => {
                        return Err(format!("unsupported Anthropic assistant block: {other}"))
                    }
                    None => return Err("Anthropic content block requires type".into()),
                }
            }
            messages.push(ChatMessageV1::Assistant {
                content: content_from_parts(parts),
                name: None,
                tool_calls: calls,
                refusal: None,
            });
        }
        "user" => {
            let mut parts = Vec::new();
            for block in &blocks {
                if block.get("type").and_then(Value::as_str) == Some("tool_result") {
                    if !parts.is_empty() {
                        messages.push(ChatMessageV1::User {
                            content: MessageContent::Parts(std::mem::take(&mut parts)),
                            name: None,
                        });
                    }
                    messages.push(ChatMessageV1::Tool {
                        content: decode_anthropic_text_content(
                            block
                                .get("content")
                                .unwrap_or(&Value::String(String::new())),
                        )?,
                        tool_call_id: block
                            .get("tool_use_id")
                            .and_then(Value::as_str)
                            .ok_or_else(|| "tool_result requires tool_use_id".to_string())?
                            .into(),
                    });
                } else {
                    parts.push(decode_anthropic_part(block)?);
                }
            }
            if !parts.is_empty() {
                messages.push(ChatMessageV1::User {
                    content: MessageContent::Parts(parts),
                    name: None,
                });
            }
        }
        other => return Err(format!("unsupported Anthropic message role: {other}")),
    }
    Ok(())
}

fn decode_anthropic_text_content(value: &Value) -> Result<MessageContent, String> {
    if let Some(text) = value.as_str() {
        return Ok(MessageContent::Text(text.into()));
    }
    let blocks = value
        .as_array()
        .ok_or_else(|| "Anthropic text content must be a string or array".to_string())?;
    let mut text = String::new();
    for block in blocks {
        let piece = block
            .get("text")
            .and_then(Value::as_str)
            .or_else(|| block.as_str())
            .ok_or_else(|| "expected Anthropic text block".to_string())?;
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(piece);
    }
    Ok(MessageContent::Text(text))
}

fn decode_anthropic_part(block: &Value) -> Result<ContentPart, String> {
    match block.get("type").and_then(Value::as_str) {
        Some("text") => Ok(ContentPart::Text {
            text: block
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or("")
                .into(),
        }),
        Some("image") => {
            let source = block.get("source").unwrap_or(&Value::Null);
            let image_url = match source.get("type").and_then(Value::as_str) {
                Some("url") => source
                    .get("url")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .into(),
                Some("base64") => format!(
                    "data:{};base64,{}",
                    source
                        .get("media_type")
                        .and_then(Value::as_str)
                        .unwrap_or("application/octet-stream"),
                    source.get("data").and_then(Value::as_str).unwrap_or("")
                ),
                _ => return Err("unsupported Anthropic image source".into()),
            };
            Ok(ContentPart::ImageUrl {
                image_url,
                detail: None,
            })
        }
        Some(other) => Err(format!("unsupported Anthropic user block: {other}")),
        None => Err("Anthropic content block requires type".into()),
    }
}

fn decode_anthropic_tool_choice(value: &Value) -> Result<ToolChoiceV1, String> {
    match value.get("type").and_then(Value::as_str) {
        Some("auto") => Ok(ToolChoiceV1::Mode(ToolChoiceMode::Auto)),
        Some("any") => Ok(ToolChoiceV1::Mode(ToolChoiceMode::Required)),
        Some("none") => Ok(ToolChoiceV1::Mode(ToolChoiceMode::None)),
        Some("tool") => Ok(ToolChoiceV1::Function {
            name: value
                .get("name")
                .and_then(Value::as_str)
                .ok_or_else(|| "Anthropic tool choice requires name".to_string())?
                .into(),
        }),
        Some(other) => Err(format!("unsupported Anthropic tool choice: {other}")),
        None => Err("Anthropic tool_choice requires type".into()),
    }
}

pub(crate) fn encode_response(dialect: IngressDialect, response: &ChatResponseV1) -> Value {
    match dialect {
        IngressDialect::OpenAi => encode_openai_response(response),
        IngressDialect::Anthropic => encode_anthropic_response(response),
    }
}

fn encode_openai_response(response: &ChatResponseV1) -> Value {
    let mut message = Map::from_iter([("role".into(), Value::String("assistant".into()))]);
    message.insert(
        "content".into(),
        response
            .output
            .content
            .as_ref()
            .map(content_text)
            .map(Value::String)
            .unwrap_or(Value::Null),
    );
    if !response.output.tool_calls.is_empty() {
        message.insert(
            "tool_calls".into(),
            Value::Array(
                response
                    .output
                    .tool_calls
                    .iter()
                    .map(|call| {
                        json!({"id":call.id,"type":"function","function":{
                            "name":call.name,"arguments":call.arguments
                        }})
                    })
                    .collect(),
            ),
        );
    }
    if let Some(refusal) = &response.output.refusal {
        message.insert("refusal".into(), Value::String(refusal.clone()));
    }
    json!({
        "id": response.id,
        "object":"chat.completion",
        "model":response.model,
        "choices":[{"index":0,"message":message,"finish_reason":openai_finish(response.finish_reason)}],
        "usage":openai_usage(&response.usage),
    })
}

fn encode_anthropic_response(response: &ChatResponseV1) -> Value {
    let mut content = Vec::new();
    if let Some(text) = response.output.content.as_ref().map(content_text) {
        if !text.is_empty() {
            content.push(json!({"type":"text","text":text}));
        }
    }
    for call in &response.output.tool_calls {
        let input = serde_json::from_str::<Value>(&call.arguments).unwrap_or(Value::Null);
        content.push(json!({"type":"tool_use","id":call.id,"name":call.name,"input":input}));
    }
    json!({
        "id":response.id,
        "type":"message",
        "role":"assistant",
        "model":response.model,
        "content":content,
        "stop_reason":anthropic_finish(response.finish_reason),
        "stop_sequence":Value::Null,
        "usage":anthropic_usage(&response.usage),
    })
}

/// One canonical stream event may expand to multiple protocol SSE events (for example a finish
/// event also closes an Anthropic message).
pub(crate) fn encode_stream_event(
    dialect: IngressDialect,
    event: &ChatStreamEventV1,
) -> Vec<(Option<&'static str>, Value)> {
    match dialect {
        IngressDialect::OpenAi => encode_openai_stream_event(event),
        IngressDialect::Anthropic => encode_anthropic_stream_event(event),
    }
}

fn encode_openai_stream_event(event: &ChatStreamEventV1) -> Vec<(Option<&'static str>, Value)> {
    let value = match event {
        ChatStreamEventV1::ResponseStart { id, model } => json!({
            "id":id,"object":"chat.completion.chunk","model":model,
            "choices":[{"index":0,"delta":{"role":"assistant"},"finish_reason":Value::Null}]
        }),
        ChatStreamEventV1::TextDelta { delta } => openai_delta(json!({"content":delta})),
        ChatStreamEventV1::ReasoningDelta { delta } => {
            openai_delta(json!({"reasoning_content":delta}))
        }
        ChatStreamEventV1::RefusalDelta { delta } => openai_delta(json!({"refusal":delta})),
        ChatStreamEventV1::ToolCallStart { index, id, name } => openai_delta(json!({
            "tool_calls":[{"index":index,"id":id,"type":"function","function":{"name":name,"arguments":""}}]
        })),
        ChatStreamEventV1::ToolCallArgumentsDelta { index, delta } => openai_delta(json!({
            "tool_calls":[{"index":index,"function":{"arguments":delta}}]
        })),
        ChatStreamEventV1::ToolCallEnd { .. } => return Vec::new(),
        ChatStreamEventV1::Usage { usage } => json!({"choices":[],"usage":openai_usage(usage)}),
        ChatStreamEventV1::Finish { reason } => json!({
            "choices":[{"index":0,"delta":{},"finish_reason":openai_finish(Some(*reason))}]
        }),
        ChatStreamEventV1::Error { error } => json!({"error":error}),
    };
    vec![(None, value)]
}

fn openai_delta(delta: Value) -> Value {
    json!({"object":"chat.completion.chunk","choices":[{"index":0,"delta":delta,"finish_reason":Value::Null}]})
}

fn encode_anthropic_stream_event(event: &ChatStreamEventV1) -> Vec<(Option<&'static str>, Value)> {
    match event {
        ChatStreamEventV1::ResponseStart { id, model } => vec![(
            Some("message_start"),
            json!({"type":"message_start","message":{
                "id":id,"type":"message","role":"assistant","model":model,"content":[],
                "stop_reason":Value::Null,"stop_sequence":Value::Null,
                "usage":{"input_tokens":0,"output_tokens":0}
            }}),
        )],
        ChatStreamEventV1::TextDelta { delta } => vec![(
            Some("content_block_delta"),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":delta}}),
        )],
        ChatStreamEventV1::ReasoningDelta { delta } => vec![(
            Some("content_block_delta"),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":delta}}),
        )],
        ChatStreamEventV1::RefusalDelta { delta } => vec![(
            Some("content_block_delta"),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":delta}}),
        )],
        ChatStreamEventV1::ToolCallStart { index, id, name } => vec![(
            Some("content_block_start"),
            json!({"type":"content_block_start","index":index,"content_block":{
                "type":"tool_use","id":id,"name":name,"input":{}
            }}),
        )],
        ChatStreamEventV1::ToolCallArgumentsDelta { index, delta } => vec![(
            Some("content_block_delta"),
            json!({"type":"content_block_delta","index":index,"delta":{
                "type":"input_json_delta","partial_json":delta
            }}),
        )],
        ChatStreamEventV1::ToolCallEnd { index } => vec![(
            Some("content_block_stop"),
            json!({"type":"content_block_stop","index":index}),
        )],
        ChatStreamEventV1::Usage { usage } => vec![(
            Some("message_delta"),
            json!({"type":"message_delta","delta":{},"usage":anthropic_usage(usage)}),
        )],
        ChatStreamEventV1::Finish { reason } => vec![
            (
                Some("message_delta"),
                json!({"type":"message_delta","delta":{
                    "stop_reason":anthropic_finish(Some(*reason)),"stop_sequence":Value::Null
                }}),
            ),
            (Some("message_stop"), json!({"type":"message_stop"})),
        ],
        ChatStreamEventV1::Error { error } => {
            vec![(Some("error"), json!({"type":"error","error":error}))]
        }
    }
}

fn openai_usage(usage: &UsageV2) -> Value {
    let prompt_tokens = usage.tokens_in.saturating_add(usage.cache_read_tokens);
    json!({
        "prompt_tokens":prompt_tokens,
        "completion_tokens":usage.tokens_out,
        "total_tokens":prompt_tokens.saturating_add(usage.tokens_out),
        "prompt_tokens_details":{"cached_tokens":usage.cache_read_tokens},
        "completion_tokens_details":{
            "reasoning_tokens":usage.reasoning_tokens,
            "audio_tokens":usage.audio_output_tokens,
            "accepted_prediction_tokens":usage.accepted_prediction_tokens,
            "rejected_prediction_tokens":usage.rejected_prediction_tokens,
        }
    })
}

fn anthropic_usage(usage: &UsageV2) -> Value {
    json!({
        "input_tokens":usage.tokens_in,
        "output_tokens":usage.tokens_out,
        "cache_creation_input_tokens":usage.cache_creation_tokens,
        "cache_read_input_tokens":usage.cache_read_tokens,
    })
}

fn openai_finish(reason: Option<FinishReasonV1>) -> Value {
    reason
        .map(|reason| match reason {
            FinishReasonV1::Stop => "stop",
            FinishReasonV1::Length => "length",
            FinishReasonV1::ToolCalls => "tool_calls",
            FinishReasonV1::ContentFilter => "content_filter",
            FinishReasonV1::FunctionCall => "function_call",
            FinishReasonV1::Unknown => "unknown",
        })
        .map(|value| Value::String(value.into()))
        .unwrap_or(Value::Null)
}

fn anthropic_finish(reason: Option<FinishReasonV1>) -> Value {
    reason
        .map(|reason| match reason {
            FinishReasonV1::Stop => "end_turn",
            FinishReasonV1::Length => "max_tokens",
            FinishReasonV1::ToolCalls | FinishReasonV1::FunctionCall => "tool_use",
            FinishReasonV1::ContentFilter | FinishReasonV1::Unknown => "end_turn",
        })
        .map(|value| Value::String(value.into()))
        .unwrap_or(Value::Null)
}

fn content_text(content: &MessageContent) -> String {
    match content {
        MessageContent::Text(text) => text.clone(),
        MessageContent::Parts(parts) => parts
            .iter()
            .filter_map(|part| match part {
                ContentPart::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join(""),
    }
}

fn content_from_parts(parts: Vec<ContentPart>) -> Option<MessageContent> {
    match parts.as_slice() {
        [] => None,
        [ContentPart::Text { text }] => Some(MessageContent::Text(text.clone())),
        _ => Some(MessageContent::Parts(parts)),
    }
}

fn decode_stop(value: Option<&Value>) -> Result<Option<Vec<String>>, String> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(stop)) => Ok(Some(vec![stop.clone()])),
        Some(Value::Array(stops)) => stops
            .iter()
            .map(|stop| {
                stop.as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| "stop entries must be strings".to_string())
            })
            .collect::<Result<Vec<_>, _>>()
            .map(Some),
        Some(_) => Err("stop must be a string or array".into()),
    }
}

fn required<'a>(object: &'a Map<String, Value>, key: &str) -> Result<&'a Value, String> {
    object
        .get(key)
        .ok_or_else(|| format!("missing required field: {key}"))
}

fn required_string(object: &Map<String, Value>, key: &str) -> Result<String, String> {
    required(object, key)?
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| format!("{key} must be a string"))
}

fn optional_string(object: &Map<String, Value>, key: &str) -> Result<Option<String>, String> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value.clone())),
        Some(_) => Err(format!("{key} must be a string")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sandhi_core::AssistantOutputV1;

    #[test]
    fn openai_ingress_covers_all_roles_and_tool_linkage() {
        let (request, stream) = decode_request(
            IngressDialect::OpenAi,
            json!({
                "model":"m","stream":true,
                "messages":[
                    {"role":"developer","content":"d"},
                    {"role":"system","content":"s"},
                    {"role":"user","content":[{"type":"text","text":"u"}]},
                    {"role":"assistant","content":null,"tool_calls":[{"id":"c1","type":"function","function":{"name":"f","arguments":"{}"}}]},
                    {"role":"tool","tool_call_id":"c1","content":"ok"},
                    {"role":"function","name":"f","content":"legacy"}
                ]
            }),
            RequestMetadataV1::default(),
        )
        .unwrap();
        assert!(stream);
        assert_eq!(request.messages.len(), 6);
        request.validate().unwrap();
    }

    #[test]
    fn anthropic_ingress_maps_tool_use_and_result() {
        let (request, _) = decode_request(
            IngressDialect::Anthropic,
            json!({
                "model":"claude","max_tokens":10,"system":"safe",
                "messages":[
                    {"role":"assistant","content":[{"type":"tool_use","id":"c1","name":"f","input":{"x":1}}]},
                    {"role":"user","content":[{"type":"tool_result","tool_use_id":"c1","content":"ok"}]}
                ]
            }),
            RequestMetadataV1::default(),
        )
        .unwrap();
        assert!(matches!(
            request.messages[1],
            ChatMessageV1::Assistant { .. }
        ));
        assert!(matches!(request.messages[2], ChatMessageV1::Tool { .. }));
    }

    #[test]
    fn both_egress_dialects_preserve_cache_split() {
        let response = ChatResponseV1 {
            schema_version: CHAT_SCHEMA_VERSION_V1.into(),
            id: Some("r".into()),
            model: "m".into(),
            output: AssistantOutputV1 {
                content: Some(MessageContent::Text("ok".into())),
                tool_calls: Vec::new(),
                refusal: None,
            },
            finish_reason: Some(FinishReasonV1::Stop),
            usage: UsageV2 {
                tokens_in: 6,
                tokens_out: 5,
                cache_read_tokens: 4,
                ..UsageV2::default()
            },
            extensions: BTreeMap::new(),
        };
        assert_eq!(
            encode_response(IngressDialect::OpenAi, &response)["usage"]["prompt_tokens"],
            10
        );
        assert_eq!(
            encode_response(IngressDialect::Anthropic, &response)["usage"]
                ["cache_read_input_tokens"],
            4
        );
    }
}
