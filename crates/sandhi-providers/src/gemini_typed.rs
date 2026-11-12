//! Canonical chat v1 ↔ Gemini `generateContent` codec.

use crate::typed::{provider_request_observed, ChatEventStream, ChatProvider};
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

mod qualified;

/// Correlate omitted wire IDs without reusing an identity in a later response or
/// after a process restart. Native supplied IDs remain untouched.
#[derive(Default)]
struct GeneratedToolIds {
    namespace: Option<String>,
}
impl GeneratedToolIds {
    fn id(&mut self, index: usize) -> Result<String, ProviderError> {
        if self.namespace.is_none() {
            let mut nonce = [0u8; 16];
            getrandom::getrandom(&mut nonce).map_err(|_| {
                ProviderError::Transport("Gemini tool identity entropy unavailable".into())
            })?;
            self.namespace = Some(nonce.iter().map(|byte| format!("{byte:02x}")).collect());
        }
        Ok(format!(
            "gemini_call_{}_{index}",
            self.namespace.as_ref().expect("initialized namespace")
        ))
    }
}

pub(crate) struct TypedGemini {
    raw: Arc<dyn Provider>,
}

impl TypedGemini {
    pub(crate) fn new(raw: Arc<dyn Provider>) -> Self {
        Self { raw }
    }

    async fn complete_inner(
        &self,
        request: ChatRequestV1,
        call_headers: http::HeaderMap,
        attempt_context: Option<crate::AttemptContext>,
        qualify: bool,
    ) -> Result<crate::QualifiedCompletion<ChatResponseV1>, ProviderError> {
        request.validate().map_err(ProviderError::InvalidRequest)?;
        let body = encode_gemini_request(&request)?;
        let response = self
            .raw
            .complete(provider_request_observed(
                &request,
                body,
                call_headers,
                attempt_context,
            ))
            .await?;
        let qualified = qualify.then(|| {
            sandhi_core::usage::qualify_buffered_usage(
                sandhi_core::EndpointFamilyV1::GeminiGenerateContent,
                &response.body,
            )
        });
        if !request.include_native_response {
            crate::buffered_fidelity::validate(
                sandhi_core::EndpointFamilyV1::GeminiGenerateContent,
                &response.body,
            )?;
        }
        let mut decoded = decode_gemini_response(response.body, response.usage, &request.model)?;
        if !request.include_native_response {
            decoded.extensions.remove("gemini");
        }
        decoded.usage.attempts = response.attempts;
        decoded.usage.outcome = Some("success".into());
        let usage = qualified
            .unwrap_or_else(|| Ok(decoded.usage.clone()))
            .map(|mut usage| {
                usage.attempts = response.attempts;
                usage.outcome = Some("success".into());
                usage
            });
        Ok(crate::QualifiedCompletion {
            response: decoded,
            usage,
        })
    }

    async fn stream_inner(
        &self,
        request: ChatRequestV1,
        call_headers: http::HeaderMap,
        attempt_context: Option<crate::AttemptContext>,
    ) -> Result<ChatEventStream, ProviderError> {
        request.validate().map_err(ProviderError::InvalidRequest)?;
        let body = encode_gemini_request(&request)?;
        let raw = self
            .raw
            .stream(provider_request_observed(
                &request,
                body,
                call_headers,
                attempt_context,
            ))
            .await?;
        Ok(decode_gemini_stream_mode(
            raw,
            request.model,
            request.include_native_stream,
        ))
    }
}

#[async_trait]
impl ChatProvider for TypedGemini {
    fn supports_qualified_streaming(&self) -> bool {
        true
    }
    async fn stream_qualified_observed(
        &self,
        request: ChatRequestV1,
        headers: http::HeaderMap,
        context: crate::AttemptContext,
    ) -> Result<ChatEventStream, ProviderError> {
        request.validate().map_err(ProviderError::InvalidRequest)?;
        let source = self
            .raw
            .stream_qualified(
                provider_request_observed(
                    &request,
                    encode_gemini_request(&request)?,
                    headers,
                    Some(context),
                ),
                crate::QualifiedStreamDelivery::Streaming,
            )
            .await?;
        Ok(crate::qualified_stream::decode(
            qualified::decode(source.stream, request.model, request.include_native_stream),
            source.observation,
        ))
    }
    fn preflight_request(&self, request: &ChatRequestV1) -> Result<(), ProviderError> {
        encode_gemini_request(request).map(|_| ())
    }
    fn allows_cache_synthesis(&self) -> bool {
        true
    }
    fn supports_native_stream(&self) -> bool {
        true
    }
    fn slug(&self) -> &str {
        "gemini"
    }

    async fn complete(
        &self,
        request: ChatRequestV1,
        call_headers: http::HeaderMap,
    ) -> Result<ChatResponseV1, ProviderError> {
        self.complete_inner(request, call_headers, None, false)
            .await
            .map(|r| r.response)
    }

    async fn complete_qualified(
        &self,
        request: ChatRequestV1,
        call_headers: http::HeaderMap,
    ) -> Result<crate::QualifiedCompletion<ChatResponseV1>, ProviderError> {
        self.complete_inner(request, call_headers, None, true).await
    }

    async fn complete_qualified_observed(
        &self,
        request: ChatRequestV1,
        call_headers: http::HeaderMap,
        context: crate::AttemptContext,
    ) -> Result<crate::QualifiedCompletion<ChatResponseV1>, ProviderError> {
        self.complete_inner(request, call_headers, Some(context), true)
            .await
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
        self.complete_inner(request, call_headers, Some(attempt_context), false)
            .await
            .map(|r| r.response)
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

pub fn encode_gemini_request(request: &ChatRequestV1) -> Result<Value, ProviderError> {
    let mut body = encode_gemini_request_base(request)?;
    crate::cache_policy::apply(
        request,
        sandhi_core::EndpointFamilyV1::GeminiGenerateContent,
        &mut body,
    )?;
    Ok(body)
}
pub(crate) fn encode_gemini_request_base(request: &ChatRequestV1) -> Result<Value, ProviderError> {
    request.validate().map_err(ProviderError::InvalidRequest)?;
    if !sandhi_core::model_id_is_path_safe(&request.model) {
        return Err(ProviderError::InvalidRequest(
            "Gemini requires a path-safe generation model".into(),
        ));
    }
    let native = request
        .native_input_for(sandhi_core::EndpointFamilyV1::GeminiGenerateContent)
        .map_err(ProviderError::InvalidRequest)?
        .is_some();
    let mut body = request
        .extensions
        .get("gemini")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    // Only preceding, unanswered calls can identify a tool result. A future
    // turn must never relabel an earlier result when a provider reuses an ID.
    let mut tool_names = BTreeMap::new();
    let mut system_parts = Vec::new();
    let mut contents = Vec::new();
    for message in request.messages.iter().filter(|_| !native) {
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
                    if tool_names
                        .insert(call.id.clone(), call.name.clone())
                        .is_some()
                    {
                        return Err(ProviderError::InvalidRequest(
                            "duplicate outstanding Gemini tool call id".into(),
                        ));
                    }
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
                let name = tool_names.remove(tool_call_id).ok_or_else(|| {
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
    if !native {
        body.insert("contents".into(), Value::Array(contents));
    }
    if !system_parts.is_empty() {
        body.insert("systemInstruction".into(), json!({"parts":system_parts}));
    }
    if !native && !request.tools.is_empty() {
        body.insert(
            "tools".into(),
            json!([{"functionDeclarations": request.tools}]),
        );
    }
    if let Some(choice) = request.tool_choice.as_ref().filter(|_| !native) {
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
        // Typed budget changes must not erase other family-owned controls when
        // a compatible native request is re-encoded (for example, to inject an
        // output cap). Normalize the two accepted budget spellings only.
        let mut thinking_config = generation
            .get("thinkingConfig")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        thinking_config.remove("thinkingBudget");
        thinking_config.remove("thinking_budget");
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
    let mut generated_ids = GeneratedToolIds::default();
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
                id: match call.get("id").and_then(Value::as_str) {
                    Some(id) => id.into(),
                    None => generated_ids.id(index)?,
                },
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
    let finish_reason = candidate
        .get("finishReason")
        .and_then(Value::as_str)
        .map(|reason| {
            if reason == "STOP" && !tool_calls.is_empty() {
                FinishReasonV1::ToolCalls
            } else {
                decode_finish_reason(reason)
            }
        });
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
        finish_reason,
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

#[cfg(test)]
fn decode_gemini_stream(raw: ByteStream, requested_model: String) -> ChatEventStream {
    decode_gemini_stream_mode(raw, requested_model, false)
}

pub(crate) fn decode_gemini_stream_mode(
    raw: ByteStream,
    requested_model: String,
    native: bool,
) -> ChatEventStream {
    crate::cache_read::decode_with_observation_policy(
        raw,
        requested_model,
        move |raw, model| decode_gemini_stream_inner(raw, model, native),
        native,
    )
}

fn decode_gemini_stream_inner(
    mut raw: ByteStream,
    requested_model: String,
    native: bool,
) -> ChatEventStream {
    use futures_util::StreamExt;
    let stream = async_stream::try_stream! {
        // TD-0014 P1: the shared bounded splitter. One ceiling across both planes; only the
        // over-budget POLICY differs, and it is applied below. See MAX_STREAM_LINE_BYTES.
        let mut splitter = crate::native_stream::SseDecoder::new(native.then_some(sandhi_core::EndpointFamilyV1::GeminiGenerateContent));
        let mut started = false;
        let mut emitted_usage = false;
        let mut native_usage = crate::native_stream::NativeUsage::default();
        let mut protocol_done = false;
        // A candidate finish can precede the metered stream's terminal usage-only chunk.
        // Defer canonical completion until successful EOF so egress sees final usage first.
        let mut pending_finish = None;
        let mut generated_ids=GeneratedToolIds::default();
        let mut next_tool_index=0u32;
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
                    cache_read_observation: None,
                    data: bytes::Bytes::new(),
                    usage: None,
                    usage_running: None,
                    attempts: 1,
                    terminal: false,
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
                while let Some((line, mut native_frame)) = splitter.next_line()? {
                    if native {
                        if let Some(usage) = native_usage.observe(&line, attempts, crate::gemini::sniff_usage_line) {
                            yield ChatStreamEventV1::Usage { usage };
                        }
                    }
                    let Some(value) = crate::sse_data_json(&line) else {
                        if let Some(frame) = native_frame.take() { yield ChatStreamEventV1::NativeFrame { frame }; }
                        continue;
                    };
                    if let Some(error) = crate::native_stream::upstream_error(&value) {
                        if let Some(frame) = native_frame.take() { yield ChatStreamEventV1::NativeFrame { frame }; }
                        Err(error)?;
                    }
                    if !started {
                        yield ChatStreamEventV1::ResponseStart {
                            id: value.get("responseId").and_then(Value::as_str).map(str::to_owned),
                            model: value.get("modelVersion").and_then(Value::as_str)
                                .unwrap_or(&requested_model).into(),
                        };
                        started = true;
                    }
                    if let Some(parts) = value.pointer("/candidates/0/content/parts").and_then(Value::as_array) {
                        for part in parts {
                            if let Some(text) = part.get("text").and_then(Value::as_str) {
                                if part.get("thought").and_then(Value::as_bool) == Some(true) {
                                    yield ChatStreamEventV1::ReasoningDelta { delta: text.into() };
                                } else {
                                    yield ChatStreamEventV1::TextDelta { delta: text.into() };
                                }
                            }
                            if let Some(call) = part.get("functionCall") {
                                let stream_index = next_tool_index;
                                next_tool_index=next_tool_index.checked_add(1).ok_or_else(||ProviderError::Transport("too many Gemini tool calls".into()))?;
                                let id = match call.get("id").and_then(Value::as_str) {
                                    Some(id)=>id.into(),None=>generated_ids.id(stream_index as usize)?,
                                };
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
                        pending_finish = Some(if reason=="STOP" && next_tool_index>0 {FinishReasonV1::ToolCalls} else {decode_finish_reason(reason)});
                    }
                    if value.pointer("/promptFeedback/blockReason").and_then(Value::as_str).is_some() {
                        pending_finish = Some(FinishReasonV1::ContentFilter);
                    }
                    if native && pending_finish.is_some() && value.get("usageMetadata").is_some() {
                        if let Some(usage) = native_usage.finish(attempts) { yield ChatStreamEventV1::Usage { usage }; }
                        emitted_usage = true; protocol_done = true;
                    }
                    if let Some(frame) = native_frame.take() { yield ChatStreamEventV1::NativeFrame { frame }; }
                    if protocol_done { break; }
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
            if native {
                if protocol_done { break; }
                continue;
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
        if native && !emitted_usage {
            if pending_finish.is_none() {
                Err(ProviderError::Transport("native Gemini stream ended before a finish or block reason".into()))?;
            }
            if let Some(usage) = native_usage.finish_last() { yield ChatStreamEventV1::Usage { usage }; }
        }
        if let Some(reason) = pending_finish {
            yield ChatStreamEventV1::Finish { reason };
        }
    };
    Box::pin(stream)
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn pending_finish_is_not_published_after_transport_failure() {
        use futures_util::StreamExt;
        let raw: crate::ByteStream = Box::pin(futures_util::stream::iter(vec![
            Ok(crate::StreamChunk {
                cache_read_observation: None,
                data: bytes::Bytes::from_static(
                    b"data: {\"candidates\":[{\"finishReason\":\"STOP\"}]}\n\n",
                ),
                usage: None,
                usage_running: None,
                attempts: 1,
                terminal: false,
            }),
            Err(crate::ProviderError::Transport("mock disconnect".into())),
        ]));
        let events = super::decode_gemini_stream(raw, "m".into())
            .collect::<Vec<_>>()
            .await;
        assert!(matches!(events.last(), Some(Err(_))));
        assert!(!events
            .iter()
            .any(|event| matches!(event, Ok(sandhi_core::ChatStreamEventV1::Finish { .. }))));
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
        let wire = format!(
            "data: {{\"candidates\":[{{\"content\":{{\"parts\":[{{\"text\":\"{big}\"}}]}}}}]}}\n\n"
        );
        let chunks: Vec<_> = wire
            .as_bytes()
            .chunks(16 * 1024)
            .map(|c| {
                Ok(crate::StreamChunk {
                    cache_read_observation: None,
                    data: bytes::Bytes::copy_from_slice(c),
                    usage: None,
                    usage_running: None,
                    attempts: 1,
                    terminal: false,
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
                    cache_read_observation: None,
                    data: filler.clone(),
                    usage: None,
                    usage_running: None,
                    attempts: 1,
                    terminal: false,
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

    #[test]
    fn tool_results_resolve_against_preceding_calls_when_ids_repeat_between_turns() {
        let request: ChatRequestV1 = serde_json::from_value(json!({"model":"fixture","messages":[
            {"role":"user","content":"hello"},
            {"role":"assistant","tool_calls":[{"id":"reused","name":"alpha","arguments":"{}"}]},
            {"role":"tool","tool_call_id":"reused","content":"alpha-result"},
            {"role":"assistant","tool_calls":[{"id":"reused","name":"beta","arguments":"{}"}]},
            {"role":"tool","tool_call_id":"reused","content":"beta-result"}
        ]}))
        .unwrap();
        let body = encode_gemini_request(&request).unwrap();
        assert_eq!(
            body["contents"][2]["parts"][0]["functionResponse"]["name"],
            "alpha"
        );
        assert_eq!(
            body["contents"][4]["parts"][0]["functionResponse"]["name"],
            "beta"
        );
    }

    #[test]
    fn buffered_generated_tool_ids_are_distinct_between_responses_and_keep_tool_finish() {
        let reply = |name| json!({"candidates":[{"content":{"parts":[{"functionCall":{"name":name,"args":{}}}]},"finishReason":"STOP"}]});
        let alpha = decode_gemini_response(reply("alpha"), Default::default(), "fixture").unwrap();
        let beta = decode_gemini_response(reply("beta"), Default::default(), "fixture").unwrap();
        assert_ne!(alpha.output.tool_calls[0].id, beta.output.tool_calls[0].id);
        assert_eq!(alpha.finish_reason, Some(FinishReasonV1::ToolCalls));
    }

    #[test]
    fn gemini_tool_results_cannot_reference_future_or_duplicate_outstanding_calls() {
        let assistant = |name| json!({"role":"assistant","tool_calls":[{"id":"call","name":name,"arguments":"{}"}]});
        let result = json!({"role":"tool","tool_call_id":"call","content":"result"});
        for messages in [
            vec![result.clone(), assistant("alpha")],
            vec![assistant("alpha"), assistant("beta"), result.clone()],
            vec![assistant("alpha"), result.clone(), result],
        ] {
            let request: ChatRequestV1 =
                serde_json::from_value(json!({"model":"fixture","messages":messages})).unwrap();
            assert!(matches!(
                encode_gemini_request(&request),
                Err(ProviderError::InvalidRequest(_))
            ));
        }
    }

    #[tokio::test]
    async fn legacy_gemini_stream_call_indices_and_generated_identities_span_chunks_and_turns() {
        use futures_util::StreamExt;
        let wire = concat!(
            "data: {\"candidates\":[{\"content\":{\"parts\":[{\"functionCall\":{\"name\":\"alpha\",\"args\":{}}}]}}]}\n\n",
            "data: {\"candidates\":[{\"content\":{\"parts\":[{\"functionCall\":{\"name\":\"beta\",\"args\":{}}}]}}]}\n\n",
            "data: {\"candidates\":[{\"finishReason\":\"STOP\"}]}\n\n");
        let mut all = std::collections::BTreeSet::new();
        for _ in 0..2 {
            let raw: ByteStream = Box::pin(futures_util::stream::iter(
                wire.as_bytes()
                    .chunks(7)
                    .map(|bytes| {
                        Ok(crate::StreamChunk {
                            data: bytes::Bytes::copy_from_slice(bytes),
                            ..Default::default()
                        })
                    })
                    .collect::<Vec<_>>(),
            ));
            let events = decode_gemini_stream(raw, "fixture".into())
                .collect::<Vec<_>>()
                .await
                .into_iter()
                .collect::<Result<Vec<_>, _>>()
                .unwrap();
            let indices = events
                .iter()
                .filter_map(|event| match event {
                    ChatStreamEventV1::ToolCallStart { index, id, .. } => {
                        assert!(all.insert(id.clone()));
                        Some(*index)
                    }
                    _ => None,
                })
                .collect::<Vec<_>>();
            assert_eq!(indices, vec![0, 1]);
            assert!(matches!(
                events.last(),
                Some(ChatStreamEventV1::Finish {
                    reason: FinishReasonV1::ToolCalls
                })
            ));
        }
        assert_eq!(all.len(), 4);
    }

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
                    cache_read_observation: None,
                    data: bytes::Bytes::copy_from_slice(&wire[..split]),
                    usage: None,
                    usage_running: None,
                    attempts: 1,
                    terminal: false,
                }),
                Ok(crate::StreamChunk {
                    cache_read_observation: None,
                    data: bytes::Bytes::copy_from_slice(&wire[split..]),
                    usage: None,
                    usage_running: None,
                    attempts: 1,
                    terminal: false,
                }),
                Ok(crate::StreamChunk {
                    cache_read_observation: None,
                    data: bytes::Bytes::new(),
                    usage: Some(crate::ParsedUsage {
                        cache_read_observation: None,
                        reasoning_included: Some(false),
                        tokens_in: 2,
                        tokens_out: 3,
                        cache_creation_tokens: 0,
                        cache_read_tokens: 0,
                        reasoning_tokens: 0,
                        duration_ms: None,
                        time_to_first_token_ms: None,
                    }),
                    usage_running: None,
                    attempts: 1,
                    terminal: true,
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
                matches!(events.last(), Some(ChatStreamEventV1::Finish { .. })),
                "split {split}: Finish must follow final usage, including an empty terminal chunk"
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
        assert_eq!(response.finish_reason, Some(FinishReasonV1::ToolCalls));
    }
}
