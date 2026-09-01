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

    async fn complete(
        &self,
        request: ChatRequestV1,
        call_headers: http::HeaderMap,
    ) -> Result<ChatResponseV1, ProviderError> {
        request.validate().map_err(ProviderError::InvalidRequest)?;
        let body = encode_gemini_request(&request)?;
        let response = self
            .raw
            .complete(provider_request(&request, body, call_headers))
            .await?;
        let mut decoded = decode_gemini_response(response.body, response.usage, &request.model)?;
        if !request.include_native_response {
            // G8: the native body is debug metadata, not contract. Decoded
            // extensions (e.g. "reasoning") always survive.
            decoded.extensions.remove("gemini");
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
        let body = encode_gemini_request(&request)?;
        let raw = self
            .raw
            .stream(provider_request(&request, body, call_headers))
            .await?;
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
    // W3d/G7: Gemini expresses thinking as generationConfig.thinkingConfig.
    // `thinkingBudget: 0` disables; a positive budget caps it; enabled with no
    // budget leaves the model default (omit thinkingBudget). Typed field wins
    // over an extensions duplicate. `reasoning_effort` has no Gemini analogue —
    // explicitly ignored (consumer-decision row).
    if let Some(thinking) = &request.thinking {
        let mut thinking_config = serde_json::Map::new();
        if !thinking.enabled {
            thinking_config.insert("thinkingBudget".into(), json!(0));
        } else if let Some(budget) = thinking.budget_tokens {
            thinking_config.insert("thinkingBudget".into(), json!(budget));
        }
        generation.insert("thinkingConfig".into(), Value::Object(thinking_config));
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
        // TD-0014 P1: the shared bounded splitter. One ceiling across both planes; only the
        // over-budget POLICY differs, and it is applied below. See MAX_STREAM_LINE_BYTES.
        let mut splitter = crate::linesplit::LineSplitter::new(crate::MAX_STREAM_LINE_BYTES);
        let mut started = false;
        let mut emitted_usage = false;
        // The last running total published, so progress is emitted on change rather than per chunk.
        let mut last_running: Option<crate::ParsedUsage> = None;
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
            } else if chunk.usage_running.is_some() && chunk.usage_running != last_running {
                // Gemini is an `Incremental` family (TD-0013 D1): `usageMetadata` rides on chunks,
                // so a real cumulative count exists before the stream ends. Publishing it as a
                // non-final `Usage` lets an interrupted stream settle that instead of a byte
                // estimate; the proxy treats it as accounting-only (D7), so it never supersedes
                // the terminal frame and never reaches the client.
                last_running = chunk.usage_running;
                let mut usage: UsageV2 = chunk.usage_running.unwrap_or_default().into();
                usage.completeness = UsageCompleteness::Partial;
                usage.attempts = attempts;
                yield ChatStreamEventV1::Usage { usage };
            }
        }
    };
    Box::pin(stream)
}

#[cfg(test)]
mod tests {

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
        let wire = format!(
            "data: {{\"candidates\":[{{\"content\":{{\"parts\":[{{\"text\":\"{big}\"}}]}}}}]}}\n\n"
        );
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
        let results: Vec<_> = super::decode_gemini_stream(raw, "m".into())
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
        let results: Vec<_> = super::decode_gemini_stream(raw, "m".into())
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
    /// refactor precisely so it can prove the refactor changed nothing: `gemini_typed` is one of the
    /// three typed decoders that had no boundary-invariance test at all, so the refactor would
    /// otherwise have had no net beneath it. Mirrors `anthropic_typed`'s equivalent.
    #[tokio::test]
    async fn stream_codec_is_invariant_across_arbitrary_byte_boundaries() {
        use futures_util::StreamExt;
        let wire = concat!(
            "data: {\"responseId\":\"g1\",\"modelVersion\":\"gemini-test\",\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"he\"}]}}]}\n\n",
            "data: {\"candidates\":[{\"content\":{\"parts\":[{\"functionCall\":{\"id\":\"t1\",\"name\":\"lookup\",\"args\":{}}}]}}]}\n\n",
            "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"llo\"}]}}]}\n\n",
            "data: {\"candidates\":[{\"finishReason\":\"STOP\"}]}\n\n",
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
            let events = decode_gemini_stream(raw, "gemini-test".into())
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
            assert!(
                events.iter().any(|event| matches!(
                    event,
                    sandhi_core::ChatStreamEventV1::ToolCallStart { id, .. } if id == "t1"
                )),
                "split {split}: ToolCallStart must survive any split"
            );
            assert!(
                events.iter().any(|event| matches!(
                    event,
                    sandhi_core::ChatStreamEventV1::ToolCallEnd { index: 0 }
                )),
                "split {split}: ToolCallEnd must survive any split"
            );
        }
    }

    #[test]
    fn w3d_thinking_maps_to_thinking_config_and_effort_is_ignored() {
        let request: ChatRequestV1 = serde_json::from_value(json!({
            "model": "gemini-test",
            "messages": [{"role": "user", "content": "hi"}],
            "reasoning_effort": "high",
            "thinking": {"enabled": true, "budget_tokens": 8192}
        }))
        .unwrap();
        let body = encode_gemini_request(&request).unwrap();
        assert_eq!(
            body["generationConfig"]["thinkingConfig"]["thinkingBudget"],
            8192
        );
        assert!(body.get("reasoning_effort").is_none());

        // Disabled thinking pins the budget to 0.
        let off: ChatRequestV1 = serde_json::from_value(json!({
            "model": "gemini-test",
            "messages": [{"role": "user", "content": "hi"}],
            "thinking": {"enabled": false}
        }))
        .unwrap();
        let body = encode_gemini_request(&off).unwrap();
        assert_eq!(
            body["generationConfig"]["thinkingConfig"]["thinkingBudget"],
            0
        );
    }

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
