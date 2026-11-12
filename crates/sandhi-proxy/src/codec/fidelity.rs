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
                    Some("message") | None if item.get("role").is_some() => {
                        for part in array(item.get("content")) {
                            if part.get("file_id").is_some() {
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
                    Some("function_call" | "function_call_output") => {}
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
                        if !inline
                            .get("mimeType")
                            .and_then(Value::as_str)
                            .is_some_and(|mime| mime.starts_with("image/"))
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
        }
        EndpointFamilyV1::OpenaiChatCompletions => {
            for message in array(body.get("messages")) {
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
