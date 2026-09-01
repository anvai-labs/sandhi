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

    async fn complete(
        &self,
        request: ChatRequestV1,
        call_headers: http::HeaderMap,
    ) -> Result<ChatResponseV1, ProviderError> {
        request.validate().map_err(ProviderError::InvalidRequest)?;
        let body = encode_ollama_request(&request)?;
        let response = self
            .raw
            .complete(provider_request(&request, body, call_headers))
            .await?;
        let mut decoded = decode_ollama_response(response.body, response.usage, &request.model)?;
        if !request.include_native_response {
            // G8: the native body is debug metadata, not contract. Decoded
            // extensions (e.g. "reasoning") always survive.
            decoded.extensions.remove("ollama");
        }
        decoded.usage.attempts = response.attempts;
        decoded.usage.outcome = Some("success".into());
        Ok(decoded)
    }

    async fn stream(
        &self,
        request: ChatRequestV1,
        call_headers: http::HeaderMap,
    ) -> Result<ChatEventStream, ProviderError> {
        request.validate().map_err(ProviderError::InvalidRequest)?;
        let body = encode_ollama_request(&request)?;
        let raw = self
            .raw
            .stream(provider_request(&request, body, call_headers))
            .await?;
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
        body.insert("format".into(), normalize_response_format(format));
    }
    Ok(Value::Object(body))
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

fn decode_ollama_stream(mut raw: ByteStream, requested_model: String) -> ChatEventStream {
    use futures_util::StreamExt;
    let stream = async_stream::try_stream! {
        // TD-0014 P1: the shared bounded splitter. One ceiling across both planes; only the
        // over-budget POLICY differs, and it is applied below. See MAX_STREAM_LINE_BYTES.
        let mut splitter = crate::linesplit::LineSplitter::new(crate::MAX_STREAM_LINE_BYTES);
        let mut started = false;
        let mut emitted_usage = false;
        // TD-0014 P2b: after the real chunks end, ONE synthetic empty chunk flushes any
        // trailing remainder (a final frame without its newline — Ollama's `done` frame) through
        // this same loop body, so per-chunk event ordering is preserved exactly and the body is
        // not duplicated. `raw.next()` returning None terminates; a false flush breaks out.
        let mut tail_pending = false;
        let mut chunks_ended = false;
        while !chunks_ended || tail_pending {
            let chunk = if chunks_ended {
                tail_pending = false;
                crate::StreamChunk {
                    data: bytes::Bytes::new(),
                    usage: None,
                    usage_running: None,
                    attempts: 1,
                }
            } else {
                match raw.next().await {
                    Some(chunk) => chunk?,
                    None => {
                        chunks_ended = true;
                        tail_pending = splitter.flush_newline();
                        continue;
                    }
                }
            };
            let attempts = chunk.attempts;
            // Unconditional: the synthetic tail chunk arrives with empty data and must still
            // drain the flushed remainder; draining after no new bytes is a no-op scan.
            // (The terminal usage-only chunk is likewise empty and previously skipped this.)
            {
                splitter.push(&chunk.data);
                while let Some(line) = splitter.next_line() {
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
                // TD-0014 P1 (gap G01): past MAX_STREAM_LINE_BYTES the upstream has sent no
                // line boundary at all, which no real provider does. The raw plane drops the
                // pending line and keeps streaming — its bytes were already forwarded, so
                // only usage suffers. A typed decoder emits decoded CONTENT, so dropping
                // silently would corrupt the response with no signal. Fail loudly instead;
                // mid-stream errors are never retried.
                if splitter.over_budget() {
                    Err(ProviderError::Transport(format!(
                        "upstream stream exceeded {} bytes with no line boundary",
                        crate::MAX_STREAM_LINE_BYTES
                    )))?;
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

    /// TD-0014 P2b: Ollama's NDJSON `done` frame may arrive WITHOUT its trailing newline. The
    /// pre-P2b decoders dropped a newline-less remainder, so the `Finish` never yielded. The
    /// synthetic tail chunk now flushes the remainder through the same loop body.
    #[tokio::test]
    async fn a_done_frame_without_a_trailing_newline_still_yields_finish() {
        use futures_util::StreamExt;
        let wire = concat!(
            "{\"created_at\":\"t1\",\"model\":\"llama3\",\"message\":{\"content\":\"he\"}}\n",
            "{\"model\":\"llama3\",\"message\":{\"content\":\"llo\"}}\n",
            // No trailing newline on the final frame — the whole point.
            "{\"model\":\"llama3\",\"done\":true,\"done_reason\":\"stop\"}"
        );
        let raw: crate::ByteStream =
            Box::pin(futures_util::stream::iter(vec![Ok(crate::StreamChunk {
                data: bytes::Bytes::from(wire),
                usage: Some(crate::ParsedUsage {
                    tokens_in: 1,
                    tokens_out: 2,
                    cache_creation_tokens: 0,
                    cache_read_tokens: 0,
                    reasoning_tokens: 0,
                }),
                usage_running: None,
                attempts: 1,
            })]));
        let events = super::decode_ollama_stream(raw, "llama3".into())
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert!(
            events.iter().any(|e| matches!(
                e,
                sandhi_core::ChatStreamEventV1::Finish {
                    reason: sandhi_core::FinishReasonV1::Stop
                }
            )),
            "the newline-less done frame must still produce Finish, got: {events:?}"
        );
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e, sandhi_core::ChatStreamEventV1::Usage { .. }))
                .count(),
            1,
            "exactly one usage frame"
        );
    }

    /// TD-0014 P1, the opposing guard. The six `..._is_bounded_and_errors_...` tests pin the
    /// ceiling from BELOW; on their own they would all still pass with the bound set to 1 KiB,
    /// which would break every legitimate large frame. Adversarial review found five of six
    /// decoders had no test in this direction — the same one-sidedness that let the original
    /// 64 KiB ceiling ship. This pins it from above.
    #[tokio::test]
    async fn a_large_but_legitimate_frame_is_not_killed_by_the_line_bound() {
        use futures_util::StreamExt;
        // 200 KB in one terminated line: far past the old 64 KiB bound, far under the real one.
        let big = "x".repeat(200 * 1024);
        let wire = format!("{{\"model\":\"m\",\"message\":{{\"content\":\"{big}\"}}}}\n");
        let chunks: Vec<_> = wire
            .as_bytes()
            .chunks(16 * 1024)
            .map(|c| {
                Ok(crate::StreamChunk {
                    data: bytes::Bytes::copy_from_slice(c),
                    usage: None,
                    usage_running: None,
                    attempts: 1,
                })
            })
            .collect();
        let raw: crate::ByteStream = Box::pin(futures_util::stream::iter(chunks));
        let results: Vec<_> = super::decode_ollama_stream(raw, "m".into())
            .collect::<Vec<_>>()
            .await;
        assert!(
            !results
                .iter()
                .any(|r| matches!(r, Err(crate::ProviderError::Transport(_)))),
            "a 200 KB terminated frame is legitimate traffic and must survive"
        );
        let text: usize = results
            .iter()
            .flatten()
            .filter_map(|e| match e {
                sandhi_core::ChatStreamEventV1::TextDelta { delta } => Some(delta.len()),
                _ => None,
            })
            .sum();
        assert_eq!(text, big.len(), "the whole delta must arrive intact");
    }

    /// TD-0014 P1 (gap G01): a newline-free upstream stream must stay BOUNDED and fail loudly.
    ///
    /// The raw plane may drop an over-budget line and keep going — its bytes were already
    /// forwarded verbatim, so only *usage* is lost. A typed decoder drops decoded **content**,
    /// so dropping silently would corrupt the response with no signal at all. It errors instead.
    /// Mid-stream errors are never retried (`resilience.rs`), so this cannot loop.
    #[tokio::test]
    async fn a_newline_free_stream_is_bounded_and_errors_rather_than_growing() {
        use futures_util::StreamExt;
        // 16 MiB with no line boundary anywhere — past MAX_STREAM_LINE_BYTES (8 MiB). The bound
        // exists to stop unbounded growth, so the test input has to be genuinely pathological;
        // a merely LARGE frame is legitimate and is covered by the regression test above.
        let filler = bytes::Bytes::from(vec![b'x'; 64 * 1024]);
        let chunks: Vec<_> = (0..256)
            .map(|_| {
                Ok(crate::StreamChunk {
                    data: filler.clone(),
                    usage: None,
                    usage_running: None,
                    attempts: 1,
                })
            })
            .collect();
        let raw: crate::ByteStream = Box::pin(futures_util::stream::iter(chunks));
        let results: Vec<_> = super::decode_ollama_stream(raw, "m".into())
            .collect::<Vec<_>>()
            .await;
        assert!(
            results
                .iter()
                .any(|item| matches!(item, Err(crate::ProviderError::Transport(_)))),
            "a newline-free stream must terminate with a Transport error rather than \
             buffering without bound"
        );
    }
    use super::*;

    /// TD-0014 P1 characterisation net (gap G01). The decoder must yield identical events no
    /// matter where the transport splits the byte stream. Written BEFORE the `LineSplitter`
    /// refactor precisely so it can prove the refactor changed nothing: `ollama_typed` is one of the
    /// three typed decoders that had no boundary-invariance test at all, so the refactor would
    /// otherwise have had no net beneath it. Mirrors `anthropic_typed`'s equivalent.
    #[tokio::test]
    async fn stream_codec_is_invariant_across_arbitrary_byte_boundaries() {
        use futures_util::StreamExt;
        let wire = concat!(
            "{\"created_at\":\"t1\",\"model\":\"llama3\",\"message\":{\"content\":\"he\"}}\n",
            "{\"model\":\"llama3\",\"message\":{\"content\":\"llo\"}}\n",
            "{\"model\":\"llama3\",\"message\":{\"content\":\"\"},\"done\":true,\"done_reason\":\"stop\"}\n",
        ).as_bytes();
        for split in 0..=wire.len() {
            let raw: crate::ByteStream = Box::pin(futures_util::stream::iter(vec![
                Ok(crate::StreamChunk {
                    data: bytes::Bytes::copy_from_slice(&wire[..split]),
                    usage: None,
                    usage_running: None,
                    attempts: 1,
                }),
                Ok(crate::StreamChunk {
                    data: bytes::Bytes::copy_from_slice(&wire[split..]),
                    usage: None,
                    usage_running: None,
                    attempts: 1,
                }),
                Ok(crate::StreamChunk {
                    data: bytes::Bytes::new(),
                    usage: Some(crate::ParsedUsage {
                        tokens_in: 2,
                        tokens_out: 3,
                        cache_creation_tokens: 0,
                        cache_read_tokens: 0,
                        reasoning_tokens: 0,
                    }),
                    usage_running: None,
                    attempts: 1,
                }),
            ]));
            let events = decode_ollama_stream(raw, "llama3".into())
                .collect::<Vec<_>>()
                .await
                .into_iter()
                .collect::<Result<Vec<_>, _>>()
                .unwrap();

            assert!(
                matches!(
                    events.first(),
                    Some(sandhi_core::ChatStreamEventV1::ResponseStart { .. })
                ),
                "split {split}: first event must be ResponseStart"
            );
            // The load-bearing assertion for a line splitter: mis-split lines drop or duplicate
            // text deltas, and concatenating them is what catches that.
            let text: String = events
                .iter()
                .filter_map(|event| match event {
                    sandhi_core::ChatStreamEventV1::TextDelta { delta } => Some(delta.as_str()),
                    _ => None,
                })
                .collect();
            assert_eq!(
                text, "hello",
                "split {split}: text deltas must reassemble exactly"
            );
            assert!(
                events
                    .iter()
                    .any(|event| matches!(event, sandhi_core::ChatStreamEventV1::Finish { .. })),
                "split {split}: Finish must survive any split"
            );
            assert_eq!(
                events
                    .iter()
                    .filter(|event| matches!(
                        event,
                        sandhi_core::ChatStreamEventV1::Usage { usage }
                            if usage.completeness == sandhi_core::UsageCompleteness::Final
                    ))
                    .count(),
                1,
                "split {split}: exactly one Final usage frame"
            );
        }
    }

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
