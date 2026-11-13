//! Native-input provenance is constructed here, never read from HTTP fields or
//! sandhi_extensions. Feature classes flag losses in the portable projection.
use sandhi_core::{ChatRequestV1, EndpointFamilyV1, NativeInputFeatureV1 as Feature};
use serde_json::Value;

pub(super) fn capture(request: &mut ChatRequestV1, family: EndpointFamilyV1) -> Result<(), String> {
    let bucket = match family {
        EndpointFamilyV1::OpenaiChatCompletions => "openai",
        EndpointFamilyV1::OpenaiResponses => "openai_responses",
        EndpointFamilyV1::AnthropicMessages => "anthropic",
        EndpointFamilyV1::GeminiGenerateContent => "gemini",
        _ => return Err("unsupported ingress native input family".into()),
    };
    let body = &request.extensions[bucket];
    let mut features = Vec::new();
    let mut mark = |feature| {
        if !features.contains(&feature) {
            features.push(feature);
        }
    };
    if sandhi_providers::contains_native_cache_controls(body, family) {
        mark(Feature::NativeControl);
    }
    match family {
        EndpointFamilyV1::OpenaiResponses => {
            if ["previous_response_id", "conversation"]
                .iter()
                .any(|key| body.get(*key).is_some_and(|v| !v.is_null()))
            {
                mark(Feature::ResourceReference);
            }
            for item in array(body.get("input")) {
                match item.get("type").and_then(Value::as_str) {
                    Some("reasoning") => mark(Feature::OpaqueState),
                    Some("item_reference") => mark(Feature::ResourceReference),
                    Some("message") | None if item.get("role").is_some() => {
                        for part in array(item.get("content")) {
                            if contains_file_reference(part) {
                                mark(Feature::ResourceReference);
                            }
                            if !matches!(
                                part.get("type").and_then(Value::as_str),
                                Some(
                                    "input_text"
                                        | "output_text"
                                        | "text"
                                        | "refusal"
                                        | "input_image"
                                        | "input_file"
                                )
                            ) {
                                mark(Feature::UnsupportedContent);
                            }
                        }
                    }
                    Some("function_call") => {}
                    Some("function_call_output") => {
                        let output = item.get("output").unwrap_or(&Value::Null);
                        // Only string output has a faithful portable projection
                        // today. Native arrays/objects remain exact on Responses;
                        // other targets must not receive JSON text as image/media.
                        if !output.is_string() {
                            mark(Feature::UnsupportedContent);
                        }
                        if contains_file_reference(output) {
                            mark(Feature::ResourceReference);
                        }
                    }
                    _ => mark(Feature::UnsupportedContent),
                }
            }
            if array(body.get("tools")).iter().any(|tool| {
                tool.get("type")
                    .and_then(Value::as_str)
                    .is_some_and(|kind| kind != "function")
            }) {
                mark(Feature::NativeControl);
            }
        }
        EndpointFamilyV1::AnthropicMessages => {
            for message in array(body.get("messages")) {
                let blocks = array(message.get("content"));
                let mut saw_tool = false;
                for block in blocks {
                    match block.get("type").and_then(Value::as_str) {
                        Some("thinking" | "redacted_thinking") => mark(Feature::OpaqueState),
                        Some("tool_use") => saw_tool = true,
                        Some("text") => {
                            if saw_tool {
                                mark(Feature::UnsupportedContent);
                            }
                        }
                        Some("image" | "tool_result") => {}
                        _ => mark(Feature::UnsupportedContent),
                    }
                    if block.get("is_error").and_then(Value::as_bool) == Some(true) {
                        mark(Feature::NativeControl);
                    }
                }
            }
            if body.get("container").is_some() {
                mark(Feature::ResourceReference);
            }
            if [
                "top_k",
                "top_p",
                "context_management",
                "output_config",
                "service_tier",
            ]
            .iter()
            .any(|key| body.get(*key).is_some())
            {
                mark(Feature::NativeControl);
            }
            if array(body.get("tools")).iter().any(|tool| {
                tool.get("type")
                    .and_then(Value::as_str)
                    .is_some_and(|kind| kind != "custom")
            }) {
                mark(Feature::NativeControl);
            }
        }
        EndpointFamilyV1::GeminiGenerateContent => {
            if body.get("cachedContent").is_some() {
                mark(Feature::ResourceReference);
            }
            if body.get("safetySettings").is_some() {
                mark(Feature::NativeControl);
            }
            for content in array(body.get("contents")) {
                for part in array(content.get("parts")) {
                    if part.get("thoughtSignature").is_some()
                        || part.get("thought").and_then(Value::as_bool) == Some(true)
                    {
                        mark(Feature::OpaqueState);
                    }
                    if !part.get("text").is_some()
                        && !part.get("functionCall").is_some()
                        && !part.get("functionResponse").is_some()
                        && !part.get("inlineData").is_some()
                    {
                        mark(Feature::UnsupportedContent);
                    }
                    if let Some(inline) = part.get("inlineData") {
                        let mime = super::gemini_inline_mime(inline)?;
                        if !mime
                            .split_once('/')
                            .is_some_and(|(kind, _)| kind.eq_ignore_ascii_case("image"))
                        {
                            mark(Feature::UnsupportedContent);
                        }
                    }
                }
            }
            if array(body.pointer("/toolConfig/functionCallingConfig/allowedFunctionNames")).len()
                > 1
            {
                mark(Feature::NativeControl);
            }
            if array(body.get("tools")).iter().any(|tool| {
                tool.as_object()
                    .is_none_or(|object| object.keys().any(|key| key != "functionDeclarations"))
            }) {
                mark(Feature::NativeControl);
            }
            if body
                .get("generationConfig")
                .and_then(Value::as_object)
                .is_some_and(|config| config.keys().any(|key| key != "thinkingConfig"))
            {
                // Only unprojected generation controls remain in this bucket.
                mark(Feature::NativeControl);
            }
            if let Some(thinking) = body.pointer("/generationConfig/thinkingConfig") {
                if thinking.as_object().is_none_or(|config| {
                    config
                        .keys()
                        .any(|key| !matches!(key.as_str(), "thinkingBudget" | "thinking_budget"))
                }) || (thinking
                    .get("thinkingBudget")
                    .or_else(|| thinking.get("thinking_budget"))
                    .is_some()
                    && request.thinking.is_none())
                {
                    mark(Feature::NativeControl);
                }
            }
        }
        EndpointFamilyV1::OpenaiChatCompletions => {
            for message in array(body.get("messages")) {
                if message.get("content").is_some_and(contains_file_reference) {
                    mark(Feature::ResourceReference);
                }
                if ["reasoning_content", "reasoning", "thinking_blocks", "audio"]
                    .iter()
                    .any(|key| message.get(*key).is_some())
                {
                    mark(Feature::OpaqueState);
                }
            }
            if [
                "top_p",
                "presence_penalty",
                "frequency_penalty",
                "logit_bias",
                "n",
                "parallel_tool_calls",
                "modalities",
                "audio",
                "functions",
                "function_call",
            ]
            .iter()
            .any(|key| body.get(*key).is_some())
            {
                mark(Feature::NativeControl);
            }
        }
        _ => unreachable!(),
    }
    request.capture_native_input(family, features)
}

fn array(value: Option<&Value>) -> &[Value] {
    value
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
}

/// Inspect structured content only, never JSON-looking text/tool arguments.
/// Provider file identifiers can appear inside file wrappers and tool outputs.
fn contains_file_reference(value: &Value) -> bool {
    match value {
        Value::Object(object) => {
            object.get("file_id").is_some_and(|value| !value.is_null())
                || object.values().any(contains_file_reference)
        }
        Value::Array(values) => values.iter().any(contains_file_reference),
        _ => false,
    }
}
