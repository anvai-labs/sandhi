//! Portable buffered output only. Native consumers keep the original envelope.
//! This stage has no dispatch or metering authority; source counts precede it.
use crate::ProviderError;
use sandhi_core::EndpointFamilyV1 as Family;
use serde_json::Value;
use std::collections::BTreeSet;

const MAX_CALLS: usize = 1024;
fn invalid() -> ProviderError {
    ProviderError::OutputProjection(
        "upstream output requires compatible native delivery or has invalid portable content"
            .into(),
    )
}
fn empty(v: &Value) -> bool {
    v.is_null()
        || v.as_str().is_some_and(str::is_empty)
        || v.as_array().is_some_and(Vec::is_empty)
        || v.as_object().is_some_and(serde_json::Map::is_empty)
}
pub(crate) fn fields(value: &Value, allowed: &[&str]) -> Result<(), ProviderError> {
    let object = value.as_object().ok_or_else(invalid)?;
    if object
        .iter()
        .any(|(key, value)| !allowed.contains(&key.as_str()) && !empty(value))
    {
        return Err(invalid());
    }
    Ok(())
}
pub(crate) fn text<'a>(value: &'a Value, key: &str) -> Result<&'a str, ProviderError> {
    value.get(key).and_then(Value::as_str).ok_or_else(invalid)
}
pub(crate) fn id<'a>(value: &'a Value, key: &str) -> Result<&'a str, ProviderError> {
    text(value, key).and_then(|v| {
        if v.is_empty() || v.len() > 256 || v.chars().any(char::is_control) {
            Err(invalid())
        } else {
            Ok(v)
        }
    })
}
pub(crate) fn array<'a>(value: &'a Value, key: &str) -> Result<&'a Vec<Value>, ProviderError> {
    value
        .get(key)
        .and_then(Value::as_array)
        .filter(|v| v.len() <= MAX_CALLS)
        .ok_or_else(invalid)
}
fn object_args(value: &Value) -> Result<(), ProviderError> {
    if value.is_object() {
        Ok(())
    } else {
        Err(invalid())
    }
}
pub(crate) fn string_args(value: &Value) -> Result<(), ProviderError> {
    let value = value.as_str().ok_or_else(invalid)?;
    let parsed = sandhi_core::policy::strict_json(value.as_bytes()).map_err(|_| invalid())?;
    object_args(&parsed)
}
pub(crate) fn optional_text(value: &Value, key: &str) -> Result<Option<String>, ProviderError> {
    match value.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        _ => Err(invalid()),
    }
}
pub(crate) fn optional_role(value: &Value, key: &str, expected: &str) -> Result<(), ProviderError> {
    if value
        .get(key)
        .is_some_and(|v| !v.is_null() && v.as_str() != Some(expected))
    {
        Err(invalid())
    } else {
        Ok(())
    }
}
fn unique(ids: &mut BTreeSet<String>, value: &Value, key: &str) -> Result<(), ProviderError> {
    if ids.insert(id(value, key)?.into()) {
        Ok(())
    } else {
        Err(invalid())
    }
}

// The neutral output renders reasoning, visible text/refusal, then calls. It
// cannot preserve an interleaving that revisits an earlier kind of content.
fn ordered(phase: &mut u8, next: u8) -> Result<(), ProviderError> {
    if next < *phase {
        return Err(invalid());
    }
    *phase = next;
    Ok(())
}

pub(crate) fn validate(family: Family, body: &Value) -> Result<Option<String>, ProviderError> {
    match family {
        Family::OpenaiChatCompletions => chat(body),
        Family::AnthropicMessages => messages(body),
        Family::OpenaiResponses => responses(body),
        Family::GeminiGenerateContent => gemini(body),
        _ => Err(invalid()),
    }
}
fn chat(body: &Value) -> Result<Option<String>, ProviderError> {
    fields(
        body,
        &[
            "id",
            "object",
            "created",
            "model",
            "choices",
            "usage",
            "system_fingerprint",
            "service_tier",
            "request_id",
        ],
    )?;
    let choices = array(body, "choices")?;
    if choices.len() != 1 {
        return Err(invalid());
    }
    let choice = &choices[0];
    fields(choice, &["index", "message", "finish_reason"])?;
    if choice.get("index").is_some_and(|v| v.as_u64() != Some(0)) {
        return Err(invalid());
    }
    if !matches!(
        text(choice, "finish_reason")?,
        "stop" | "length" | "tool_calls" | "content_filter"
    ) {
        return Err(invalid());
    }
    let message = choice.get("message").ok_or_else(invalid)?;
    fields(
        message,
        &[
            "role",
            "content",
            "tool_calls",
            "refusal",
            "reasoning_content",
            "reasoning",
        ],
    )?;
    optional_role(message, "role", "assistant")?;
    optional_text(message, "content")?;
    optional_text(message, "refusal")?;
    let reasoning = optional_text(message, "reasoning_content")?;
    let alternate = optional_text(message, "reasoning")?;
    if reasoning
        .as_ref()
        .zip(alternate.as_ref())
        .is_some_and(|(a, b)| a != b)
    {
        return Err(invalid());
    }
    let mut ids = BTreeSet::new();
    if message.get("tool_calls").is_some_and(|v| !v.is_null()) {
        for call in array(message, "tool_calls")? {
            fields(call, &["id", "type", "function"])?;
            if text(call, "type")? != "function" {
                return Err(invalid());
            }
            unique(&mut ids, call, "id")?;
            let function = call.get("function").ok_or_else(invalid)?;
            fields(function, &["name", "arguments"])?;
            id(function, "name")?;
            string_args(function.get("arguments").ok_or_else(invalid)?)?;
        }
    }
    Ok(reasoning.or(alternate))
}
fn messages(body: &Value) -> Result<Option<String>, ProviderError> {
    fields(
        body,
        &[
            "id",
            "type",
            "role",
            "model",
            "content",
            "stop_reason",
            "stop_sequence",
            "usage",
        ],
    )?;
    optional_role(body, "role", "assistant")?;
    optional_role(body, "type", "message")?;
    if !matches!(
        text(body, "stop_reason")?,
        "end_turn" | "stop_sequence" | "max_tokens" | "model_context_window_exceeded" | "tool_use"
    ) {
        return Err(invalid());
    }
    let mut ids = BTreeSet::new();
    let mut reasoning = String::new();
    let mut phase = 0;
    for block in array(body, "content")? {
        match text(block, "type")? {
            "text" => {
                ordered(&mut phase, 1)?;
                fields(block, &["type", "text"])?;
                text(block, "text")?;
            }
            "thinking" => {
                ordered(&mut phase, 0)?;
                fields(block, &["type", "thinking"])?;
                reasoning.push_str(text(block, "thinking")?);
            }
            "tool_use" => {
                ordered(&mut phase, 3)?;
                fields(block, &["type", "id", "name", "input"])?;
                unique(&mut ids, block, "id")?;
                id(block, "name")?;
                object_args(block.get("input").ok_or_else(invalid)?)?;
            }
            _ => return Err(invalid()),
        }
    }
    Ok((!reasoning.is_empty()).then_some(reasoning))
}
pub(crate) const RESPONSES_FIELDS: &[&str] = &[
    "id",
    "object",
    "created_at",
    "completed_at",
    "model",
    "status",
    "output",
    "usage",
    "incomplete_details",
    "background",
    "instructions",
    "max_output_tokens",
    "max_tool_calls",
    "metadata",
    "parallel_tool_calls",
    "prompt_cache_key",
    "prompt_cache_retention",
    "reasoning",
    "safety_identifier",
    "service_tier",
    "store",
    "temperature",
    "text",
    "tool_choice",
    "tools",
    "top_logprobs",
    "top_p",
    "truncation",
    "user",
];
fn responses(body: &Value) -> Result<Option<String>, ProviderError> {
    fields(body, RESPONSES_FIELDS)?;
    let status = text(body, "status")?;
    if !matches!(status, "completed" | "incomplete") {
        return Err(invalid());
    }
    if status == "incomplete" && body.get("incomplete_details").is_none_or(Value::is_null) {
        return Err(invalid());
    }
    if let Some(details) = body.get("incomplete_details").filter(|v| !v.is_null()) {
        if status != "incomplete" {
            return Err(invalid());
        }
        fields(details, &["reason"])?;
        if !matches!(
            text(details, "reason")?,
            "max_output_tokens" | "content_filter"
        ) {
            return Err(invalid());
        }
    }
    let mut ids = BTreeSet::new();
    let mut reasoning = String::new();
    let mut phase = 0;
    for item in array(body, "output")? {
        match text(item, "type")? {
            "message" => {
                fields(item, &["type", "id", "role", "status", "content"])?;
                optional_role(item, "role", "assistant")?;
                for part in array(item, "content")? {
                    match text(part, "type")? {
                        "output_text" => {
                            ordered(&mut phase, 1)?;
                            fields(part, &["type", "text"])?;
                            text(part, "text")?;
                        }
                        "refusal" => {
                            ordered(&mut phase, 2)?;
                            fields(part, &["type", "refusal"])?;
                            text(part, "refusal")?;
                        }
                        _ => return Err(invalid()),
                    }
                }
            }
            "function_call" => {
                ordered(&mut phase, 3)?;
                fields(
                    item,
                    &["type", "id", "call_id", "name", "arguments", "status"],
                )?;
                unique(&mut ids, item, "call_id")?;
                id(item, "name")?;
                string_args(item.get("arguments").ok_or_else(invalid)?)?;
            }
            "reasoning" => {
                ordered(&mut phase, 0)?;
                fields(item, &["type", "id", "status", "summary", "content"])?;
                for (key, kind) in [("summary", "summary_text"), ("content", "reasoning_text")] {
                    if item.get(key).is_some_and(|v| !v.is_null()) {
                        for part in array(item, key)? {
                            fields(part, &["type", "text"])?;
                            if text(part, "type")? != kind {
                                return Err(invalid());
                            }
                            reasoning.push_str(text(part, "text")?);
                        }
                    }
                }
            }
            _ => return Err(invalid()),
        }
    }
    Ok((!reasoning.is_empty()).then_some(reasoning))
}
fn gemini(body: &Value) -> Result<Option<String>, ProviderError> {
    fields(
        body,
        &[
            "responseId",
            "modelVersion",
            "createTime",
            "candidates",
            "usageMetadata",
        ],
    )?;
    let candidates = array(body, "candidates")?;
    if candidates.len() != 1 {
        return Err(invalid());
    }
    let candidate = &candidates[0];
    fields(candidate, &["index", "content", "finishReason"])?;
    if candidate
        .get("index")
        .is_some_and(|v| v.as_u64() != Some(0))
    {
        return Err(invalid());
    }
    if !matches!(
        text(candidate, "finishReason")?,
        "STOP"
            | "MAX_TOKENS"
            | "SAFETY"
            | "RECITATION"
            | "BLOCKLIST"
            | "PROHIBITED_CONTENT"
            | "SPII"
            | "MODEL_ARMOR"
    ) {
        return Err(invalid());
    }
    let content = candidate.get("content").ok_or_else(invalid)?;
    fields(content, &["role", "parts"])?;
    optional_role(content, "role", "model")?;
    let mut ids = BTreeSet::new();
    let mut reasoning = String::new();
    let mut phase = 0;
    for part in array(content, "parts")? {
        fields(part, &["text", "thought", "functionCall"])?;
        if part.get("thought").is_some_and(|v| !v.is_boolean()) {
            return Err(invalid());
        }
        match (part.get("text"), part.get("functionCall")) {
            (Some(Value::String(value)), None) => {
                if part.get("thought").and_then(Value::as_bool) == Some(true) {
                    ordered(&mut phase, 0)?;
                    reasoning.push_str(value);
                } else {
                    ordered(&mut phase, 1)?;
                }
            }
            (None, Some(call)) if part.get("thought").is_none_or(|v| v == &Value::Bool(false)) => {
                ordered(&mut phase, 3)?;
                fields(call, &["id", "name", "args"])?;
                if call.get("id").is_some() {
                    unique(&mut ids, call, "id")?;
                }
                id(call, "name")?;
                object_args(call.get("args").ok_or_else(invalid)?)?;
            }
            _ => return Err(invalid()),
        }
    }
    Ok((!reasoning.is_empty()).then_some(reasoning))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn valid(family: Family) -> Value {
        match family {
            Family::OpenaiChatCompletions => {
                json!({"choices":[{"message":{"role":"assistant","content":"reply","reasoning_content":"materialized"},"finish_reason":"stop"}]})
            }
            Family::AnthropicMessages => {
                json!({"type":"message","role":"assistant","stop_reason":"end_turn","content":[
                {"type":"thinking","thinking":"materialized","signature":""},{"type":"text","text":"reply","citations":[]}]})
            }
            Family::OpenaiResponses => {
                json!({"status":"completed","output":[{"type":"reasoning","summary":[{"type":"summary_text","text":"materialized"}],"encrypted_content":null},
                {"type":"message","role":"assistant","content":[{"type":"output_text","text":"reply","annotations":[]}]}]})
            }
            _ => {
                json!({"candidates":[{"content":{"role":"model","parts":[{"text":"materialized","thought":true},{"text":"reply"}]},"finishReason":"STOP"}]})
            }
        }
    }
    #[test]
    fn materialized_reasoning_is_preserved_and_opaque_state_never_becomes_portable() {
        for family in [
            Family::OpenaiChatCompletions,
            Family::AnthropicMessages,
            Family::OpenaiResponses,
            Family::GeminiGenerateContent,
        ] {
            let body = valid(family);
            assert_eq!(
                validate(family, &body).unwrap().as_deref(),
                Some("materialized")
            );
            let mut opaque = body.clone();
            match family {
                Family::OpenaiChatCompletions => {
                    opaque["choices"][0]["message"]["audio"] = json!({"data":"opaque"})
                }
                Family::AnthropicMessages => opaque["content"][0]["signature"] = "opaque".into(),
                Family::OpenaiResponses => {
                    opaque["output"][0]["encrypted_content"] = "opaque".into()
                }
                _ => {
                    opaque["candidates"][0]["content"]["parts"][0]["thoughtSignature"] =
                        "opaque".into()
                }
            }
            assert!(validate(family, &opaque).is_err(), "{family:?}");
        }
    }
    #[test]
    fn incomplete_responses_require_a_representable_reason() {
        let mut body = valid(Family::OpenaiResponses);
        body["status"] = "incomplete".into();
        assert!(validate(Family::OpenaiResponses, &body).is_err());
        body["incomplete_details"] = json!({"reason":"future_reason"});
        assert!(validate(Family::OpenaiResponses, &body).is_err());
        for reason in ["max_output_tokens", "content_filter"] {
            body["incomplete_details"] = json!({"reason":reason});
            assert!(validate(Family::OpenaiResponses, &body).is_ok());
        }
        body["status"] = "completed".into();
        assert!(validate(Family::OpenaiResponses, &body).is_err());
    }
    #[test]
    fn interleaved_output_requires_native_delivery_instead_of_reordering() {
        for family in [
            Family::AnthropicMessages,
            Family::OpenaiResponses,
            Family::GeminiGenerateContent,
        ] {
            let mut body = valid(family);
            let parts = match family {
                Family::AnthropicMessages => body["content"].as_array_mut().unwrap(),
                Family::OpenaiResponses => body["output"].as_array_mut().unwrap(),
                _ => body["candidates"][0]["content"]["parts"]
                    .as_array_mut()
                    .unwrap(),
            };
            parts.reverse();
            assert!(validate(family, &body).is_err(), "{family:?}");
        }
    }
    #[test]
    fn malformed_tools_and_unsupported_output_do_not_silently_disappear() {
        for family in [
            Family::OpenaiChatCompletions,
            Family::AnthropicMessages,
            Family::OpenaiResponses,
            Family::GeminiGenerateContent,
        ] {
            for args in [json!({}), Value::Null, json!([]), json!("{}"), json!(42)] {
                let body = match family {
                    Family::OpenaiChatCompletions => {
                        json!({"choices":[{"message":{"tool_calls":[{"id":"t","type":"function","function":{"name":"f","arguments":args.to_string()}}]},"finish_reason":"tool_calls"}]})
                    }
                    Family::AnthropicMessages => {
                        json!({"stop_reason":"tool_use","content":[{"type":"tool_use","id":"t","name":"f","input":args}]})
                    }
                    Family::OpenaiResponses => {
                        json!({"status":"completed","output":[{"type":"function_call","call_id":"t","name":"f","arguments":args.to_string()}]})
                    }
                    _ => {
                        json!({"candidates":[{"finishReason":"STOP","content":{"parts":[{"functionCall":{"name":"f","args":args}}]}}]})
                    }
                };
                assert_eq!(
                    validate(family, &body).is_ok(),
                    args.is_object(),
                    "{family:?}/{args}"
                );
            }
        }
        for (family, body) in [
            (
                Family::OpenaiChatCompletions,
                json!({"choices":[{"message":{"content":[{"type":"image_url","image_url":{"url":"opaque"}}]},"finish_reason":"stop"}]}),
            ),
            (
                Family::AnthropicMessages,
                json!({"stop_reason":"end_turn","content":[{"type":"redacted_thinking","data":"opaque"}]}),
            ),
            (
                Family::OpenaiResponses,
                json!({"status":"completed","output":[{"type":"image_generation_call","result":"opaque"}]}),
            ),
            (
                Family::GeminiGenerateContent,
                json!({"candidates":[{"finishReason":"STOP","content":{"parts":[{"inlineData":{"mimeType":"image/png","data":"opaque"}}]}}]}),
            ),
            (
                Family::OpenaiResponses,
                json!({"status":"completed","output":[{"type":"function_call","call_id":"t","name":"f","arguments":"{\"a\":1,\"a\":2}"}]}),
            ),
        ] {
            assert!(validate(family, &body).is_err());
        }
    }
}
