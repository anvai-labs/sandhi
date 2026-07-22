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
    /// OpenAI Responses item/event protocol (`/v1/responses`). Normalized through the same
    /// `ChatRequestV1` as the other dialects; the field mapping mirrors the typed Responses
    /// codec in `sandhi-providers::openai_responses_typed`.
    Responses,
}

pub(crate) fn decode_request(
    dialect: IngressDialect,
    body: Value,
    metadata: RequestMetadataV1,
) -> Result<(ChatRequestV1, bool), String> {
    match dialect {
        IngressDialect::OpenAi => decode_openai_request(body, metadata),
        IngressDialect::Anthropic => decode_anthropic_request(body, metadata),
        IngressDialect::Responses => decode_responses_request(body, metadata),
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

/// Decode an OpenAI Responses-API ingress body into the canonical `ChatRequestV1`.
///
/// This is the inverse of `openai_responses_typed::encode_responses_request`: the caller's
/// `instructions` + typed `input` items (`message` / `function_call` / `function_call_output`)
/// become the canonical message list, and `tools`/`tool_choice`/sampling fields map across. The
/// raw body is retained under `extensions["openai_responses"]` so a Responses-backend upstream can
/// re-read Responses-only fields (e.g. `reasoning.effort`) that have no canonical home.
fn decode_responses_request(
    body: Value,
    metadata: RequestMetadataV1,
) -> Result<(ChatRequestV1, bool), String> {
    let object = body
        .as_object()
        .ok_or_else(|| "request body must be a JSON object".to_string())?;
    let mut messages = Vec::new();
    if let Some(instructions) = optional_string(object, "instructions")? {
        messages.push(ChatMessageV1::System {
            content: MessageContent::Text(instructions),
            name: None,
        });
    }
    match object.get("input") {
        Some(Value::String(text)) => messages.push(ChatMessageV1::User {
            content: MessageContent::Text(text.clone()),
            name: None,
        }),
        Some(Value::Array(items)) => {
            for item in items {
                decode_responses_input_item(item, &mut messages)?;
            }
        }
        Some(_) => return Err("Responses input must be a string or array".into()),
        None => {}
    }
    let tools = object
        .get("tools")
        .and_then(Value::as_array)
        .map(|values| {
            values
                .iter()
                .map(|value| {
                    let name = value
                        .get("name")
                        .and_then(Value::as_str)
                        .or_else(|| value.pointer("/function/name").and_then(Value::as_str))
                        .ok_or_else(|| "Responses tool requires name".to_string())?;
                    Ok(ToolDefinitionV1 {
                        name: name.into(),
                        description: value
                            .get("description")
                            .and_then(Value::as_str)
                            .map(str::to_owned),
                        parameters: value
                            .get("parameters")
                            .or_else(|| value.get("function").and_then(|f| f.get("parameters")))
                            .cloned()
                            .unwrap_or_else(|| json!({"type":"object"})),
                        strict: value.get("strict").and_then(Value::as_bool),
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
            .map(decode_responses_tool_choice)
            .transpose()?,
        temperature: object.get("temperature").and_then(Value::as_f64),
        max_output_tokens: object.get("max_output_tokens").and_then(Value::as_u64),
        stop: None,
        response_format: object
            .get("text")
            .and_then(|text| text.get("format"))
            .map(responses_response_format)
            .transpose()?,
        seed: None,
        metadata,
        extensions: BTreeMap::from([("openai_responses".into(), body.clone())]),
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

fn decode_responses_input_item(
    item: &Value,
    messages: &mut Vec<ChatMessageV1>,
) -> Result<(), String> {
    let kind = item.get("type").and_then(Value::as_str).unwrap_or("");
    match kind {
        "message" => messages.push(decode_responses_message_item(item)?),
        "function_call" => {
            let id = item
                .get("call_id")
                .and_then(Value::as_str)
                .or_else(|| item.get("id").and_then(Value::as_str))
                .ok_or_else(|| "Responses function_call requires call_id".to_string())?;
            let name = required_string(
                item.as_object()
                    .ok_or_else(|| "Responses item must be an object".to_string())?,
                "name",
            )?;
            messages.push(ChatMessageV1::Assistant {
                content: None,
                name: None,
                tool_calls: vec![ToolCallV1 {
                    id: id.into(),
                    name,
                    arguments: item
                        .get("arguments")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .into(),
                    extensions: BTreeMap::new(),
                }],
                refusal: None,
            });
        }
        "function_call_output" => {
            let call_id = item
                .get("call_id")
                .and_then(Value::as_str)
                .ok_or_else(|| "Responses function_call_output requires call_id".to_string())?;
            let output = item.get("output").cloned().unwrap_or(Value::Null);
            let content = match &output {
                Value::String(text) => MessageContent::Text(text.clone()),
                other => MessageContent::Text(other.to_string()),
            };
            messages.push(ChatMessageV1::Tool {
                content,
                tool_call_id: call_id.into(),
            });
        }
        // `reasoning` items carry encrypted/summary-only reasoning state that has no canonical
        // representation; unknown item types are forward-compatible noise. Both are skipped.
        _ => {}
    }
    Ok(())
}

fn decode_responses_message_item(item: &Value) -> Result<ChatMessageV1, String> {
    let object = item
        .as_object()
        .ok_or_else(|| "Responses message item must be an object".to_string())?;
    let role = required_string(object, "role")?;
    let raw_content = object.get("content").unwrap_or(&Value::Null);
    let parts = if let Some(text) = raw_content.as_str() {
        vec![json!({"type":"input_text","text":text})]
    } else {
        raw_content
            .as_array()
            .cloned()
            .ok_or_else(|| "Responses message content must be a string or array".to_string())?
    };
    let mut text = String::new();
    let mut refusal = String::new();
    for part in &parts {
        match part.get("type").and_then(Value::as_str) {
            Some("input_text") | Some("output_text") | Some("text") => {
                text.push_str(part.get("text").and_then(Value::as_str).unwrap_or(""))
            }
            Some("refusal") => {
                refusal.push_str(part.get("refusal").and_then(Value::as_str).unwrap_or(""))
            }
            _ => {}
        }
    }
    match role.as_str() {
        "developer" => Ok(ChatMessageV1::Developer {
            content: MessageContent::Text(text),
            name: None,
        }),
        "system" => Ok(ChatMessageV1::System {
            content: MessageContent::Text(text),
            name: None,
        }),
        "user" => Ok(ChatMessageV1::User {
            content: MessageContent::Text(text),
            name: None,
        }),
        "assistant" => Ok(ChatMessageV1::Assistant {
            content: (!text.is_empty()).then_some(MessageContent::Text(text)),
            name: None,
            tool_calls: Vec::new(),
            refusal: (!refusal.is_empty()).then_some(refusal),
        }),
        other => Err(format!("unsupported Responses message role: {other}")),
    }
}

fn decode_responses_tool_choice(value: &Value) -> Result<ToolChoiceV1, String> {
    if let Some(mode) = value.as_str() {
        return Ok(ToolChoiceV1::Mode(match mode {
            "none" => ToolChoiceMode::None,
            "auto" => ToolChoiceMode::Auto,
            "required" => ToolChoiceMode::Required,
            other => return Err(format!("unsupported Responses tool_choice mode: {other}")),
        }));
    }
    value
        .get("name")
        .and_then(Value::as_str)
        .map(|name| ToolChoiceV1::Function { name: name.into() })
        .ok_or_else(|| "Responses tool_choice must be a mode or {type:function,name}".into())
}

/// Inverse of `responses_text_format`: recover the Chat Completions `response_format` from the
/// Responses `text.format` object.
fn responses_response_format(format: &Value) -> Result<Value, String> {
    if format.get("type").and_then(Value::as_str) == Some("json_schema") {
        Ok(json!({
            "type":"json_schema",
            "json_schema":{
                "name":format.get("name").cloned().unwrap_or(Value::Null),
                "schema":format.get("schema").cloned().unwrap_or(Value::Null),
                "strict":format.get("strict").cloned().unwrap_or(Value::Bool(false))
            }
        }))
    } else {
        Ok(format.clone())
    }
}

pub(crate) fn encode_response(dialect: IngressDialect, response: &ChatResponseV1) -> Value {
    match dialect {
        IngressDialect::OpenAi => encode_openai_response(response),
        IngressDialect::Anthropic => encode_anthropic_response(response),
        IngressDialect::Responses => encode_responses_response(response),
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

/// Encode the canonical response as an OpenAI Responses document — the inverse of
/// `openai_responses_typed::decode_responses_response`. The neutral assistant output becomes
/// `output` items (a `message` with `output_text` content and/or `function_call` items), with
/// `status` derived from the finish reason and a Responses-shaped `usage` block.
fn encode_responses_response(response: &ChatResponseV1) -> Value {
    let mut output = Vec::new();
    let text = response.output.content.as_ref().map(content_text);
    let has_text = text.as_deref().is_some_and(|text| !text.is_empty());
    let has_refusal = response
        .output
        .refusal
        .as_deref()
        .is_some_and(|refusal| !refusal.is_empty());
    if has_text || has_refusal {
        let mut content = Vec::new();
        if let Some(text) = text {
            if !text.is_empty() {
                content.push(json!({"type":"output_text","text":text}));
            }
        }
        if let Some(refusal) = &response.output.refusal {
            if !refusal.is_empty() {
                content.push(json!({"type":"refusal","refusal":refusal}));
            }
        }
        output.push(json!({"type":"message","role":"assistant","content":content}));
    }
    for call in &response.output.tool_calls {
        output.push(json!({
            "type":"function_call","call_id":call.id,"name":call.name,"arguments":call.arguments
        }));
    }
    let status = responses_status(response.finish_reason);
    let mut body = json!({
        "id":response.id,
        "object":"response",
        "model":response.model,
        "status":status,
        "output":output,
        "usage":responses_usage(&response.usage),
    });
    if matches!(response.finish_reason, Some(FinishReasonV1::Length)) {
        body["incomplete_details"] = json!({"reason":"max_output_tokens"});
    } else if matches!(response.finish_reason, Some(FinishReasonV1::ContentFilter)) {
        body["incomplete_details"] = json!({"reason":"content_filter"});
    }
    body
}

fn responses_status(reason: Option<FinishReasonV1>) -> &'static str {
    match reason {
        Some(FinishReasonV1::Length) | Some(FinishReasonV1::ContentFilter) => "incomplete",
        // Stop, ToolCalls, FunctionCall, Unknown, and None all surface as a completed run.
        _ => "completed",
    }
}

fn responses_usage(usage: &UsageV2) -> Value {
    json!({
        "input_tokens":usage.tokens_in,
        "output_tokens":usage.tokens_out,
        "input_tokens_details":{"cached_tokens":usage.cache_read_tokens},
        "output_tokens_details":{
            "reasoning_tokens":usage.reasoning_tokens.unwrap_or(0)
        }
    })
}

/// One canonical stream event may expand to multiple protocol SSE events (for example a finish
/// event also closes an Anthropic message).
///
/// `last_usage` carries the most recently observed terminal `Usage` event so the Responses
/// egress can fold it into the terminal `response.completed` frame (in the canonical stream a
/// `Usage` event always precedes `Finish`). The OpenAI/Anthropic encoders ignore it.
pub(crate) fn encode_stream_event(
    dialect: IngressDialect,
    event: &ChatStreamEventV1,
    last_usage: Option<&UsageV2>,
) -> Vec<(Option<&'static str>, Value)> {
    match dialect {
        IngressDialect::OpenAi => encode_openai_stream_event(event, last_usage),
        IngressDialect::Anthropic => encode_anthropic_stream_event(event, last_usage),
        IngressDialect::Responses => encode_responses_stream_event(event, last_usage),
    }
}

fn encode_openai_stream_event(
    event: &ChatStreamEventV1,
    _last_usage: Option<&UsageV2>,
) -> Vec<(Option<&'static str>, Value)> {
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

fn encode_anthropic_stream_event(
    event: &ChatStreamEventV1,
    _last_usage: Option<&UsageV2>,
) -> Vec<(Option<&'static str>, Value)> {
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

/// Encode the canonical stream as OpenAI Responses SSE events — the inverse of
/// `openai_responses_typed::decode_responses_stream`. Each event mirrors a Responses event kind
/// the typed decoder consumes; the terminal `response.completed` folds in the last-seen usage.
fn encode_responses_stream_event(
    event: &ChatStreamEventV1,
    last_usage: Option<&UsageV2>,
) -> Vec<(Option<&'static str>, Value)> {
    match event {
        ChatStreamEventV1::ResponseStart { id, model } => vec![(
            Some("response.created"),
            json!({
                "type":"response.created",
                "response":{
                    "id":id,"object":"response","status":"in_progress","model":model,"output":[]
                }
            }),
        )],
        ChatStreamEventV1::TextDelta { delta } => vec![(
            Some("response.output_text.delta"),
            json!({"type":"response.output_text.delta","output_index":0,"content_index":0,"delta":delta}),
        )],
        ChatStreamEventV1::ReasoningDelta { delta } => vec![(
            Some("response.reasoning_summary_text.delta"),
            json!({"type":"response.reasoning_summary_text.delta","delta":delta}),
        )],
        ChatStreamEventV1::RefusalDelta { delta } => vec![(
            Some("response.refusal.delta"),
            json!({"type":"response.refusal.delta","delta":delta}),
        )],
        ChatStreamEventV1::ToolCallStart { index, id, name } => vec![(
            Some("response.output_item.added"),
            json!({
                "type":"response.output_item.added","output_index":index,
                "item":{
                    "type":"function_call","id":id,"call_id":id,"name":name,
                    "arguments":"","status":"in_progress"
                }
            }),
        )],
        ChatStreamEventV1::ToolCallArgumentsDelta { index, delta } => vec![(
            Some("response.function_call_arguments.delta"),
            json!({"type":"response.function_call_arguments.delta","output_index":index,"delta":delta}),
        )],
        ChatStreamEventV1::ToolCallEnd { index } => vec![(
            Some("response.output_item.done"),
            json!({"type":"response.output_item.done","output_index":index,
                   "item":{"type":"function_call","status":"completed"}}),
        )],
        // Usage folds into the terminal `response.completed` emitted on Finish; nothing to stream.
        ChatStreamEventV1::Usage { .. } => Vec::new(),
        ChatStreamEventV1::Finish { reason } => {
            let usage = last_usage
                .map(responses_usage)
                .unwrap_or_else(|| responses_usage(&UsageV2::default()));
            vec![(
                Some("response.completed"),
                json!({
                    "type":"response.completed",
                    "response":{
                        "object":"response","status":responses_status(Some(*reason)),
                        "output":[],"usage":usage
                    }
                }),
            )]
        }
        ChatStreamEventV1::Error { error } => vec![(
            Some("error"),
            json!({"type":"error","error":{"code":error.code,"message":error.message}}),
        )],
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

    #[test]
    fn responses_ingress_maps_instructions_items_and_tools() {
        let (request, stream) = decode_request(
            IngressDialect::Responses,
            json!({
                "model":"gpt-test","stream":true,
                "instructions":"be precise",
                "input":[
                    {"type":"message","role":"user","content":[{"type":"input_text","text":"weather?"}]},
                    {"type":"function_call","call_id":"call_1","name":"weather","arguments":"{\"city\":\"Austin\"}"},
                    {"type":"function_call_output","call_id":"call_1","output":"sunny"}
                ],
                "tools":[{"type":"function","name":"weather","parameters":{"type":"object"}}],
                "tool_choice":{"type":"function","name":"weather"},
                "temperature":0.5,
                "max_output_tokens":100
            }),
            RequestMetadataV1::default(),
        )
        .unwrap();
        assert!(stream);
        // instructions → System, user message, assistant tool call, tool output.
        assert_eq!(request.messages.len(), 4);
        assert!(matches!(request.messages[0], ChatMessageV1::System { .. }));
        assert!(matches!(request.messages[1], ChatMessageV1::User { .. }));
        assert!(matches!(
            request.messages[2],
            ChatMessageV1::Assistant { ref tool_calls, .. } if tool_calls.len() == 1
                && tool_calls[0].id == "call_1"
                && tool_calls[0].name == "weather"
        ));
        assert!(matches!(
            request.messages[3],
            ChatMessageV1::Tool { ref tool_call_id, .. } if tool_call_id == "call_1"
        ));
        assert_eq!(request.tools.len(), 1);
        assert_eq!(request.tools[0].name, "weather");
        assert!(matches!(
            request.tool_choice,
            Some(ToolChoiceV1::Function { ref name }) if name == "weather"
        ));
        assert_eq!(request.temperature, Some(0.5));
        assert_eq!(request.max_output_tokens, Some(100));
        // The raw body is retained for a Responses-backend upstream.
        assert!(request.extensions.contains_key("openai_responses"));
        request.validate().unwrap();
    }

    #[test]
    fn responses_ingress_rejects_missing_model() {
        let err = decode_request(
            IngressDialect::Responses,
            json!({"input":"hi"}),
            RequestMetadataV1::default(),
        )
        .unwrap_err();
        assert!(err.contains("model"));
    }

    #[test]
    fn responses_egress_shapes_message_function_call_and_usage() {
        let response = ChatResponseV1 {
            schema_version: CHAT_SCHEMA_VERSION_V1.into(),
            id: Some("resp_1".into()),
            model: "gpt-test".into(),
            output: AssistantOutputV1 {
                content: Some(MessageContent::Text("hi".into())),
                tool_calls: vec![ToolCallV1 {
                    id: "call_1".into(),
                    name: "weather".into(),
                    arguments: "{\"city\":\"Austin\"}".into(),
                    extensions: BTreeMap::new(),
                }],
                refusal: None,
            },
            finish_reason: Some(FinishReasonV1::ToolCalls),
            usage: UsageV2 {
                tokens_in: 40,
                tokens_out: 20,
                cache_read_tokens: 60,
                reasoning_tokens: Some(7),
                ..UsageV2::default()
            },
            extensions: BTreeMap::new(),
        };
        let body = encode_response(IngressDialect::Responses, &response);
        assert_eq!(body["object"], "response");
        assert_eq!(body["id"], "resp_1");
        assert_eq!(body["status"], "completed");
        // First output item is the assistant message with output_text; second is the function_call.
        assert_eq!(body["output"][0]["type"], "message");
        assert_eq!(body["output"][0]["content"][0]["type"], "output_text");
        assert_eq!(body["output"][0]["content"][0]["text"], "hi");
        assert_eq!(body["output"][1]["type"], "function_call");
        assert_eq!(body["output"][1]["call_id"], "call_1");
        assert_eq!(body["output"][1]["name"], "weather");
        assert_eq!(body["usage"]["input_tokens"], 40);
        assert_eq!(body["usage"]["output_tokens"], 20);
        assert_eq!(body["usage"]["input_tokens_details"]["cached_tokens"], 60);
        assert_eq!(
            body["usage"]["output_tokens_details"]["reasoning_tokens"],
            7
        );
    }

    #[test]
    fn responses_egress_marks_length_as_incomplete() {
        let response = ChatResponseV1 {
            schema_version: CHAT_SCHEMA_VERSION_V1.into(),
            id: None,
            model: "m".into(),
            output: AssistantOutputV1 {
                content: Some(MessageContent::Text("...".into())),
                tool_calls: Vec::new(),
                refusal: None,
            },
            finish_reason: Some(FinishReasonV1::Length),
            usage: UsageV2::default(),
            extensions: BTreeMap::new(),
        };
        let body = encode_response(IngressDialect::Responses, &response);
        assert_eq!(body["status"], "incomplete");
        assert_eq!(body["incomplete_details"]["reason"], "max_output_tokens");
    }

    #[test]
    fn responses_stream_events_fold_terminal_usage_into_completed() {
        // Start → created; deltas → named delta events; usage folds into the completed frame.
        let start = encode_responses_stream_event(
            &ChatStreamEventV1::ResponseStart {
                id: Some("resp_1".into()),
                model: "gpt-test".into(),
            },
            None,
        );
        assert_eq!(start[0].0, Some("response.created"));
        assert_eq!(start[0].1["response"]["model"], "gpt-test");

        let delta = encode_responses_stream_event(
            &ChatStreamEventV1::TextDelta { delta: "hi".into() },
            None,
        );
        assert_eq!(delta[0].0, Some("response.output_text.delta"));
        assert_eq!(delta[0].1["delta"], "hi");

        let usage = UsageV2 {
            tokens_in: 5,
            tokens_out: 2,
            ..UsageV2::default()
        };
        // Usage itself emits nothing; the terminal completed carries it.
        assert!(encode_responses_stream_event(
            &ChatStreamEventV1::Usage {
                usage: usage.clone()
            },
            None,
        )
        .is_empty());

        let finish = encode_responses_stream_event(
            &ChatStreamEventV1::Finish {
                reason: FinishReasonV1::Stop,
            },
            Some(&usage),
        );
        assert_eq!(finish[0].0, Some("response.completed"));
        assert_eq!(finish[0].1["response"]["status"], "completed");
        assert_eq!(finish[0].1["response"]["usage"]["input_tokens"], 5);
        assert_eq!(finish[0].1["response"]["usage"]["output_tokens"], 2);
    }

    #[test]
    fn responses_stream_events_shape_tool_call_lifecycle() {
        let start = encode_responses_stream_event(
            &ChatStreamEventV1::ToolCallStart {
                index: 0,
                id: "call_1".into(),
                name: "weather".into(),
            },
            None,
        );
        assert_eq!(start[0].0, Some("response.output_item.added"));
        assert_eq!(start[0].1["item"]["type"], "function_call");
        assert_eq!(start[0].1["item"]["call_id"], "call_1");

        let args = encode_responses_stream_event(
            &ChatStreamEventV1::ToolCallArgumentsDelta {
                index: 0,
                delta: "{\"x\":".into(),
            },
            None,
        );
        assert_eq!(args[0].0, Some("response.function_call_arguments.delta"));

        let end = encode_responses_stream_event(&ChatStreamEventV1::ToolCallEnd { index: 0 }, None);
        assert_eq!(end[0].0, Some("response.output_item.done"));
    }
}
